// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests for the sim harness's opt-in real-handshake mode.
//!
//! Unlike the rest of the `endpoint::tests` suite — which establishes path secrets with the
//! deterministic `insert_fake_path_pair` shortcut — these tests set
//! [`SimEndpointConfig::real_handshake`], so each connection is preceded by a genuine QUIC/TLS PSK
//! handshake (driven through the bach-backed s2n-quic IO provider) that populates the same
//! path-secret map the data plane reads from. This exercises the handshake, transport-parameter
//! data-address exchange, and secret derivation that the fake path skips.
//!
//! Real TLS key material is seeded from the OS RNG (which bach does not control), so the
//! `credential_id` debug lines vary per run and cannot be snapshotted; these tests use
//! `without_snapshots` and assert behavior via explicit checks and a data round-trip.

use crate::{
    stream::endpoint::testing::sim::{crash_group, Client, SimEndpointConfig, SERVER_PORT},
    testing::{ext::*, sim, without_snapshots},
};
use bach::time::timeout;
use s2n_quic_core::{stream::testing::Data, varint::VarInt};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

/// Drives one real-handshake echo exchange from the client side: connect, send `body_len` bytes,
/// read the echo back. Used by the recovery tests to exercise the data plane before and after a
/// simulated crash.
async fn client_echo(client: &mut Client, acceptor_id: VarInt, body_len: usize) {
    let stream = timeout(
        TRANSFER_TIMEOUT,
        client.connect(("server", SERVER_PORT), acceptor_id),
    )
    .await
    .expect("connect timed out")
    .expect("connect failed");

    let (mut reader, mut writer) = stream.into_split();

    timeout(TRANSFER_TIMEOUT, async {
        let mut body = Data::new(body_len as u64);
        writer
            .write_all_from_fin(&mut body)
            .await
            .expect("client write");

        let mut rx = Data::new(body_len as u64);
        let _ = reader.read_to_end(&mut rx).await.expect("client read");
        assert!(rx.is_finished(), "client did not receive complete echo");
    })
    .await
    .expect("transfer timed out");
}

/// Server group body: accept streams and echo each one back, forever.
async fn run_echo_server(acceptor_id: VarInt, body_len: usize) {
    let server = SimEndpointConfig::default().real_handshake(true).server();
    let mut acceptor = server
        .register_acceptor_channel(acceptor_id, 8)
        .expect("acceptor registration failed");

    while let Some(stream) = acceptor.recv().await {
        async move {
            let (mut reader, mut writer) = stream.into_split();

            let mut rx = Data::new(body_len as u64);
            let _ = reader.read_to_end(&mut rx).await.expect("server read");
            assert!(rx.is_finished(), "server did not receive complete request");

            let mut response = Data::new(body_len as u64);
            writer
                .write_all_from_fin(&mut response)
                .await
                .expect("server write");
        }
        .primary()
        .spawn();
    }
}

/// A client performs a real PSK handshake with a server (no `insert_fake_path_pair`), then the two
/// exchange a stream echo over the dc data plane using the secrets the handshake derived.
#[test]
fn real_handshake_stream_round_trip() {
    let _no_snapshots = without_snapshots();

    let body_len = 64 * 1024usize;
    let done = Arc::new(AtomicBool::new(false));
    let done_check = done.clone();

    sim(|| {
        let acceptor_id = VarInt::from_u8(1);

        async move {
            run_echo_server(acceptor_id, body_len).await;
        }
        .group("server")
        .spawn();

        let done = done.clone();
        async move {
            let mut client = SimEndpointConfig::default().real_handshake(true).client();
            client_echo(&mut client, acceptor_id, body_len).await;

            tracing::info!("real_handshake_stream_round_trip passed");
            done.store(true, Ordering::SeqCst);
        }
        .group("client")
        .primary()
        .spawn();
    });

    assert!(
        done_check.load(Ordering::SeqCst),
        "client task did not complete"
    );
}

/// Node-crash recovery via the **drop + recreate** model: the client establishes a connection,
/// then simulates a process crash ([`crash_group`] + dropping its handles) and rebuilds a fresh
/// endpoint, map, and PSK client in the same group. The recovered client must re-handshake from a
/// clean slate and complete a second exchange.
///
/// This is the closest analogue to a real node restart: the survivor (server) keeps running, while
/// the crashed node comes back with no memory of the prior handshake and must re-establish it.
#[test]
fn real_handshake_recovers_after_client_crash() {
    let _no_snapshots = without_snapshots();

    let body_len = 16 * 1024usize;
    let exchanges = Arc::new(AtomicUsize::new(0));
    let exchanges_check = exchanges.clone();

    sim(|| {
        let acceptor_id = VarInt::from_u8(1);

        async move {
            run_echo_server(acceptor_id, body_len).await;
        }
        .group("server")
        .spawn();

        let exchanges = exchanges.clone();
        async move {
            // First exchange establishes the handshake.
            {
                let mut client = SimEndpointConfig::default().real_handshake(true).client();
                client_echo(&mut client, acceptor_id, body_len).await;
                exchanges.fetch_add(1, Ordering::SeqCst);
                tracing::info!("client: first exchange complete; simulating crash");

                // Drop the client handle (releasing the strong Arc<Endpoint>) and clear the harness
                // caches so the next `client()` rebuilds everything from scratch.
                drop(client);
                crash_group();
            }

            // Recovery: a brand-new client in the same group, with a fresh empty map, must
            // re-handshake with the still-running server and complete a second exchange.
            let mut client = SimEndpointConfig::default().real_handshake(true).client();
            client_echo(&mut client, acceptor_id, body_len).await;
            exchanges.fetch_add(1, Ordering::SeqCst);

            tracing::info!("real_handshake_recovers_after_client_crash passed");
        }
        .group("client")
        .primary()
        .spawn();
    });

    assert_eq!(
        exchanges_check.load(Ordering::SeqCst),
        2,
        "client did not complete both the initial and post-crash exchanges"
    );
}

/// Node-recovery via the **blackhole + restore** model: a transient network partition drops every
/// packet between the two nodes during the start of a handshake, then delivery is restored after a
/// bounded window. The endpoints stay alive throughout. The test verifies that a single
/// handshake-and-exchange begun *during* the partition recovers — QUIC retransmission carries it
/// through once connectivity returns — rather than failing outright.
///
/// This exercises the data plane and handshake across a transient partition, complementing the
/// process-restart model in [`real_handshake_recovers_after_client_crash`]. Driving one exchange
/// (rather than abandoning a stalled one) keeps the scenario free of orphaned half-open streams.
#[test]
fn real_handshake_recovers_after_blackhole() {
    let _no_snapshots = without_snapshots();

    let body_len = 16 * 1024usize;
    let done = Arc::new(AtomicBool::new(false));
    let done_check = done.clone();

    // The partition is active at the start of the run and heals after `PARTITION` of simulated time
    // — short enough that QUIC handshake/data retransmission recovers within the 30s budget.
    const PARTITION: Duration = Duration::from_secs(1);
    let blackholed = Arc::new(AtomicBool::new(true));

    sim(|| {
        let acceptor_id = VarInt::from_u8(1);

        {
            let blackholed = blackholed.clone();
            bach::net::monitor::on_packet_sent(move |_packet| {
                if blackholed.load(Ordering::SeqCst) {
                    bach::net::monitor::Command::Drop
                } else {
                    bach::net::monitor::Command::Pass
                }
            });
        }

        // Healer: lift the partition after a bounded window.
        {
            let blackholed = blackholed.clone();
            async move {
                bach::time::sleep(PARTITION).await;
                blackholed.store(false, Ordering::SeqCst);
                tracing::info!("network: partition healed");
            }
            .group("client")
            .spawn();
        }

        async move {
            run_echo_server(acceptor_id, body_len).await;
        }
        .group("server")
        .spawn();

        let done = done.clone();
        async move {
            // The handshake begins while the network is partitioned; every initial packet is
            // dropped. Once the healer lifts the partition, QUIC retransmission completes the
            // handshake and the echo round-trip succeeds within the transfer budget.
            let mut client = SimEndpointConfig::default().real_handshake(true).client();
            client_echo(&mut client, acceptor_id, body_len).await;

            tracing::info!("real_handshake_recovers_after_blackhole passed");
            done.store(true, Ordering::SeqCst);
        }
        .group("client")
        .primary()
        .spawn();
    });

    assert!(
        done_check.load(Ordering::SeqCst),
        "client did not recover after the partition healed"
    );
}
