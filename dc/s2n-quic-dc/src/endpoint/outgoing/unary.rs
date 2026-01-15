//! Unary RPC request builder and response types.
//!
//! A unary RPC sends a single request and receives a single response.

use crate::{message, stream::Error, ByteVec};

/// Builder for configuring a unary RPC request.
pub struct Builder {
    _todo: (),
}

impl Builder {
    /// Sets application metadata for RPC handler dispatch.
    pub fn metadata(self, metadata: ByteVec) -> Self {
        let _ = metadata;
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
    /// Consumes the response and returns the body as bytes.
    pub async fn recv(self) -> Result<ByteVec, Error> {
        todo!()
    }
}
