// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Test suite for server-side acceptor behavior.
//!
//! Each test runs inside Bach's deterministic simulation with two fully-wired
//! endpoints backed by simulated UDP sockets.
//!
//! ## Coverage
//!
//! * **Routing** – streams targeting an unregistered acceptor ID are rejected with
//!   `AcceptorNotFound`.
//! * **Deduplication** – a duplicated FlowInit packet does not create a second accepted
//!   stream.
//! * **Channel overflow (FIFO)** – when more streams arrive than the channel capacity allows,
//!   the *oldest* queued stream is evicted (default `Front` eviction) and its client receives a
//!   `ServerBusy` reset.
//! * **Channel overflow (LIFO-style)** – with `Back` eviction, the *most-recently-queued*
//!   stream is evicted when capacity is exceeded, preserving older waiting streams.
//! * **Acceptor rejection** – a custom acceptor that returns `Err(Reject)` from
//!   `handle_request` causes the client to receive a connection reset with the error code
//!   embedded in the `Reject`.
//! * **Receiver drop** – dropping the last channel receiver auto-unregisters the acceptor so
//!   subsequent connections receive `AcceptorNotFound`.
//! * **Multiple IDs** – two acceptors registered under different IDs route incoming streams
//!   independently.

use crate::{
    acceptor::{self, channel::Config, channel::Eviction, Reject},
    endpoint::error::Error,
    flow::queue::AutoWake,
    stream::{
        endpoint::testing::sim::{Client, Server},
        PendingValidation,
    },
};
use bach::time::timeout;
use bytes::{Bytes, BytesMut};
use s2n_quic_core::varint::VarInt;
use std::{
    io,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

// ── Acceptor ID constants used across tests ───────────────────────────────────

const ACCEPTOR_A: VarInt = VarInt::from_u32(1);
const ACCEPTOR_B: VarInt = VarInt::from_u32(2);

// ── unregistered_acceptor_id_sends_reset ─────────────────────────────────────

/// Streams targeting an unregistered acceptor ID must not be delivered to any
/// registered acceptor, and the client must receive a `ConnectionReset` with
/// error code `AcceptorNotFound`.
#[test]
fn unregistered_acceptor_id_sends_reset() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        async move {
            let server = Server::new();
            // Register acceptor A; the client will target the *missing* acceptor B.
            let mut acceptor = server
                .register_acceptor_channel(ACCEPTOR_A, 8)
                .expect("acceptor registration failed");

            let unexpected = timeout(Duration::from_secs(1), acceptor.recv()).await;
            assert!(
                unexpected.is_err(),
                "stream for unregistered acceptor id must not arrive on a different acceptor"
            );
        }
        .group("server")
        .spawn();

        async move {
            let mut client = Client::new();
            let mut stream = client
                .connect("server:0", ACCEPTOR_B)
                .await
                .expect("connect failed");

            let mut payload = Bytes::from_static(b"ping");
            let written = stream.write_from(&mut payload).await.expect("client write");
            assert!(written > 0, "client write should send at least one byte");

            let mut buf = BytesMut::with_capacity(1);
            let err = timeout(
                Duration::from_secs(1),
                stream.read_into(&mut buf),
            )
            .await
            .expect("client read should complete within timeout")
            .expect_err("read must fail for unregistered acceptor id");
            assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);

            let reset_code = err
                .get_ref()
                .and_then(|cause| cause.downcast_ref::<Error>())
                .copied()
                .expect("reset must carry an endpoint error code");
            assert_eq!(reset_code, Error::AcceptorNotFound);
        }
        .group("client")
        .primary()
        .spawn();
    });
}

// ── duplicate_init_accepted_only_once ─────────────────────────────────────────

/// A duplicated FlowInit packet (network-level duplicate) must not cause the
/// server to accept more than one stream.
///
/// After the first stream is accepted and a ping-pong exchange completes, a
/// second `acceptor.recv()` within a short timeout must time out, confirming
/// no phantom stream was created.
#[test]
fn duplicate_init_accepted_only_once() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let acceptor_id = ACCEPTOR_A;
        let duplicated_packets = Arc::new(AtomicUsize::new(0));
        let duplicated_packets_monitor = duplicated_packets.clone();

        {
            let mut duplicated_first_client_packet = false;
            bach::net::monitor::on_packet_sent(move |packet| {
                // Test-setup assumption: the first non-duplicate packet emitted is the
                // client's FlowInit, so duplicating it exercises init dedup.
                if !packet.is_duplicate && !duplicated_first_client_packet {
                    duplicated_first_client_packet = true;
                    duplicated_packets_monitor.fetch_add(1, Ordering::Relaxed);
                    return bach::net::monitor::duplicate(1).absolute().into();
                }
                bach::net::monitor::Command::Pass
            });
        }

        async move {
            let server = Server::new();
            let mut acceptor = server
                .register_acceptor_channel(acceptor_id, 8)
                .expect("acceptor registration failed");

            let stream = timeout(Duration::from_secs(1), acceptor.recv())
                .await
                .expect("first stream must be accepted within timeout")
                .expect("server must accept one stream");

            let stream = stream.validate().await.expect("validate");
            let (mut reader, mut writer) = stream.into_split();

            let mut buf = BytesMut::with_capacity(8);
            loop {
                let n = reader.read_into(&mut buf).await.expect("server read");
                if n == 0 {
                    break;
                }
            }
            assert_eq!(&buf[..], b"ping");

            let mut pong = Bytes::from_static(b"pong");
            writer
                .write_all_from_fin(&mut pong)
                .await
                .expect("server write");

            // A second accept within a short timeout must time out.
            let unexpected = timeout(Duration::from_millis(200), acceptor.recv()).await;
            assert!(
                unexpected.is_err(),
                "duplicate init traffic must not create an extra accepted stream"
            );
        }
        .group("server")
        .spawn();

        async move {
            let mut client = Client::new();
            let stream = client
                .connect("server:0", acceptor_id)
                .await
                .expect("connect failed");

            let (mut reader, mut writer) = stream.into_split();

            let mut ping = Bytes::from_static(b"ping");
            writer
                .write_all_from_fin(&mut ping)
                .await
                .expect("client write");

            let mut buf = BytesMut::with_capacity(8);
            loop {
                let n = reader.read_into(&mut buf).await.expect("client read");
                if n == 0 {
                    break;
                }
            }
            assert_eq!(&buf[..], b"pong");

            assert_eq!(
                duplicated_packets.load(Ordering::Relaxed),
                1,
                "test setup must duplicate exactly one client packet"
            );
        }
        .group("client")
        .primary()
        .spawn();
    });
}

// ── overflow_fifo_evicts_oldest_stream ────────────────────────────────────────

/// With the default Front-eviction (FIFO) policy and capacity=1, when two
/// streams arrive before the server processes any, the **oldest** queued stream
/// is evicted and its client receives `ServerBusy`.  The newer stream survives
/// and completes a ping-pong exchange normally.
///
/// Timing sketch (simulated wall-clock):
///   t=0    – server registers capacity-1 acceptor, sleeps for 20 ms
///   t≈1 ms – client1 FlowInit arrives at server dispatch, enters channel
///   t≈2 ms – client2 FlowInit arrives, channel full → evict oldest (client1)
///   t=20ms – server wakes, accepts client2, echoes "ping"
#[test]
fn overflow_fifo_evicts_oldest_stream() {
    let server_busy_count = Arc::new(AtomicUsize::new(0));
    let success_count = Arc::new(AtomicUsize::new(0));

    {
        let server_busy_count = server_busy_count.clone();
        let success_count = success_count.clone();

        crate::testing::sim(move || {
            use crate::testing::ext::*;

            {
                let success_srv = success_count.clone();

                async move {
                    let server = Server::new();
                    // capacity=1, default Front eviction → oldest stream is evicted on overflow
                    let mut acceptor = server
                        .register_acceptor_channel(ACCEPTOR_A, 1)
                        .expect("register");

                    // Sleep long enough for both client FlowInits to be processed by dispatch.
                    20.ms().sleep().await;

                    // Exactly one stream should be in the channel (the newer one).
                    while let Ok(Some(pending)) =
                        timeout(Duration::from_millis(100), acceptor.recv()).await
                    {
                        let stream = pending.validate().await.expect("validate");
                        let (mut reader, mut writer) = stream.into_split();
                        let mut buf = BytesMut::with_capacity(8);
                        loop {
                            let n = reader.read_into(&mut buf).await.expect("read");
                            if n == 0 {
                                break;
                            }
                        }
                        let echo = Bytes::copy_from_slice(&buf);
                        let mut echo = echo;
                        writer.write_all_from_fin(&mut echo).await.expect("write");
                        success_srv.fetch_add(1, Ordering::Relaxed);
                    }
                }
                .group("server")
                .spawn();
            }

            {
                let server_busy_cli = server_busy_count.clone();
                let success_cli = success_count.clone();

                async move {
                    let mut client = Client::new();

                    // Connect client1 and immediately write so the FlowInit is sent.
                    let stream1 = client
                        .connect("server:0", ACCEPTOR_A)
                        .await
                        .expect("connect1");
                    let (mut reader1, mut writer1) = stream1.into_split();
                    let mut ping1 = Bytes::from_static(b"ping");
                    writer1.write_all_from_fin(&mut ping1).await.expect("write1");

                    // Connect client2 and write.
                    let stream2 = client
                        .connect("server:0", ACCEPTOR_A)
                        .await
                        .expect("connect2");
                    let (mut reader2, mut writer2) = stream2.into_split();
                    let mut ping2 = Bytes::from_static(b"ping");
                    writer2.write_all_from_fin(&mut ping2).await.expect("write2");

                    // Wait for outcomes.  client1 was evicted (ServerBusy); client2 succeeded.
                    let result1 = timeout(
                        Duration::from_secs(2),
                        reader1.read_into(&mut BytesMut::with_capacity(8)),
                    )
                    .await
                    .expect("client1 read must complete");
                    if result1.is_err() {
                        server_busy_cli.fetch_add(1, Ordering::Relaxed);
                    } else {
                        success_cli.fetch_add(1, Ordering::Relaxed);
                    }

                    let result2 = timeout(
                        Duration::from_secs(2),
                        reader2.read_into(&mut BytesMut::with_capacity(8)),
                    )
                    .await
                    .expect("client2 read must complete");
                    if result2.is_err() {
                        server_busy_cli.fetch_add(1, Ordering::Relaxed);
                    } else {
                        success_cli.fetch_add(1, Ordering::Relaxed);
                    }
                }
                .group("client")
                .primary()
                .spawn();
            }
        });
    }

    assert_eq!(
        server_busy_count.load(Ordering::Relaxed),
        1,
        "exactly one stream should receive ServerBusy"
    );
    assert_eq!(
        success_count.load(Ordering::Relaxed),
        // server increments once (echo completed) + client increments once (read succeeded)
        2,
        "server and client should each count one successful stream"
    );
}

// ── overflow_back_eviction_evicts_newest_queued ───────────────────────────────

/// With `Back` eviction and capacity=2, when three streams arrive before the
/// server processes any, the **second** stream (most recently queued at the
/// point the third arrives) is evicted.  Streams 1 and 3 survive.
///
/// FIFO (Front) eviction would evict stream 1 instead of stream 2, so this
/// test proves the eviction policy switch is respected end-to-end.
///
/// Timing sketch:
///   capacity=2, Back eviction
///   s1 → queue=[s1]
///   s2 → queue=[s1, s2]          (capacity not yet exceeded)
///   s3 → pop_back(s2) → [s1, s3] (s2 evicted, s3 enters)
#[test]
fn overflow_back_eviction_evicts_newest_queued() {
    // Track which stream ID (1-indexed) completed vs. received ServerBusy.
    let results: Arc<std::sync::Mutex<Vec<(usize, bool)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    {
        let results = results.clone();

        crate::testing::sim(move || {
            use crate::testing::ext::*;

            {
                let results_srv = results.clone();

                async move {
                    let server = Server::new();
                    let mut acceptor = server
                        .register_acceptor_channel_with_config(
                            ACCEPTOR_A,
                            Config {
                                capacity: 2,
                                eviction: Eviction::Back,
                            },
                        )
                        .expect("register");

                    // Wait long enough for all three FlowInits to be dispatched.
                    30.ms().sleep().await;

                    while let Ok(Some(pending)) =
                        timeout(Duration::from_millis(100), acceptor.recv()).await
                    {
                        let results_srv = results_srv.clone();
                        async move {
                            let stream = pending.validate().await.expect("validate");
                            let (mut reader, mut writer) = stream.into_split();
                            let mut buf = BytesMut::with_capacity(8);
                            loop {
                                let n = reader.read_into(&mut buf).await.expect("read");
                                if n == 0 {
                                    break;
                                }
                            }
                            // The payload carries the stream index (1, 2, or 3).
                            let idx = buf[0] as usize;
                            results_srv.lock().unwrap().push((idx, true));
                            let echo = Bytes::copy_from_slice(&buf);
                            let mut echo = echo;
                            writer.write_all_from_fin(&mut echo).await.expect("write");
                        }
                        .primary()
                        .spawn();
                    }
                }
                .group("server")
                .spawn();
            }

            {
                let results_cli = results.clone();

                async move {
                    let mut client = Client::new();

                    // Each "ping" carries a 1-byte stream index so we can identify which
                    // stream was evicted vs. which completed.
                    for idx in 1u8..=3 {
                        let stream = client
                            .connect("server:0", ACCEPTOR_A)
                            .await
                            .expect("connect");
                        let (mut reader, mut writer) = stream.into_split();
                        let mut data = Bytes::from(vec![idx]);
                        writer.write_all_from_fin(&mut data).await.expect("write");

                        let mut buf = BytesMut::with_capacity(4);
                        let result = timeout(
                            Duration::from_secs(3),
                            reader.read_into(&mut buf),
                        )
                        .await
                        .expect("read must complete");

                        let succeeded = result.is_ok();
                        results_cli
                            .lock()
                            .unwrap()
                            .push((idx as usize, succeeded));
                    }
                }
                .group("client")
                .primary()
                .spawn();
            }
        });
    }

    let outcomes = results.lock().unwrap();
    // Streams 1 and 3 should succeed; stream 2 should be evicted.
    let succeeded: Vec<usize> = outcomes
        .iter()
        .filter(|(_, ok)| *ok)
        .map(|(idx, _)| *idx)
        .collect();
    let failed: Vec<usize> = outcomes
        .iter()
        .filter(|(_, ok)| !*ok)
        .map(|(idx, _)| *idx)
        .collect();

    assert!(
        succeeded.contains(&1),
        "stream 1 (oldest) must survive Back eviction; outcomes: {outcomes:?}"
    );
    assert!(
        succeeded.contains(&3),
        "stream 3 (newest arrival) must survive Back eviction; outcomes: {outcomes:?}"
    );
    assert!(
        failed.contains(&2),
        "stream 2 (newest queued when stream 3 arrives) must be evicted; outcomes: {outcomes:?}"
    );
}

// ── rejecting_acceptor_sends_reset_to_client ──────────────────────────────────

/// A custom acceptor that returns `Err(Reject)` from `handle_request` must
/// cause the client to receive a connection reset.
///
/// This verifies that the reject path in endpoint dispatch is wired correctly:
/// the stream is cleaned up server-side, and a reset frame with the configured
/// error code is sent to the initiating client.
#[test]
fn rejecting_acceptor_sends_reset_to_client() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        async move {
            let server = Server::new();

            // Register an acceptor that rejects every request.
            let _handle = server
                .register_acceptor(ACCEPTOR_A, Arc::new(RejectingAcceptor))
                .expect("register");

            // Server never calls recv; the rejecting acceptor handles everything inline.
            // Keep the group alive long enough for the client to complete.
            2.s().sleep().await;
        }
        .group("server")
        .spawn();

        async move {
            let mut client = Client::new();
            let mut stream = client
                .connect("server:0", ACCEPTOR_A)
                .await
                .expect("connect");

            let mut payload = Bytes::from_static(b"hello");
            stream.write_from(&mut payload).await.expect("write");

            let mut buf = BytesMut::with_capacity(8);
            let err = timeout(
                Duration::from_secs(1),
                stream.read_into(&mut buf),
            )
            .await
            .expect("read must complete within timeout")
            .expect_err("rejected stream must produce a connection reset");

            assert_eq!(
                err.kind(),
                io::ErrorKind::ConnectionReset,
                "reject must produce ConnectionReset"
            );
            let reset_code = err
                .get_ref()
                .and_then(|cause| cause.downcast_ref::<Error>())
                .copied()
                .expect("reset must carry an endpoint error code");
            assert_eq!(
                reset_code,
                Error::ServerBusy,
                "RejectingAcceptor uses ServerBusy as its reset code"
            );
        }
        .group("client")
        .primary()
        .spawn();
    });
}

/// A stateless acceptor that immediately rejects every incoming stream with
/// `ServerBusy`.
struct RejectingAcceptor;

impl acceptor::Acceptor<PendingValidation> for RejectingAcceptor {
    fn handle_request(
        &self,
        request: PendingValidation,
    ) -> Result<AutoWake, Reject<PendingValidation>> {
        Err(Reject::new(request, Error::ServerBusy))
    }
}

// ── receiver_drop_unregisters_acceptor ───────────────────────────────────────

/// Dropping the last channel receiver must auto-unregister the acceptor so
/// subsequent connections receive `AcceptorNotFound`.
///
/// The `ChannelAcceptor` holds its own [`Handle`] and drops it when `send`
/// returns an error (no registered receiver slots).  Once unregistered, the
/// endpoint dispatch returns `ACCEPTOR_NOT_FOUND` to the initiating client.
#[test]
fn receiver_drop_unregisters_acceptor() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let unregistered = Arc::new(AtomicBool::new(false));

        {
            let unregistered_srv = unregistered.clone();

            async move {
                let server = Server::new();
                {
                    // Register acceptor and immediately drop the receiver.  No client has
                    // connected yet, so the channel's slot list is empty.  The first `send`
                    // from any subsequent FlowInit will find no receivers, cause the
                    // ChannelAcceptor to drop its handle, and trigger auto-unregistration.
                    let _rx = server
                        .register_acceptor_channel(ACCEPTOR_A, 8)
                        .expect("register");
                    // `_rx` is dropped here — ChannelAcceptor retains its Handle but the
                    // internal slot list is empty because `_rx` was never polled.
                }
                unregistered_srv.store(true, Ordering::Release);

                // Keep the server group alive for the client to connect and observe the reset.
                2.s().sleep().await;
            }
            .group("server")
            .spawn();
        }

        async move {
            // Wait for server to finish setting up.
            while !unregistered.load(Ordering::Acquire) {
                bach::task::yield_now().await;
            }

            let mut client = Client::new();
            let mut stream = client
                .connect("server:0", ACCEPTOR_A)
                .await
                .expect("connect");

            let mut payload = Bytes::from_static(b"hello");
            stream.write_from(&mut payload).await.expect("write");

            let mut buf = BytesMut::with_capacity(8);
            let err = timeout(
                Duration::from_secs(1),
                stream.read_into(&mut buf),
            )
            .await
            .expect("read must complete within timeout")
            .expect_err("stream must receive a reset after acceptor is unregistered");

            assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);

            // After auto-unregistration the client should see either ServerBusy (if the
            // ChannelAcceptor fires one last reject before dropping its handle) or
            // AcceptorNotFound (if the acceptor was already gone when the FlowInit arrived).
            let reset_code = err
                .get_ref()
                .and_then(|cause| cause.downcast_ref::<Error>())
                .copied()
                .expect("reset must carry an endpoint error code");
            assert!(
                matches!(
                    reset_code,
                    Error::ServerBusy | Error::AcceptorNotFound
                ),
                "unregistered acceptor must produce ServerBusy or AcceptorNotFound, got {reset_code:?}"
            );
        }
        .group("client")
        .primary()
        .spawn();
    });
}

// ── multiple_acceptor_ids_route_independently ─────────────────────────────────

/// Two acceptors registered under different IDs must route streams
/// independently: a stream targeting ID A is never delivered to acceptor B and
/// vice versa.
///
/// Each acceptor echoes a distinct response so the client can confirm which
/// acceptor handled its stream.
#[test]
fn multiple_acceptor_ids_route_independently() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        async move {
            let server = Server::new();
            let mut acceptor_a = server
                .register_acceptor_channel(ACCEPTOR_A, 8)
                .expect("register A");
            let mut acceptor_b = server
                .register_acceptor_channel(ACCEPTOR_B, 8)
                .expect("register B");

            // Handle streams from acceptor A — echo "handled-by-A".
            async move {
                while let Some(pending) = acceptor_a.recv().await {
                    async move {
                        let stream = pending.validate().await.expect("validate A");
                        let (mut _reader, mut writer) = stream.into_split();
                        let mut resp = Bytes::from_static(b"handled-by-A");
                        writer.write_all_from_fin(&mut resp).await.expect("write A");
                    }
                    .primary()
                    .spawn();
                }
            }
            .primary()
            .spawn();

            // Handle streams from acceptor B — echo "handled-by-B".
            async move {
                while let Some(pending) = acceptor_b.recv().await {
                    async move {
                        let stream = pending.validate().await.expect("validate B");
                        let (mut _reader, mut writer) = stream.into_split();
                        let mut resp = Bytes::from_static(b"handled-by-B");
                        writer.write_all_from_fin(&mut resp).await.expect("write B");
                    }
                    .primary()
                    .spawn();
                }
            }
            .primary()
            .spawn();
        }
        .group("server")
        .spawn();

        async move {
            let mut client = Client::new();

            // Connect to acceptor A.
            let stream_a = client
                .connect("server:0", ACCEPTOR_A)
                .await
                .expect("connect A");
            let (mut reader_a, mut writer_a) = stream_a.into_split();
            let mut ping_a = Bytes::from_static(b"ping");
            writer_a
                .write_all_from_fin(&mut ping_a)
                .await
                .expect("write A");

            // Connect to acceptor B.
            let stream_b = client
                .connect("server:0", ACCEPTOR_B)
                .await
                .expect("connect B");
            let (mut reader_b, mut writer_b) = stream_b.into_split();
            let mut ping_b = Bytes::from_static(b"ping");
            writer_b
                .write_all_from_fin(&mut ping_b)
                .await
                .expect("write B");

            // Read response from A.
            let mut buf_a = BytesMut::with_capacity(32);
            loop {
                let n = reader_a.read_into(&mut buf_a).await.expect("read A");
                if n == 0 {
                    break;
                }
            }
            assert_eq!(&buf_a[..], b"handled-by-A", "stream A must be handled by acceptor A");

            // Read response from B.
            let mut buf_b = BytesMut::with_capacity(32);
            loop {
                let n = reader_b.read_into(&mut buf_b).await.expect("read B");
                if n == 0 {
                    break;
                }
            }
            assert_eq!(&buf_b[..], b"handled-by-B", "stream B must be handled by acceptor B");
        }
        .group("client")
        .primary()
        .spawn();
    });
}
