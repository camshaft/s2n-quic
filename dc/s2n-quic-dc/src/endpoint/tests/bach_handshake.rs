// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Spike: prove that a full QUIC/TLS handshake runs to completion inside a `bach` simulation, over
//! the `bach::net`-backed s2n-quic IO provider.
//!
//! This is the feasibility check for running real s2n-quic-dc PSK handshakes deterministically in
//! the bach harness. It does *not* use the dc PSK layer (no secret `Map`, no data-address
//! exchange) — it only exercises the QUIC connection + TLS handshake + a single application stream
//! round-trip, which is the part previously only reachable on a real tokio runtime with OS sockets.

use crate::testing::{ext::*, sim};
use bach::time::timeout;
use s2n_quic::{
    client::Connect,
    provider::{io::bach::Provider as BachIo, tls::default as tls},
    Client, Server,
};
use s2n_quic_core::crypto::tls::testing::certificates;
use std::{sync::Arc, time::Duration};

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
