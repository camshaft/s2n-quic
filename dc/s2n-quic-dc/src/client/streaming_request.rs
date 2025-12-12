//! Streaming request RPC builder and response types.
//!
//! A streaming request RPC sends multiple requests and receives a single response.

use crate::{
    causality, message,
    priority::Priority,
    stream::{Error, Item},
    ByteVec,
};

/// Builder for configuring a streaming request RPC.
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

    /// Opens the streaming request channel.
    ///
    /// # Returns
    ///
    /// A `Request` object that can be used to send multiple requests and receive the final response.
    pub fn build(self) -> Request {
        todo!()
    }
}

/// A streaming request handle for sending multiple requests.
pub struct Request {
    _todo: (),
}

impl Request {
    /// Returns the causality token for this request stream.
    pub fn causal_token(&self) -> Option<causality::Token> {
        todo!()
    }

    /// Returns the allocator associated with this request
    ///
    /// This is used for allocating messages for transmitting items
    pub fn allocator(&self) -> &message::Allocator {
        todo!()
    }

    /// Sends a request in the stream.
    pub async fn send(&mut self, item: Item) -> Result<causality::Token, Error> {
        let _ = item;
        todo!()
    }

    /// Closes the request stream and waits for the response.
    pub async fn finish(self) -> Result<Response, Error> {
        todo!()
    }
}

/// A response from a streaming request RPC.
pub struct Response {
    _todo: (),
}

impl Response {
    /// Returns the optional response header.
    pub fn header(&self) -> Option<&ByteVec> {
        todo!()
    }

    /// Consumes the response and returns the body as bytes.
    pub async fn recv(self) -> Result<ByteVec, Error> {
        todo!()
    }
}
