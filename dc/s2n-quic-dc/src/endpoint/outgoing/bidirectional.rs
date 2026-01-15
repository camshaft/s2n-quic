//! Bidirectional streaming RPC builder and stream types.
//!
//! A bidirectional streaming RPC sends multiple requests and receives multiple responses.
//! The request and response sides can be used concurrently or sent to different tasks.

use crate::{
    message::{self, Message},
    stream::Error,
    ByteVec,
};

/// Builder for configuring a bidirectional streaming RPC.
pub struct Builder {
    _todo: (),
}

impl Builder {
    /// Sets application metadata for RPC handler dispatch.
    pub fn metadata(self, metadata: ByteVec) -> Self {
        let _ = metadata;
        todo!()
    }

    /// Opens the bidirectional streaming channel.
    ///
    /// Returns separate handles for sending requests and receiving responses,
    /// which can be used concurrently or sent to different tasks.
    pub fn build(self) -> (RequestSink, ResponseStream) {
        todo!()
    }
}

/// The request side of a bidirectional stream.
///
/// This handle can be moved to a different task to send requests concurrently
/// with receiving responses.
pub struct RequestSink {
    _todo: (),
}

impl RequestSink {
    /// Returns the allocator associated with this stream.
    ///
    /// This is used for allocating messages for transmitting items.
    pub fn allocator(&self) -> &message::Allocator {
        todo!()
    }

    /// Sends a request item in the stream.
    pub async fn send(&mut self, item: Message) -> Result<(), Error> {
        let _ = item;
        todo!()
    }

    /// Closes the request side of the stream with an optional error code.
    pub fn close(self, error: Option<ByteVec>) -> Result<(), Error> {
        let _ = error;
        todo!()
    }
}

impl Drop for RequestSink {
    fn drop(&mut self) {
        // TODO
    }
}

/// The response side of a bidirectional stream.
///
/// This handle can be moved to a different task to receive responses concurrently
/// with sending requests.
pub struct ResponseStream {
    _todo: (),
}

impl ResponseStream {
    /// Receives the next response from the stream.
    ///
    /// Returns `None` when the stream is complete.
    pub async fn recv(&mut self) -> Option<Result<ByteVec, Error>> {
        todo!()
    }

    /// Closes the response side of the stream with an optional error code.
    pub fn close(self, error: Option<ByteVec>) -> Result<(), Error> {
        let _ = error;
        todo!()
    }
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        // TODO
    }
}
