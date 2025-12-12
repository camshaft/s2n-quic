//! Server-side streaming response RPC.
//!
//! A streaming response RPC receives a single request and sends multiple responses.

use crate::{
    priority::Priority,
    server::{AcceptError, RejectReason},
    stream::{self, Backpressure},
};
use s2n_quic_core::buffer::{reader, writer};
use std::net::SocketAddr;

/// A request for an incoming streaming response RPC transfer.
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
    pub fn header(&self) -> Option<&bytes::Bytes> {
        todo!()
    }

    /// Reads the request payload into the provided buffer.
    pub async fn read_payload(
        self,
        buf: &mut impl writer::Storage,
    ) -> Result<Response, stream::Error> {
        let _ = buf;
        todo!()
    }
}

/// The response side for sending multiple responses.
pub struct Response {
    _todo: (),
}

impl Response {
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

    /// Closes the stream normally.
    pub fn close(self) -> Result<(), stream::Error> {
        todo!()
    }

    /// Closes the stream with an error code.
    pub fn close_with_error(self, code: u32) -> Result<(), stream::Error> {
        let _ = code;
        todo!()
    }
}
