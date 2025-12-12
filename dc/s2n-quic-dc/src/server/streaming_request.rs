//! Server-side streaming request RPC.
//!
//! A streaming request RPC receives multiple requests and sends a single response.

use crate::{
    priority::Priority,
    server::{AcceptError, RejectReason},
    stream::{self, Backpressure, Item},
    ByteVec,
};
use std::net::SocketAddr;

/// A request for an incoming streaming request RPC transfer.
pub struct Request {
    _todo: (),
}

impl Request {
    pub fn remote_addr(&self) -> SocketAddr {
        todo!()
    }

    pub fn metadata(&self) -> Option<&ByteVec> {
        todo!()
    }

    pub fn priority(&self) -> Priority {
        todo!()
    }

    pub fn header(&self) -> Option<&ByteVec> {
        todo!()
    }

    pub async fn accept(self) -> Result<Stream, AcceptError> {
        self.accept_with(Backpressure::default()).await
    }

    pub async fn accept_with(self, config: Backpressure) -> Result<Stream, AcceptError> {
        let _ = config;
        todo!()
    }

    pub fn reject(self, reason: RejectReason) {
        let _ = reason;
        todo!()
    }
}

/// A streaming request RPC stream.
pub struct Stream {
    _todo: (),
}

impl Stream {
    pub fn header(&self) -> Option<&ByteVec> {
        todo!()
    }

    /// Receives the next request item from the stream.
    pub async fn recv(&mut self) -> Option<Result<ByteVec, stream::Error>> {
        todo!()
    }

    /// Finishes receiving and returns a handle to send the response.
    pub async fn finish(self) -> Result<Response, stream::Error> {
        todo!()
    }
}

/// The response side of a streaming request RPC.
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
