// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Server-side freed-queue notification.
//!
//! When a server-side receiver is dropped the stream's slot index must be
//! recycled back to the client so it can reuse that queue slot.  This module
//! implements the per-peer accumulator and the batch submission channel.
//!
//! ## Flow
//!
//! 1. Stream completes → `StreamReceiver` / `ControlReceiver` dropped.
//! 2. `FreedSender::record(queue_id)` is called.
//! 3. The `queue_id` is inserted into the pending set under the inner lock.
//!    If no batch token is in-flight, a new `FreedBatch` token is submitted to
//!    the global endpoint channel and `in_flight` is set.
//! 4. The emission task calls `FreedBatch::take_snapshot` at serialisation
//!    time to drain the pending set.  Any IDs that accumulated between step 3
//!    and step 4 are included naturally — this is the "snapshot at transmission
//!    time" property that yields free batching.
//! 5. After the frame is sent the emission task calls
//!    `FreedBatch::check_and_resubmit`.  If more IDs accumulated the token
//!    requeues itself; otherwise `in_flight` is cleared and the next `record`
//!    call will submit a fresh token.

use crate::path::secret::map::Entry as PathSecretEntry;
use s2n_quic_core::{interval_set::IntervalSet, varint::VarInt};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// A token submitted to the endpoint emission task.
///
/// The actual freed ranges are NOT captured at submission time.  Instead,
/// `take_snapshot` drains the shared `FreedInner` state at serialisation time,
/// picking up any IDs that arrived after the token was enqueued.
pub struct FreedBatch {
    inner: Arc<FreedInner>,
    pub path_entry: Arc<PathSecretEntry>,
    endpoint_tx: FreedBatchTx,
}

impl FreedBatch {
    /// Drain the pending freed set into a snapshot.
    ///
    /// Returns `None` if there is nothing to send (should not happen in normal
    /// flow, but the emission task may call this defensively).
    pub fn take_snapshot(&self) -> Option<(VarInt, IntervalSet<VarInt>)> {
        let mut state = self.inner.state.lock().unwrap();
        if state.freed.is_empty() {
            return None;
        }
        let ranges = core::mem::replace(&mut state.freed, IntervalSet::new());
        let request_id = state.take_next_request_id();
        Some((request_id, ranges))
    }

    /// Called by the emission task after the frame has been transmitted.
    ///
    /// If more IDs have accumulated the token resubmits itself to the channel;
    /// otherwise `in_flight` is cleared so the next `record` call can submit a
    /// fresh token.
    pub fn check_and_resubmit(self) {
        let mut state = self.inner.state.lock().unwrap();
        if state.freed.is_empty() {
            state.in_flight = false;
        } else {
            // More IDs arrived; keep in_flight and requeue the token.
            drop(state);
            // Extract tx before moving self to avoid borrow-while-move.
            let tx = self.endpoint_tx.clone();
            let _ = tx.send(self);
        }
    }
}

/// Channel handle for submitting `FreedBatch` tokens to the global emission task.
pub type FreedBatchTx = mpsc::UnboundedSender<FreedBatch>;
pub type FreedBatchRx = mpsc::UnboundedReceiver<FreedBatch>;

/// Create the global channel for freed-batch emission.
pub fn freed_batch_channel() -> (FreedBatchTx, FreedBatchRx) {
    mpsc::unbounded_channel()
}

// ── Per-peer accumulator ──────────────────────────────────────────────────────

/// Shareable per-peer freed-queue state.
///
/// Cloned into each `StreamReceiver` / `ControlReceiver` that belongs to the
/// same peer so that the receiver Drop path can cheaply record a freed ID.
#[derive(Clone)]
pub struct FreedSender {
    inner: Arc<FreedInner>,
    path_entry: Arc<PathSecretEntry>,
    endpoint_tx: FreedBatchTx,
}

impl core::fmt::Debug for FreedSender {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FreedSender")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

pub(crate) struct FreedInner {
    state: Mutex<FreedState>,
}

impl core::fmt::Debug for FreedInner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.state.try_lock() {
            Ok(s) => f
                .debug_struct("FreedInner")
                .field("freed_count", &s.freed.count())
                .field("in_flight", &s.in_flight)
                .finish(),
            Err(_) => write!(f, "FreedInner(<locked>)"),
        }
    }
}

struct FreedState {
    /// IDs that have been freed but not yet included in a transmitted batch.
    freed: IntervalSet<VarInt>,
    /// Monotonically increasing request ID for the next batch.
    ///
    /// Saturates at `VarInt::MAX` rather than wrapping to preserve monotonicity
    /// and per-batch dedup on the client side.
    next_request_id: VarInt,
    /// True while a batch token has been submitted and not yet re-submitted or
    /// cleared by `check_and_resubmit`.
    in_flight: bool,
}

impl FreedState {
    fn take_next_request_id(&mut self) -> VarInt {
        let id = self.next_request_id;
        // Saturating increment within VarInt range preserves monotonicity.
        if let Ok(next) = VarInt::new(id.as_u64().saturating_add(1)) {
            self.next_request_id = next;
        }
        // If already at VarInt::MAX we keep re-using it; the client dedup
        // mechanism handles this edge case by never rejecting VarInt::MAX.
        id
    }
}

impl FreedSender {
    pub fn new(path_entry: Arc<PathSecretEntry>, endpoint_tx: FreedBatchTx) -> Self {
        Self {
            inner: Arc::new(FreedInner {
                state: Mutex::new(FreedState {
                    freed: IntervalSet::new(),
                    next_request_id: VarInt::from_u8(0),
                    in_flight: false,
                }),
            }),
            path_entry,
            endpoint_tx,
        }
    }

    /// Record that `queue_id` has been freed and, if no token is in-flight,
    /// submit a fresh one.
    ///
    /// Called on the application / receiver-drop path; designed to be cheap
    /// (a single lock + an optional channel send).
    pub fn record(&self, queue_id: VarInt) {
        let mut state = self.inner.state.lock().unwrap();

        let _ = state.freed.insert_value(queue_id);

        if state.in_flight {
            // A token is already outstanding; our ID will be included when
            // `take_snapshot` is called at serialisation time.
            return;
        }

        // No token in flight — submit one now.
        state.in_flight = true;
        drop(state);

        let token = FreedBatch {
            inner: self.inner.clone(),
            path_entry: self.path_entry.clone(),
            endpoint_tx: self.endpoint_tx.clone(),
        };
        // Best-effort: if the channel is closed we simply drop the token.
        let _ = self.endpoint_tx.send(token);
    }
}
