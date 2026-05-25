// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Clean-slate queue module.
//!
//! This replaces the generic `flow/queue` implementation with a concrete,
//! no-generics design tailored to `msg::Stream` / `msg::Control`.
//!
//! ## Modules
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
pub(crate) mod page_table;
pub(crate) mod server;
pub(crate) mod slot;

// Public API surface

pub use half::{AutoWake, Closed};
pub use handle::{AllocResult, ControlReceiver, StreamReceiver};
pub use server::BindResult;

pub use client::{ClientAllocator, ClientDispatch};
pub use server::ServerDispatch;
pub use freed::{FreedBatch, FreedBatchRx, FreedBatchTx, FreedSender, freed_batch_channel};

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
    /// The provided `binding_id` does not match the slot's stored value.
    BindingMismatch(T),
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
