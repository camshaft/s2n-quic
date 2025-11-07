// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{config::Config, metrics::MetricsSubscriber};
use anyhow::Result;
use s2n_quic_dc::{
    clock::tokio::Clock,
    path::secret::{stateless_reset::Signer, Map},
    psk, stream,
};
use std::net::SocketAddr;

/// Creates a test TLS provider for DC QUIC
fn tls_server() -> impl s2n_quic::provider::tls::Provider {
    use s2n_quic::provider::tls::default as tls;
    use s2n_quic_core::crypto::tls::testing::certificates;

    tls::Server::builder()
        .with_application_protocols(["dc"].iter())
        .expect("failed to set app protocols")
        .with_certificate(certificates::CERT_PEM, certificates::KEY_PEM)
        .expect("failed to set certificate")
        .build()
        .expect("failed to build TLS server")
}

fn tls_client() -> impl s2n_quic::provider::tls::Provider {
    use s2n_quic::provider::tls::default as tls;
    use s2n_quic_core::crypto::tls::testing::certificates;

    tls::Client::builder()
        .with_application_protocols(["dc"].iter())
        .expect("failed to set app protocols")
        .with_certificate(certificates::CERT_PEM)
        .expect("failed to set certificate")
        .build()
        .expect("failed to build TLS client")
}

fn secret_map() -> Map {
    let signer = Signer::new(b"test-secret");
    let capacity = 50_000;
    let clock = Clock::default();
    let subscriber = s2n_quic_dc::event::tracing::Subscriber::default();
    Map::new(signer, capacity, clock, subscriber)
}

pub fn server_name() -> s2n_quic::server::Name {
    s2n_quic::server::Name::from_static("localhost")
}

/// Server endpoint for handling incoming streams
pub struct Server {
    pub handshake: psk::server::Provider,
    pub stream_server: stream::server::tokio::Server<psk::server::Provider, EventSubscriber>,
    pub metrics: MetricsSubscriber,
}

pub type EventSubscriber = (
    s2n_quic_dc::event::tracing::Subscriber,
    crate::metrics::MetricsSubscriber,
);

impl Server {
    pub async fn new(config: &Config) -> Result<Self> {
        let metrics = MetricsSubscriber::new();

        let mut handshake_addr = config.server.listen_address;
        handshake_addr.set_port(handshake_addr.port() + 1);

        // Create PSK handshake server
        let handshake = psk::server::Provider::builder()
            .start(
                handshake_addr,
                tls_server(),
                s2n_quic::provider::event::tracing::Subscriber::default(),
                secret_map(),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Failed to start PSK server: {}", e))?;

        // Create stream server with metrics subscriber
        let subscriber = (
            s2n_quic_dc::event::tracing::Subscriber::default(),
            metrics.clone(),
        );
        let stream_server = stream::server::tokio::Builder::default()
            .with_address(config.server.listen_address)
            .with_tcp(false)
            .with_udp(true)
            .build(handshake.clone(), subscriber)?;

        tracing::info!(
            "Server listening on acceptor={} handshake={}",
            stream_server.acceptor_addr()?,
            handshake_addr,
        );

        Ok(Self {
            handshake,
            stream_server,
            metrics,
        })
    }

    pub fn acceptor_addr(&self) -> Result<SocketAddr> {
        Ok(self.stream_server.acceptor_addr()?)
    }

    pub fn handshake_addr(&self) -> Result<SocketAddr> {
        Ok(self.stream_server.handshake_addr()?)
    }
}

/// Client endpoint for opening streams
pub struct Client {
    pub handshake: psk::client::Provider,
    pub stream_client: stream::client::tokio::Client<psk::client::Provider, EventSubscriber>,
    pub metrics: MetricsSubscriber,
}

impl Client {
    pub async fn new(_config: &Config) -> Result<Self> {
        let metrics = MetricsSubscriber::new();

        // Create PSK handshake client
        let handshake = psk::client::Provider::builder()
            .start(
                "[::]:0".parse()?,
                secret_map(),
                tls_client(),
                s2n_quic::provider::event::tracing::Subscriber::default(),
                |_, _| {},
                server_name(),
            )
            .map_err(|e| anyhow::anyhow!("Failed to start PSK client: {}", e))?;

        // Create stream client with metrics subscriber
        let subscriber = (
            s2n_quic_dc::event::tracing::Subscriber::default(),
            metrics.clone(),
        );
        let stream_client = stream::client::tokio::Builder::default()
            .with_default_protocol(stream::socket::Protocol::Udp)
            .build(handshake.clone(), subscriber)?;

        tracing::info!("Client initialized");

        Ok(Self {
            handshake,
            stream_client,
            metrics,
        })
    }
}
