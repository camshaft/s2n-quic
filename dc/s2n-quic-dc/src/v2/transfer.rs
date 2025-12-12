// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Transfer types and options
//!
//! This module defines the distinct transfer patterns supported by the
//! transport: unary RPC, streaming response, streaming request,
//! bidirectional, and raw bulk transfer.

use crate::v2::{Buffer, BufferId, CausalityToken, Dependency, Priority};
use bytes::Bytes;
use std::net::SocketAddr;
use tokio::sync::{mpsc, oneshot};

/// Options for configuring a transfer
#[derive(Debug, Clone)]
pub struct TransferOptions {
    /// Priority level for this transfer
    pub priority: Priority,
    /// Causality dependencies for this transfer
    pub dependencies: Vec<Dependency>,
    /// Application metadata for RPC handler dispatch
    pub metadata: Option<Bytes>,
    /// Whether to request causality tracking for this transfer
    pub track_causality: bool,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            priority: Priority::default(),
            dependencies: Vec::new(),
            metadata: None,
            track_causality: false,
        }
    }
}

impl TransferOptions {
    /// Create new transfer options
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the priority level
    pub fn with_priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    /// Add a dependency
    pub fn with_dependency(mut self, dependency: Dependency) -> Self {
        self.dependencies.push(dependency);
        self
    }

    /// Add multiple dependencies
    pub fn with_dependencies(mut self, dependencies: Vec<Dependency>) -> Self {
        self.dependencies.extend(dependencies);
        self
    }

    /// Set application metadata
    pub fn with_metadata(mut self, metadata: Bytes) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Enable causality tracking
    pub fn with_causality_tracking(mut self) -> Self {
        self.track_causality = true;
        self
    }
}

/// Result of initiating a transfer
///
/// Contains an optional causality token if causality tracking was requested.
#[derive(Debug)]
pub struct TransferResult<T> {
    /// The response receiver or stream
    pub response: T,
    /// Causality token if tracking was requested
    pub causality_token: Option<CausalityToken>,
}

/// Marker trait for transfer types
pub trait TransferType: Send + 'static {}

/// Unary RPC: single request, single response
///
/// This is the simplest transfer pattern where the application sends
/// a single request and receives a single response.
pub struct UnaryRpc;
impl TransferType for UnaryRpc {}

/// Streaming response: single request, multiple responses
///
/// The application sends a single request and receives multiple
/// responses over time.
pub struct StreamingResponse;
impl TransferType for StreamingResponse {}

/// Streaming request: multiple requests, single response
///
/// The application sends multiple requests over time and receives
/// a single response when complete.
pub struct StreamingRequest;
impl TransferType for StreamingRequest {}

/// Bidirectional: multiple requests and responses
///
/// The application can send multiple requests and receive multiple
/// responses concurrently.
pub struct Bidirectional;
impl TransferType for Bidirectional {}

/// Raw bulk transfer: direct memory or disk placement
///
/// For large data transfers with optional cache teeing.
pub struct RawBulkTransfer;
impl TransferType for RawBulkTransfer {}

/// Request for a unary RPC
#[derive(Debug)]
pub struct UnaryRequest {
    /// Destination peer address
    pub peer: SocketAddr,
    /// Request payload buffer
    pub buffer: Buffer,
    /// Transfer options
    pub options: TransferOptions,
}

impl UnaryRequest {
    /// Create a new unary request
    pub fn new(peer: SocketAddr, buffer: Buffer, options: TransferOptions) -> Self {
        Self {
            peer,
            buffer,
            options,
        }
    }
}

/// Response for a unary RPC
#[derive(Debug)]
pub struct UnaryResponse {
    /// Response payload
    pub payload: Bytes,
    /// Optional response headers
    pub headers: Option<Bytes>,
}

/// Request for a streaming response RPC
#[derive(Debug)]
pub struct StreamingResponseRequest {
    /// Destination peer address
    pub peer: SocketAddr,
    /// Request payload buffer
    pub buffer: Buffer,
    /// Transfer options
    pub options: TransferOptions,
}

impl StreamingResponseRequest {
    /// Create a new streaming response request
    pub fn new(peer: SocketAddr, buffer: Buffer, options: TransferOptions) -> Self {
        Self {
            peer,
            buffer,
            options,
        }
    }
}

/// A single response message in a streaming response
#[derive(Debug)]
pub struct StreamingResponseMessage {
    /// Response payload
    pub payload: Bytes,
    /// Optional headers
    pub headers: Option<Bytes>,
}

/// Stream of responses for a streaming response RPC
pub type ResponseStream = mpsc::Receiver<Result<StreamingResponseMessage, TransferError>>;

/// Request sender for streaming request pattern
pub type RequestStream = mpsc::Sender<Result<StreamingRequestMessage, TransferError>>;

/// A single request message in a streaming request
#[derive(Debug)]
pub struct StreamingRequestMessage {
    /// Request payload
    pub payload: Bytes,
}

/// Request for a bidirectional RPC
#[derive(Debug)]
pub struct BidirectionalRequest {
    /// Destination peer address
    pub peer: SocketAddr,
    /// Initial request payload buffer
    pub buffer: Buffer,
    /// Transfer options
    pub options: TransferOptions,
}

impl BidirectionalRequest {
    /// Create a new bidirectional request
    pub fn new(peer: SocketAddr, buffer: Buffer, options: TransferOptions) -> Self {
        Self {
            peer,
            buffer,
            options,
        }
    }
}

/// A bidirectional stream with both request and response channels
#[derive(Debug)]
pub struct BidirectionalStream {
    /// Sender for additional requests
    pub request_tx: mpsc::Sender<Result<StreamingRequestMessage, TransferError>>,
    /// Receiver for responses
    pub response_rx: mpsc::Receiver<Result<StreamingResponseMessage, TransferError>>,
}

/// Request for a raw bulk transfer
#[derive(Debug)]
pub struct RawBulkTransferRequest {
    /// Destination peer address
    pub peer: SocketAddr,
    /// Buffer ID for the bulk data
    pub buffer_id: BufferId,
    /// Transfer options
    pub options: TransferOptions,
}

impl RawBulkTransferRequest {
    /// Create a new raw bulk transfer request
    pub fn new(peer: SocketAddr, buffer_id: BufferId, options: TransferOptions) -> Self {
        Self {
            peer,
            buffer_id,
            options,
        }
    }
}

/// Completion notification for a raw bulk transfer
#[derive(Debug)]
pub struct RawBulkTransferComplete {
    /// Bytes transferred
    pub bytes_transferred: u64,
}

/// Error types for transfers
#[derive(Debug, Clone, thiserror::Error)]
pub enum TransferError {
    /// Peer is unreachable or not responding
    #[error("peer unreachable: {0}")]
    PeerUnreachable(String),
    
    /// Transfer timed out
    #[error("transfer timed out")]
    Timeout,
    
    /// Dependency failed and error cascaded
    #[error("dependency failed: {0}")]
    DependencyFailed(String),
    
    /// Transfer was cancelled
    #[error("transfer cancelled")]
    Cancelled,
    
    /// Backpressure applied
    #[error("backpressure applied")]
    Backpressure,
    
    /// Invalid buffer state
    #[error("invalid buffer state")]
    InvalidBufferState,
    
    /// Other error
    #[error("transfer error: {0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::{BufferDescriptor, BufferId};

    #[test]
    fn test_transfer_options_builder() {
        let options = TransferOptions::new()
            .with_priority(Priority::HIGHEST)
            .with_metadata(Bytes::from("test"))
            .with_causality_tracking();

        assert_eq!(options.priority, Priority::HIGHEST);
        assert!(options.metadata.is_some());
        assert!(options.track_causality);
    }

    #[test]
    fn test_unary_request() {
        let buffer_id = BufferId::new(1, 1);
        let desc = BufferDescriptor::new(buffer_id, 1024);
        let buffer = Buffer::new(desc);
        let peer = "127.0.0.1:8080".parse().unwrap();
        let options = TransferOptions::new();

        let request = UnaryRequest::new(peer, buffer, options);
        assert_eq!(request.peer, peer);
    }
}
