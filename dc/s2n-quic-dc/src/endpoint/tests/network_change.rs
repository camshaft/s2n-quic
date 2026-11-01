// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests of a phone's network changes: a move to another Wi-Fi network, a change of the
//! address the host sees, and the app's suspension and resumption.
//!
//! In each test a phone (the client) uploads to a host (the server) and waits for the host's
//! answer, and the network changes in the middle of the upload. Each test asserts that the
//! phone's reader and writer each end in an `io::Error` of the expected kind within a bound,
//! or that the work completes on a new connection within a bound. Every read, write and
//! connect is wrapped in a timeout, so a stream that stops without an error fails the test.
//!
//! How the changes are modeled: in bach each group has one address, and a network monitor can
//! drop a packet but can't change its addresses. The endpoint doesn't route a stream's packets
//! by their source address: a receive context is keyed by the credential id and the sender id
//! (`recv::Cache::get_or_insert`), and a send context sends to the addresses the peer
//! advertised in its path-secret entry (`send::Context::new`). So a packet from a new source
//! address is handled as one from the old address, and each test models a change by what it
//! does to delivery:
//!
//! - After a move or a rebinding, the phone's packets still reach the host, and the host's
//!   packets to the address it knows are lost.
//! - A new network is a new bach group, which has an address of its own.
//! - A suspension drops every packet both ways.

use crate::{
    stream::{
        endpoint::{
            testing::sim::{lookup_sim_map, Client, MonitorHostAddr, Server},
            DEFAULT_DEAD_PEER_COOLDOWN,
        },
        Reader, Stream,
    },
    testing::{ext::*, sim},
    tracing::*,
};
use bach::time::timeout;
use bytes::{Bytes, BytesMut};
use s2n_quic_core::{stream::testing::Data, varint::VarInt};
use std::{
    io,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::oneshot;

const ACCEPTOR_ID: VarInt = VarInt::from_u32(1);

/// The phone's upload: larger than the 1 MiB initial flow-control window, so the phone's
/// writer is still sending, or waits for MAX_DATA, when the network changes.
const UPLOAD_LEN: u64 = 4 * 1024 * 1024;

/// The network changes once the host has read this much of the first upload.
const CHANGE_AT: u64 = 256 * 1024;

/// The host's answer, with FIN, once the whole upload arrived.
const ANSWER: &[u8] = b"done";

/// What the host's handler of the first stream it accepts reports.
struct FirstStream {
    /// Sent once the host has read `CHANGE_AT` bytes of the upload.
    mid_stream: oneshot::Sender<()>,
    /// How the host's read of the upload ended: the bytes read before its end, or the error.
    read: Option<oneshot::Sender<io::Result<u64>>>,
}

/// Runs the host in the "server" group. It accepts each stream, reads the phone's upload to
/// its end, and writes `ANSWER` with FIN once the whole upload arrived. The handler of the
/// first stream reports on `first`.
fn spawn_host(first: FirstStream) {
    async move {
        let server = Server::new();
        let mut acceptor = server
            .register_acceptor_channel(ACCEPTOR_ID, 8)
            .expect("acceptor registration failed");
        let mut first = Some(first);

        while let Some(stream) = acceptor.recv().await {
            let report = first.take();
            async move {
                let (mut reader, mut writer) = stream.into_split();
                let (mut mid_stream, read_report) = match report {
                    Some(report) => (Some(report.mid_stream), report.read),
                    None => (None, None),
                };

                let mut upload = Data::new(UPLOAD_LEN);
                let mut received = 0u64;
                let read = loop {
                    match reader.read_into(&mut upload).await {
                        Ok(0) => break Ok(received),
                        Ok(n) => {
                            received += n as u64;
                            if received >= CHANGE_AT {
                                if let Some(mid_stream) = mid_stream.take() {
                                    let _ = mid_stream.send(());
                                }
                            }
                        }
                        Err(err) => break Err(err),
                    }
                };
                info!(?read, "host read of the upload ended");

                let complete = read.is_ok() && upload.is_finished();
                if let Some(read_report) = read_report {
                    let _ = read_report.send(read);
                }
                if complete {
                    let mut answer = Bytes::from_static(ANSWER);
                    let _ = writer.write_all_from_fin(&mut answer).await;
                }
            }
            .spawn();
        }
    }
    .group("server")
    .spawn();
}

/// The idle timeout of the phone's path-secret entry for the host. A send context of the
/// phone that has packets the host hasn't acknowledged expires this long after the last packet
/// from the host, and marks the host dead (`send_idle_wheel_drain` in `endpoint/tasks.rs`).
fn host_idle_timeout(phone: &Client, host: SocketAddr) -> Duration {
    lookup_sim_map(phone.data_addr())
        .expect("the phone's path-secret map is registered")
        .get_raw(host)
        .expect("the phone holds a path-secret entry for the host")
        .idle_timeout()
}

/// Reads `reader` to its end into `buf`.
async fn read_to_end(reader: &mut Reader, buf: &mut BytesMut) -> io::Result<()> {
    loop {
        if reader.read_into(buf).await? == 0 {
            return Ok(());
        }
    }
}

/// Reads the host's answer to its end.
async fn read_answer(reader: &mut Reader) -> io::Result<BytesMut> {
    let mut answer = BytesMut::with_capacity(64);
    read_to_end(reader, &mut answer).await?;
    Ok(answer)
}

/// Asserts that `result`, a read, write or connect wrapped in a timeout, ended within its
/// bound in an error of `kind`.
#[track_caller]
fn assert_error_kind<T, E>(what: &str, result: Result<io::Result<T>, E>, kind: io::ErrorKind) {
    match result {
        Err(_elapsed) => {
            panic!("{what} was still pending at its bound; expected an error of kind {kind:?}")
        }
        Ok(Ok(_)) => panic!("{what} succeeded; expected an error of kind {kind:?}"),
        Ok(Err(err)) => assert_eq!(
            err.kind(),
            kind,
            "{what} ended in {err:?}; expected an error of kind {kind:?}"
        ),
    }
}

/// Runs the phone's work on `stream`, the old connection, and changes the network in the
/// middle of the upload: once the host has read `CHANGE_AT` bytes (`mid_stream`), it calls
/// `change`. Asserts that the phone's read of the answer and its upload each end in an error
/// of `kind` within `bound` of the change, and returns what `change` returned.
async fn change_mid_stream<R>(
    stream: Stream,
    mid_stream: oneshot::Receiver<()>,
    change: impl FnOnce() -> R,
    bound: Duration,
    kind: io::ErrorKind,
) -> R {
    let (mut reader, mut writer) = stream.into_split();
    let upload = async move {
        let mut data = Data::new(UPLOAD_LEN);
        writer.write_all_from_fin(&mut data).await
    }
    .spawn();

    mid_stream
        .await
        .expect("the host reads the first part of the upload");
    let changed = change();
    info!("the network changed in the middle of the upload");

    let (read, upload) = tokio::join!(
        timeout(bound, read_answer(&mut reader)),
        timeout(bound, upload),
    );
    assert_error_kind("the phone's read of the answer", read, kind);
    assert_error_kind(
        "the phone's upload",
        upload.map(|joined| joined.expect("the upload task completes")),
        kind,
    );
    changed
}

/// Does the phone's work on `stream`, a new connection: uploads `UPLOAD_LEN` bytes with FIN
/// and reads the host's answer, and asserts that both complete within `bound`.
async fn complete_work(stream: Stream, bound: Duration, what: &str) {
    let (mut reader, mut writer) = stream.into_split();
    let (upload, answer) = tokio::join!(
        timeout(bound, async {
            let mut data = Data::new(UPLOAD_LEN);
            writer.write_all_from_fin(&mut data).await
        }),
        timeout(bound, read_answer(&mut reader)),
    );
    let uploaded = upload
        .unwrap_or_else(|_| panic!("the upload {what} was still pending after {bound:?}"))
        .unwrap_or_else(|err| panic!("the upload {what} failed: {err:?}"));
    assert_eq!(uploaded as u64, UPLOAD_LEN, "the upload {what} queued every byte");
    let answer = answer
        .unwrap_or_else(|_| {
            panic!("the read of the answer {what} was still pending after {bound:?}")
        })
        .unwrap_or_else(|err| panic!("the read of the answer {what} failed: {err:?}"));
    assert_eq!(&answer[..], ANSWER, "the answer {what}");
    info!("the work completed {what}");
}

/// A move to another Wi-Fi network in the middle of an upload.
///
/// After the move, the phone's packets leave from its new address and still reach the host,
/// and the host's packets to the old address are lost, so the phone doesn't hear from the
/// host. The phone has packets the host hasn't acknowledged, so once its idle timeout passes it
/// marks the host dead, and its reader and writer end in `TimedOut`. The app then opens its
/// endpoint again on the new network, a group of its own with an address of its own, and the
/// work completes on a new connection there; from then on the old address doesn't send
/// anything. The host's read of the first upload ends in `TimedOut` too.
#[test]
fn wifi_move_mid_stream_times_out_then_completes_on_the_new_network() {
    // Snapshot disabled: the test waits through idle timeouts, and its trace would be large.
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let moved = Arc::new(AtomicBool::new(false));
        let on_new_network = Arc::new(AtomicBool::new(false));
        {
            let moved = moved.clone();
            let on_new_network = on_new_network.clone();
            let mut host = MonitorHostAddr::new("server");
            let mut old_address = MonitorHostAddr::new("client");
            bach::net::monitor::on_packet_sent(move |packet| {
                if on_new_network.load(Ordering::Relaxed) {
                    // The old network is gone: nothing sent from the old address is delivered.
                    if old_address.is_packet_source(packet) {
                        return bach::net::monitor::Command::Drop;
                    }
                } else if moved.load(Ordering::Relaxed) && host.is_packet_source(packet) {
                    // The host's packets go to the old address, which doesn't receive any more.
                    return bach::net::monitor::Command::Drop;
                }
                bach::net::monitor::Command::Pass
            });
        }

        let (mid_stream, mid_stream_rx) = oneshot::channel();
        let (host_read, host_read_rx) = oneshot::channel();
        spawn_host(FirstStream {
            mid_stream,
            read: Some(host_read),
        });

        // The phone on the old network.
        let old_network = async move {
            let mut phone = Client::new();
            let stream = phone
                .connect("server:0", ACCEPTOR_ID)
                .await
                .expect("connect on the old network");
            let idle = host_idle_timeout(&phone, stream.peer_addr());
            change_mid_stream(
                stream,
                mid_stream_rx,
                || moved.store(true, Ordering::Relaxed),
                idle * 2,
                io::ErrorKind::TimedOut,
            )
            .await;
            (phone, idle)
        }
        .group("client")
        .spawn();

        // The phone on the new network, once the old connection's reader and writer ended.
        async move {
            let (old_phone, idle) = old_network.await.expect("the old network's task ended");
            on_new_network.store(true, Ordering::Relaxed);

            let mut phone = Client::new();
            let stream = timeout(idle, phone.connect("server:0", ACCEPTOR_ID))
                .await
                .expect("the connect on the new network was still pending at its bound")
                .expect("connect on the new network");
            complete_work(stream, idle, "on the new network").await;

            assert_error_kind(
                "the host's read of the first upload",
                timeout(idle * 3, host_read_rx)
                    .await
                    .map(|sent| sent.expect("the host reports its read")),
                io::ErrorKind::TimedOut,
            );
            drop(old_phone);
        }
        .group("phone_new_network")
        .primary()
        .spawn();
    });
}

/// A change of the phone's address as the host sees it, as a NAT rebinding does, in the middle
/// of an upload.
///
/// The phone's own address doesn't change, and its packets still reach the host from the new
/// mapping. The host's packets go to the address it knows, which the rebinding doesn't forward
/// any more, so the phone doesn't hear from the host. The phone has packets the host hasn't
/// acknowledged, so once its idle timeout passes it marks the host dead, and its reader and
/// writer end in `TimedOut`. A connect right after is refused at once with `TimedOut`, since
/// the dead-peer cooldown holds. Once the phone stops sending, the host's read of the upload
/// ends in `TimedOut` too.
#[test]
fn address_change_mid_stream_times_out() {
    // Snapshot disabled: the test waits through idle timeouts, and its trace would be large.
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let rebound = Arc::new(AtomicBool::new(false));
        {
            let rebound = rebound.clone();
            let mut host = MonitorHostAddr::new("server");
            bach::net::monitor::on_packet_sent(move |packet| {
                if rebound.load(Ordering::Relaxed) && host.is_packet_source(packet) {
                    // The host's packets go to the old mapping, which doesn't forward them.
                    return bach::net::monitor::Command::Drop;
                }
                bach::net::monitor::Command::Pass
            });
        }

        let (mid_stream, mid_stream_rx) = oneshot::channel();
        let (host_read, host_read_rx) = oneshot::channel();
        spawn_host(FirstStream {
            mid_stream,
            read: Some(host_read),
        });

        async move {
            let mut phone = Client::new();
            let stream = phone
                .connect("server:0", ACCEPTOR_ID)
                .await
                .expect("connect");
            let idle = host_idle_timeout(&phone, stream.peer_addr());
            change_mid_stream(
                stream,
                mid_stream_rx,
                || rebound.store(true, Ordering::Relaxed),
                idle * 2,
                io::ErrorKind::TimedOut,
            )
            .await;

            let connect = phone.connect("server:0", ACCEPTOR_ID);
            assert_error_kind(
                "a connect within the dead-peer cooldown",
                timeout(1.s(), connect).await,
                io::ErrorKind::TimedOut,
            );

            assert_error_kind(
                "the host's read of the upload",
                timeout(idle * 3, host_read_rx)
                    .await
                    .map(|sent| sent.expect("the host reports its read")),
                io::ErrorKind::TimedOut,
            );
        }
        .group("client")
        .primary()
        .spawn();
    });
}

/// The app's suspension and resumption in the middle of an upload.
///
/// While the app is suspended, no packet goes either way, for half an idle timeout longer than
/// the idle timeout. The phone has packets the host hasn't acknowledged, so once its idle
/// timeout passes it marks the host dead, and its reader and writer end in `TimedOut`. When
/// delivery resumes, the dead-peer cooldown still holds, so a connect is refused at once with
/// `TimedOut`. Once the cooldown has passed, the work completes on a new connection.
///
/// The test doesn't assert how the host's read of the first upload ends: the host marks the
/// phone dead only where it has packets the phone hasn't acknowledged when the suspension
/// starts, which depends on when it sent its last MAX_DATA.
/// `reading_only_phone_across_a_suspension_ends_or_completes` is the case of a side that has
/// none.
#[test]
fn suspension_mid_stream_times_out_then_completes_on_a_new_connection() {
    // Snapshot disabled: the test waits through idle timeouts, and its trace would be large.
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let suspended = Arc::new(AtomicBool::new(false));
        {
            let suspended = suspended.clone();
            bach::net::monitor::on_packet_sent(move |_packet| {
                if suspended.load(Ordering::Relaxed) {
                    return bach::net::monitor::Command::Drop;
                }
                bach::net::monitor::Command::Pass
            });
        }

        let (mid_stream, mid_stream_rx) = oneshot::channel();
        spawn_host(FirstStream {
            mid_stream,
            read: None,
        });

        async move {
            let mut phone = Client::new();
            let stream = phone
                .connect("server:0", ACCEPTOR_ID)
                .await
                .expect("connect");
            let idle = host_idle_timeout(&phone, stream.peer_addr());
            let cooldown = DEFAULT_DEAD_PEER_COOLDOWN;
            let suspension = idle + idle / 2;
            assert!(
                suspension < idle + cooldown,
                "the phone resumes while its dead-peer cooldown holds"
            );

            let resumed = change_mid_stream(
                stream,
                mid_stream_rx,
                move || {
                    suspended.store(true, Ordering::Relaxed);
                    async move {
                        suspension.sleep().await;
                        suspended.store(false, Ordering::Relaxed);
                        info!("delivery resumed");
                    }
                    .spawn()
                },
                idle * 2,
                io::ErrorKind::TimedOut,
            )
            .await;
            resumed.await.expect("the suspension ends");

            let connect = phone.connect("server:0", ACCEPTOR_ID);
            assert_error_kind(
                "a connect right after the resumption",
                timeout(1.s(), connect).await,
                io::ErrorKind::TimedOut,
            );

            cooldown.sleep().await;
            let stream = timeout(idle, phone.connect("server:0", ACCEPTOR_ID))
                .await
                .expect("the connect after the cooldown was still pending at its bound")
                .expect("connect after the cooldown");
            complete_work(stream, idle, "after the resumption").await;
        }
        .group("client")
        .primary()
        .spawn();
    });
}

const FIRST_UPDATE: &[u8] = b"first update";
const SECOND_UPDATE: &[u8] = b"second update";

/// The app's suspension and resumption while the phone only reads.
///
/// The phone sends a request and reads the host's first update, so the host has acknowledged
/// every packet the phone sent. Then, while no packet goes either way for half an idle timeout
/// longer than the idle timeout, the host sends its second update. The host has packets the
/// phone hasn't acknowledged, so it marks the phone dead and fails them, and its writer doesn't
/// send a reset to a dead peer. The phone doesn't have any packet the host hasn't acknowledged,
/// so it doesn't mark the host dead. As the endpoint stands, the phone's read of the second
/// update stays pending after delivery resumes, so the test is ignored until the endpoint ends
/// that read. It asserts that the read completes, or ends in an error, within its bound.
#[test]
#[ignore = "the phone's read stays pending: a side without unacknowledged packets doesn't mark its peer dead, and its peer doesn't reset the stream"]
fn reading_only_phone_across_a_suspension_ends_or_completes() {
    // Snapshot disabled: the test waits through idle timeouts, and its trace would be large.
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let suspended = Arc::new(AtomicBool::new(false));
        {
            let suspended = suspended.clone();
            bach::net::monitor::on_packet_sent(move |_packet| {
                if suspended.load(Ordering::Relaxed) {
                    return bach::net::monitor::Command::Drop;
                }
                bach::net::monitor::Command::Pass
            });
        }

        let (suspend, suspend_rx) = oneshot::channel::<()>();

        async move {
            let server = Server::new();
            let mut acceptor = server
                .register_acceptor_channel(ACCEPTOR_ID, 8)
                .expect("acceptor registration failed");
            let mut suspend_rx = Some(suspend_rx);

            while let Some(stream) = acceptor.recv().await {
                let suspend_rx = suspend_rx.take();
                async move {
                    let (mut reader, mut writer) = stream.into_split();
                    let mut request = BytesMut::with_capacity(64);
                    read_to_end(&mut reader, &mut request)
                        .await
                        .expect("host read of the request");

                    let mut first = Bytes::from_static(FIRST_UPDATE);
                    writer
                        .write_all_from(&mut first)
                        .await
                        .expect("host write of the first update");

                    // The second update goes out while the phone is suspended.
                    if let Some(suspend_rx) = suspend_rx {
                        if suspend_rx.await.is_ok() {
                            let mut second = Bytes::from_static(SECOND_UPDATE);
                            let _ = writer.write_all_from_fin(&mut second).await;
                        }
                    }
                }
                .spawn();
            }
        }
        .group("server")
        .spawn();

        async move {
            let mut phone = Client::new();
            let stream = phone
                .connect("server:0", ACCEPTOR_ID)
                .await
                .expect("connect");
            let idle = host_idle_timeout(&phone, stream.peer_addr());
            let (mut reader, mut writer) = stream.into_split();

            let mut request = Bytes::from_static(b"subscribe");
            writer
                .write_all_from_fin(&mut request)
                .await
                .expect("phone write of the request");

            // Once the first update arrives, the host has acknowledged the request.
            let mut updates = BytesMut::with_capacity(64);
            while updates.len() < FIRST_UPDATE.len() {
                let n = timeout(idle, reader.read_into(&mut updates))
                    .await
                    .expect("the read of the first update was still pending at its bound")
                    .expect("phone read of the first update");
                assert_ne!(n, 0, "the host ended the stream before its first update");
            }
            assert_eq!(&updates[..], FIRST_UPDATE);

            suspended.store(true, Ordering::Relaxed);
            let _ = suspend.send(());
            let suspension = idle + idle / 2;
            let resumed = {
                let suspended = suspended.clone();
                async move {
                    suspension.sleep().await;
                    suspended.store(false, Ordering::Relaxed);
                    info!("delivery resumed");
                }
                .spawn()
            };

            // Within the suspension, and an idle timeout and the dead-peer cooldown after it,
            // the read of the second update completes or ends in an error.
            let bound = suspension + idle + DEFAULT_DEAD_PEER_COOLDOWN;
            let read = read_to_end(&mut reader, &mut updates);
            let rest = timeout(bound, read).await;
            resumed.await.expect("the suspension ends");
            match rest {
                Err(_elapsed) => panic!(
                    "the read of the second update was still pending {bound:?} after the suspension started"
                ),
                Ok(Ok(())) => assert_eq!(&updates[FIRST_UPDATE.len()..], SECOND_UPDATE),
                Ok(Err(err)) => assert_ne!(
                    err.kind(),
                    io::ErrorKind::Other,
                    "the read of the second update ended in an error without a kind: {err:?}"
                ),
            }
            drop(writer);
        }
        .group("client")
        .primary()
        .spawn();
    });
}
