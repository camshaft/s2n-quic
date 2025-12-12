// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    config::{Config, WorkloadConfig},
    endpoint,
    protocol::Request,
    shared_state::SharedState,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use s2n_quic_core::{
    buffer::reader::{self, storage::Infallible},
    stream::testing::Data,
};
use std::{net::SocketAddr, sync::Arc, time::Instant};

pub struct Client {
    config: Config,
    endpoint: Arc<endpoint::Client>,
    server_handshake_addr: SocketAddr,
    server_acceptor_addr: SocketAddr,
    state: Option<SharedState>,
}

impl Client {
    pub async fn new(config: Config, server_addr: SocketAddr) -> Result<Self> {
        let endpoint = Arc::new(endpoint::Client::new(&config).await?);

        // Server acceptor is on the specified port, handshake is on port+1
        let server_acceptor_addr = server_addr;
        let mut server_handshake_addr = server_addr;
        server_handshake_addr.set_port(server_addr.port() + 1);

        Ok(Self {
            config,
            endpoint,
            server_handshake_addr,
            server_acceptor_addr,
            state: None,
        })
    }

    pub fn with_state(mut self, state: SharedState) -> Self {
        self.state = Some(state);
        self
    }

    pub async fn run_workload(&self, workload_name: &str) -> Result<()> {
        let workload =
            self.config.workload.get(workload_name).with_context(|| {
                format!("Workload '{}' not found in configuration", workload_name)
            })?;

        let status_msg = format!(
            "Starting workload '{}': {} streams, request_size={}, response_size={}, delay={}ms",
            workload_name,
            workload.num_streams,
            workload.request_size,
            workload.response_size,
            workload.delay_ms
        );

        tracing::info!("{}", status_msg);
        if let Some(state) = &self.state {
            state.add_log(status_msg.clone());
            state.set_status(format!("Running workload '{}'", workload_name));
        }

        let start = Instant::now();

        // Start metrics update task if using TUI
        let metrics_handle = if let Some(state) = &self.state {
            let state = state.clone();
            let metrics = self.endpoint.metrics.clone();
            Some(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    state.update_metrics(&metrics);
                }
            }))
        } else {
            None
        };

        // Launch all streams concurrently
        let mut handles = Vec::with_capacity(workload.num_streams);

        for stream_id in 0..workload.num_streams {
            let endpoint = self.endpoint.clone();
            let handshake_addr = self.server_handshake_addr;
            let acceptor_addr = self.server_acceptor_addr;
            let workload = workload.clone();
            let state = self.state.clone();

            let handle = tokio::spawn(async move {
                let result = Self::run_stream(
                    stream_id,
                    endpoint,
                    handshake_addr,
                    acceptor_addr,
                    &workload,
                )
                .await;

                // Update counters immediately as each stream completes
                if let Some(ref state) = state {
                    state.increment_total_connections();
                    if result.is_err() {
                        state.increment_stream_errors();
                    }
                }

                result
            });

            handles.push(handle);
        }

        // Wait for all streams to complete
        let mut successful = 0;
        let mut failed = 0;

        for handle in handles {
            match handle.await {
                Ok(Ok(())) => {
                    successful += 1;
                }
                Ok(Err(e)) => {
                    tracing::error!("Stream failed: {}", e);
                    failed += 1;
                }
                Err(e) => {
                    tracing::error!("Task panicked: {}", e);
                    // Task panics also need to update counters since they didn't run the completion code
                    if let Some(ref state) = self.state {
                        state.increment_stream_errors();
                        state.increment_total_connections();
                    }
                    failed += 1;
                }
            }
        }

        // Stop metrics update task
        if let Some(handle) = metrics_handle {
            handle.abort();
        }

        let elapsed = start.elapsed();

        let completion_msg = format!(
            "Workload '{}' completed in {:?}: {} successful, {} failed",
            workload_name, elapsed, successful, failed
        );
        tracing::info!("{}", completion_msg);
        if let Some(state) = &self.state {
            state.add_log(completion_msg);
        }

        // Report metrics
        let snapshot = self.metrics().snapshot();
        let goodput_pct = snapshot.goodput.goodput_percentage();
        let loss_rate = snapshot.packet_loss_rate();
        let avg_latency = snapshot
            .average_latency()
            .map(|d| format!("{:?}", d))
            .unwrap_or_else(|| "N/A".to_string());

        let metrics_msg = format!(
            "Metrics: goodput={:.2}%, loss={:.2}%, avg_latency={}, acked_bytes={}, tx_bytes={}, ctrl_bytes={}",
            goodput_pct,
            loss_rate,
            avg_latency,
            snapshot.goodput.acked_payload_bytes,
            snapshot.goodput.stream_packet_bytes,
            snapshot.goodput.control_packet_bytes
        );
        tracing::info!("{}", metrics_msg);
        if let Some(state) = &self.state {
            state.add_log(metrics_msg);
            state.set_status("Workload completed".to_string());
        }

        Ok(())
    }

    async fn run_stream(
        stream_id: usize,
        endpoint: Arc<endpoint::Client>,
        handshake_addr: SocketAddr,
        acceptor_addr: SocketAddr,
        workload: &WorkloadConfig,
    ) -> Result<()> {
        let start = Instant::now();

        // Add timeout to prevent hanging indefinitely
        let timeout_duration = std::time::Duration::from_secs(30);

        let result = tokio::time::timeout(timeout_duration, async {
            // Connect to server
            tracing::debug!("Stream {} connecting to server...", stream_id);
            let stream = endpoint
                .stream_client
                .connect(handshake_addr, acceptor_addr, endpoint::server_name())
                .await
                .with_context(|| {
                    format!(
                        "Stream {} failed to connect to handshake={} acceptor={}",
                        stream_id, handshake_addr, acceptor_addr
                    )
                })?;

            tracing::debug!("Stream {} connected successfully", stream_id);
            let (mut reader, mut writer) = stream.into_split();

            // Create request
            let request = Request::new(workload.delay_ms, workload.response_size as u64);
            let mut request = RequestData {
                header: request.encode(),
                body: Data::new(workload.request_size as u64),
            };

            tracing::debug!("Stream {} sending request...", stream_id);
            writer
                .write_all_from_fin(&mut request)
                .await
                .with_context(|| format!("Stream {} failed to send request", stream_id))?;

            tracing::debug!("Stream {} request sent, waiting for response...", stream_id);
            // Read response
            let mut response_buf: Vec<u8> = Vec::with_capacity(workload.response_size);
            loop {
                let len = reader
                    .read_into(&mut response_buf)
                    .await
                    .with_context(|| format!("Stream {} failed to read response", stream_id))?;
                if len == 0 {
                    break;
                }
            }

            tracing::debug!("Stream {} received {} bytes", stream_id, response_buf.len());
            Ok::<(), anyhow::Error>(())
        })
        .await
        .with_context(|| {
            format!(
                "Stream {} timed out after {:?}",
                stream_id, timeout_duration
            )
        })?;

        result?;

        let elapsed = start.elapsed();

        tracing::debug!("Stream {} completed in {:?}", stream_id, elapsed);

        Ok(())
    }

    pub fn workload_names(&self) -> Vec<String> {
        self.config.workload.keys().cloned().collect()
    }

    pub fn metrics(&self) -> &crate::metrics::MetricsSubscriber {
        &self.endpoint.metrics
    }
}

struct RequestData {
    header: Bytes,
    body: Data,
}

impl reader::Storage for RequestData {
    type Error = core::convert::Infallible;

    fn buffered_len(&self) -> usize {
        self.header.len() + self.body.buffered_len()
    }

    fn read_chunk(&mut self, watermark: usize) -> Result<reader::storage::Chunk, Self::Error> {
        if !self.header.is_empty() {
            self.header.read_chunk(watermark)
        } else {
            self.body.read_chunk(watermark)
        }
    }

    fn partial_copy_into<Dest>(
        &mut self,
        dest: &mut Dest,
    ) -> Result<reader::storage::Chunk, Self::Error>
    where
        Dest: s2n_quic_core::buffer::writer::Storage + ?Sized,
    {
        if !self.header.is_empty() {
            self.header.infallible_copy_into(dest);
        }
        if dest.has_remaining_capacity() {
            self.body.partial_copy_into(dest)
        } else {
            Ok(reader::storage::Chunk::empty())
        }
    }

    fn buffer_is_empty(&self) -> bool {
        self.header.is_empty() && self.body.buffer_is_empty()
    }
}
