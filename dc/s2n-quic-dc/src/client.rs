//! Client interface for initiating transfers.
//!
//! The client provides builder patterns for creating different types of transfers.
//! Builders enable applications to configure priorities, dependencies, and metadata
//! before initiating the actual transfer.

use crate::message;
use std::{net::SocketAddr, sync::Arc};

pub mod bidirectional;
pub mod streaming_request;
pub mod streaming_response;
pub mod unary;

/// Client handle for initiating transfers to peers.
///
/// This is the main entry point for client-side transfer operations. It can be
/// cloned cheaply and shared across threads.
#[derive(Clone)]
pub struct Client {
    #[expect(dead_code)]
    inner: Arc<Inner>,
}

struct Inner {
    _todo: (),
}

impl Client {
    /// Returns the buffer allocator for this client.
    ///
    /// Applications use the allocator to prepare message payloads before
    /// initiating transfers.
    pub fn allocator(&self) -> &message::Allocator {
        todo!()
    }

    /// Starts building a unary RPC request.
    ///
    /// A unary RPC sends a single request and receives a single response.
    ///
    /// # Arguments
    ///
    /// * `peer` - The peer to send the request to
    ///
    /// # Example
    ///
    /// ```ignore
    /// let message = "Hello world!";
    ///
    /// // Allocate a buffer for the initial message
    /// let message = client
    ///     .allocator()
    ///     .allocate(message.len())
    ///     .await
    ///     .fill_plaintext(&mut &message[..]);
    ///
    /// // Create the request object with the given message
    /// let request = client
    ///     .unary(peer)
    ///     .priority(Priority::HIGH)
    ///     .header(Bytes::from("header"))
    ///     .build(message);
    ///
    /// // Optionally get the causality token for the request to be used in other requests
    /// let _ = request.token();
    ///
    /// // Submit the request to the peer
    /// let response = request.send().await.unwrap();
    ///
    /// // Finish the request and get the response bytes
    /// let mut body = BytesMut::new();
    /// response.read_into(&mut body).await.unwrap();
    /// ```
    pub fn unary(&self, remote_addr: SocketAddr) -> unary::Builder {
        let _ = remote_addr;
        todo!()
    }

    /// Starts building a streaming response RPC.
    ///
    /// Sends a single request and receives multiple responses as a stream.
    ///
    /// # Arguments
    ///
    /// * `peer` - The peer to send the request to
    pub fn streaming_response(&self, remote_addr: SocketAddr) -> streaming_response::Builder {
        let _ = remote_addr;
        todo!()
    }

    /// Starts building a streaming request RPC.
    ///
    /// Sends multiple requests and receives a single response.
    ///
    /// # Arguments
    ///
    /// * `peer` - The peer to send the requests to
    pub fn streaming_request(&self, remote_addr: SocketAddr) -> streaming_request::Builder {
        let _ = remote_addr;
        todo!()
    }

    /// Starts building a bidirectional streaming RPC.
    ///
    /// Sends multiple requests and receives multiple responses.
    ///
    /// # Arguments
    ///
    /// * `peer` - The peer to communicate with
    pub fn bidirectional(&self, remote_addr: SocketAddr) -> bidirectional::Builder {
        let _ = remote_addr;
        todo!()
    }
}
