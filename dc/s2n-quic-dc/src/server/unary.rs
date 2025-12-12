//! Server-side unary RPC stream.
//!
//! A unary RPC receives a single request and sends a single response.

use crate::{
    priority::Priority,
    server::{AcceptError, RejectReason},
    stream::{self, Backpressure},
};
use s2n_quic_core::buffer::{reader, writer};
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
    pub fn metadata(&self) -> Option<&[u8]> {
        todo!()
    }

    /// Returns the priority of this transfer.
    pub fn priority(&self) -> Priority {
        todo!()
    }

    /// Returns the optional request header.
    pub fn header(&self) -> Option<&bytes::Bytes> {
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
    pub async fn read_payload(
        self,
        buf: &mut impl writer::Storage,
    ) -> Result<Response, stream::Error> {
        let _ = buf;
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
    /// Sends a response
    ///
    /// # Arguments
    ///
    /// * `payload` - Response payload data
    pub async fn send(
        self,
        payload: &mut impl reader::storage::Infallible,
    ) -> Result<(), stream::Error> {
        let _ = payload;
        todo!()
    }

    /// Sends a response with a header.
    ///
    /// # Arguments
    ///
    /// * `header` - Response header
    /// * `payload` - Response payload data
    pub async fn send_with_header(
        self,
        header: &mut impl reader::storage::Infallible,
        payload: &mut impl reader::storage::Infallible,
    ) -> Result<(), stream::Error> {
        let _ = (header, payload);
        todo!()
    }

    /// Sends an error response.
    pub fn send_error(self, code: u32) -> Result<(), stream::Error> {
        let _ = code;
        todo!()
    }
}
