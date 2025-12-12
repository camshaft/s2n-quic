// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{config::Config, endpoint, protocol::Request, shared_state::SharedState};
use anyhow::Result;
use std::{sync::Arc, time::Duration};

pub struct Server {
    endpoint: Arc<endpoint::Server>,
    state: Option<SharedState>,
}

impl Server {
    pub async fn new(config: Config) -> Result<Self> {
        let endpoint = Arc::new(endpoint::Server::new(&config).await?);
        Ok(Self {
            endpoint,
            state: None,
        })
    }

    pub fn with_state(mut self, state: SharedState) -> Self {
        self.state = Some(state);
        self
    }

    pub fn metrics(&self) -> &crate::metrics::MetricsSubscriber {
        &self.endpoint.metrics
    }

    pub async fn run(&self) -> Result<()> {
        let listen_msg = format!(
            "Server listening on acceptor={} handshake={}",
            self.endpoint.acceptor_addr()?,
            self.endpoint.handshake_addr()?
        );
        tracing::info!("{}", listen_msg);

        if let Some(state) = &self.state {
            state.add_log(listen_msg);
            state.set_status("Running - accepting connections".to_string());
        }

        // Start metrics update task if using TUI
        let metrics_handle = if let Some(state) = &self.state {
            let state = state.clone();
            let metrics = self.endpoint.metrics.clone();
            Some(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    state.update_metrics(&metrics);
                }
            }))
        } else {
            None
        };

        let state = self.state.clone();
        let active_connections = Arc::new(std::sync::atomic::AtomicU64::new(0));

        loop {
            // Accept incoming stream
            match self.endpoint.stream_server.accept().await {
                Ok((stream, peer_addr)) => {
                    tracing::info!("Accepted stream from {}", peer_addr);

                    let current_active =
                        active_connections.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;

                    if let Some(ref state) = state {
                        state.set_active_connections(current_active);
                        state.increment_total_connections();
                    }

                    let state_clone = state.clone();
                    let active_connections_clone = active_connections.clone();
                    tokio::spawn(async move {
                        let result = Self::handle_stream(stream).await;

                        // Decrement active connections when stream completes
                        let current_active = active_connections_clone
                            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed)
                            - 1;
                        if let Some(ref state) = state_clone {
                            state.set_active_connections(current_active);
                        }

                        if let Err(e) = result {
                            let error_msg = format!("Error handling stream: {}", e);
                            tracing::error!("{}", error_msg);
                            if let Some(ref state) = state_clone {
                                state.add_log(error_msg);
                            }
                        } else {
                            tracing::info!("Stream handled successfully");
                        }
                    });
                }
                Err(e) => {
                    let error_msg = format!("Error accepting stream: {}", e);
                    tracing::error!("{}", error_msg);
                    if let Some(ref state) = state {
                        state.add_log(error_msg);
                    }
                }
            }
        }

        // This is unreachable, but if we ever add a shutdown mechanism:
        #[allow(unreachable_code)]
        {
            if let Some(handle) = metrics_handle {
                handle.abort();
            }
            Ok(())
        }
    }

    async fn handle_stream(
        stream: s2n_quic_dc::stream::application::Stream<endpoint::EventSubscriber>,
    ) -> Result<()> {
        let (mut reader, mut writer) = stream.into_split();

        let mut request_buf: Vec<u8> = Vec::with_capacity(9000);
        loop {
            let len = reader.read_into(&mut request_buf).await?;
            if len == 0 {
                break;
            }
        }

        let request = Request::decode(&request_buf)
            .map_err(|e| anyhow::anyhow!("Failed to decode request: {:?}", e))?;

        tracing::debug!(
            "Received request: delay={}ms, response_size={}",
            request.response_delay_ms,
            request.response_size
        );

        // Wait for the requested delay
        if request.response_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(request.response_delay_ms)).await;
        }

        // Send response of the requested size
        let mut response = s2n_quic_core::stream::testing::Data::new(request.response_size);
        writer.write_all_from_fin(&mut response).await?;

        tracing::debug!("Sent response of {} bytes", request.response_size);

        Ok(())
    }
}
