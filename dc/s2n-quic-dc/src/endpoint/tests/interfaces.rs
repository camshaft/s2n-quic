// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The interface of each advertised receive address, as a peer's path secret entry holds it.

use crate::{
    socket::interface,
    stream::endpoint::testing::sim::{
        insert_fake_path_pair, lookup_sim_map, SimEndpointConfig, SERVER_PORT,
    },
    testing::{ext::*, sim},
};
use s2n_quic_core::varint::VarInt;

const ACCEPTOR_ID: VarInt = VarInt::from_u32(1);

/// Two interfaces of four sockets each: the server's sockets 0 to 3 are on the first, and its
/// sockets 4 to 7 are on the second.
fn server_interfaces() -> Vec<interface::Config> {
    vec![
        interface::Config {
            send_sockets: (0..4).collect(),
            recv_sockets: (0..4).collect(),
            ..Default::default()
        },
        interface::Config {
            send_sockets: (4..8).collect(),
            recv_sockets: (4..8).collect(),
            ..Default::default()
        },
    ]
}

/// A client's entry for the server holds the server's receive addresses with their
/// interfaces, and the server's entry for a client that doesn't list interfaces holds an
/// empty interface list.
#[test]
fn a_client_learns_the_interface_of_each_server_address() {
    sim(|| {
        async move {
            let server = SimEndpointConfig::default()
                .interfaces(server_interfaces())
                .server();
            // The acceptor's wait keeps the server's task alive, with a waker, until the
            // client's task ends the sim.
            let mut acceptor = server
                .register_acceptor_channel(ACCEPTOR_ID, 1)
                .expect("acceptor registration");
            while acceptor.recv().await.is_some() {}
        }
        .group("server")
        .spawn();

        async move {
            let client = SimEndpointConfig::default().client();
            let mut server_addr = bach::net::lookup_host("server:0")
                .await
                .expect("server should resolve")
                .next()
                .expect("server should have an address");
            server_addr.set_port(SERVER_PORT);
            let client_addr = client.data_addr();

            let server_map = lookup_sim_map(server_addr).expect("server map");
            let client_map = lookup_sim_map(client_addr).expect("client map");
            insert_fake_path_pair(&client_map, client_addr, &server_map, server_addr);

            let entry = client_map.get_raw(server_addr).expect("client entry");
            assert_eq!(entry.peer_data_addrs().len(), 8);
            assert!(entry.peer_data_addrs().contains(&server_addr));
            // The server's sockets 0 to 3 are on its interface 1, and 4 to 7 on its interface 2.
            let mut expected = vec![Some(1); 4];
            expected.extend([Some(2); 4]);
            assert_eq!(&entry.peer_data_interfaces()[..], &expected[..]);

            let server_entry = server_map.get_raw(client_addr).expect("server entry");
            assert_eq!(&server_entry.peer_data_addrs()[..], client.data_addrs());
            assert!(server_entry.peer_data_interfaces().is_empty());
        }
        .group("client")
        .primary()
        .spawn();
    });
}
