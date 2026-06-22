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
    stream::endpoint::testing::sim::{Client, Server, SimEndpointConfig, SERVER_PORT},
    testing::{ext::*, sim, without_snapshots},
};
use bach::time::timeout;
use s2n_quic_core::{stream::testing::Data, varint::VarInt};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

/// Opens a connection and runs one `body_len` echo exchange, returning the result rather than
/// panicking, so recovery tests can assert on a rejected exchange.
async fn try_client_echo(
    client: &mut Client,
    acceptor_id: VarInt,
    body_len: usize,
) -> std::io::Result<()> {
    let stream = timeout(
        TRANSFER_TIMEOUT,
        client.connect(("server", SERVER_PORT), acceptor_id),
    )
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out"))??;

    let (mut reader, mut writer) = stream.into_split();

    let mut body = Data::new(body_len as u64);
    writer.write_all_from_fin(&mut body).await?;

    let mut rx = Data::new(body_len as u64);
    let _ = reader.read_to_end(&mut rx).await?;
    assert!(rx.is_finished(), "echo response was incomplete");
    Ok(())
}

/// Like [`try_client_echo`] but panics on failure — for the happy-path round trip.
async fn client_echo(client: &mut Client, acceptor_id: VarInt, body_len: usize) {
    timeout(
        TRANSFER_TIMEOUT,
        try_client_echo(client, acceptor_id, body_len),
    )
    .await
    .expect("transfer timed out")
    .expect("echo failed");
}

/// Server group body: accept streams and echo each one back, forever. Reads/echoes are best-effort
/// (`ok()`) so that a stream left half-open by a peer that crashed mid-exchange doesn't panic the
/// acceptor loop.
async fn run_echo_server(acceptor_id: VarInt, body_len: usize) -> Server {
    let server = SimEndpointConfig::default().real_handshake(true).server();
    let mut acceptor = server
        .register_acceptor_channel(acceptor_id, 8)
        .expect("acceptor registration failed");

    // Drive the accept loop in the background so the caller keeps the `Server` handle (needed to
    // `forget_secrets`). The handle owns the endpoint; returning it keeps everything alive.
    let server_clone = server.clone();
    async move {
        while let Some(stream) = acceptor.recv().await {
            async move {
                let (mut reader, mut writer) = stream.into_split();

                let mut rx = Data::new(body_len as u64);
                if reader.read_to_end(&mut rx).await.is_err() || !rx.is_finished() {
                    return;
                }

                let mut response = Data::new(body_len as u64);
                let _ = writer.write_all_from_fin(&mut response).await;
            }
            .primary()
            .spawn();
        }
    }
    .spawn();

    server_clone
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
            // Hold the server handle for the group's lifetime so the endpoint stays alive. Park on
            // a bounded sleep (a wakerless `pending` would trip bach's task contract); the sim ends
            // when the primary client task completes.
            let _server = run_echo_server(acceptor_id, body_len).await;
            bach::time::sleep(Duration::from_secs(300)).await;
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

/// Node-crash recovery via the **server-forgets-secrets** model — the scenario that actually
/// exercises the `UnknownPathSecret` recovery machinery.
///
/// The client handshakes and exchanges data, then the server "restarts" by dropping its
/// path-secret map ([`Server::forget_secrets`]) while keeping its endpoint and addresses. The
/// client still holds the now-stale secret, so its next data packet carries a credential the server
/// no longer recognizes. The chain under test:
///
/// 1. client sends on the stale secret → server replies `UnknownPathSecret`;
/// 2. the client's Writer surfaces that as `ConnectionRefused` ("path secret rejected by peer");
/// 3. handling the `UnknownPathSecret` evicts the client's stale entry (the map was built with
///    `with_evict_on_unknown_path_secret(true)`);
/// 4. because the entry is gone, the client's next `connect` re-handshakes from scratch and the
///    exchange succeeds again.
///
/// A full endpoint teardown would *not* test this: the restarted server would rebind new ephemeral
/// data ports, so the stale packets would hit dead ports and never provoke `UnknownPathSecret`.
/// Forgetting the secrets in place is what drives the recovery path.
#[test]
fn real_handshake_recovers_after_server_forgets_secrets() {
    let _no_snapshots = without_snapshots();

    let body_len = 16 * 1024usize;

    // Phase coordination across the two groups.
    let established = Arc::new(AtomicBool::new(false)); // client → server: first exchange done
    let forgotten = Arc::new(AtomicBool::new(false)); // server → client: secrets dropped
    let recovered = Arc::new(AtomicBool::new(false)); // client: final assertion
    let rejected = Arc::new(AtomicBool::new(false)); // client: observed the stale-secret rejection

    let recovered_check = recovered.clone();
    let rejected_check = rejected.clone();

    sim(|| {
        let acceptor_id = VarInt::from_u8(1);

        // ── Server ────────────────────────────────────────────────────────
        {
            let established = established.clone();
            let forgotten = forgotten.clone();
            async move {
                let server = run_echo_server(acceptor_id, body_len).await;

                // Wait for the client's first exchange, then forget all path secrets — the
                // in-place "restart".
                while !established.load(Ordering::SeqCst) {
                    bach::time::sleep(Duration::from_millis(10)).await;
                }
                server.forget_secrets();
                tracing::info!("server: dropped path secrets (simulated restart)");
                forgotten.store(true, Ordering::SeqCst);

                bach::time::sleep(Duration::from_secs(300)).await;
            }
            .group("server")
            .spawn();
        }

        // ── Client ────────────────────────────────────────────────────────
        let established = established.clone();
        let forgotten = forgotten.clone();
        let recovered = recovered.clone();
        let rejected = rejected.clone();
        async move {
            let mut client = SimEndpointConfig::default().real_handshake(true).client();

            // 1. Establish and prove data flows.
            client_echo(&mut client, acceptor_id, body_len).await;
            established.store(true, Ordering::SeqCst);
            tracing::info!("client: first exchange complete");

            // 2. Wait until the server has forgotten the secret.
            while !forgotten.load(Ordering::SeqCst) {
                bach::time::sleep(Duration::from_millis(10)).await;
            }

            // 3. An exchange on the stale secret must be rejected. The server returns
            //    UnknownPathSecret, which the Writer surfaces as ConnectionRefused and which evicts
            //    the client's stale entry. (Retry a few times: the very first post-restart packet
            //    races the eviction, and a fresh connect right after eviction re-handshakes.)
            let mut attempts = 0;
            loop {
                attempts += 1;
                match try_client_echo(&mut client, acceptor_id, body_len).await {
                    Ok(()) => {
                        // Re-handshake happened and the exchange recovered.
                        tracing::info!(attempts, "client: recovered after re-handshake");
                        recovered.store(true, Ordering::SeqCst);
                        break;
                    }
                    Err(e) => {
                        tracing::info!(attempts, error = %e, "client: exchange rejected (expected)");
                        rejected.store(true, Ordering::SeqCst);
                        assert!(
                            attempts < 10,
                            "client never recovered after the server forgot its secrets"
                        );
                    }
                }
            }

            tracing::info!("real_handshake_recovers_after_server_forgets_secrets passed");
        }
        .group("client")
        .primary()
        .spawn();
    });

    assert!(
        rejected_check.load(Ordering::SeqCst),
        "the stale-secret exchange was never rejected — the UnknownPathSecret path did not fire"
    );
    assert!(
        recovered_check.load(Ordering::SeqCst),
        "client did not recover after the server forgot its secrets"
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
