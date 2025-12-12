//! Bidirectional streaming RPC builder and stream types.
//!
//! A bidirectional streaming RPC sends multiple requests and receives multiple responses.
//! The request and response sides can be used concurrently or sent to different tasks.

use crate::{
    causality, message,
    priority::Priority,
    stream::{Error, Item},
    ByteVec,
};

/// Builder for configuring a bidirectional streaming RPC.
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
    pub fn metadata(self, metadata: ByteVec) -> Self {
        let _ = metadata;
        todo!()
    }

    /// Sets an optional request header.
    pub fn header(self, header: ByteVec) -> Self {
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

    /// Opens the bidirectional streaming channel.
    ///
    /// Returns separate handles for sending requests and receiving responses,
    /// which can be used concurrently or sent to different tasks.
    pub fn build(self) -> (RequestSink, ResponseStream) {
        todo!()
    }
}

/// The request side of a bidirectional stream.
///
/// This handle can be moved to a different task to send requests concurrently
/// with receiving responses.
pub struct RequestSink {
    _todo: (),
}

impl RequestSink {
    /// Returns the causality token for this stream.
    pub fn causal_token(&self) -> Option<causality::Token> {
        todo!()
    }

    /// Returns the allocator associated with this stream.
    ///
    /// This is used for allocating messages for transmitting items.
    pub fn allocator(&self) -> &message::Allocator {
        todo!()
    }

    /// Sends a request item in the stream.
    pub async fn send(&mut self, item: Item) -> Result<causality::Token, Error> {
        let _ = item;
        todo!()
    }

    /// Closes the request side of the stream with an optional error code.
    pub fn close(self, error: Option<ByteVec>) -> Result<(), Error> {
        let _ = error;
        todo!()
    }
}

impl Drop for RequestSink {
    fn drop(&mut self) {
        // TODO
    }
}

/// The response side of a bidirectional stream.
///
/// This handle can be moved to a different task to receive responses concurrently
/// with sending requests.
pub struct ResponseStream {
    _todo: (),
}

impl ResponseStream {
    /// Returns the optional response header.
    pub fn header(&self) -> Option<&ByteVec> {
        todo!()
    }

    /// Receives the next response from the stream.
    ///
    /// Returns `None` when the stream is complete.
    pub async fn recv(&mut self) -> Option<Result<ByteVec, Error>> {
        todo!()
    }

    /// Closes the response side of the stream with an optional error code.
    pub fn close(self, error: Option<ByteVec>) -> Result<(), Error> {
        let _ = error;
        todo!()
    }
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        // TODO
    }
}
