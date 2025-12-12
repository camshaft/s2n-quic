// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Transport Pool - the single entry point for all application transfers
//!
//! The pool provides Arc-based cloneable access to the transport,
//! managing worker lifecycle and providing distinct APIs for each
//! transfer pattern.

use crate::v2::{
    Buffer, BufferDescriptor, BufferId, CausalityToken,
    transfer::*,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::oneshot;

/// Configuration for the transport pool
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Number of dedicated CPU cores for workers
    pub dedicated_cores: usize,
    /// CPU IDs to pin workers to
    pub cpu_ids: Option<Vec<usize>>,
    /// Initial causality map size
    pub initial_causality_map_size: usize,
    /// Maximum causality map size
    pub max_causality_map_size: usize,
    /// Buffer pool size limit (bytes)
    pub buffer_pool_size_limit: usize,
    /// Worker queue depth per priority level
    pub worker_queue_depth_per_priority: usize,
    /// Per-peer concurrent transfer limit
    pub per_peer_transfer_limit: usize,
    /// Expected bandwidth per NIC (Gbps)
    pub expected_bandwidth_gbps: Option<u32>,
    /// Whether to enable EFA support
    pub enable_efa: bool,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            dedicated_cores: 1,
            cpu_ids: None,
            initial_causality_map_size: 1024,
            max_causality_map_size: 1_000_000,
            buffer_pool_size_limit: 1024 * 1024 * 1024, // 1 GB
            worker_queue_depth_per_priority: 1000,
            per_peer_transfer_limit: 100,
            expected_bandwidth_gbps: None,
            enable_efa: true,
        }
    }
}

impl PoolConfig {
    /// Create a new pool configuration with defaults
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the number of dedicated cores
    pub fn with_dedicated_cores(mut self, cores: usize) -> Self {
        self.dedicated_cores = cores;
        self
    }

    /// Set specific CPU IDs to pin workers to
    pub fn with_cpu_ids(mut self, cpu_ids: Vec<usize>) -> Self {
        self.cpu_ids = Some(cpu_ids);
        self
    }

    /// Set the initial causality map size
    pub fn with_initial_causality_map_size(mut self, size: usize) -> Self {
        self.initial_causality_map_size = size;
        self
    }

    /// Set the buffer pool size limit
    pub fn with_buffer_pool_size_limit(mut self, limit: usize) -> Self {
        self.buffer_pool_size_limit = limit;
        self
    }

    /// Set the expected bandwidth per NIC
    pub fn with_expected_bandwidth_gbps(mut self, bandwidth: u32) -> Self {
        self.expected_bandwidth_gbps = Some(bandwidth);
        self
    }

    /// Enable or disable EFA support
    pub fn with_efa(mut self, enable: bool) -> Self {
        self.enable_efa = enable;
        self
    }
}

/// Inner state of the transport pool
struct PoolInner {
    config: PoolConfig,
    // Additional state will be added in implementation
}

/// Transport pool handle
///
/// This is the main entry point for all application transfers.
/// It can be cheaply cloned to share across multiple threads.
#[derive(Clone)]
pub struct Pool {
    inner: Arc<PoolInner>,
}

impl Pool {
    /// Create a new transport pool with the given configuration
    ///
    /// # Returns
    ///
    /// Returns a Result with the initialized pool or an error if
    /// initialization fails (e.g., unable to allocate cores, EFA
    /// unavailable when required, etc.)
    pub async fn new(config: PoolConfig) -> Result<Self, PoolError> {
        // TODO: Implementation
        // - Initialize NIC workers
        // - Set up request router
        // - Allocate causality map
        // - Initialize buffer pool
        // - Set up ring buffers for app-worker communication
        // - Configure core pinning
        todo!("Pool initialization not yet implemented")
    }

    /// Allocate a buffer for transfer data
    ///
    /// # Arguments
    ///
    /// * `size` - The size of the buffer to allocate in bytes
    ///
    /// # Returns
    ///
    /// Returns a buffer descriptor that can be written to and then
    /// used in transfer requests.
    pub async fn allocate_buffer(&self, size: usize) -> Result<BufferDescriptor, PoolError> {
        // TODO: Implementation
        // - Allocate from buffer pool
        // - Apply backpressure if pool is full
        // - Return buffer descriptor in Allocated state
        todo!("Buffer allocation not yet implemented")
    }

    /// Initiate a unary RPC transfer
    ///
    /// Sends a single request and receives a single response.
    ///
    /// # Arguments
    ///
    /// * `request` - The unary request with peer, buffer, and options
    ///
    /// # Returns
    ///
    /// Returns a result containing a oneshot receiver for the response
    /// and an optional causality token if tracking was requested.
    pub async fn unary_rpc(
        &self,
        request: UnaryRequest,
    ) -> Result<TransferResult<oneshot::Receiver<Result<UnaryResponse, TransferError>>>, PoolError>
    {
        // TODO: Implementation
        // - Validate buffer state
        // - Route request to appropriate worker
        // - Allocate causality token if requested
        // - Create oneshot channel for response
        // - Enqueue transfer in worker ring buffer
        // - Apply backpressure if queue is full
        todo!("Unary RPC not yet implemented")
    }

    /// Initiate a streaming response transfer
    ///
    /// Sends a single request and receives multiple responses over time.
    ///
    /// # Arguments
    ///
    /// * `request` - The streaming response request
    ///
    /// # Returns
    ///
    /// Returns a result containing an async stream of responses and
    /// an optional causality token.
    pub async fn streaming_response(
        &self,
        request: StreamingResponseRequest,
    ) -> Result<TransferResult<ResponseStream>, PoolError> {
        // TODO: Implementation
        // - Validate buffer state
        // - Route request to appropriate worker
        // - Allocate causality token if requested
        // - Create mpsc channel for response stream
        // - Enqueue transfer in worker ring buffer
        todo!("Streaming response not yet implemented")
    }

    /// Initiate a streaming request transfer
    ///
    /// Sends multiple requests over time and receives a single response.
    ///
    /// # Arguments
    ///
    /// * `peer` - The destination peer address
    /// * `options` - Transfer options
    ///
    /// # Returns
    ///
    /// Returns a result containing a request stream sender and a
    /// response oneshot receiver, plus an optional causality token.
    pub async fn streaming_request(
        &self,
        peer: SocketAddr,
        options: TransferOptions,
    ) -> Result<
        TransferResult<(RequestStream, oneshot::Receiver<Result<UnaryResponse, TransferError>>)>,
        PoolError,
    > {
        // TODO: Implementation
        // - Route request to appropriate worker
        // - Allocate causality token if requested
        // - Create mpsc channel for request stream
        // - Create oneshot channel for response
        // - Enqueue transfer in worker ring buffer
        todo!("Streaming request not yet implemented")
    }

    /// Initiate a bidirectional transfer
    ///
    /// Sends multiple requests and receives multiple responses concurrently.
    ///
    /// # Arguments
    ///
    /// * `request` - The bidirectional request with initial payload
    ///
    /// # Returns
    ///
    /// Returns a result containing a bidirectional stream with both
    /// request sender and response receiver, plus an optional causality token.
    pub async fn bidirectional(
        &self,
        request: BidirectionalRequest,
    ) -> Result<TransferResult<BidirectionalStream>, PoolError> {
        // TODO: Implementation
        // - Validate buffer state
        // - Route request to appropriate worker
        // - Allocate causality token if requested
        // - Create bidirectional channels
        // - Enqueue transfer in worker ring buffer
        todo!("Bidirectional not yet implemented")
    }

    /// Initiate a raw bulk transfer
    ///
    /// Transfers bulk data with direct memory/disk placement.
    ///
    /// # Arguments
    ///
    /// * `request` - The raw bulk transfer request
    ///
    /// # Returns
    ///
    /// Returns a result containing a oneshot receiver for completion
    /// notification and an optional causality token.
    pub async fn raw_bulk_transfer(
        &self,
        request: RawBulkTransferRequest,
    ) -> Result<
        TransferResult<oneshot::Receiver<Result<RawBulkTransferComplete, TransferError>>>,
        PoolError,
    > {
        // TODO: Implementation
        // - Validate buffer exists and is in Filled state
        // - Route request to appropriate worker
        // - Allocate causality token if requested
        // - Create oneshot channel for completion
        // - Enqueue transfer in worker ring buffer
        todo!("Raw bulk transfer not yet implemented")
    }

    /// Gracefully shutdown the transport pool
    ///
    /// Waits for in-flight transfers to complete (up to a timeout)
    /// before shutting down workers.
    pub async fn shutdown(&self) -> Result<(), PoolError> {
        // TODO: Implementation
        // - Stop accepting new transfers
        // - Wait for in-flight transfers to complete
        // - Shutdown workers
        // - Clean up resources
        todo!("Shutdown not yet implemented")
    }
}

/// Errors that can occur during pool operations
#[derive(Debug, Clone, thiserror::Error)]
pub enum PoolError {
    /// Failed to initialize the pool
    #[error("initialization failed: {0}")]
    InitializationFailed(String),

    /// Failed to allocate buffer
    #[error("buffer allocation failed: {0}")]
    BufferAllocationFailed(String),

    /// Pool is at capacity and applying backpressure
    #[error("pool at capacity")]
    AtCapacity,

    /// Invalid configuration
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),

    /// Worker failure
    #[error("worker failure: {0}")]
    WorkerFailure(String),

    /// Pool is shutting down
    #[error("pool is shutting down")]
    ShuttingDown,

    /// Other error
    #[error("pool error: {0}")]
    Other(String),
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("config", &self.inner.config)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_config_builder() {
        let config = PoolConfig::new()
            .with_dedicated_cores(4)
            .with_cpu_ids(vec![0, 1, 2, 3])
            .with_buffer_pool_size_limit(2 * 1024 * 1024 * 1024)
            .with_expected_bandwidth_gbps(25);

        assert_eq!(config.dedicated_cores, 4);
        assert_eq!(config.cpu_ids, Some(vec![0, 1, 2, 3]));
        assert_eq!(config.buffer_pool_size_limit, 2 * 1024 * 1024 * 1024);
        assert_eq!(config.expected_bandwidth_gbps, Some(25));
    }

    #[test]
    fn test_pool_clone() {
        // Pool should be cheaply cloneable
        let config = PoolConfig::new();
        // We can't actually create a pool in tests without full implementation,
        // but we can verify the Clone trait is implemented
        let _config2 = config.clone();
    }
}
