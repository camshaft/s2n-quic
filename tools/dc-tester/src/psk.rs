// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use s2n_quic::provider::tls::default as tls;
use s2n_quic_core::crypto::tls::testing::certificates;
use s2n_quic_dc::{path::secret, psk};
use std::{io, net::SocketAddr};

pub fn server_name() -> s2n_quic::server::Name {
    s2n_quic::server::Name::from("localhost")
}

/// Per-stream initial receive window the tester advertises to its peer (`local_recv_max_data`).
///
/// The transport default is 64 KiB, deliberately small so a high fan-out of peers can each be
/// serviced from the recv credit pool. For this point-to-point load tester that fan-out constraint
/// does not apply, and the small default costs a full round trip on any response larger than the
/// window: the sender fills 64 KiB, then blocks waiting for a pool-backed `MAX_DATA` grant (~1 RTT)
/// before it can send the rest. Measured on a two-host cluster placement group (RTT ~76 us), a
/// single-connection request/response pays that extra RTT for responses above 64 KiB:
///   * 64 KiB response: time-to-last-byte p50 166 us -> 133 us (-20%)
///   * 1 MiB response:  time-to-last-byte p50 510 us -> 379 us (-26%), throughput 16.4 -> 20.2 Gbps
/// The effect is concentrated at low concurrency; at higher concurrency other in-flight streams
/// hide the per-stream grant latency. Advertising a larger initial window lets a response ship in a
/// single flight, which is the behavior a latency-sensitive benchmark should measure.
const RECV_WINDOW: u64 = 2 * 1024 * 1024;

fn tls_server() -> io::Result<tls::Server> {
    tls::Server::builder()
        .with_application_protocols(["dcquic"].iter())
        .map_err(io::Error::other)?
        .with_certificate(certificates::CERT_PEM, certificates::KEY_PEM)
        .map_err(io::Error::other)?
        .build()
        .map_err(io::Error::other)
}

fn tls_client() -> io::Result<tls::Client> {
    tls::Client::builder()
        .with_application_protocols(["dcquic"].iter())
        .map_err(io::Error::other)?
        .with_certificate(certificates::CERT_PEM)
        .map_err(io::Error::other)?
        .build()
        .map_err(io::Error::other)
}

pub async fn server(
    handshake_addr: SocketAddr,
    map: secret::Map,
) -> io::Result<psk::server::Provider> {
    let tls = tls_server()?;
    let subscriber = s2n_quic::provider::event::default::Subscriber::default();

    psk::server::Provider::builder()
        .with_recv_window(RECV_WINDOW)
        .start(handshake_addr, tls, subscriber, map)
        .await
        .map_err(io::Error::other)
}

pub fn client(map: secret::Map) -> io::Result<psk::client::Provider> {
    let tls = tls_client()?;
    let subscriber = s2n_quic::provider::event::default::Subscriber::default();

    psk::client::Provider::builder()
        .with_recv_window(RECV_WINDOW)
        .start(
            "[::]:0".parse().unwrap(),
            map,
            tls,
            subscriber,
            server_name(),
        )
        .map_err(io::Error::other)
}
