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
//! 2. `FreedSender::record(queue_id)` is called once (when both halves are gone).
//! 3. Under a single lock the `queue_id` is inserted into the pending
//!    `IntervalSet`.  If no batch is in-flight a new `FreedBatch` is submitted
//!    to the global endpoint channel and `in_flight` is set.
//! 4. The emission task processes `FreedBatch`, encodes a `QueueFree` frame,
//!    and calls `FreedSender::complete_in_flight`.
//! 5. `complete_in_flight` clears the flag and, if more IDs accumulated in the
//!    meantime, submits another batch immediately.

use crate::path::secret::map::Entry as PathSecretEntry;
use s2n_quic_core::{interval_set::IntervalSet, varint::VarInt};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// A ready-to-emit batch submitted to the endpoint emission channel.
pub struct FreedBatch {
    pub path_entry: Arc<PathSecretEntry>,
    pub request_id: VarInt,
    pub ranges: IntervalSet<VarInt>,
}

/// Channel handle for submitting `FreedBatch` items to the global emission task.
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
    /// IDs that have been freed but not yet submitted in a batch.
    freed: IntervalSet<VarInt>,
    /// Monotonically increasing request ID for the next batch.
    next_request_id: u64,
    /// True while a batch has been submitted and not yet acknowledged.
    in_flight: bool,
}

impl FreedSender {
    pub fn new(path_entry: Arc<PathSecretEntry>, endpoint_tx: FreedBatchTx) -> Self {
        Self {
            inner: Arc::new(FreedInner {
                state: Mutex::new(FreedState {
                    freed: IntervalSet::new(),
                    next_request_id: 0,
                    in_flight: false,
                }),
            }),
            path_entry,
            endpoint_tx,
        }
    }

    /// Record that `queue_id` has been freed and optionally emit a batch.
    ///
    /// Called on the application / receiver-drop path; must be cheap.
    pub fn record(&self, queue_id: VarInt) {
        let mut state = self.inner.state.lock().unwrap();

        let _ = state.freed.insert_value(queue_id);

        if state.in_flight {
            // A batch is already outstanding; our ID will be picked up when
            // complete_in_flight runs.
            return;
        }

        // No batch in flight — submit one now.
        let batch = self.take_batch(&mut state);
        drop(state);

        if let Some(batch) = batch {
            // Best-effort: if the channel is closed we simply drop the batch.
            let _ = self.endpoint_tx.send(batch);
        }
    }

    /// Called by the emission task after a batch has been transmitted.
    ///
    /// Clears `in_flight` and immediately re-submits if more IDs accumulated.
    pub fn complete_in_flight(&self) {
        let mut state = self.inner.state.lock().unwrap();
        state.in_flight = false;

        if state.freed.is_empty() {
            return;
        }

        let batch = self.take_batch(&mut state);
        drop(state);

        if let Some(batch) = batch {
            let _ = self.endpoint_tx.send(batch);
        }
    }

    /// Drain the pending freed set into a `FreedBatch` and mark `in_flight`.
    ///
    /// Returns `None` if the set is empty (should not happen in normal flow).
    fn take_batch(&self, state: &mut FreedState) -> Option<FreedBatch> {
        if state.freed.is_empty() {
            return None;
        }
        let ranges = core::mem::replace(&mut state.freed, IntervalSet::new());
        let request_id = VarInt::new(state.next_request_id).unwrap_or(VarInt::MAX);
        state.next_request_id = state.next_request_id.wrapping_add(1);
        state.in_flight = true;
        Some(FreedBatch {
            path_entry: self.path_entry.clone(),
            request_id,
            ranges,
        })
    }
}
