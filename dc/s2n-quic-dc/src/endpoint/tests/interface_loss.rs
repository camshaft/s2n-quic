// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests that a peer's traffic moves to its other interfaces while one interface is lost, and
//! comes back to that interface once it delivers again.
//!
//! The client and the server each put their eight sockets on two interfaces, four on each, as
//! the interface pairing tests do: the second interface is the faster one and carries the
//! highest-priority traffic. The client writes eight streams of 1 MiB to the server, and the
//! server answers each stream once it has read it whole. Once the server has read a quarter of
//! those bytes, the network drops every packet to or from the client's second interface. The
//! streams must still finish. The interface then stays lost while neither endpoint sends for
//! longer than the paths' idle timeout, so the send contexts of the lost pairs expire with their
//! probes unacknowledged: the peer must not be marked dead, and a stream the client opens next
//! must still get its answer. The network then delivers that interface's packets again, and
//! once the client writes on that interface again, which waits for the next probes of its
//! pairs, the client's second interface must carry a share of eight more streams.

use crate::{
    socket::{interface, rate::Rate},
    stream::endpoint::testing::sim::{Client, SimEndpointConfig},
    testing::{ext::*, sim, without_snapshots, without_tracing},
};
use bach::time::timeout;
use s2n_quic_core::{stream::testing::Data, varint::VarInt};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const ACCEPTOR_ID: VarInt = VarInt::from_u32(1);
const SLOW: Rate = Rate::new(1.0);
const FAST: Rate = Rate::new(2.5);
const STREAMS: usize = 8;
const STREAM_LEN: u64 = 1024 * 1024;
const REPLY_LEN: u64 = 1024;
/// The bytes the server reads before the client's second interface stops delivering.
const LOSS_AFTER: u64 = STREAMS as u64 * STREAM_LEN / 4;
/// How long the client may take, once the interface delivers again, to write on it again: a
/// lost pair probes again at its next probe timeout, whose backoff grows with the outage.
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(300);
/// How long neither endpoint sends while the interface is lost: longer than the 30 s idle
/// timeout of the sim's paths, so that the send contexts of the lost pairs expire.
const QUIET: Duration = Duration::from_secs(45);
/// How long each round of streams may take before the test fails.
const ROUND_TIMEOUT: Duration = Duration::from_secs(60);
/// The interface of each client socket: sockets 0 to 3 on the slow one, 4 to 7 on the fast one.
const CLIENT_INTERFACE: [usize; 8] = [0, 0, 0, 0, 1, 1, 1, 1];
/// The interface of each server socket: the even sockets on the slow one, the odd ones on the
/// fast one.
const SERVER_INTERFACE: [usize; 8] = [0, 1, 0, 1, 0, 1, 0, 1];
/// The client interface the test loses.
const LOST: usize = 1;

/// One socket write: its local and peer addresses and its bytes.
struct Write {
    local: SocketAddr,
    peer: SocketAddr,
    bytes: u64,
}

/// What the network, the server and the client share in the sim.
#[derive(Default)]
struct Shared {
    /// Whether the network drops the packets to and from the client's lost interface.
    losing: AtomicBool,
    /// The client's addresses on the interface the test loses.
    lost_addrs: Mutex<Vec<SocketAddr>>,
    /// The packets the network dropped.
    dropped: AtomicU64,
    /// The bytes the server read.
    read: AtomicU64,
    /// The bytes the server read while the interface was lost.
    read_while_lost: AtomicU64,
    /// Whether the monitor records the socket writes.
    recording: AtomicBool,
    /// The socket writes the monitor recorded.
    writes: Mutex<Vec<Write>>,
    /// The client's data addresses, by socket.
    client_addrs: Mutex<Vec<SocketAddr>>,
    /// The server's data addresses, by socket.
    server_addrs: Mutex<Vec<SocketAddr>>,
}

impl Shared {
    /// Keeps the client's addresses, and the ones on the interface the test loses.
    fn set_client_addrs(&self, addrs: &[SocketAddr]) {
        *self.lost_addrs.lock().unwrap() = addrs
            .iter()
            .zip(CLIENT_INTERFACE)
            .filter(|&(_, interface)| interface == LOST)
            .map(|(&addr, _)| addr)
            .collect();
        *self.client_addrs.lock().unwrap() = addrs.to_vec();
    }

    /// Counts `len` bytes the server read, and loses the client's interface once the server
    /// has read `LOSS_AFTER` bytes.
    fn on_read(&self, len: u64) {
        if self.losing.load(Ordering::Relaxed) {
            self.read_while_lost.fetch_add(len, Ordering::Relaxed);
        }
        let before = self.read.fetch_add(len, Ordering::Relaxed);
        if before < LOSS_AFTER && before + len >= LOSS_AFTER {
            self.losing.store(true, Ordering::Relaxed);
        }
    }

    /// Whether the network drops a packet from `source` to `destination`.
    fn drops(&self, source: SocketAddr, destination: SocketAddr) -> bool {
        if !self.losing.load(Ordering::Relaxed) {
            return false;
        }
        let lost_addrs = self.lost_addrs.lock().unwrap();
        lost_addrs.contains(&source) || lost_addrs.contains(&destination)
    }

    /// Whether the monitor recorded a write from the client's lost interface to the server.
    fn wrote_on_lost_interface(&self) -> bool {
        let lost_addrs = self.lost_addrs.lock().unwrap();
        let server_addrs = self.server_addrs.lock().unwrap();
        self.writes.lock().unwrap().iter().any(|write| {
            lost_addrs.contains(&write.local) && server_addrs.contains(&write.peer)
        })
    }
}

/// Two interfaces, the first at `SLOW` and the second at `FAST`, which also carries the
/// highest-priority traffic, each with the sockets `layout` puts on it.
fn interfaces(layout: &[usize; 8]) -> Vec<interface::Config> {
    let sockets = |interface: usize| -> Vec<usize> {
        (0..layout.len())
            .filter(|&socket| layout[socket] == interface)
            .collect()
    };
    vec![
        interface::Config {
            send_rate: Some(SLOW),
            send_sockets: sockets(0),
            recv_sockets: sockets(0),
            ..Default::default()
        },
        interface::Config {
            send_rate: Some(FAST),
            send_sockets: sockets(1),
            recv_sockets: sockets(1),
            carries_highest_priority: true,
            ..Default::default()
        },
    ]
}

/// Writes `count` streams of `STREAM_LEN` bytes to the server at once, and waits for the
/// server's answer to each.
async fn exchange(client: &mut Client, count: usize) {
    let mut handles = Vec::with_capacity(count);
    for _ in 0..count {
        let stream = client
            .connect("server:0", ACCEPTOR_ID)
            .await
            .expect("connect");
        let handle = async move {
            let (mut reader, mut writer) = stream.into_split();
            let mut request = Data::new(STREAM_LEN);
            writer
                .write_all_from_fin(&mut request)
                .await
                .expect("client write");
            let mut reply = Data::new(REPLY_LEN);
            while reader.read_into(&mut reply).await.expect("client read") > 0 {}
            assert!(reply.is_finished(), "a reply ended early");
        }
        .primary()
        .spawn();
        handles.push(handle);
    }
    for handle in handles {
        handle.await.expect("a stream's task ended before its answer");
    }
}

/// While one of the client's interfaces is lost, the client and the server move their traffic to
/// the pairs that still deliver, so the streams the client opened before the loss finish, and
/// the expiry of the lost pairs' send contexts doesn't mark the peer dead. Once the interface
/// delivers again, its pairs carry traffic again.
#[test]
fn traffic_moves_off_a_lost_interface_and_back() {
    let _snapshots = without_snapshots();
    let _tracing = without_tracing();

    let shared = Arc::new(Shared::default());
    let in_sim = shared.clone();

    sim(move || {
        let monitor = in_sim.clone();
        bach::net::monitor::on_socket_write(move |write| {
            if monitor.recording.load(Ordering::Relaxed) {
                let bytes: usize = write.payload.iter().map(|slice| slice.len()).sum();
                monitor.writes.lock().unwrap().push(Write {
                    local: *write.local_addr,
                    peer: *write.peer_addr,
                    bytes: bytes as u64,
                });
            }
            Ok(())
        });

        let network = in_sim.clone();
        bach::net::monitor::on_packet_sent(move |packet| {
            if network.drops(packet.source(), packet.destination()) {
                network.dropped.fetch_add(1, Ordering::Relaxed);
                return bach::net::monitor::Command::Drop;
            }
            bach::net::monitor::Command::Pass
        });

        let server_side = in_sim.clone();
        async move {
            let server = SimEndpointConfig::default()
                .interfaces(interfaces(&SERVER_INTERFACE))
                .server();
            *server_side.server_addrs.lock().unwrap() = server.data_addrs().to_vec();
            let mut acceptor = server
                .register_acceptor_channel(ACCEPTOR_ID, STREAMS * 2)
                .expect("acceptor registration");
            while let Some(stream) = acceptor.recv().await {
                let shared = server_side.clone();
                async move {
                    let (mut reader, mut writer) = stream.into_split();
                    let mut request = Data::new(STREAM_LEN);
                    loop {
                        let len = reader.read_into(&mut request).await.expect("server read");
                        if len == 0 {
                            break;
                        }
                        shared.on_read(len as u64);
                    }
                    assert!(request.is_finished(), "a request ended early");
                    let mut reply = Data::new(REPLY_LEN);
                    writer
                        .write_all_from_fin(&mut reply)
                        .await
                        .expect("server write");
                }
                .primary()
                .spawn();
            }
        }
        .group("server")
        .spawn();

        let client_side = in_sim;
        async move {
            let mut client = SimEndpointConfig::default()
                .interfaces(interfaces(&CLIENT_INTERFACE))
                .client();
            client_side.set_client_addrs(client.data_addrs());

            // The server loses the client's second interface while it reads these streams.
            timeout(ROUND_TIMEOUT, exchange(&mut client, STREAMS))
                .await
                .expect("the streams finish while an interface is lost");
            assert!(
                client_side.losing.load(Ordering::Relaxed),
                "the interface wasn't lost",
            );

            // Neither endpoint sends for longer than the paths' idle timeout, so the lost pairs'
            // send contexts expire with their probes unacknowledged. The peer isn't marked dead,
            // so the next connect isn't refused, and a stream the client opens while the
            // interface is lost gets its answer.
            QUIET.sleep().await;
            timeout(ROUND_TIMEOUT, exchange(&mut client, 1))
                .await
                .expect("a new stream finishes while an interface is lost");

            // The interface delivers again, and its pairs probe at their next probe timeouts,
            // whose backoff grows with the outage. The client opens a stream at a time until it
            // writes on that interface again, and then opens the streams whose writes the test
            // counts.
            client_side.losing.store(false, Ordering::Relaxed);
            client_side.recording.store(true, Ordering::Relaxed);
            timeout(RECOVERY_TIMEOUT, async {
                while !client_side.wrote_on_lost_interface() {
                    timeout(ROUND_TIMEOUT, exchange(&mut client, 1))
                        .await
                        .expect("a stream finishes while the interface recovers");
                }
            })
            .await
            .expect("the client writes on the interface again once it delivers");
            client_side.writes.lock().unwrap().clear();

            timeout(ROUND_TIMEOUT, exchange(&mut client, STREAMS))
                .await
                .expect("the streams finish once the interface delivers again");
            client_side.recording.store(false, Ordering::Relaxed);
        }
        .group("client")
        .primary()
        .spawn();
    });

    let dropped = shared.dropped.load(Ordering::Relaxed);
    assert!(dropped > 0, "the network didn't drop a packet");
    let read_while_lost = shared.read_while_lost.load(Ordering::Relaxed);
    assert!(
        read_while_lost >= STREAM_LEN,
        "the server read {read_while_lost} bytes while the interface was lost",
    );

    let client_addrs = shared.client_addrs.lock().unwrap();
    let server_addrs = shared.server_addrs.lock().unwrap();
    assert_eq!(client_addrs.len(), 8, "the client has eight sockets");
    assert_eq!(server_addrs.len(), 8, "the server has eight sockets");

    // In the sim, send socket i is recv socket i, so the position of an address in `data_addrs`
    // is its socket, and `CLIENT_INTERFACE` holds that socket's interface.
    let mut restored_bytes = [0u64; 2];
    for write in shared.writes.lock().unwrap().iter() {
        let Some(socket) = client_addrs.iter().position(|&addr| addr == write.local) else {
            continue;
        };
        if server_addrs.contains(&write.peer) {
            restored_bytes[CLIENT_INTERFACE[socket]] += write.bytes;
        }
    }
    let total: u64 = restored_bytes.iter().sum();
    assert!(total > 0, "the client didn't write after the loss");
    // The lost interface is the faster one, so once it delivers again it carries more than an
    // eighth of the client's bytes.
    assert!(
        restored_bytes[LOST] * 8 > total,
        "the client wrote {restored_bytes:?} bytes on its interfaces after the loss",
    );
}
