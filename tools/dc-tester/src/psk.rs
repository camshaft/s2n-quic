// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Pre-shared key management for the rpc-tester.
//!
//! Uses the PSK builder API from s2n-quic-dc to set up handshake-based
//! client and server providers using test TLS certificates.

use s2n_quic::provider::tls::default as tls;
use s2n_quic_core::crypto::tls::testing::certificates;
use s2n_quic_dc::{
    event::diagnostic,
    path::secret::{self, stateless_reset::Signer},
    psk,
};
use std::{io, net::SocketAddr, path::PathBuf};

pub type Subscriber = (
    s2n_quic_dc::event::tracing::Subscriber,
    diagnostic::Subscriber,
);

pub fn subscriber(trace_dir: &PathBuf) -> Subscriber {
    (
        s2n_quic_dc::event::tracing::Subscriber::default(),
        diagnostic::Subscriber::new(trace_dir.clone()),
    )
}

pub fn server_name() -> s2n_quic::server::Name {
    s2n_quic::server::Name::from("localhost")
}

fn map(trace_dir: &PathBuf) -> secret::Map {
    let signer = Signer::new(b"rpc-tester");
    let clock = s2n_quic_dc::clock::tokio::Clock::default();
    let subscriber = subscriber(trace_dir);
    secret::Map::new(signer, 50_000, true, clock, subscriber)
}

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
    trace_dir: &PathBuf,
) -> io::Result<psk::server::Provider> {
    let map = map(trace_dir);
    let tls = tls_server()?;
    let subscriber = s2n_quic::provider::event::default::Subscriber;

    psk::server::Provider::builder()
        .start(handshake_addr, tls, subscriber, map)
        .await
        .map_err(io::Error::other)
}

pub fn client(trace_dir: &PathBuf) -> io::Result<psk::client::Provider> {
    let map = map(trace_dir);
    let tls = tls_client()?;
    let subscriber = s2n_quic::provider::event::default::Subscriber;

    psk::client::Provider::builder()
        .start(
            "[::]:0".parse().unwrap(),
            map,
            tls,
            subscriber,
            server_name(),
        )
        .map_err(io::Error::other)
}
