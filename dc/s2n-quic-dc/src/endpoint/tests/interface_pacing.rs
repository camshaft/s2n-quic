// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests that frame dispatch paces the send sockets of each local interface at that interface's
//! send rate.
//!
//! The client puts its eight send sockets on two interfaces, four on each, at 1 and 2.5 Gbps,
//! and writes eight streams of 2 MiB to the server. The test counts, in virtual time, the bytes
//! each client socket writes and the time of its last write.

use crate::{
    socket::{interface, rate::Rate},
    stream::endpoint::testing::sim::SimEndpointConfig,
    testing::{ext::*, sim, without_snapshots, without_tracing},
};
use s2n_quic_core::{stream::testing::Data, varint::VarInt};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

const ACCEPTOR_ID: VarInt = VarInt::from_u32(1);
const SLOW: Rate = Rate::new(1.0);
const FAST: Rate = Rate::new(2.5);
const STREAMS: usize = 8;
const STREAM_LEN: u64 = 2 * 1024 * 1024;

/// What one socket wrote: its bytes, and the virtual time of its last write.
#[derive(Clone, Copy, Default)]
struct Written {
    bytes: u64,
    last_nanos: u64,
}

/// Two interfaces: the first at `SLOW` with sockets 0 to 3, the second at `FAST` with sockets 4
/// to 7.
fn interfaces() -> Vec<interface::Config> {
    vec![
        interface::Config {
            send_rate: Some(SLOW),
            send_sockets: (0..4).collect(),
            recv_sockets: (0..4).collect(),
            ..Default::default()
        },
        interface::Config {
            send_rate: Some(FAST),
            send_sockets: (4..8).collect(),
            recv_sockets: (4..8).collect(),
            ..Default::default()
        },
    ]
}

/// Each interface's sockets write within its send rate and burst, and the fast interface writes
/// more than the slow interface's rate allows, so the slow one doesn't hold it back.
#[test]
fn each_interface_sends_at_its_own_rate() {
    let _snapshots = without_snapshots();
    let _tracing = without_tracing();

    let written = Arc::new(Mutex::new(HashMap::<SocketAddr, Written>::new()));
    let client_addrs = Arc::new(Mutex::new(Vec::<SocketAddr>::new()));
    let written_in_sim = written.clone();
    let client_addrs_in_sim = client_addrs.clone();

    sim(move || {
        bach::net::monitor::on_socket_write(move |write| {
            let bytes: usize = write.payload.iter().map(|slice| slice.len()).sum();
            let at = bach::time::Instant::now().elapsed_since_start().as_nanos() as u64;
            let mut written = written_in_sim.lock().unwrap();
            let entry = written.entry(*write.local_addr).or_default();
            entry.bytes += bytes as u64;
            entry.last_nanos = at;
            Ok(())
        });

        async move {
            let server = SimEndpointConfig::default().server();
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
                .interfaces(interfaces())
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

    let written = written.lock().unwrap();
    let client_addrs = client_addrs.lock().unwrap();
    assert_eq!(client_addrs.len(), 8, "the client has eight sockets");

    // In the sim, send socket i is recv socket i, so `data_addrs` holds the address of each send
    // socket, in order.
    let interface_total = |sockets: std::ops::Range<usize>| {
        sockets.fold(Written::default(), |total, socket| {
            let socket = written
                .get(&client_addrs[socket])
                .copied()
                .unwrap_or_default();
            Written {
                bytes: total.bytes + socket.bytes,
                last_nanos: total.last_nanos.max(socket.last_nanos),
            }
        })
    };
    let slow = interface_total(0..4);
    let fast = interface_total(4..8);

    for (name, sent, rate) in [("slow", slow, SLOW), ("fast", fast, FAST)] {
        // The socket writes count packet headers, which the paced byte costs leave out, so allow
        // 10% over the rate, beside the burst and two batches that the timers may round past.
        let allowed = sent.last_nanos
            + sent.last_nanos / 10
            + rate.burst_nanos()
            + rate.nanos_for_bytes(2 * u16::MAX as u64);
        let bytes = sent.bytes;
        let last = sent.last_nanos;
        assert!(
            rate.nanos_for_bytes(bytes) <= allowed,
            "the {name} interface wrote {bytes} bytes by {last} ns, more than {rate:?} allows",
        );
    }

    assert!(slow.bytes > 0, "the slow interface didn't write");
    // The fast interface writes more than the slow one's rate allows in the time both took, so
    // the slow interface's pacing doesn't hold the fast one to the slow rate.
    let end = slow.last_nanos.max(fast.last_nanos);
    let fast_bytes = fast.bytes;
    assert!(
        SLOW.nanos_for_bytes(fast_bytes) > end,
        "the fast interface wrote {fast_bytes} bytes by {end} ns, within the slow one's rate",
    );
}
