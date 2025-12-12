//! Server-side unary RPC stream.
//!
//! A unary RPC receives a single request and sends a single response.

use crate::{
    priority::Priority,
    server::{AcceptError, RejectReason},
    stream::{self, Backpressure, Item},
    ByteVec,
};
use std::net::SocketAddr;

/// A request for an incoming unary RPC transfer.
///
/// The request contains metadata about the transfer and can be accepted or rejected
/// by the application.
pub struct Request {
    _todo: (),
}

impl Request {
    /// Returns the remote peer address that initiated this transfer.
    pub fn remote_addr(&self) -> SocketAddr {
        todo!()
    }

    /// Returns the application metadata included in the transfer.
    ///
    /// This metadata can be used for handler dispatch and routing decisions.
    pub fn metadata(&self) -> Option<&ByteVec> {
        todo!()
    }

    /// Returns the priority of this transfer.
    pub fn priority(&self) -> Priority {
        todo!()
    }

    /// Returns the optional request header.
    pub fn header(&self) -> Option<&ByteVec> {
        todo!()
    }

    /// Returns the size of the request payload.
    pub fn payload_size(&self) -> usize {
        todo!()
    }

    /// Accepts the transfer with default backpressure settings.
    pub async fn accept(self) -> Result<Stream, AcceptError> {
        self.accept_with(Backpressure::default()).await
    }

    /// Accepts the transfer with custom backpressure settings.
    pub async fn accept_with(self, config: Backpressure) -> Result<Stream, AcceptError> {
        let _ = config;
        todo!()
    }

    /// Rejects the transfer with a reason code.
    pub fn reject(self, reason: RejectReason) {
        let _ = reason;
        todo!()
    }
}

/// A unary RPC stream for reading the request and sending the response.
pub struct Stream {
    _todo: (),
}

impl Stream {
    /// Reads the request payload into the provided buffer.
    pub async fn recv(self) -> Result<(ByteVec, Response), stream::Error> {
        todo!()
    }
}

/// The response side of a unary RPC.
///
/// This handle is used to send the response back to the client.
pub struct Response {
    _todo: (),
}

impl Response {
    pub async fn send_header(&mut self, header: ByteVec) -> Result<(), stream::Error> {
        let _ = header;
        todo!()
    }

    pub fn set_header(&mut self, header: ByteVec) -> Result<(), stream::Error> {
        let _ = header;
        todo!()
    }

    /// Sends the response on the stream.
    pub async fn send(&mut self, item: Item) -> Result<(), stream::Error> {
        let _ = item;
        todo!()
    }

    /// Closes the response side of the stream normally.
    pub fn close(self, error: Option<ByteVec>) -> Result<(), stream::Error> {
        let _ = error;
        todo!()
    }
}
