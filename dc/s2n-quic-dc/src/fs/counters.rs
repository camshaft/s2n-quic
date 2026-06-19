// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Counters for the storage IO scheduler.
//!
//! Naming follows the crate convention: `namespace.name`, lower-snake, with a leading `!` on
//! counters that indicate a problem (so they stand out in dashboards). The scheduler's namespaces
//! are `fs.submit.*` (admission), `fs.complete.*` (completions), and `fs.credit.*` (the credit
//! pools' own gauges, registered via [`crate::credit::Pool::register_gauges`]).

use crate::counter::{Counter, Registry};
use crate::sync::Arc;

/// Scheduler-wide counters, shared (`Arc`) across the submit path and the completion dispatcher.
pub struct Counters {
    // ── submission ──────────────────────────────────────────────────────────
    /// Ops admitted (credit acquired + enqueued to a lane).
    pub submit_ok: Counter,
    /// Total bytes admitted (read + write payload).
    pub submit_bytes: Counter,
    /// Submissions that parked on credit at least once before admission (contention signal).
    pub submit_parked: Counter,
    /// `!` Submissions rejected before admission (bad device, misaligned/oversized, closed): a
    /// caller-visible error, surfaced for visibility.
    pub submit_rejected: Counter,

    // ── completion ──────────────────────────────────────────────────────────
    /// Ops completed successfully.
    pub complete_ok: Counter,
    /// Bytes actually transferred on completion.
    pub complete_bytes: Counter,
    /// `!` Ops whose backend execution failed (errored syscall/CQE).
    pub complete_failed: Counter,
    /// `!` Completed ops whose submitter had already dropped its completion receiver (the result is
    /// discarded) — usually a cancelled read-ahead, occasionally a bug.
    pub complete_orphaned: Counter,

    // ── dispatch ────────────────────────────────────────────────────────────
    /// `!` Ops the dispatcher could not hand to a lane (lane/backend closed); surfaced as failed.
    pub dispatch_lane_closed: Counter,
}

impl Counters {
    /// Register the scheduler's counters against `registry`. Call once per scheduler; clone the
    /// resulting `Arc` to the submit path and the completion dispatcher.
    pub fn register(registry: &Registry) -> Arc<Self> {
        Arc::new(Self {
            submit_ok: registry.register("fs.submit.ok"),
            submit_bytes: registry.register_bytes("fs.submit.bytes"),
            submit_parked: registry.register("fs.submit.parked"),
            submit_rejected: registry.register("fs.submit.!rejected"),

            complete_ok: registry.register("fs.complete.ok"),
            complete_bytes: registry.register_bytes("fs.complete.bytes"),
            complete_failed: registry.register("fs.complete.!failed"),
            complete_orphaned: registry.register("fs.complete.!orphaned"),

            dispatch_lane_closed: registry.register("fs.dispatch.!lane_closed"),
        })
    }
}
