//! Streaming response RPC builder and response types.
//!
//! A streaming response RPC sends a single request and receives multiple responses as a stream.

use crate::{causality, message, priority::Priority, stream::Error};
use s2n_quic_core::buffer::writer;

/// Builder for configuring a streaming response RPC.
pub struct Builder {
    _todo: (),
}

impl Builder {
    /// Sets the priority for this transfer.
    pub fn priority(self, priority: Priority) -> Self {
        let _ = priority;
        todo!()
    }

    /// Adds a causality dependency.
    pub fn depends_on(self, dependency: causality::Dependency) -> Self {
        let _ = dependency;
        todo!()
    }

    /// Sets application metadata for RPC handler dispatch.
    pub fn metadata(self, metadata: impl Into<bytes::Bytes>) -> Self {
        let _ = metadata;
        todo!()
    }

    /// Sets an optional request header.
    pub fn header(self, header: impl Into<bytes::Bytes>) -> Self {
        let _ = header;
        todo!()
    }

    /// Indicates that response headers are expected.
    pub fn expect_response_headers(self, enabled: bool) -> Self {
        let _ = enabled;
        todo!()
    }

    /// Enables causality tracking for this request.
    ///
    /// When enabled, the request will be assigned a causality token that can be
    /// used as a dependency for subsequent requests. This incurs tracking overhead,
    /// so should only be enabled when dependencies are needed.
    pub fn causal_token(self, enabled: bool) -> Self {
        let _ = enabled;
        todo!()
    }

    /// Builds the request with the given message.
    ///
    /// This returns a `Request` object that can be sent to receive a stream of responses.
    ///
    /// # Arguments
    ///
    /// * `message` - The request message payload
    pub fn build(self, message: message::Message) -> Request {
        let _ = message;
        todo!()
    }

    /// Enables direct placement mode for bulk data transfers.
    ///
    /// In this mode, response chunks are placed directly into application-provided
    /// buffers using zero-copy techniques when possible:
    /// - For RDMA/EFA: Direct RDMA read into pre-registered buffer regions
    /// - For UDP: Decrypt directly into application buffers
    ///
    /// Returns a specialized builder for configuring direct placement.
    pub fn build_direct_placement<H>(
        self,
        message: message::Message,
        handler: H,
    ) -> DirectPlacementRequest
    where
        H: ChunkHandler,
    {
        let _ = message;
        let _ = handler;
        todo!()
    }
}

/// A request for direct placement bulk transfer.
pub struct DirectPlacementRequest {
    _todo: (),
}

impl DirectPlacementRequest {
    /// Returns the causality token for this request.
    pub fn causal_token(&self) -> Option<causality::Token> {
        todo!()
    }

    /// Sends the request and returns a handler for receiving chunks.
    ///
    /// The handler will be called by the worker thread for each received chunk,
    /// enabling zero-copy placement strategies.
    pub async fn send(self) -> Result<(), Error> {
        todo!()
    }
}

/// Trait for handling chunks in direct placement mode.
///
/// Implementations provide pre-allocated buffers and receive callbacks
/// when chunks are placed.
///
/// TODO: Refine this trait to support:
/// - Buffer registration for RDMA (vectored regions)
/// - Chunk placement callbacks (offset, encrypted flag)
/// - Completion signaling
/// - Error handling
pub trait ChunkHandler: Send {
    // TODO: Define methods for:
    // - Providing buffer regions (for RDMA registration)
    // - Handling placed chunks (offset, size, encrypted flag)
    // - Signaling completion
    // - Error handling
}

/// A prepared streaming response request ready to be sent.
pub struct Request {
    _todo: (),
}

impl Request {
    /// Returns the causality token for this request.
    ///
    /// This token can be used as a dependency for subsequent requests.
    pub fn causal_token(&self) -> Option<causality::Token> {
        todo!()
    }

    /// Sends the request and returns a stream of responses.
    pub async fn send(self) -> Result<Stream, Error> {
        todo!()
    }
}

/// A stream of responses from a streaming response RPC.
pub struct Stream {
    _todo: (),
}

impl Stream {
    /// Returns the optional response header.
    pub fn header(&self) -> Option<&bytes::Bytes> {
        todo!()
    }

    /// Reads the response body into the provided buffer.
    pub async fn recv(&mut self, buf: &mut impl writer::Storage) -> Option<Result<(), Error>> {
        let _ = buf;
        todo!()
    }

    /// Closes the stream and stops receiving responses.
    pub fn close(self, error: Option<u32>) -> Result<(), Error> {
        let _ = error;
        todo!()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // TODO
    }
}
