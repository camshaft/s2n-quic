// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Spike: prove that a full QUIC/TLS handshake runs to completion inside a `bach` simulation, over
//! the `bach::net`-backed s2n-quic IO provider.
//!
//! This is the feasibility check for running real s2n-quic-dc PSK handshakes deterministically in
//! the bach harness. It does *not* use the dc PSK layer (no secret `Map`, no data-address
//! exchange) — it only exercises the QUIC connection + TLS handshake + a single application stream
//! round-trip, which is the part previously only reachable on a real tokio runtime with OS sockets.

use crate::{
    path::secret::{self, stateless_reset::Signer},
    testing::{ext::*, sim, NoopSubscriber, TestTlsProvider},
};
use bach::time::timeout;
use s2n_quic::{
    client::Connect,
    provider::{
        dc::{ConfirmComplete, MtuConfirmComplete},
        io::bach::Provider as BachIo,
        tls::{default as tls, Provider as _},
    },
    Client, Server,
};
use s2n_quic_core::crypto::tls::testing::certificates;
use std::{net::SocketAddr, sync::Arc, time::Duration};

const SERVER_PORT: u16 = 4433;

fn tls_server() -> tls::Server {
    tls::Server::builder()
        .with_application_protocols(["spike"].iter())
        .unwrap()
        .with_certificate(certificates::CERT_PEM, certificates::KEY_PEM)
        .unwrap()
        .build()
        .unwrap()
}

fn tls_client() -> tls::Client {
    tls::Client::builder()
        .with_application_protocols(["spike"].iter())
        .unwrap()
        .with_certificate(certificates::CERT_PEM)
        .unwrap()
        .build()
        .unwrap()
}

/// A client and server, each in their own bach group, perform a real QUIC/TLS handshake over
/// `bach::net` and exchange one request/response on a bidirectional stream.
#[test]
fn quic_handshake_over_bach_net() {
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_check = done.clone();

    sim(|| {
        // ── Server — group "server" ──────────────────────────────────────
        async move {
            let io = BachIo::new((std::net::Ipv4Addr::UNSPECIFIED, SERVER_PORT).into()).unwrap();

            let mut server = Server::builder()
                .with_io(io)
                .unwrap()
                .with_tls(tls_server())
                .unwrap()
                .start()
                .unwrap();

            // Accept exactly one connection and echo one stream.
            let mut connection = server.accept().await.expect("server accept");
            tracing::info!("server: connection accepted, handshake complete");

            let mut stream = connection
                .accept_bidirectional_stream()
                .await
                .expect("server accept stream")
                .expect("server stream present");

            // Read the request and echo it back.
            let req = stream
                .receive()
                .await
                .expect("server receive")
                .expect("data");
            assert_eq!(&req[..], b"hello");
            stream.send(req).await.expect("server send");
            stream.finish().expect("server finish");
        }
        .group("server")
        .spawn();

        // ── Client — group "client" (primary) ────────────────────────────
        let done = done.clone();
        async move {
            let io = BachIo::new((std::net::Ipv4Addr::UNSPECIFIED, 0).into()).unwrap();

            let client = Client::builder()
                .with_io(io)
                .unwrap()
                .with_tls(tls_client())
                .unwrap()
                .start()
                .unwrap();

            // Resolve the server group's address by hostname (bach assigns each group an IP).
            let server_addr = bach::net::try_lookup(("server", SERVER_PORT)).unwrap();

            let connect = Connect::new(server_addr).with_server_name("localhost");

            // Guard against a silent hang: if the handshake never drives forward, fail loudly.
            let mut connection = timeout(Duration::from_secs(30), client.connect(connect))
                .await
                .expect("handshake timed out")
                .expect("client connect");
            tracing::info!("client: connected, handshake complete");

            let mut stream = connection
                .open_bidirectional_stream()
                .await
                .expect("client open stream");

            stream
                .send(bytes::Bytes::from_static(b"hello"))
                .await
                .expect("client send");
            stream.finish().expect("client finish");

            let resp = stream
                .receive()
                .await
                .expect("client receive")
                .expect("data");
            assert_eq!(&resp[..], b"hello");

            tracing::info!("quic_handshake_over_bach_net passed");
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        .group("client")
        .primary()
        .spawn();
    });

    assert!(
        done_check.load(std::sync::atomic::Ordering::SeqCst),
        "client task did not complete"
    );
}

/// Builds a dc `secret::Map` for the current bach group that advertises `data_addrs` as its
/// data-plane recv addresses via the `DcDataAddresses` transport parameter.
///
/// This is the production wiring (`.with_dc(map)` plus `with_advertised_data_addrs`) reduced to the
/// pieces needed to prove that a real handshake — rather than `insert_fake_path_pair` — populates
/// the map the dc data plane reads from.
fn dc_map(data_addrs: &[SocketAddr]) -> secret::Map {
    secret::Map::builder()
        .with_signer(Signer::random())
        .with_capacity(50_000)
        .with_clock(crate::time::bach::Clock::default())
        .with_subscriber(crate::event::tracing::Subscriber::default())
        .with_advertised_data_addrs(data_addrs)
        .build()
        .unwrap()
}

/// Drives a real QUIC/TLS handshake (over `bach::net`) that carries the dc transport parameters,
/// and asserts that it populates *both* endpoints' `secret::Map`s with reciprocal entries — derived
/// secrets plus the peer's advertised data addresses.
///
/// This is the integration the fake-handshake harness skips: `insert_fake_path_pair` stamps secrets
/// and `peer_data_addrs` directly, whereas here every field is produced by the handshake itself
/// (TLS exporter → secret derivation; `DcDataAddresses` TP → `peer_data_addrs`). Proving it works
/// under bach is the prerequisite for an opt-in real-handshake mode in the sim harness.
#[test]
fn dc_handshake_populates_path_secret_map_over_bach_net() {
    // The data-plane addresses each side advertises. These are deliberately *different* from the
    // handshake listener addresses so the assertion proves the values came from the TP exchange and
    // not from the handshake connection's socket addresses.
    const SERVER_DATA_PORT: u16 = 9001;
    const CLIENT_DATA_PORT: u16 = 9002;

    // A real handshake derives its credential IDs from TLS key material (aws-lc-rs system
    // randomness), which bach's seeded RNG does not intercept — so the `credential_id=[..]` debug
    // line differs every run and cannot be snapshotted. The test's value lives in the explicit
    // `peer_data_addrs` assertions below, which are deterministic.
    let _no_snapshots = crate::testing::without_snapshots();

    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_check = done.clone();

    sim(|| {
        // ── Server — group "server" ──────────────────────────────────────
        async move {
            let server_ip = bach::net::try_lookup(("server", SERVER_PORT)).unwrap().ip();
            let server_data_addr = SocketAddr::new(server_ip, SERVER_DATA_PORT);
            let map = dc_map(&[server_data_addr]);

            let io = BachIo::new((std::net::Ipv4Addr::UNSPECIFIED, SERVER_PORT).into()).unwrap();

            let mut server = Server::builder()
                .with_io(io)
                .unwrap()
                .with_dc(map.clone())
                .unwrap()
                .with_event((ConfirmComplete, MtuConfirmComplete))
                .unwrap()
                .with_tls(tls_server())
                .unwrap()
                .start()
                .unwrap();

            let mut connection = server.accept().await.expect("server accept");
            let peer_addr = connection.remote_addr().expect("server remote addr");

            // Drive dc-handshake confirmation so the entry is committed to the map.
            ConfirmComplete::wait_ready(&mut connection)
                .await
                .expect("server dc confirm");
            tracing::info!("server: dc handshake confirmed");

            // The client advertised CLIENT_DATA_PORT; the entry the server stored for the client
            // must reflect that (with the handshake peer IP substituted for the wildcard bind).
            let entry = map
                .get_raw(peer_addr)
                .expect("server map has an entry for the client after handshake");
            let expected_client_data = SocketAddr::new(peer_addr.ip(), CLIENT_DATA_PORT);
            assert_eq!(
                entry.peer_data_addrs().as_ref(),
                &[expected_client_data],
                "server entry must carry the client's advertised data address"
            );
            tracing::info!("server: entry carries client data addr");

            // Keep the connection alive until the client has finished its own confirmation.
            let _ = MtuConfirmComplete::wait_ready(&mut connection);
        }
        .group("server")
        .spawn();

        // ── Client — group "client" (primary) ────────────────────────────
        let done = done.clone();
        async move {
            let client_ip = bach::net::try_lookup(("client", 0)).unwrap().ip();
            let client_data_addr = SocketAddr::new(client_ip, CLIENT_DATA_PORT);
            let map = dc_map(&[client_data_addr]);

            let io = BachIo::new((std::net::Ipv4Addr::UNSPECIFIED, 0).into()).unwrap();

            let client = Client::builder()
                .with_io(io)
                .unwrap()
                .with_dc(map.clone())
                .unwrap()
                .with_event((ConfirmComplete, MtuConfirmComplete))
                .unwrap()
                .with_tls(tls_client())
                .unwrap()
                .start()
                .unwrap();

            let server_addr = bach::net::try_lookup(("server", SERVER_PORT)).unwrap();
            let connect = Connect::new(server_addr).with_server_name("localhost");

            let mut connection = timeout(Duration::from_secs(30), client.connect(connect))
                .await
                .expect("handshake timed out")
                .expect("client connect");

            ConfirmComplete::wait_ready(&mut connection)
                .await
                .expect("client dc confirm");
            tracing::info!("client: dc handshake confirmed");

            // The server advertised SERVER_DATA_PORT; the client's entry must reflect it.
            let entry = map
                .get_raw(server_addr)
                .expect("client map has an entry for the server after handshake");
            let expected_server_data = SocketAddr::new(server_addr.ip(), SERVER_DATA_PORT);
            assert_eq!(
                entry.peer_data_addrs().as_ref(),
                &[expected_server_data],
                "client entry must carry the server's advertised data address"
            );

            tracing::info!("dc_handshake_populates_path_secret_map_over_bach_net passed");
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        .group("client")
        .primary()
        .spawn();
    });

    assert!(
        done_check.load(std::sync::atomic::Ordering::SeqCst),
        "client task did not complete"
    );
}

/// Builds a dc `secret::Map` suitable for a bach handshake: deterministic signer, bach clock, and
/// the given advertised data addresses.
fn psk_map(data_addrs: &[SocketAddr]) -> secret::Map {
    secret::Map::builder()
        .with_signer(Signer::new(b"default"))
        .with_capacity(50_000)
        .with_clock(crate::time::bach::Clock::default())
        .with_subscriber(crate::event::tracing::Subscriber::default())
        .with_advertised_data_addrs(data_addrs)
        .build()
        .unwrap()
}

/// Drives the *full* production PSK handshake stack — `psk::server::Provider` (accept loop) and
/// `psk::client::Provider` (the `HandshakeQueue` with its de-duplication, rate limiting, and
/// confirmation bookkeeping) — inside a bach simulation.
///
/// This is the next rung above `dc_handshake_populates_path_secret_map_over_bach_net`: that test
/// drove the bare s2n-quic builders, whereas this exercises the same code paths production uses (no
/// OS thread, no tokio runtime — bach scope only). A successful `handshake_with` must leave the
/// client's map holding a usable entry for the server keyed by the resolved server address.
#[test]
fn psk_provider_handshake_over_bach_net() {
    const SERVER_DATA_PORT: u16 = 9101;
    const CLIENT_DATA_PORT: u16 = 9102;

    // Real TLS key material → non-deterministic credential IDs; see the note on the previous test.
    let _no_snapshots = crate::testing::without_snapshots();

    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_check = done.clone();

    sim(|| {
        // ── Server — group "server" ──────────────────────────────────────
        async move {
            let server_ip = bach::net::try_lookup(("server", SERVER_PORT)).unwrap().ip();
            let map = psk_map(&[SocketAddr::new(server_ip, SERVER_DATA_PORT)]);

            // Hold the provider for the lifetime of the group so its accept loop keeps running.
            let _server = crate::psk::server::Provider::builder()
                .start(
                    (std::net::Ipv4Addr::UNSPECIFIED, SERVER_PORT).into(),
                    TestTlsProvider {}.start_server().unwrap(),
                    NoopSubscriber {},
                    map,
                )
                .await
                .expect("server start");
            tracing::info!("server: psk provider listening");

            // Hold the provider alive long enough for the handshake to complete. The accept loop
            // runs as its own bach task (spawned by `server::Provider::setup`); this task only needs
            // to keep `_server` from dropping. A bounded sleep parks with a real waker (bach rejects
            // a wakerless `Pending`), and the whole sim ends once the primary client task finishes.
            bach::time::sleep(Duration::from_secs(300)).await;
            drop(_server);
        }
        .group("server")
        .spawn();

        // ── Client — group "client" (primary) ────────────────────────────
        let done = done.clone();
        async move {
            let client_ip = bach::net::try_lookup(("client", 0)).unwrap().ip();
            let map = psk_map(&[SocketAddr::new(client_ip, CLIENT_DATA_PORT)]);

            let client = crate::psk::client::Provider::builder()
                .with_success_jitter(Duration::ZERO)
                .start(
                    (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
                    map.clone(),
                    TestTlsProvider {}.start_client().unwrap(),
                    NoopSubscriber {},
                    "localhost".into(),
                )
                .expect("client start");

            let server_addr = bach::net::try_lookup(("server", SERVER_PORT)).unwrap();

            timeout(
                Duration::from_secs(30),
                client.handshake_with(server_addr, "localhost".into()),
            )
            .await
            .expect("handshake timed out")
            .expect("psk handshake");
            tracing::info!("client: psk handshake complete");

            // The handshake must have populated the client's map with a usable entry that carries
            // the server's advertised data address.
            let entry = map
                .get_raw(server_addr)
                .expect("client map has an entry for the server after handshake");
            let expected_server_data = SocketAddr::new(server_addr.ip(), SERVER_DATA_PORT);
            assert_eq!(
                entry.peer_data_addrs().as_ref(),
                &[expected_server_data],
                "client entry must carry the server's advertised data address"
            );

            tracing::info!("psk_provider_handshake_over_bach_net passed");
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        .group("client")
        .primary()
        .spawn();
    });

    assert!(
        done_check.load(std::sync::atomic::Ordering::SeqCst),
        "client task did not complete"
    );
}
