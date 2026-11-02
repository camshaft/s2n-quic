// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-peer queue slots for routing inbound `msg::Stream` and `msg::Control`
//! frames to the correct stream handler.
//!
//! Each peer connection owns a flat array of [`slot::Slot`]s held in a pinned
//! [`page_table::PageTable`].  A slot is identified by its index, which is
//! exchanged in the QUIC handshake as a `queue_id`.
//!
//! ## Roles
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! | [`half`] | Single queue half with push/pop/poll and open/close lifecycle |
//! | [`slot`] | One queue slot (two halves + atomic `binding_id`) |
//! | [`page_table`] | Pinned page-table; stable slot addresses, O(1) dispatch |
//! | [`freed`] | Server freed-queue accumulator and batch emission |
//! | [`handle`] | `StreamReceiver`, `ControlReceiver`, `AllocResult` |
//! | [`client`] | `ClientAllocator` + `ClientDispatch` + `ClientFreeList` |
//! | [`server`] | `ServerDispatch` + `BindResult` |

pub(crate) mod client;
pub(crate) mod freed;
pub(crate) mod half;
pub(crate) mod handle;
pub(crate) mod msg_entry;
pub(crate) mod msg_table;
pub(crate) mod page_table;
pub(crate) mod server;
pub(crate) mod slot;

#[cfg(test)]
pub(crate) mod testing;

// Public API surface

pub use crate::sync::AutoWake;
pub use half::Closed;
pub use handle::{AllocResult, ControlReceiver, StreamReceiver};
pub use server::BindResult;

pub use client::{ClientAllocFuture, ClientDispatch, ClientState};
pub use freed::{freed_batch_channel, FreedBatchRx, FreedBatchTx, FreedInner};
pub use server::{ServerState, ServerView};

// ── Error ─────────────────────────────────────────────────────────────────────

/// Error returned by dispatch operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error<T> {
    /// No slot is allocated at this `queue_id`.
    Unallocated(T),
    /// The slot is allocated but this receiver half has been dropped.
    HalfClosed(T),
    /// The sender was closed (path secret evicted).
    SenderClosed,
    /// The binding_id refers to an old (already-recycled) generation of this slot.
    StaleBinding(T),
    /// The binding_id is ahead of the current slot binding.  This indicates a
    /// bug — the slot hasn't been freed yet but a future binding arrived.
    FutureBinding(T),
    /// The queue_id exceeds the negotiated cap.  Protocol violation.
    CapExceeded(T),
}

/// Error returned by QueueMsg dispatch operations.
#[derive(Debug)]
pub enum MsgError<E> {
    /// Queue validation failed before the write callback could complete.
    Queue(Error<()>),
    /// The `MsgTable` rejected the chunk's geometry (duplicate, stale, gap, overflow, size/chunk
    /// mismatch, …) — a reject that fires *before* the write callback runs. On the fast path the
    /// write callback IS the scatter-decrypt (the only AEAD authentication on that path), so a
    /// reject here means the packet was never authenticated; the caller must authenticate before
    /// ACKing (see `endpoint::dispatch::FastPathError::AuthForDrop`). On the slow path the packet
    /// was already decrypted up front, so this is just a dropped-but-ACKed chunk.
    InsertRejected,
    /// The caller-provided write callback failed.
    Write(E),
}

impl<T> From<half::Error<T>> for Error<T> {
    fn from(e: half::Error<T>) -> Self {
        match e {
            half::Error::Unallocated(t) => Error::Unallocated(t),
            half::Error::HalfClosed(t) => Error::HalfClosed(t),
            half::Error::SenderClosed => Error::SenderClosed,
        }
    }
}

/// A stream reset that this side owes its peer, by the routing of the stream's frames: their
/// queue pair as this side sends them, and the stream's binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OwedReset {
    pub queue_pair: crate::packet::datagram::QueuePair,
    pub binding_id: s2n_quic_core::varint::VarInt,
}

/// The streams this side reset when it found its peer unresponsive and marked it dead, kept on
/// the path's queue state: the streams whose frames it failed, and the streams open on the path.
/// The peer isn't told that this side reset them, so its end of each may still wait on this
/// side: each is reset once a packet from the peer clears the dead mark
/// (`path::secret::map::Entry::touch_activity`). Each stream is kept once, so the set holds at
/// most the streams whose frames failed and the streams open on the path.
#[derive(Debug, Default)]
pub struct OwedResets {
    streams: std::sync::Mutex<std::collections::HashSet<OwedReset>>,
}

impl OwedResets {
    /// Adds each of `resets` that isn't owed already.
    pub fn record(&self, resets: impl IntoIterator<Item = OwedReset>) {
        self.streams.lock().unwrap().extend(resets);
    }

    /// Takes every owed reset.
    pub fn take(&self) -> Vec<OwedReset> {
        self.streams.lock().unwrap().drain().collect()
    }
}
