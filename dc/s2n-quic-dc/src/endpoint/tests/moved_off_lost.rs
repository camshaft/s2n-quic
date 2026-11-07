// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests that the endpoint passes its path secret map to the tasks that encode and flush its
//! acknowledgements, `ack_burst` and `ack_completion`, and to the router of its freed batches,
//! which publish through that map the events for an acknowledgement and a freed batch that move
//! off a sender whose interface pair is marked lost.
//!
//! The server puts its eight sockets on two interfaces, in turn, and the second one carries its
//! highest-priority traffic, so the server's acknowledgements go out from its senders 1, 3, 5 and
//! 7, and its freed-batch router draws from those senders too. The client doesn't list
//! interfaces, so the server doesn't pair an acknowledgement with one of the client's
//! interfaces. After a first round of streams, the test marks those four senders lost on the
//! server's entry for the client, and checks that the server's map publishes the moves of the
//! second round's acknowledgements and freed batches to its testing subscriber.

use crate::{
    endpoint::{
        id::LocalSenderId,
        routing::observe::{self, Kind, Routed, Submitter},
    },
    path::secret::{stateless_reset::Signer, Map},
    socket::{interface, rate::Rate},
    stream::endpoint::testing::sim::{lookup_sim_map, Client, SimEndpointConfig, SERVER_PORT},
    testing::{ext::*, sim, without_snapshots, without_tracing},
};
use s2n_quic_core::{stream::testing::Data, varint::VarInt};
use std::{
    net::SocketAddr,
    sync::{atomic::Ordering, Arc, Mutex},
};

const ACCEPTOR_ID: VarInt = VarInt::from_u32(1);
/// The streams that the client writes at once in each round.
const STREAMS: usize = 4;
const STREAM_LEN: u64 = 256 * 1024;
const REPLY_LEN: u64 = 1024;
/// The interface of each server socket: the even sockets on the first one, the odd ones on the
/// second.
const SERVER_INTERFACE: [usize; 8] = [0, 1, 0, 1, 0, 1, 0, 1];
/// The server interface that carries the highest-priority traffic.
const HIGHEST_PRIORITY: usize = 1;

/// The server sockets that `SERVER_INTERFACE` puts on the interface at `position`.
fn server_sockets(position: usize) -> Vec<usize> {
    (0..SERVER_INTERFACE.len())
        .filter(|&socket| SERVER_INTERFACE[socket] == position)
        .collect()
}

/// The server's two interfaces, each with the sockets `SERVER_INTERFACE` puts on it.
fn server_interfaces() -> Vec<interface::Config> {
    (0..2)
        .map(|position| interface::Config {
            send_rate: Some(Rate::new(1.0)),
            send_sockets: server_sockets(position),
            recv_sockets: server_sockets(position),
            carries_highest_priority: position == HIGHEST_PRIORITY,
            ..Default::default()
        })
        .collect()
}

/// The local sender id of the send socket at `socket`.
fn sender(socket: usize) -> LocalSenderId {
    LocalSenderId::new(VarInt::from_u32(socket as u32))
}

/// The server's senders for its highest-priority traffic: the senders that its acknowledgements
/// go out from, and that its freed-batch router draws from.
fn priority_senders() -> Vec<LocalSenderId> {
    server_sockets(HIGHEST_PRIORITY)
        .into_iter()
        .map(sender)
        .collect()
}

/// Writes `count` streams of `STREAM_LEN` bytes to the server at once, and waits for the server's
/// answer to each.
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
        handle
            .await
            .expect("a stream's task ended before its answer");
    }
}

/// The server moves the second round's acknowledgements and freed batches off its senders for
/// the highest-priority traffic, which the test marks lost, and the server's map publishes those
/// moves, which it can do only where the endpoint passes the map to `ack_burst`,
/// `ack_completion` and the freed-batch router.
#[test]
fn the_endpoint_passes_its_path_secret_map_to_the_ack_and_freed_batch_routers() {
    let _snapshots = without_snapshots();
    let _tracing = without_tracing();

    let subscriber = Arc::new(crate::event::testing::Subscriber::no_snapshot());
    let subscriber_in_sim = subscriber.clone();
    let routed = Arc::new(Mutex::new(Vec::<Routed>::new()));
    let routed_in_sim = routed.clone();
    let client_addrs = Arc::new(Mutex::new(Vec::<SocketAddr>::new()));
    let client_addrs_in_sim = client_addrs.clone();

    sim(move || {
        let server_subscriber = subscriber_in_sim.clone();
        async move {
            // A map like the one the endpoint makes where its config doesn't hold one, which
            // publishes its events to the testing subscriber.
            let map = Map::builder()
                .with_signer(Signer::random())
                .with_capacity(50_000)
                .with_clock(crate::time::bach::Clock::default())
                .with_subscriber(server_subscriber)
                .build()
                .unwrap();
            let server = SimEndpointConfig::default()
                .interfaces(server_interfaces())
                .path_secret_map(map)
                .server();
            let mut acceptor = server
                .register_acceptor_channel(ACCEPTOR_ID, STREAMS)
                .expect("acceptor registration");
            while let Some(stream) = acceptor.recv().await {
                async move {
                    let (mut reader, mut writer) = stream.into_split();
                    let mut request = Data::new(STREAM_LEN);
                    while reader.read_into(&mut request).await.expect("server read") > 0 {}
                    assert!(request.is_finished(), "a request ended early");
                    // The reader's drop frees the stream's receive queue, so the server routes
                    // the stream's freed batch before it answers.
                    drop(reader);
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

        async move {
            let mut client = SimEndpointConfig::default().client();

            // The first round inserts the server's entry for the client and starts the server's
            // send contexts on its senders, since a sender's new send context clears its mark.
            exchange(&mut client, STREAMS).await;
            let moves = (
                subscriber_in_sim
                    .path_secret_map_acknowledgement_moved
                    .load(Ordering::Relaxed),
                subscriber_in_sim
                    .path_secret_map_freed_batch_moved
                    .load(Ordering::Relaxed),
            );
            assert_eq!(
                moves,
                (0, 0),
                "the server moved a message before the test marked a sender lost"
            );

            // The connect inserts the server's entry for the client under the client's first
            // data address, in the map that the server registered at its address at
            // `SERVER_PORT`.
            let server_addr = bach::net::try_lookup(("server", SERVER_PORT))
                .unwrap_or_else(|err| panic!("failed to resolve the server: {err}"));
            let map =
                lookup_sim_map(server_addr).expect("the server registered its path secret map");
            let addrs = client.data_addrs().to_vec();
            let entry = map
                .get_raw(addrs[0])
                .expect("the server's entry for the client");
            *client_addrs_in_sim.lock().unwrap() = addrs;

            // The endpoints route on the thread that runs the sim, which records what they route
            // from here on, while the server's senders for its highest-priority traffic are
            // marked lost.
            observe::record_into(routed_in_sim);
            for sender in priority_senders() {
                assert!(
                    entry.set_sender_lost(sender, true),
                    "sender {sender} was marked lost already"
                );
            }

            exchange(&mut client, STREAMS).await;
        }
        .group("client")
        .primary()
        .spawn();
    });

    let client_addrs = client_addrs.lock().unwrap();
    let routes = routed.lock().unwrap();
    let priority = priority_senders();
    // The messages of the second round that the server routed to the client.
    let server_routes: Vec<&Routed> = routes
        .iter()
        .filter(|route| client_addrs.contains(&route.peer))
        .collect();

    // An acknowledgement's context sends from one of the senders for the highest-priority
    // traffic. Where that sender is marked lost, the acknowledgement goes to the next sender by
    // slot whose pair isn't marked lost, an even one, so each acknowledgement that went to an
    // even sender moved off its context's sender, and each such move publishes one event.
    let moved_acks = server_routes
        .iter()
        .filter(|route| route.kind == Kind::PendingAck && !priority.contains(&route.sender))
        .count() as u64;
    assert!(
        moved_acks > 0,
        "no acknowledgement of the server moved off a sender marked lost"
    );

    // The server records which task submitted each acknowledgement. The `ack_burst` task submits
    // the scheduled acknowledgements of a context, and the `ack_completion` task submits them
    // again at the completion of the last one, where packets arrived after it was encoded. Each
    // task publishes the moves of its own submissions, so the count of events below finds a
    // task that doesn't publish them only where an acknowledgement of each task moved.
    assert!(
        server_routes
            .iter()
            .filter(|route| route.kind == Kind::PendingAck)
            .all(|route| route.submitter.is_some()),
        "an acknowledgement of the server went to the router outside `observe::submitting`"
    );
    for submitter in [Submitter::AckBurst, Submitter::AckCompletion] {
        let moved = server_routes
            .iter()
            .filter(|route| {
                route.kind == Kind::PendingAck
                    && route.submitter == Some(submitter)
                    && !priority.contains(&route.sender)
            })
            .count();
        assert!(
            moved > 0,
            "no acknowledgement that {submitter:?} submitted moved off a sender marked lost"
        );
    }

    let ack_events = subscriber
        .path_secret_map_acknowledgement_moved
        .load(Ordering::Relaxed);
    assert_eq!(
        ack_events, moved_acks,
        "the server's map didn't publish one event for each acknowledgement it moved"
    );

    // Where both senders that the router draws for a freed batch are marked lost, the batch goes
    // to the next sender by slot whose pair isn't marked lost, an even one, and the move publishes
    // one event. A sender's new send context clears its mark, so a batch can also move to the
    // other drawn sender, which publishes an event and keeps the batch on an odd sender.
    let freed: Vec<&&Routed> = server_routes
        .iter()
        .filter(|route| route.kind == Kind::PendingFreed)
        .collect();
    let moved_freed = freed
        .iter()
        .filter(|route| !priority.contains(&route.sender))
        .count() as u64;
    assert!(
        moved_freed > 0,
        "no freed batch of the server moved off a sender marked lost"
    );
    let freed_events = subscriber
        .path_secret_map_freed_batch_moved
        .load(Ordering::Relaxed);
    assert!(
        moved_freed <= freed_events && freed_events <= freed.len() as u64,
        "the server's map published {freed_events} moves of {} freed batches, {moved_freed} of \
         them on an even sender",
        freed.len(),
    );
}
