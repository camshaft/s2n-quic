// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests that the endpoint passes its path secret map to the routers of frame dispatch, which
//! publish through that map the events that start and end a sender's count as lost for sharing
//! a lost pair's interface.
//!
//! The server puts its eight sockets on two interfaces, in turn, and tags its data addresses
//! with those interfaces. The client doesn't list interfaces, so its router is pick-two, and its
//! sender `i` sends to the server's address `i`. The test marks the client's sender 0 lost
//! before the client writes a stream, so that senders 2, 4 and 6, which send to the server
//! interface of sender 0's address, count as lost, and checks that the client's map publishes
//! the start of those counts to its testing subscriber.

use crate::{
    endpoint::id::{Id, LocalSenderId},
    path::secret::{stateless_reset::Signer, Map},
    socket::{interface, rate::Rate},
    stream::endpoint::testing::sim::{SimEndpointConfig, SERVER_PORT},
    testing::{ext::*, sim, without_snapshots, without_tracing},
};
use s2n_quic_core::{stream::testing::Data, varint::VarInt};
use std::sync::{atomic::Ordering, Arc};

const ACCEPTOR_ID: VarInt = VarInt::from_u32(1);
const STREAM_LEN: u64 = 64 * 1024;
/// The interface of each server socket: the even sockets on the first one, the odd ones on the
/// second.
const SERVER_INTERFACE: [usize; 8] = [0, 1, 0, 1, 0, 1, 0, 1];

/// The server's two interfaces, each with the sockets `SERVER_INTERFACE` puts on it.
fn server_interfaces() -> Vec<interface::Config> {
    let sockets = |position: usize| -> Vec<usize> {
        (0..SERVER_INTERFACE.len())
            .filter(|&socket| SERVER_INTERFACE[socket] == position)
            .collect()
    };
    (0..2)
        .map(|position| interface::Config {
            send_rate: Some(Rate::new(1.0)),
            send_sockets: sockets(position),
            recv_sockets: sockets(position),
            ..Default::default()
        })
        .collect()
}

/// The client's router counts the senders that send to the server interface of a lost pair as
/// lost, and publishes the start of each count through the path secret map that the client
/// endpoint passes to frame dispatch.
#[test]
fn the_endpoint_passes_its_path_secret_map_to_the_routers() {
    let _snapshots = without_snapshots();
    let _tracing = without_tracing();

    let subscriber = Arc::new(crate::event::testing::Subscriber::no_snapshot());
    let subscriber_in_sim = subscriber.clone();

    sim(move || {
        async move {
            let server = SimEndpointConfig::default()
                .interfaces(server_interfaces())
                .server();
            let mut acceptor = server
                .register_acceptor_channel(ACCEPTOR_ID, 2)
                .expect("acceptor registration");
            while let Some(stream) = acceptor.recv().await {
                async move {
                    let (mut reader, _writer) = stream.into_split();
                    let mut data = Data::new(STREAM_LEN);
                    while reader.read_into(&mut data).await.expect("server read") > 0 {}
                    assert!(data.is_finished(), "the stream ended early");
                }
                .primary()
                .spawn();
            }
        }
        .group("server")
        .spawn();

        async move {
            // A map like the one the endpoint makes where its config doesn't hold one, which
            // publishes its events to the testing subscriber.
            let map = Map::builder()
                .with_signer(Signer::random())
                .with_capacity(50_000)
                .with_clock(crate::time::bach::Clock::default())
                .with_subscriber(subscriber_in_sim)
                .build()
                .unwrap();
            let mut client = SimEndpointConfig::default()
                .path_secret_map(map.clone())
                .client();
            let stream = client
                .connect("server:0", ACCEPTOR_ID)
                .await
                .expect("connect");

            // The connect inserts the client's entry for the server under the server's address
            // at `SERVER_PORT`.
            let server_addr = bach::net::try_lookup(("server", SERVER_PORT))
                .unwrap_or_else(|err| panic!("failed to resolve the server: {err}"));
            let entry = map
                .get_raw(server_addr)
                .expect("the client's entry for the server");
            assert!(
                entry.set_sender_lost(LocalSenderId::from_index(0), true),
                "sender 0 was marked lost already"
            );

            let (_reader, mut writer) = stream.into_split();
            let mut data = Data::new(STREAM_LEN);
            writer
                .write_all_from_fin(&mut data)
                .await
                .expect("client write");
        }
        .group("client")
        .primary()
        .spawn();
    });

    // Senders 2, 4 and 6 count as lost once the router reads the marks. The simulation's timing
    // decides whether a count ends and starts again, so the test checks for at least one start.
    let starts = subscriber
        .path_secret_map_sender_counted_lost
        .load(Ordering::Relaxed);
    assert!(starts > 0, "the client's map didn't publish the start of a count as lost");
}
