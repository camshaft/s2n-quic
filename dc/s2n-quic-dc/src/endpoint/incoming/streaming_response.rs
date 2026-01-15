//! Server-side streaming response RPC.
//!
//! A streaming response RPC receives a single request and sends multiple responses.

use crate::{
    endpoint::incoming::handler::{AcceptError, RejectReason},
    message::Message,
    peer,
    stream::{self, Backpressure},
    ByteVec,
};

/// A request for an incoming streaming response RPC transfer.
pub struct Request {
    _todo: (),
}

impl Request {
    /// Returns the remote peer that initiated this transfer.
    pub fn peer(&self) -> &peer::Handle {
        todo!()
    }

    pub fn metadata(&self) -> Option<&ByteVec> {
        todo!()
    }

    pub fn payload_size(&self) -> usize {
        todo!()
    }

    pub fn accept(self) -> Result<Stream, AcceptError> {
        self.accept_with(Backpressure::default())
    }

    pub fn accept_with(self, config: Backpressure) -> Result<Stream, AcceptError> {
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
