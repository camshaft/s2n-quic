//! Unary RPC request builder and response types.
//!
//! A unary RPC sends a single request and receives a single response.

use crate::{causality, message, priority::Priority, stream::Error};
use s2n_quic_core::buffer::writer;

/// Builder for configuring a unary RPC request.
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

    /// Indicates that a response header is expected.
    pub fn expect_response_header(self, enabled: bool) -> Self {
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
    /// This returns a `Request` object that contains the causality token
    /// and can be sent to the peer.
    ///
    /// # Arguments
    ///
    /// * `message` - The request message payload
    pub fn build(self, message: message::Message) -> Request {
        let _ = message;
        todo!()
    }
}

/// A prepared unary request ready to be sent.
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

    /// Sends the request and waits for the response.
    pub async fn send(self) -> Result<Response, Error> {
        todo!()
    }
}

/// A response from a unary RPC.
pub struct Response {
    _todo: (),
}

impl Response {
    /// Returns the optional response header.
    pub fn header(&self) -> Option<&bytes::Bytes> {
        todo!()
    }

    /// Reads the response body into the provided buffer.
    pub async fn read_into(self, buf: &mut impl writer::Storage) -> Result<(), Error> {
        let _ = buf;
        todo!()
    }

    /// Consumes the response and returns the body as bytes.
    pub async fn into_bytes(self) -> Result<bytes::Bytes, Error> {
        todo!()
    }
}
