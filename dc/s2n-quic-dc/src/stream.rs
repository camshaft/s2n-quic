//! Common stream types and configuration shared between client and server.

use crate::ByteVec;

/// Backpressure configuration for a transfer.
///
/// These limits control when the receiver stops issuing PULL_REQUEST messages
/// to apply backpressure on the sender.
#[derive(Clone, Debug)]
pub struct Backpressure {
    /// Maximum number of messages that can be queued before backpressure is applied.
    ///
    /// When this limit is reached, no more PULL_REQUEST messages will be issued
    /// until messages are consumed by the application.
    pub max_message_queue_depth: usize,

    /// Maximum number of bytes that can be buffered before backpressure is applied.
    ///
    /// This limit can be exceeded to complete a partially-buffered message,
    /// preventing deadlock scenarios.
    pub max_buffered_bytes: usize,
}

impl Default for Backpressure {
    fn default() -> Self {
        Self {
            max_message_queue_depth: 16,
            max_buffered_bytes: 1024 * 1024, // 1 MB
        }
    }
}

/// Errors that can occur during stream operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("peer unreachable")]
    PeerUnreachable,

    #[error("dependency failed")]
    DependencyFailed,

    #[error("transfer timed out")]
    TransferTimeout,

    #[error("stream open timed out")]
    OpenTimeout,

    #[error("stream closed without error")]
    Closed,

    #[error("stream reset")]
    Reset(ByteVec),
}
