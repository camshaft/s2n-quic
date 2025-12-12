//! Server interface for handling incoming transfers.
//!
//! The server uses a trait-based approach where applications implement the `Handler` trait
//! to receive incoming transfers. The transport calls the handler methods, and the application
//! decides what to do (spawn a task, drop it, queue it, etc.).

use crate::message;
use std::{sync::Arc, time::Duration};

pub mod bidirectional;
pub mod streaming_request;
pub mod streaming_response;
pub mod unary;

/// Server handle for managing incoming transfers.
///
/// This is the main entry point for server-side operations. It can be cloned
/// cheaply and shared across threads.
#[derive(Clone)]
pub struct Server {
    #[expect(dead_code)]
    inner: Arc<Inner>,
}

struct Inner {
    _todo: (),
}

impl Server {
    /// Returns the buffer allocator for this server.
    ///
    /// Applications use the allocator to prepare response payloads.
    pub fn allocator(&self) -> &message::Allocator {
        todo!()
    }
}

/// Handler trait for incoming transfers.
///
/// Applications implement this trait to handle different transfer patterns.
/// The transport calls these methods when new transfers arrive, and the application
/// decides how to handle them (spawn tasks, queue, drop, etc.).
///
/// # Example
///
/// ```ignore
/// struct MyHandler;
///
/// impl Handler for MyHandler {
///     fn handle_unary(&self, request: unary::Request) {
///         // Application chooses what to do:
///         let stream = request.accept(Backpressure::default()).unwrap();
///         tokio::spawn(async move {
///             // ... handle the stream
///         });
///         
///         // Or could just drop to reject
///         // Or could queue for later processing
///     }
///     
///     fn is_open(&self) -> bool {
///         true // Return false to stop accepting new transfers
///     }
/// }
/// ```
pub trait Handler: Send + Sync {
    /// Handle an incoming unary RPC transfer.
    ///
    /// The application receives a `Request` and can choose to accept, reject,
    /// or defer the transfer.
    fn handle_unary(&self, request: unary::Request);

    /// Handle an incoming streaming request transfer.
    fn handle_streaming_request(&self, request: streaming_request::Request);

    /// Handle an incoming streaming response transfer.
    fn handle_streaming_response(&self, request: streaming_response::Request);

    /// Handle an incoming bidirectional streaming transfer.
    fn handle_bidirectional(&self, request: bidirectional::Request);

    /// Returns whether the handler is accepting new transfers.
    ///
    /// When this returns false, the transport may stop calling handler methods
    /// and apply backpressure at the transport level.
    fn is_open(&self) -> bool;
}

/// Reasons for rejecting a transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// Server is overloaded
    TooManyRequests,

    /// Retry after a delay
    RetryAfter(Duration),

    /// Handler not found for this RPC method
    HandlerNotFound,

    /// Transfer size exceeds limits
    PayloadTooLarge,

    /// Custom application reason
    Application(u32),
}

/// Errors that can occur during accept operations.
#[derive(Debug, thiserror::Error)]
pub enum AcceptError {
    #[error("transfer cancelled by peer")]
    Cancelled,

    #[error("invalid transfer")]
    Invalid,

    #[error("timeout waiting for transfer")]
    Timeout,
}
