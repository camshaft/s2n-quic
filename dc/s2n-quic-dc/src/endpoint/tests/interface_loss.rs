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
//! pairs, the client writes rounds of eight streams until that interface carries more than an
//! eighth of a round's bytes.
//!
//! The endpoints record each acknowledgement and freed batch they route to a send worker. None
//! may go to a sender whose interface pair its path marks lost while another sender of that path
//! delivers, and the endpoints must route at least one of each while a pair of its path is lost.

use crate::{
    endpoint::routing::observe::{self, Kind, Routed},
    socket::{interface, rate::Rate},
    stream::endpoint::testing::sim::{Client, SimEndpointConfig},
    testing::{ext::*, sim, without_snapshots, without_tracing},
};
use bach::time::timeout;
use s2n_quic_core::{stream::testing::Data, varint::VarInt};
use std::{
    io,
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
/// How long the client may wait, once the interface delivers again, for its first write on it,
/// and then for a round of streams in which that interface carries its share: a lost pair probes
/// again at its next probe timeout, whose backoff grows with the outage.
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(300);
/// How long the client waits after each stream while the interface recovers, so that the sim
/// doesn't carry one stream after another until the lost pairs probe again.
const RECOVERY_PAUSE: Duration = Duration::from_secs(1);
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
    /// Whether the monitor watches for a socket write from the client's lost interface to the
    /// server.
    watching: AtomicBool,
    /// Whether the client wrote from its lost interface to the server while the monitor
    /// watched.
    wrote_on_lost: AtomicBool,
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

    /// The bytes the monitor recorded from each of the client's interfaces to the server.
    fn client_bytes(&self) -> [u64; 2] {
        let client_addrs = self.client_addrs.lock().unwrap();
        let server_addrs = self.server_addrs.lock().unwrap();
        // In the sim, send socket i is recv socket i, so the position of an address in
        // `data_addrs` is its socket, and `CLIENT_INTERFACE` holds that socket's interface.
        let mut bytes = [0u64; 2];
        for write in self.writes.lock().unwrap().iter() {
            let Some(socket) = client_addrs.iter().position(|&addr| addr == write.local) else {
                continue;
            };
            if server_addrs.contains(&write.peer) {
                bytes[CLIENT_INTERFACE[socket]] += write.bytes;
            }
        }
        bytes
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

    /// Records a socket write from `local` to `peer` where the monitor watches for one from the
    /// client's lost interface to the server.
    fn on_write(&self, local: SocketAddr, peer: SocketAddr) {
        if !self.watching.load(Ordering::Relaxed) {
            return;
        }
        let from_lost = self.lost_addrs.lock().unwrap().contains(&local);
        if from_lost && self.server_addrs.lock().unwrap().contains(&peer) {
            self.wrote_on_lost.store(true, Ordering::Relaxed);
        }
    }

    /// Whether the client wrote from its lost interface to the server while the monitor
    /// watched.
    fn wrote_on_lost_interface(&self) -> bool {
        self.wrote_on_lost.load(Ordering::Relaxed)
    }
}

/// Whether the client's lost interface carried more than an eighth of `bytes`, the bytes the
/// client wrote on each of its interfaces: the lost interface is the faster one, so once it
/// delivers again it carries more than that.
fn restored_share(bytes: [u64; 2]) -> bool {
    bytes[LOST] * 8 > bytes.iter().sum::<u64>()
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

/// Panics with `round`, the failed `step` and its `error`, and the bytes the server read and
/// the packets the network dropped until then.
fn fail(round: &str, step: &str, error: impl core::fmt::Debug, shared: &Shared) -> ! {
    panic!(
        "{round}: {step}: {error:?}; the server read {} bytes and the network dropped {} packets",
        shared.read.load(Ordering::Relaxed),
        shared.dropped.load(Ordering::Relaxed),
    )
}

/// Writes `count` streams of `STREAM_LEN` bytes to the server at once, and waits for the
/// server's answer to each. A failure states `round`, and the bytes the server read and the
/// packets the network dropped until then.
async fn exchange(client: &mut Client, count: usize, round: &'static str, shared: &Arc<Shared>) {
    let mut handles = Vec::with_capacity(count);
    for _ in 0..count {
        let stream = client
            .connect("server:0", ACCEPTOR_ID)
            .await
            .unwrap_or_else(|error| fail(round, "connect", error, shared));
        let shared = shared.clone();
        let handle = async move {
            let (mut reader, mut writer) = stream.into_split();
            let mut request = Data::new(STREAM_LEN);
            if let Err(error) = writer.write_all_from_fin(&mut request).await {
                fail(round, "client write", error, &shared);
            }
            let mut reply = Data::new(REPLY_LEN);
            loop {
                match reader.read_into(&mut reply).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) => fail(round, "client read", error, &shared),
                }
            }
            assert!(reply.is_finished(), "{round}: a reply ended early");
        }
        .primary()
        .spawn();
        handles.push(handle);
    }
    for handle in handles {
        handle
            .await
            .expect("a stream's task ended before its answer");
    }
}

/// The acceptor of the streams the test resets both ways.
const RESET_ACCEPTOR_ID: VarInt = VarInt::from_u32(2);
/// The bytes each side writes on a stream the test resets.
const RESET_STREAM_LEN: u64 = 4;

/// Opens a stream to the server's reset acceptor, writes a few bytes and reads the server's
/// answer, without a FIN either way. The client then drops its reader, which resets the server's
/// writer, and the server then drops its reader, which resets the client's writer. Each reset is
/// its endpoint's highest-priority traffic.
async fn reset_round_trip(client: &mut Client) {
    let stream = client
        .connect("server:0", RESET_ACCEPTOR_ID)
        .await
        .expect("connect");
    let (mut reader, mut writer) = stream.into_split();
    let mut request = Data::new(RESET_STREAM_LEN);
    writer
        .write_all_from(&mut request)
        .await
        .expect("client write");
    let mut reply = Data::new(RESET_STREAM_LEN);
    while !reply.is_finished() {
        let len = reader.read_into(&mut reply).await.expect("client read");
        assert!(len > 0, "the server's answer ended early");
    }
    // The client stops reading before the server's FIN, so this drop resets the server's writer.
    drop(reader);
    let error = writer.peer_cancellation().await;
    assert_eq!(
        error.kind(),
        io::ErrorKind::ConnectionReset,
        "the server's reset didn't reach the client: {error:?}",
    );
}

/// While one of the client's interfaces is lost, the client and the server move their traffic to
/// the pairs that still deliver, so the streams the client opened before the loss finish, a reset
/// sent each way on a new stream gets through though the lost interface carries each endpoint's
/// highest-priority traffic, and the expiry of the lost pairs' send contexts doesn't mark the
/// peer dead. Once the interface delivers again, its pairs carry traffic again.
#[test]
fn traffic_moves_off_a_lost_interface_and_back() {
    let _snapshots = without_snapshots();
    let _tracing = without_tracing();

    let shared = Arc::new(Shared::default());
    let in_sim = shared.clone();
    let routed = Arc::new(Mutex::new(Vec::<Routed>::new()));
    let routed_in_sim = routed.clone();

    sim(move || {
        // The endpoints route on the thread that runs the sim, which records what they route.
        observe::record_into(routed_in_sim);
        let monitor = in_sim.clone();
        bach::net::monitor::on_socket_write(move |write| {
            monitor.on_write(*write.local_addr, *write.peer_addr);
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
            // The server answers a stream to the reset acceptor, waits for the client's reset of
            // its writer, and then resets the client's writer.
            let mut reset_acceptor = server
                .register_acceptor_channel(RESET_ACCEPTOR_ID, 1)
                .expect("reset acceptor registration");
            async move {
                while let Some(stream) = reset_acceptor.recv().await {
                    async move {
                        let (mut reader, mut writer) = stream.into_split();
                        let mut request = Data::new(RESET_STREAM_LEN);
                        while !request.is_finished() {
                            let len = reader.read_into(&mut request).await.expect("server read");
                            assert!(len > 0, "the client's request ended early");
                        }
                        let mut reply = Data::new(RESET_STREAM_LEN);
                        writer
                            .write_all_from(&mut reply)
                            .await
                            .expect("server write");
                        let error = writer.peer_cancellation().await;
                        assert_eq!(
                            error.kind(),
                            io::ErrorKind::ConnectionReset,
                            "the client's reset didn't reach the server: {error:?}",
                        );
                        // The server stops reading before the client's FIN, so this drop resets
                        // the client's writer.
                        drop(reader);
                    }
                    .primary()
                    .spawn();
                }
            }
            .spawn();
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
            timeout(
                ROUND_TIMEOUT,
                exchange(&mut client, STREAMS, "the first round", &client_side),
            )
            .await
            .expect("the streams finish while an interface is lost");
            assert!(
                client_side.losing.load(Ordering::Relaxed),
                "the interface wasn't lost",
            );

            // Each endpoint's resets are its highest-priority traffic, whose pairs cross the lost
            // interface, so they make their round trip on the pairs that still deliver.
            timeout(ROUND_TIMEOUT, reset_round_trip(&mut client))
                .await
                .expect("a reset makes its round trip while an interface is lost");

            // Neither endpoint sends for longer than the paths' idle timeout, so the lost pairs'
            // send contexts expire with their probes unacknowledged. The peer isn't marked dead,
            // so the next connect isn't refused, and a stream the client opens while the
            // interface is lost gets its answer.
            QUIET.sleep().await;
            timeout(
                ROUND_TIMEOUT,
                exchange(&mut client, 1, "the quiet round", &client_side),
            )
            .await
            .expect("a new stream finishes while an interface is lost");

            // The interface delivers again, and its pairs probe at their next probe timeouts,
            // whose backoff grows with the outage. The client opens one stream a second until it
            // writes on that interface again, and then opens the streams whose writes the test
            // counts.
            client_side.losing.store(false, Ordering::Relaxed);
            client_side.watching.store(true, Ordering::Relaxed);
            timeout(RECOVERY_TIMEOUT, async {
                while !client_side.wrote_on_lost_interface() {
                    timeout(
                        ROUND_TIMEOUT,
                        exchange(&mut client, 1, "a recovery round", &client_side),
                    )
                    .await
                    .expect("a stream finishes while the interface recovers");
                    RECOVERY_PAUSE.sleep().await;
                }
            })
            .await
            .expect("the client writes on the interface again once it delivers");
            client_side.recording.store(true, Ordering::Relaxed);

            // The client writes rounds of streams until that interface carries more than an
            // eighth of a round's bytes, and the monitor keeps the writes of the last round.
            timeout(RECOVERY_TIMEOUT, async {
                loop {
                    client_side.writes.lock().unwrap().clear();
                    timeout(
                        ROUND_TIMEOUT,
                        exchange(&mut client, STREAMS, "a recovery round", &client_side),
                    )
                    .await
                    .expect("the streams finish once the interface delivers again");
                    if restored_share(client_side.client_bytes()) {
                        break;
                    }
                }
            })
            .await
            .expect("the interface carries its share of a round's bytes once it delivers again");
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

    let client_sockets = shared.client_addrs.lock().unwrap().len();
    let server_sockets = shared.server_addrs.lock().unwrap().len();
    assert_eq!(client_sockets, 8, "the client has eight sockets");
    assert_eq!(server_sockets, 8, "the server has eight sockets");

    // The last round of streams ended once the client's lost interface carried its share, and
    // the monitor holds that round's writes.
    let restored_bytes = shared.client_bytes();
    assert!(
        restored_share(restored_bytes),
        "the client wrote {restored_bytes:?} bytes on its interfaces in the last round",
    );

    // While a pair of a path was marked lost and another delivered, neither endpoint routed an
    // acknowledgement or a freed batch of that path to a sender whose pair was marked lost, and
    // the endpoints routed at least one of each.
    let routed = routed.lock().unwrap();
    let (mut acks, mut freed) = (0, 0);
    for route in routed
        .iter()
        .filter(|route| route.path_has_lost && route.path_delivers)
    {
        assert!(
            !route.lost,
            "{route:?} went to a lost sender while another sender of its path delivers",
        );
        match route.kind {
            Kind::PendingAck => acks += 1,
            Kind::PendingFreed => freed += 1,
        }
    }
    assert!(acks > 0, "no acknowledgement was routed while a pair was lost");
    assert!(freed > 0, "no freed batch was routed while a pair was lost");
}
