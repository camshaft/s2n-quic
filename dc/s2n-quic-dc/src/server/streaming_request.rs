//! Server-side streaming request RPC.
//!
//! A streaming request RPC receives multiple requests and sends a single response.

use crate::{
    priority::Priority,
    server::{AcceptError, RejectReason},
    stream::{self, Backpressure},
};
use s2n_quic_core::buffer::{reader, writer};
use std::net::SocketAddr;

/// A request for an incoming streaming request RPC transfer.
pub struct Request {
    _todo: (),
}

impl Request {
    pub fn remote_addr(&self) -> SocketAddr {
        todo!()
    }

    pub fn metadata(&self) -> Option<&[u8]> {
        todo!()
    }

    pub fn priority(&self) -> Priority {
        todo!()
    }

    pub fn header(&self) -> Option<&bytes::Bytes> {
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
    pub fn header(&self) -> Option<&bytes::Bytes> {
        todo!()
    }

    /// Receives the next request item from the stream.
    pub async fn recv(
        &mut self,
        buf: &mut impl writer::Storage,
    ) -> Option<Result<(), stream::Error>> {
        let _ = buf;
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
    pub async fn send(
        self,
        payload: &mut impl reader::storage::Infallible,
    ) -> Result<(), stream::Error> {
        let _ = payload;
        todo!()
    }

    pub async fn send_with_header(
        self,
        header: &mut impl reader::storage::Infallible,
        payload: &mut impl reader::storage::Infallible,
    ) -> Result<(), stream::Error> {
        let _ = (header, payload);
        todo!()
    }

    pub fn send_error(self, code: u32) -> Result<(), stream::Error> {
        let _ = code;
        todo!()
    }
}
