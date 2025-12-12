//! Server-side streaming response RPC.
//!
//! A streaming response RPC receives a single request and sends multiple responses.

use crate::{
    priority::Priority,
    server::{AcceptError, RejectReason},
    stream::{self, Backpressure, Item},
    ByteVec,
};
use std::net::SocketAddr;

/// A request for an incoming streaming response RPC transfer.
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

    pub fn payload_size(&self) -> usize {
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

/// A streaming response RPC stream.
pub struct Stream {
    _todo: (),
}

impl Stream {
    pub fn header(&self) -> Option<&ByteVec> {
        todo!()
    }

    /// Reads the request payload into the provided buffer.
    pub async fn recv(self) -> Result<(ByteVec, Response), stream::Error> {
        todo!()
    }
}

/// The response side for sending multiple responses.
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

    /// Sends a response item in the stream.
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
