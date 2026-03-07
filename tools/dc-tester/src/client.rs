// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::config::{ClientConfig, WorkloadConfig};
use s2n_quic_core::{buffer::Reader as _, stream::testing::Data};
use s2n_quic_dc::{psk, stream::socket};
use std::{
    io,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tracing::{error, info, warn};

type Subscriber = crate::psk::Subscriber;
type Client = s2n_quic_dc::stream::client::tokio::Client<psk::client::Provider, Subscriber>;

/// Client statistics per workload
struct WorkloadStats {
    workload_name: String,
    requests_in_flight: AtomicU64,
    requests_completed: AtomicU64,
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
    errors: AtomicU64,
}

impl WorkloadStats {
    fn new(workload_name: String) -> Arc<Self> {
        Arc::new(Self {
            workload_name,
            requests_in_flight: AtomicU64::new(0),
            requests_completed: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        })
    }

    fn start_request(&self) {
        self.requests_in_flight.fetch_add(1, Ordering::Relaxed);
    }

    fn finish_request(&self, bytes_sent: u64, bytes_received: u64, is_error: bool) {
        self.requests_in_flight.fetch_sub(1, Ordering::Relaxed);
        self.requests_completed.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent.fetch_add(bytes_sent, Ordering::Relaxed);
        self.bytes_received
            .fetch_add(bytes_received, Ordering::Relaxed);
        if is_error {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn report(&self) {
        let in_flight = self.requests_in_flight.load(Ordering::Relaxed);
        let completed = self.requests_completed.swap(0, Ordering::Relaxed);
        let sent = self.bytes_sent.swap(0, Ordering::Relaxed);
        let received = self.bytes_received.swap(0, Ordering::Relaxed);
        let errors = self.errors.swap(0, Ordering::Relaxed);
        let success = completed.saturating_sub(errors);
        let success_rate = success as f64 / completed.max(1) as f64;

        info!(
            workload = %self.workload_name,
            requests_in_flight = in_flight,
            requests_completed = completed,
            bytes_sent = sent,
            bytes_received = received,
            errors,
            success_rate = %format_args!("{:.2}%", success_rate * 100.0),
            "Workload stats"
        );
    }
}

pub async fn run(
    config: ClientConfig,
    acceptor_addr: SocketAddr,
    handshake_addr: SocketAddr,
) -> io::Result<()> {
    info!(
        workload_count = config.workloads.len(),
        %acceptor_addr,
        %handshake_addr,
        "Starting RPC test client"
    );

    if config.workloads.is_empty() {
        warn!("No workloads configured");
        return Ok(());
    }

    let handshake = crate::psk::client()?;

    let client: Client =
        s2n_quic_dc::stream::client::tokio::Client::<psk::client::Provider, Subscriber>::builder()
            .with_default_protocol(socket::Protocol::Udp)
            .with_send_socket_workers(crate::busy_poll::send_pool().into())
            .with_recv_socket_workers(crate::busy_poll::recv_pool().into())
            .build(handshake, crate::psk::subscriber())?;

    let server_name = crate::psk::server_name();

    let mut handles = Vec::new();

    for workload in config.workloads {
        let workload_stats = WorkloadStats::new(workload.name.clone());

        // Spawn stats reporter for this workload
        {
            let stats = workload_stats.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(10));
                loop {
                    interval.tick().await;
                    stats.report();
                }
            });
        }

        for worker_id in 0..workload.workers {
            let client = client.clone();
            let workload = workload.clone();
            let stats = workload_stats.clone();
            let server_name = server_name.clone();
            let handle = tokio::spawn(async move {
                run_worker(
                    client,
                    acceptor_addr,
                    handshake_addr,
                    server_name,
                    workload,
                    worker_id,
                    stats,
                )
                .await
            });
            handles.push(handle);
        }
    }

    // Wait for all workers (they run forever)
    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}

async fn run_worker(
    client: Client,
    acceptor_addr: SocketAddr,
    handshake_addr: SocketAddr,
    server_name: s2n_quic::server::Name,
    workload: WorkloadConfig,
    worker_id: usize,
    stats: Arc<WorkloadStats>,
) {
    info!(
        workload = %workload.name,
        worker_id,
        "Starting worker"
    );

    let delay = if workload.request_delay_ms > 0 {
        Some(Duration::from_millis(workload.request_delay_ms))
    } else {
        None
    };

    loop {
        stats.start_request();
        let (bytes_sent, bytes_received, is_error) = match execute_request(
            &client,
            acceptor_addr,
            handshake_addr,
            server_name.clone(),
            &workload,
        )
        .await
        {
            Ok((sent, received)) => (sent, received, false),
            Err(e) => {
                // error!(
                //     workload = %workload.name,
                //     worker_id,
                //     error = %e,
                //     "Request failed"
                // );
                (0, 0, true)
            }
        };
        stats.finish_request(bytes_sent, bytes_received, is_error);

        // Delay before next request if configured
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
    }
}

async fn execute_request(
    client: &Client,
    acceptor_addr: SocketAddr,
    handshake_addr: SocketAddr,
    server_name: s2n_quic::server::Name,
    workload: &WorkloadConfig,
) -> io::Result<(u64, u64)> {
    // Connect to the server
    let mut stream = client
        .connect(handshake_addr, acceptor_addr, server_name)
        .await?;

    // Write the 8-byte response size header
    stream.write_u64(workload.response_size).await?;

    // Write request body using Data
    let mut request = Data::new(workload.request_size);
    while !request.is_finished() {
        stream.write_from_fin(&mut request).await?;
    }

    let bytes_sent = 8 + workload.request_size;

    // Read and validate response using Data
    let mut response = Data::new(workload.response_size);
    loop {
        let n = stream.read_into(&mut response).await?;
        if n == 0 {
            break;
        }
    }

    if !response.is_finished() {
        return Err(io::Error::other(format!(
            "response was not fully received: expected {} bytes, got {} bytes",
            workload.response_size,
            response.current_offset()
        )));
    }

    Ok((bytes_sent, workload.response_size))
}
