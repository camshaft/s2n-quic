//! Streaming request RPC builder and response types.
//!
//! A streaming request RPC sends multiple requests and receives a single response.

use crate::{
    message::{self, Message},
    stream::Error,
    ByteVec,
};

/// Builder for configuring a streaming request RPC.
pub struct Builder {
    _todo: (),
}

impl Builder {
    /// Sets application metadata for RPC handler dispatch.
    pub fn metadata(self, metadata: ByteVec) -> Self {
        let _ = metadata;
        todo!()
    }

    /// Opens the streaming request channel.
    ///
    /// # Returns
    ///
    /// A `Request` object that can be used to send multiple requests and receive the final response.
    pub fn build(self) -> Request {
        todo!()
    }
}

/// A streaming request handle for sending multiple requests.
pub struct Request {
    _todo: (),
}

impl Request {
    /// Returns the allocator associated with this request
    ///
    /// This is used for allocating messages for transmitting items
    pub fn allocator(&self) -> &message::Allocator {
        todo!()
    }

    /// Sends a request in the stream.
    pub async fn send(&mut self, item: Message) -> Result<(), Error> {
        let _ = item;
        todo!()
    }

    /// Closes the request stream and waits for the response.
    pub async fn finish(self) -> Result<Response, Error> {
        todo!()
    }
}

/// A response from a streaming request RPC.
pub struct Response {
    _todo: (),
}

impl Response {
    /// Consumes the response and returns the body as bytes.
    pub async fn recv(self) -> Result<ByteVec, Error> {
        todo!()
    }
}
