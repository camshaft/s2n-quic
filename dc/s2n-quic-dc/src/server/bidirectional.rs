//! Server-side bidirectional streaming RPC.
//!
//! A bidirectional streaming RPC receives multiple requests and sends multiple responses.
//! The request and response sides can be used concurrently.

use crate::{
    priority::Priority,
    server::{AcceptError, RejectReason},
    stream::{self, Backpressure},
};
use s2n_quic_core::buffer::{reader, writer};
use std::net::SocketAddr;

/// A request for an incoming bidirectional streaming RPC transfer.
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

    pub async fn accept(self) -> Result<(RequestStream, ResponseSink), AcceptError> {
        self.accept_with(Backpressure::default()).await
    }

    pub async fn accept_with(
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

    /// Closes the request side of the stream.
    pub fn close(self) -> Result<(), stream::Error> {
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
    pub async fn send(
        &mut self,
        payload: &mut impl reader::storage::Infallible,
    ) -> Result<(), stream::Error> {
        let _ = payload;
        todo!()
    }

    /// Sends a response item with a header.
    pub async fn send_with_header(
        &mut self,
        header: &mut impl reader::storage::Infallible,
        payload: &mut impl reader::storage::Infallible,
    ) -> Result<(), stream::Error> {
        let _ = (header, payload);
        todo!()
    }

    /// Closes the response side of the stream normally.
    pub fn close(self) -> Result<(), stream::Error> {
        todo!()
    }

    /// Closes the response side with an error code.
    pub fn close_with_error(self, code: u32) -> Result<(), stream::Error> {
        let _ = code;
        todo!()
    }
}

impl Drop for ResponseSink {
    fn drop(&mut self) {
        // TODO
    }
}
