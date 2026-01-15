//! Server-side bidirectional streaming RPC.
//!
//! A bidirectional streaming RPC receives multiple requests and sends multiple responses.
//! The request and response sides can be used concurrently.

use crate::{
    endpoint::incoming::handler::{AcceptError, RejectReason},
    message::Message,
    peer,
    stream::{self, Backpressure},
    ByteVec,
};

/// A request for an incoming bidirectional streaming RPC transfer.
pub struct Request {
    _todo: (),
}

impl Request {
    /// Returns the remote peer that initiated this transfer.
    pub fn peer(&self) -> &peer::Handle {
        todo!()
    }

    pub fn metadata(&self) -> Option<&[u8]> {
        todo!()
    }

    pub fn accept(self) -> Result<(RequestStream, ResponseSink), AcceptError> {
        self.accept_with(Backpressure::default())
    }

    pub fn accept_with(
        self,
        config: Backpressure,
    ) -> Result<(RequestStream, ResponseSink), AcceptError> {
        let _ = config;
        todo!()
    }

    pub fn reject(self, reason: RejectReason) {
        let _ = reason;
        todo!()
    }
}

/// The request side of a bidirectional stream.
///
/// This handle receives requests from the client.
pub struct RequestStream {
    _todo: (),
}

impl RequestStream {
    /// Receives the next request item from the stream.
    pub async fn recv(&mut self) -> Option<Result<ByteVec, stream::Error>> {
        todo!()
    }

    /// Closes the request side of the stream.
    pub fn close(self, error: Option<ByteVec>) -> Result<(), stream::Error> {
        let _ = error;
        todo!()
    }
}

impl Drop for RequestStream {
    fn drop(&mut self) {
        // TODO
    }
}

/// The response side of a bidirectional stream.
///
/// This handle sends responses back to the client.
pub struct ResponseSink {
    _todo: (),
}

impl ResponseSink {
    /// Sends a response item in the stream.
    pub async fn send(&mut self, item: Message) -> Result<(), stream::Error> {
        let _ = item;
        todo!()
    }

    /// Closes the response side of the stream normally.
    pub fn close(self, error: Option<ByteVec>) -> Result<(), stream::Error> {
        let _ = error;
        todo!()
    }
}

impl Drop for ResponseSink {
    fn drop(&mut self) {
        // TODO
    }
}
