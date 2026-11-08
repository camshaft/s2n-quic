// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests that each endpoint sends its acknowledgements on the interface pairs that their packets
//! crossed, and marks a sender lost only while an interface its traffic crosses has failed,
//! between a host with two interfaces and a host with three.
//!
//! The client puts its eight sockets on two interfaces in turn, four on each. The server puts
//! its sixteen sockets on three interfaces: four on the first, four on the second and eight on
//! the third, since the endpoint needs a power of two of send sockets. Each test fails one
//! interface of one host: the network drops every packet to or from that interface's sockets.
//! The client writes a round of streams to the server, and the server answers each stream with
//! as many bytes, once before the failure and once while the interface has failed. Both rounds
//! must finish.
//!
//! The endpoints record each acknowledgement they route to a send worker, with the senders of
//! its path that deliver when they route it. Each must go from the interface that the
//! acknowledged packets arrived on to the peer's interface that they came from, on a sender
//! that delivers where one of its path does. It may leave that pair only where no sender of the
//! pair delivers, and then goes on a delivering sender of the interface the packets arrived on
//! where one delivers, and else on any delivering sender. While the routers prefer the senders
//! the peer acknowledged since the path's lost pairs stopped delivering, a sender delivers where
//! the peer acknowledged it since then, and otherwise where its pair isn't marked lost. Each
//! endpoint must send at least one acknowledgement on its pair from each of its interfaces, so
//! that the check can't pass where an interface doesn't carry any.
//!
//! Neither endpoint may mark a sender lost in the first round. Once the second round finishes,
//! each sender that either endpoint marks lost must write to or from the failed interface, and at
//! least one sender must be marked lost.

use crate::{
    endpoint::{
        id::LocalSenderId,
        routing::observe::{self, Kind, Routed},
    },
    socket::interface,
    stream::endpoint::testing::sim::{lookup_sim_map, Client, SimEndpointConfig},
    testing::{ext::*, sim, without_snapshots, without_tracing},
};
use bach::time::timeout;
use s2n_quic_core::{stream::testing::Data, varint::VarInt};
use std::{
    collections::BTreeSet,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const ACCEPTOR_ID: VarInt = VarInt::from_u32(1);
const STREAMS: usize = 16;
const STREAM_LEN: u64 = 256 * 1024;
/// How long each round of streams may take before the test fails.
const ROUND_TIMEOUT: Duration = Duration::from_secs(60);
/// The interface of each client socket: the even sockets on the first one, the odd ones on the
/// second.
const CLIENT_INTERFACE: [usize; 8] = [0, 1, 0, 1, 0, 1, 0, 1];
/// The interface of each server socket: sockets 0, 4, 8 and 12 on the first one, 1, 5, 9 and 13
/// on the second, and the other eight on the third.
const SERVER_INTERFACE: [usize; 16] = [0, 1, 2, 2, 0, 1, 2, 2, 0, 1, 2, 2, 0, 1, 2, 2];

/// One of the two endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Host {
    Client,
    Server,
}

impl Host {
    /// The interface of each of the host's sockets.
    fn layout(self) -> &'static [usize] {
        match self {
            Self::Client => &CLIENT_INTERFACE,
            Self::Server => &SERVER_INTERFACE,
        }
    }

    /// How many interfaces the host has.
    fn interface_count(self) -> usize {
        match self {
            Self::Client => 2,
            Self::Server => 3,
        }
    }

    /// The other endpoint.
    fn peer(self) -> Self {
        match self {
            Self::Client => Self::Server,
            Self::Server => Self::Client,
        }
    }
}

/// The interface a test fails: the interface `interface` of `host`.
#[derive(Clone, Copy, Debug)]
struct Failure {
    host: Host,
    interface: usize,
}

impl Failure {
    /// Whether the traffic of the sender at `socket` of `host` crosses the failed interface,
    /// where `peer_interface` is the peer's interface that the sender writes to, if it wrote.
    fn crosses(self, host: Host, socket: usize, peer_interface: Option<usize>) -> bool {
        if host == self.host {
            host.layout()[socket] == self.interface
        } else {
            peer_interface == Some(self.interface)
        }
    }
}

/// What the network and the endpoints share in the sim.
#[derive(Default)]
struct Shared {
    /// Whether the network drops the packets to and from the failed interface.
    failing: AtomicBool,
    /// The addresses of the failed interface's sockets.
    failed_addrs: Mutex<Vec<SocketAddr>>,
    /// The packets the network dropped.
    dropped: AtomicU64,
    /// The local and peer addresses of each socket write.
    writes: Mutex<BTreeSet<(SocketAddr, SocketAddr)>>,
    /// The client's data addresses, by socket.
    client_addrs: Mutex<Vec<SocketAddr>>,
    /// The server's data addresses, by socket.
    server_addrs: Mutex<Vec<SocketAddr>>,
}

impl Shared {
    /// The data addresses of `host`, by socket.
    fn addrs(&self, host: Host) -> Vec<SocketAddr> {
        match host {
            Host::Client => self.client_addrs.lock().unwrap().clone(),
            Host::Server => self.server_addrs.lock().unwrap().clone(),
        }
    }

    /// Makes the network drop every packet to or from the sockets of the failed interface.
    fn start_failure(&self, failure: Failure) {
        let addrs = self.addrs(failure.host);
        *self.failed_addrs.lock().unwrap() = addrs
            .iter()
            .zip(failure.host.layout())
            .filter(|&(_, &interface)| interface == failure.interface)
            .map(|(&addr, _)| addr)
            .collect();
        self.failing.store(true, Ordering::Relaxed);
    }

    /// Whether the network drops a packet from `source` to `destination`.
    fn drops(&self, source: SocketAddr, destination: SocketAddr) -> bool {
        if !self.failing.load(Ordering::Relaxed) {
            return false;
        }
        let failed_addrs = self.failed_addrs.lock().unwrap();
        failed_addrs.contains(&source) || failed_addrs.contains(&destination)
    }
}

/// The interfaces of `host`, each with the sockets its layout puts on it, at the endpoint's
/// overall send rate.
fn interfaces(host: Host) -> Vec<interface::Config> {
    let layout = host.layout();
    (0..host.interface_count())
        .map(|index| {
            let sockets: Vec<usize> = (0..layout.len())
                .filter(|&socket| layout[socket] == index)
                .collect();
            interface::Config {
                send_sockets: sockets.clone(),
                recv_sockets: sockets,
                ..Default::default()
            }
        })
        .collect()
}

/// The peer's interface that each socket of `host` writes to, by socket, or `None` for a socket
/// that didn't write to the peer. Each socket must write to one interface of the peer alone.
fn peer_interfaces(host: Host, shared: &Shared) -> Vec<Option<usize>> {
    let local_addrs = shared.addrs(host);
    let peer_addrs = shared.addrs(host.peer());
    let mut interfaces = vec![None; local_addrs.len()];
    // In the sim, send socket i is recv socket i, so the position of an address in `data_addrs`
    // is its socket, and the host's layout holds that socket's interface.
    for &(local, peer) in shared.writes.lock().unwrap().iter() {
        let Some(socket) = local_addrs.iter().position(|&addr| addr == local) else {
            continue;
        };
        let Some(peer_socket) = peer_addrs.iter().position(|&addr| addr == peer) else {
            continue;
        };
        let peer_interface = host.peer().layout()[peer_socket];
        let first = *interfaces[socket].get_or_insert(peer_interface);
        assert_eq!(
            first, peer_interface,
            "{host:?} socket {socket} wrote to the peer's interfaces {first} and {peer_interface}",
        );
    }
    interfaces
}

/// Each sender whose pair its endpoint's path secret entry for the peer marks lost, as its host
/// and its socket.
fn lost_senders(shared: &Shared) -> Vec<(Host, usize)> {
    let mut lost = Vec::new();
    for host in [Host::Client, Host::Server] {
        let entry = path_entry(&shared.addrs(host), &shared.addrs(host.peer()));
        for socket in 0..host.layout().len() {
            if entry.is_sender_lost(sender(socket)) {
                lost.push((host, socket));
            }
        }
    }
    lost
}

/// The local sender id of the send socket at `socket`.
fn sender(socket: usize) -> LocalSenderId {
    LocalSenderId::new(VarInt::from_u32(socket as u32))
}

/// The path secret entry of the endpoint at `local` for the peer at `peer`.
fn path_entry(local: &[SocketAddr], peer: &[SocketAddr]) -> Arc<crate::path::secret::map::Entry> {
    let map = lookup_sim_map(local[0]).expect("the endpoint registered its path secret map");
    peer.iter()
        .find_map(|&addr| map.get_raw(addr))
        .expect("the endpoint holds a path secret for the peer")
}

/// Panics with `round`, the failed `step` and its `error`, and the packets the network dropped
/// until then.
fn fail(round: &str, step: &str, error: impl core::fmt::Debug, shared: &Shared) -> ! {
    panic!(
        "{round}: {step}: {error:?}; the network dropped {} packets",
        shared.dropped.load(Ordering::Relaxed),
    )
}

/// Writes `STREAMS` streams of `STREAM_LEN` bytes to the server at once, and reads the server's
/// answer of `STREAM_LEN` bytes to each. A failure states `round`, and the packets the network
/// dropped until then.
async fn exchange(client: &mut Client, round: &'static str, shared: &Arc<Shared>) {
    let mut handles = Vec::with_capacity(STREAMS);
    for _ in 0..STREAMS {
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
            let mut reply = Data::new(STREAM_LEN);
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

/// Asserts that each sender either endpoint marks lost writes to or from the failed interface,
/// and that at least one sender is marked lost, so that the check can't pass where no mark is
/// set at all.
fn assert_lost_marks(failure: Failure, shared: &Shared) {
    let lost = lost_senders(shared);
    assert!(
        !lost.is_empty(),
        "neither endpoint marked a sender lost while {failure:?} failed",
    );
    let pairs = [
        peer_interfaces(Host::Client, shared),
        peer_interfaces(Host::Server, shared),
    ];
    for (host, socket) in lost {
        let peer_interface = pairs[host as usize][socket];
        assert!(
            failure.crosses(host, socket, peer_interface),
            "{host:?} marked its socket {socket} lost while {failure:?} failed, though the socket \
             writes to the peer's interface {peer_interface:?}",
        );
    }
}

/// Asserts that each acknowledgement either endpoint routes goes from the interface that the
/// acknowledged packets arrived on to the peer's interface that they came from, or leaves that
/// pair only as the move off a sender that doesn't deliver allows, and that each endpoint sends
/// at least one acknowledgement on its pair from each of its interfaces.
///
/// The move takes the first sender that delivers, as the acknowledgement's record reads the
/// senders, in this order: the sender that the pairing picks for the acknowledged peer sender,
/// which is on that pair, the other senders of the pair, the other senders of the interface the
/// packets arrived on, and any other sender. An acknowledgement on its pair goes on a sender that
/// delivers, unless no sender of its path delivers. The sim doesn't mark an interface as the one
/// for the endpoints' highest-priority traffic, whose senders the order takes ahead of any other
/// sender, so that step isn't checked.
fn assert_acknowledgements_paired(routed: &[Routed], shared: &Shared) {
    let client_addrs = shared.addrs(Host::Client);
    let pairs = [
        peer_interfaces(Host::Client, shared),
        peer_interfaces(Host::Server, shared),
    ];
    // The interfaces from which each endpoint sent at least one acknowledgement on its pair.
    let mut paired_from = [BTreeSet::new(), BTreeSet::new()];
    // The acknowledgements whose peer sender or local sender didn't write in the sim, which the
    // check can't place on an interface pair.
    let mut unplaced = Vec::new();
    for route in routed {
        if route.kind != Kind::PendingAck {
            continue;
        }
        // An endpoint's path has an address of the other endpoint as its peer, and in the sim
        // the peer's sender i is its send socket i.
        let host = if client_addrs.contains(&route.peer) {
            Host::Server
        } else {
            Host::Client
        };
        let peer = host.peer();
        let remote = route
            .remote
            .expect("an acknowledgement answers a peer sender");
        let remote = remote.as_varint().as_u64() as usize;
        let local = route.sender.as_usize();
        // The acknowledged packets left the peer on the interface of the peer's sender, and
        // arrived on the interface that the sender writes to. The acknowledgement leaves on the
        // interface of the local sender, and arrives on the interface that it writes to.
        let Some(arrival) = pairs[peer as usize][remote] else {
            unplaced.push(route);
            continue;
        };
        let Some(destination) = pairs[host as usize][local] else {
            unplaced.push(route);
            continue;
        };
        let source = peer.layout()[remote];
        let from = host.layout()[local];
        let marks = route
            .marks
            .expect("the endpoint records which senders of an acknowledgement's path deliver");
        // Whether the host's sender at a socket delivers, as the routers read it when the
        // endpoint routed the acknowledgement.
        let delivers = |socket: usize| marks.delivers(socket, route.path_delivers_since_lost);
        let sockets = 0..host.layout().len();
        if from == arrival && destination == source {
            assert!(
                // Call `Iterator::any` by its path: a generator trait in scope also has an
                // `any` method on ranges.
                delivers(local) || !Iterator::any(&mut sockets.clone(), delivers),
                "{host:?} acknowledged packets on their pair from its socket {local}, which \
                 doesn't deliver, while a sender of its path delivers: {route:?}",
            );
            paired_from[host as usize].insert(from);
            continue;
        }
        // The acknowledgement left its pair, so no sender of the pair may deliver, and where it
        // left the interface the packets arrived on, no sender of that interface may deliver.
        let on_arrival = |&socket: &usize| host.layout()[socket] == arrival;
        let pair_delivers = sockets
            .clone()
            .filter(on_arrival)
            .any(|socket| pairs[host as usize][socket] == Some(source) && delivers(socket));
        let arrival_delivers = sockets.filter(on_arrival).any(delivers);
        assert!(
            !pair_delivers,
            "{host:?} acknowledged packets of the peer's interface {source}, which arrived on \
             its interface {arrival}, from its interface {from} to the peer's {destination}, \
             while a sender of that pair delivers: {route:?}",
        );
        assert!(
            from == arrival || !arrival_delivers,
            "{host:?} acknowledged packets that arrived on its interface {arrival} from its \
             interface {from}, while a sender of the arrival interface delivers: {route:?}",
        );
        assert!(
            delivers(local),
            "{host:?} moved an acknowledgement off its pair to its socket {local}, which \
             doesn't deliver: {route:?}",
        );
    }
    // The sim records each socket write before the network drops its packet, so a peer sender
    // has a write once it sent the packets an endpoint acknowledges, and a local sender once it
    // sent anything: the check places every acknowledgement on a pair, and doesn't skip one.
    assert!(
        unplaced.is_empty(),
        "the check can't place {} acknowledgements on an interface pair, since their peer \
         sender or local sender didn't write in the sim: {unplaced:?}",
        unplaced.len(),
    );
    for host in [Host::Client, Host::Server] {
        for interface in 0..host.interface_count() {
            assert!(
                paired_from[host as usize].contains(&interface),
                "{host:?} didn't acknowledge a packet on its pair from its interface {interface}",
            );
        }
    }
}

/// The client's first interface fails.
#[test]
fn client_interface_0_fails() {
    run(Host::Client, 0);
}

/// The client's second interface fails.
#[test]
fn client_interface_1_fails() {
    run(Host::Client, 1);
}

/// The server's first interface fails.
#[test]
fn server_interface_0_fails() {
    run(Host::Server, 0);
}

/// The server's second interface fails.
#[test]
fn server_interface_1_fails() {
    run(Host::Server, 1);
}

/// The server's third interface, which holds half of its sockets, fails.
#[test]
fn server_interface_2_fails() {
    run(Host::Server, 2);
}

/// Fails the interface `interface` of `host` in the second of two rounds of streams, and checks
/// where the endpoints route their acknowledgements and which senders they mark lost.
fn run(host: Host, interface: usize) {
    let _snapshots = without_snapshots();
    let _tracing = without_tracing();

    let failure = Failure { host, interface };
    let shared = Arc::new(Shared::default());
    let in_sim = shared.clone();
    let routed = Arc::new(Mutex::new(Vec::<Routed>::new()));
    let routed_in_sim = routed.clone();

    sim(move || {
        // The endpoints route on the thread that runs the sim, which records what they route.
        observe::record_into(routed_in_sim);
        let monitor = in_sim.clone();
        bach::net::monitor::on_socket_write(move |write| {
            let addrs = (*write.local_addr, *write.peer_addr);
            monitor.writes.lock().unwrap().insert(addrs);
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
            let config = SimEndpointConfig {
                num_send_sockets: SERVER_INTERFACE.len(),
                num_recv_sockets: SERVER_INTERFACE.len(),
                ..Default::default()
            };
            let server = config.interfaces(interfaces(Host::Server)).server();
            *server_side.server_addrs.lock().unwrap() = server.data_addrs().to_vec();
            let mut acceptor = server
                .register_acceptor_channel(ACCEPTOR_ID, STREAMS * 2)
                .expect("acceptor registration");
            while let Some(stream) = acceptor.recv().await {
                async move {
                    let (mut reader, mut writer) = stream.into_split();
                    let mut request = Data::new(STREAM_LEN);
                    while reader.read_into(&mut request).await.expect("server read") > 0 {}
                    assert!(request.is_finished(), "a request ended early");
                    let mut reply = Data::new(STREAM_LEN);
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
                .interfaces(interfaces(Host::Client))
                .client();
            *client_side.client_addrs.lock().unwrap() = client.data_addrs().to_vec();

            // Every interface delivers in the first round, so neither endpoint marks a sender
            // lost.
            timeout(
                ROUND_TIMEOUT,
                exchange(&mut client, "the first round", &client_side),
            )
            .await
            .expect("the first round finishes");
            let lost = lost_senders(&client_side);
            assert!(
                lost.is_empty(),
                "the endpoints marked {lost:?} lost before an interface failed",
            );

            // The network drops each packet to or from the failed interface from now on.
            client_side.start_failure(failure);
            timeout(
                ROUND_TIMEOUT,
                exchange(&mut client, "the second round", &client_side),
            )
            .await
            .expect("the second round finishes while an interface has failed");
            assert_lost_marks(failure, &client_side);
        }
        .group("client")
        .primary()
        .spawn();
    });

    let routed = routed.lock().unwrap();
    assert_acknowledgements_paired(&routed, &shared);
    assert!(
        shared.dropped.load(Ordering::Relaxed) > 0,
        "the network didn't drop a packet to or from the failed interface",
    );
}
