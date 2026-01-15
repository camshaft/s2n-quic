//! Unified endpoint for mesh networking.
//!
//! The `Endpoint` provides both client and server functionality in a single type,
//! enabling any node in a mesh to both initiate requests to peers and handle
//! incoming requests from peers.

use crate::{message, peer};
use std::sync::Arc;

pub mod incoming;
pub mod outgoing;

pub use incoming::Handler;

/// An endpoint can both initiate transfers to remote peers (client operations)
/// and handle incoming transfers from remote peers (server operations).
///
/// This is the main entry point for all data path operations.
///
/// The endpoint can be cloned cheaply and shared across threads.
///
/// # Example
///
/// ```ignore
/// // Create an endpoint
/// let endpoint = Endpoint::builder()
///     .with_handler(MyHandler)
///     .build()
///     .await?;
///
/// // Use as a client to initiate requests
/// let response = endpoint.unary(peer_addr)
///     .build(message)
///     .send()
///     .await?;
///
/// // Handler receives incoming requests automatically
/// ```
#[derive(Clone)]
pub struct Endpoint {
    inner: Arc<Inner>,
}

struct Inner {
    _todo: (),
}

impl Endpoint {
    /// Creates a new endpoint builder.
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// Returns the buffer allocator for this endpoint.
    ///
    /// Applications use the allocator to prepare message payloads before
    /// initiating transfers or sending responses.
    pub fn allocator(&self) -> &message::Allocator {
        todo!()
    }

    /// Returns the peer registry.
    ///
    /// The peer registry is used to manage remote peers and their addresses.
    pub fn peers(&self) -> &peer::Registry {
        todo!()
    }

    /// Starts building a unary RPC request
    ///
    /// A unary RPC sends a single request and receives a single response.
    ///
    /// # Arguments
    ///
    /// * `peer` - The peer handle to send the request to
    pub fn unary(&self, peer: peer::Handle) -> outgoing::unary::Builder {
        let _ = peer;
        todo!()
    }

    /// Starts building a streaming response RPC.
    ///
    /// Sends a single request and receives multiple responses as a stream.
    ///
    /// # Arguments
    ///
    /// * `peer` - The peer handle to send the request to
    pub fn streaming_response(&self, peer: peer::Handle) -> outgoing::streaming_response::Builder {
        let _ = peer;
        todo!()
    }

    /// Starts building a streaming request RPC.
    ///
    /// Sends multiple requests and receives a single response.
    ///
    /// # Arguments
    ///
    /// * `peer` - The peer handle to send the request to
    pub fn streaming_request(&self, peer: peer::Handle) -> outgoing::streaming_request::Builder {
        let _ = peer;
        todo!()
    }

    /// Starts building a bidirectional streaming RPC.
    ///
    /// Sends multiple requests and receives multiple responses.
    ///
    /// # Arguments
    ///
    /// * `peer` - The peer handle to send the request to
    pub fn bidirectional(&self, peer: peer::Handle) -> outgoing::bidirectional::Builder {
        let _ = peer;
        todo!()
    }
}

/// Builder for configuring and creating an endpoint.
#[derive(Default)]
pub struct Builder {
    _todo: (),
}

impl Builder {
    /// Sets the handler for incoming transfers.
    ///
    /// The handler will be called for all incoming transfers to this endpoint.
    /// If no handler is set, incoming transfers will be rejected.
    pub fn with_handler<H: Handler + 'static>(self, handler: H) -> Self {
        let _ = handler;
        self
    }

    /// Builds the endpoint.
    ///
    /// This will initialize the underlying transport and start accepting connections.
    pub async fn build(self) -> Result<Endpoint, BuildError> {
        todo!()
    }
}

/// Errors that can occur when building an endpoint.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("failed to bind socket")]
    BindFailed,

    #[error("invalid configuration")]
    InvalidConfiguration,
}
