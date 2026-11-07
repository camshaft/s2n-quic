// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests that a peer's acknowledgements and the freed batches go to a send socket of the
//! interface that carries the endpoint's highest-priority traffic.
//!
//! The server puts its eight sockets on two interfaces, four on each, in turn. The client doesn't
//! list interfaces, so its data addresses go untagged, and the server can't pair its interfaces
//! with the client's: the flagged-interface rule chooses each acknowledgement's sender. The
//! client writes many short streams to the server, which acknowledges the client's packets and
//! frees each stream's queue as the stream ends. The test records each acknowledgement and freed
//! batch that the server routes to a send worker, with the sender it goes to.

use crate::{
    endpoint::routing::observe::{self, Kind, Routed},
    socket::{interface, rate::Rate},
    stream::endpoint::testing::sim::SimEndpointConfig,
    testing::{ext::*, sim, without_snapshots, without_tracing},
};
use s2n_quic_core::{stream::testing::Data, varint::VarInt};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

const ACCEPTOR_ID: VarInt = VarInt::from_u32(1);
const SLOW: Rate = Rate::new(1.0);
const FAST: Rate = Rate::new(2.5);
/// Many short streams, so that the server frees many queues.
const STREAMS: usize = 32;
const STREAM_LEN: u64 = 64 * 1024;
/// The fewest freed batches the server must route where its fast interface carries its
/// highest-priority traffic, so that the test catches a router that picks among every sender
/// unless each of those picks falls on the fast interface.
const FREED_BATCHES: usize = 8;
/// The interface of each server socket: the even sockets on the slow one, the odd ones on the
/// fast one.
const SERVER_INTERFACE: [usize; 8] = [0, 1, 0, 1, 0, 1, 0, 1];

/// Two interfaces, the first at `SLOW` and the second at `FAST`, each with the sockets `layout`
/// puts on it. The second carries the highest-priority traffic where `highest_priority` is set.
fn interfaces(layout: &[usize; 8], highest_priority: bool) -> Vec<interface::Config> {
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
            carries_highest_priority: highest_priority,
            ..Default::default()
        },
    ]
}

/// Runs the client's streams to the server, and returns each acknowledgement and freed batch
/// that the server routed to a send worker.
fn server_routes(server_highest_priority: bool) -> Vec<Routed> {
    let routed = Arc::new(Mutex::new(Vec::<Routed>::new()));
    let client_addrs = Arc::new(Mutex::new(Vec::<SocketAddr>::new()));
    let routed_in_sim = routed.clone();
    let client_addrs_in_sim = client_addrs.clone();

    sim(move || {
        // The endpoints route on the thread that runs the sim, which records what they route.
        observe::record_into(routed_in_sim);

        async move {
            let server = SimEndpointConfig::default()
                .interfaces(interfaces(&SERVER_INTERFACE, server_highest_priority))
                .server();
            let mut acceptor = server
                .register_acceptor_channel(ACCEPTOR_ID, STREAMS * 2)
                .expect("acceptor registration");
            while let Some(stream) = acceptor.recv().await {
                async move {
                    let (mut reader, _writer) = stream.into_split();
                    let mut data = Data::new(STREAM_LEN);
                    while reader.read_into(&mut data).await.expect("server read") > 0 {}
                    assert!(data.is_finished(), "a stream ended early");
                }
                .primary()
                .spawn();
            }
        }
        .group("server")
        .spawn();

        async move {
            let mut client = SimEndpointConfig::default().client();
            *client_addrs_in_sim.lock().unwrap() = client.data_addrs().to_vec();
            for _ in 0..STREAMS {
                let stream = client
                    .connect("server:0", ACCEPTOR_ID)
                    .await
                    .expect("connect");
                async move {
                    let (_reader, mut writer) = stream.into_split();
                    let mut data = Data::new(STREAM_LEN);
                    writer
                        .write_all_from_fin(&mut data)
                        .await
                        .expect("client write");
                }
                .primary()
                .spawn();
            }
        }
        .group("client")
        .primary()
        .spawn();
    });

    // The server's path to the client has a client address as its peer, and the client's path
    // to the server has a server address, so the peer tells the two endpoints apart.
    let client_addrs = client_addrs.lock().unwrap();
    assert_eq!(client_addrs.len(), 8, "the client has eight sockets");
    let routed = routed.lock().unwrap();
    routed
        .iter()
        .filter(|routed| client_addrs.contains(&routed.peer))
        .copied()
        .collect()
}

/// Where the server's fast interface carries its highest-priority traffic, the server routes
/// each acknowledgement and each freed batch to a send socket of that interface.
#[test]
fn acknowledgements_and_freed_batches_go_to_the_highest_priority_interface() {
    let _snapshots = without_snapshots();
    let _tracing = without_tracing();

    let routed = server_routes(true);
    for kind in [Kind::PendingAck, Kind::PendingFreed] {
        assert!(
            routed.iter().any(|routed| routed.kind == kind),
            "the server didn't route a {kind:?}",
        );
    }
    let freed = routed
        .iter()
        .filter(|routed| routed.kind == Kind::PendingFreed)
        .count();
    assert!(
        freed >= FREED_BATCHES,
        "the server routed {freed} freed batches, fewer than {FREED_BATCHES}",
    );
    for routed in &routed {
        assert_eq!(
            SERVER_INTERFACE[routed.sender.as_usize()],
            1,
            "the server routed {routed:?} to a send socket of the slow interface",
        );
    }
}

/// Where no interface carries the server's highest-priority traffic, the server routes the
/// acknowledgements and the freed batches as before, and some of each go to a send socket of
/// the slow interface.
#[test]
fn without_a_highest_priority_interface_the_server_routes_to_both_interfaces() {
    let _snapshots = without_snapshots();
    let _tracing = without_tracing();

    let routed = server_routes(false);
    for kind in [Kind::PendingAck, Kind::PendingFreed] {
        assert!(
            routed.iter().any(|routed| {
                routed.kind == kind && SERVER_INTERFACE[routed.sender.as_usize()] == 0
            }),
            "the server didn't route a {kind:?} to a send socket of the slow interface",
        );
    }
}
