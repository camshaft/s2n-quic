// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests that each send socket of a local interface sends to the addresses of the peer's
//! interface it pairs with, and that a slow pair doesn't hold a fast pair back.
//!
//! The client and the server each put their eight sockets on two interfaces, four on each, at 1
//! and 2.5 Gbps: the client's in two runs of four, the server's in turn, so that a choice of the
//! peer address by the sender's index alone would cross the interfaces. The client writes eight
//! streams of 2 MiB to the server. The test records, in virtual time, each socket write between
//! a client address and a server address.

use crate::{
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
const STREAMS: usize = 8;
const STREAM_LEN: u64 = 2 * 1024 * 1024;
/// The interface of each client socket: sockets 0 to 3 on the slow one, 4 to 7 on the fast one.
const CLIENT_INTERFACE: [usize; 8] = [0, 0, 0, 0, 1, 1, 1, 1];
/// The interface of each server socket: the even sockets on the slow one, the odd ones on the
/// fast one.
const SERVER_INTERFACE: [usize; 8] = [0, 1, 0, 1, 0, 1, 0, 1];

/// One socket write: its local and peer addresses, its bytes and its virtual time.
#[derive(Clone, Copy)]
struct Write {
    local: SocketAddr,
    peer: SocketAddr,
    bytes: u64,
    at_nanos: u64,
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

/// Each client socket writes only to the server addresses of the interface it pairs with, and
/// each server socket only to the client addresses of its pair. Both pairs carry data, and the
/// fast pair writes more than the slow interface's rate allows, so the slow pair doesn't hold it
/// back.
#[test]
fn each_interface_sends_to_the_peer_interface_it_pairs_with() {
    let _snapshots = without_snapshots();
    let _tracing = without_tracing();

    let writes = Arc::new(Mutex::new(Vec::<Write>::new()));
    let server_addrs = Arc::new(Mutex::new(Vec::<SocketAddr>::new()));
    let client_addrs = Arc::new(Mutex::new(Vec::<SocketAddr>::new()));
    let writes_in_sim = writes.clone();
    let server_addrs_in_sim = server_addrs.clone();
    let client_addrs_in_sim = client_addrs.clone();

    sim(move || {
        bach::net::monitor::on_socket_write(move |write| {
            let bytes: usize = write.payload.iter().map(|slice| slice.len()).sum();
            let at_nanos = bach::time::Instant::now().elapsed_since_start().as_nanos() as u64;
            writes_in_sim.lock().unwrap().push(Write {
                local: *write.local_addr,
                peer: *write.peer_addr,
                bytes: bytes as u64,
                at_nanos,
            });
            Ok(())
        });

        async move {
            let server = SimEndpointConfig::default()
                .interfaces(interfaces(&SERVER_INTERFACE))
                .server();
            *server_addrs_in_sim.lock().unwrap() = server.data_addrs().to_vec();
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
            let mut client = SimEndpointConfig::default()
                .interfaces(interfaces(&CLIENT_INTERFACE))
                .client();
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

    let writes = writes.lock().unwrap();
    let server_addrs = server_addrs.lock().unwrap();
    let client_addrs = client_addrs.lock().unwrap();
    assert_eq!(server_addrs.len(), 8, "the server has eight sockets");
    assert_eq!(client_addrs.len(), 8, "the client has eight sockets");

    // In the sim, send socket i is recv socket i, so the position of an address in `data_addrs`
    // is its socket, and `layout` holds that socket's interface.
    let interface_of = |addrs: &[SocketAddr], layout: &[usize; 8], addr: SocketAddr| {
        addrs
            .iter()
            .position(|&data_addr| data_addr == addr)
            .map(|socket| layout[socket])
    };

    let mut client_bytes = [0u64; 2];
    let mut server_writes = [0usize; 2];
    let mut end = 0;
    for write in writes.iter() {
        let Write {
            local,
            peer,
            bytes,
            at_nanos,
        } = *write;
        if let (Some(from), Some(to)) = (
            interface_of(&client_addrs, &CLIENT_INTERFACE, local),
            interface_of(&server_addrs, &SERVER_INTERFACE, peer),
        ) {
            assert_eq!(
                from, to,
                "client socket {local} on interface {from} wrote to server {peer} on {to}",
            );
            client_bytes[from] += bytes;
            end = end.max(at_nanos);
        }
        if let (Some(from), Some(to)) = (
            interface_of(&server_addrs, &SERVER_INTERFACE, local),
            interface_of(&client_addrs, &CLIENT_INTERFACE, peer),
        ) {
            assert_eq!(
                from, to,
                "server socket {local} on interface {from} wrote to client {peer} on {to}",
            );
            server_writes[from] += 1;
        }
    }

    let [slow_bytes, fast_bytes] = client_bytes;
    assert!(slow_bytes > 0, "the slow pair didn't carry data");
    assert!(fast_bytes > 0, "the fast pair didn't carry data");
    assert!(
        server_writes.iter().all(|&count| count > 0),
        "the server wrote {server_writes:?} times on its interfaces",
    );
    // The fast pair writes more than the slow interface's rate allows in the time the client
    // wrote, so the slow pair's pacing doesn't hold the fast pair to the slow rate.
    assert!(
        SLOW.nanos_for_bytes(fast_bytes) > end,
        "the fast pair wrote {fast_bytes} bytes by {end} ns, within the slow one's rate",
    );
}
