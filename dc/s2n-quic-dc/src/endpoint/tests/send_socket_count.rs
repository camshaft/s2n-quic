// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests an endpoint whose count of send sockets isn't a power of two.
//!
//! The server has twelve send sockets and twelve recv sockets. The client writes a round of
//! streams to the server, and the server answers each stream with as many bytes. The round must
//! finish. Neither endpoint lists interfaces, so the server must send the acknowledgements of the
//! packets of the client's sender `i` from its own sender `i % 12`.

use crate::{
    endpoint::routing::observe::{self, Kind, Routed},
    stream::endpoint::testing::sim::SimEndpointConfig,
    testing::{ext::*, sim, without_snapshots, without_tracing},
};
use s2n_quic_core::{stream::testing::Data, varint::VarInt};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

const ACCEPTOR_ID: VarInt = VarInt::from_u32(1);
const STREAMS: usize = 16;
const STREAM_LEN: u64 = 64 * 1024;
/// The server's count of send sockets, which isn't a power of two.
const SERVER_SOCKETS: usize = 12;

/// The server starts with twelve send sockets and answers each stream, and it acknowledges the
/// packets of each client sender from the sender that the client sender's id modulo twelve
/// picks. At least one acknowledgement must answer a client sender for which a mask of eleven
/// picks another sender, so that the check can't pass where the route masks the id.
#[test]
fn twelve_send_sockets() {
    let _snapshots = without_snapshots();
    let _tracing = without_tracing();

    let routed = Arc::new(Mutex::new(Vec::<Routed>::new()));
    let routed_in_sim = routed.clone();
    let client_addrs = Arc::new(Mutex::new(Vec::<SocketAddr>::new()));
    let client_addrs_in_sim = client_addrs.clone();

    sim(move || {
        // The endpoints route on the thread that runs the sim, which records what they route.
        observe::record_into(routed_in_sim);

        async move {
            let config = SimEndpointConfig {
                num_send_sockets: SERVER_SOCKETS,
                num_recv_sockets: SERVER_SOCKETS,
                ..Default::default()
            };
            // The recv dispatch workers count a power of two, which a mask routes over, and the
            // send sockets don't.
            assert!(config.num_recv_dispatch_workers.is_power_of_two());
            let server = config.server();
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

        async move {
            let mut client = SimEndpointConfig::default().client();
            *client_addrs_in_sim.lock().unwrap() = client.data_addrs().to_vec();
            let mut handles = Vec::with_capacity(STREAMS);
            for _ in 0..STREAMS {
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
                    let mut reply = Data::new(STREAM_LEN);
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
        .group("client")
        .primary()
        .spawn();
    });

    let client_addrs = client_addrs.lock().unwrap();
    let routed = routed.lock().unwrap();
    // The server's acknowledgements, whose paths have a client address as their peer.
    let mut acknowledged = 0;
    let mut mask_differs = 0;
    for route in routed.iter() {
        if route.kind != Kind::PendingAck || !client_addrs.contains(&route.peer) {
            continue;
        }
        let remote = route
            .remote
            .expect("an acknowledgement answers a peer sender");
        let remote = remote.as_varint().as_u64() as usize;
        assert_eq!(
            route.sender.as_usize(),
            remote % SERVER_SOCKETS,
            "the server acknowledged client sender {remote} from another sender: {route:?}",
        );
        acknowledged += 1;
        if remote & (SERVER_SOCKETS - 1) != remote % SERVER_SOCKETS {
            mask_differs += 1;
        }
    }
    assert!(acknowledged > 0, "the server didn't acknowledge a packet");
    assert!(
        mask_differs > 0,
        "the server didn't acknowledge a client sender for which a mask picks another sender",
    );
}
