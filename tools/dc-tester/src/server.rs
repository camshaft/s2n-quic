// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::config::ServerConfig;
use s2n_quic_core::stream::testing::Data;
use s2n_quic_dc::psk;
use std::{
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tracing::{error, info};

type Subscriber = crate::psk::Subscriber;
type Server = s2n_quic_dc::stream::server::tokio::Server<psk::server::Provider, Subscriber>;

/// Server statistics
struct Stats {
    requests_processing: AtomicU64,
    requests_completed: AtomicU64,
    bytes_received: AtomicU64,
    bytes_sent: AtomicU64,
    errors: AtomicU64,
}

impl Stats {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests_processing: AtomicU64::new(0),
            requests_completed: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        })
    }

    fn start_request(&self) {
        self.requests_processing.fetch_add(1, Ordering::Relaxed);
    }

    fn finish_request(&self, bytes_received: u64, bytes_sent: u64, is_error: bool) {
        self.requests_processing.fetch_sub(1, Ordering::Relaxed);
        self.requests_completed.fetch_add(1, Ordering::Relaxed);
        self.bytes_received
            .fetch_add(bytes_received, Ordering::Relaxed);
        self.bytes_sent.fetch_add(bytes_sent, Ordering::Relaxed);
        if is_error {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn report(&self) {
        let processing = self.requests_processing.load(Ordering::Relaxed);
        let completed = self.requests_completed.swap(0, Ordering::Relaxed);
        let received = self.bytes_received.swap(0, Ordering::Relaxed);
        let sent = self.bytes_sent.swap(0, Ordering::Relaxed);
        let errors = self.errors.swap(0, Ordering::Relaxed);
        let success = completed.saturating_sub(errors);
        let success_rate = success as f64 / completed.max(1) as f64;

        info!(
            requests_processing = processing,
            requests_completed = completed,
            bytes_sent = sent,
            bytes_received = received,
            errors,
            success,
            success_rate = %format_args!("{:.2}%", success_rate * 100.0),
            "Server stats"
        );
    }
}

pub async fn run(config: ServerConfig, trace_dir: &PathBuf) -> io::Result<()> {
    info!("Starting RPC test server");

    // Handshake (QUIC) address uses port - 1 to avoid conflict with the acceptor port
    let mut handshake_bind = config.address;
    handshake_bind.set_port(config.address.port() - 1);
    let handshake = crate::psk::server(handshake_bind, trace_dir).await?;

    let server: Server =
        s2n_quic_dc::stream::server::tokio::Server::<psk::server::Provider, Subscriber>::builder()
            .with_address(config.address)
            .with_send_socket_workers(crate::busy_poll::send_pool().into())
            .with_recv_socket_workers(crate::busy_poll::recv_pool().into())
            .build(handshake, crate::psk::subscriber(trace_dir))?;

    let acceptor_addr = server.acceptor_addr()?;
    let handshake_addr = server.handshake_addr()?;

    info!(
        %acceptor_addr,
        %handshake_addr,
        "Server listening"
    );

    let stats = Stats::new();

    // Spawn stats reporter
    {
        let stats = stats.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                stats.report();
            }
        });
    }

    loop {
        let (stream, peer_addr) = server.accept().await?;

        let stats = stats.clone();
        tokio::spawn(async move {
            stats.start_request();
            let (bytes_received, bytes_sent, is_error) = match handle_connection(stream).await {
                Ok((recv, sent)) => (recv, sent, false),
                Err(e) => {
                    // error!(%peer_addr, error = %e, "Error handling connection");
                    (0, 0, true)
                }
            };
            stats.finish_request(bytes_received, bytes_sent, is_error);
        });
    }
}

async fn handle_connection(
    mut stream: s2n_quic_dc::stream::application::Stream<Subscriber>,
) -> io::Result<(u64, u64)> {
    // Read the 8-byte response size header
    let response_size = stream.read_u64().await?;
    let mut total_received = 8u64;

    // Read and validate the rest of the request body using Data
    let mut receiver = Data::new(u64::MAX);
    loop {
        let n = stream.read_into(&mut receiver).await?;
        if n == 0 {
            break;
        }
        total_received += n as u64;
    }

    // Generate and send response data using Data
    let mut response = Data::new(response_size);
    while !response.is_finished() {
        stream.write_from_fin(&mut response).await?;
    }

    Ok((total_received, response_size))
}
