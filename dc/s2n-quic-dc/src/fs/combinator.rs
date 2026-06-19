// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Op completion — finishing an [`IoOp`] in place.
//!
//! Because an op carries its own `Arc<Device>` (the pool to release credit to) and its own
//! `Send + Sync` completion sender (the submitter to notify), **whatever thread finishes the op can
//! complete it directly** — there is no global reaper, no completion dispatcher, and no `!Send`
//! completion bridge. A backend worker thread stamps the op's status, then calls [`complete`] on its
//! own thread; the submission dispatch task does the same for an op it could not route to a lane.
//! This mirrors the networking side, where the worker that finishes a frame completes it rather than
//! funnelling every completion through one task.
//!
//! [`complete`] performs, in this exact order:
//!   1. record the disposition (ok / bytes / failed) in the scheduler counters,
//!   2. release the op's borrowed credit back to its device pool (the release half of the
//!      acquire-on-submit / release-on-complete conservation invariant),
//!   3. route the op to its submitter's completion channel (or drop it, for fire-and-forget).
//!
//! Step 2 **must** precede step 3 — see the ordering note on [`complete`].

use crate::{
    fs::{
        counters::Counters,
        op::{IoOp, IoStatus},
    },
    intrusive::Entry,
};

/// Budget per poll cycle for the pipeline tasks — how many ops a task processes before yielding.
pub(crate) const DRAIN_BUDGET: usize = 1024;

/// Complete an op in place: record its disposition, release its credit to its own device pool, and
/// notify its submitter. Called by the backend worker that finished the op (or by the dispatch task
/// for an op it could not deliver to a lane), on that worker's thread.
///
/// **Ordering is load-bearing.** Credit is released (step 2) *before* the op is routed/dropped
/// (step 3) so that a completion whose submitter already vanished — a dropped `MaterializeStream`
/// or a cancelled `submit`, where step 3 silently discards the op on a dead receiver — still returns
/// its credit to the pool. Releasing after a discard would leak the credit for every
/// cancelled-mid-flight op. (`materialize_drop_mid_spray_conserves_credit` is the regression guard.)
pub fn complete(mut op: Entry<IoOp>, counters: &Counters) {
    // 1. Record the completion's disposition. The backend always stamps a terminal status before
    //    completing; a still-`Pending` op here would be a backend bug.
    match op.status {
        IoStatus::Done(n) => {
            counters.complete_ok.add(1);
            counters.complete_bytes.add(n as u64);
        }
        IoStatus::Failed(_) => counters.complete_failed.add(1),
        IoStatus::Pending => {
            debug_assert!(false, "complete() saw a still-pending op");
        }
    }

    // 2. Release the borrowed credit back to the op's own device pool — exactly once. The op carries
    //    the `Arc<Device>`, so this needs no table lookup. (See the load-bearing ordering note above:
    //    this MUST happen before step 3.)
    op.release_credits();

    // 3. Route the op to its submitter. Take the completion sender out of the op so the op (with its
    //    filled buffer + final status) can be sent on its own completion channel. If there is no
    //    sender (fire-and-forget) the op is simply dropped here.
    let Some(sender) = op.completion.take() else {
        return;
    };
    // If the submitter already dropped its receiver (e.g. a cancelled read-ahead), the completion is
    // discarded — count it so an unexpectedly high rate is visible. `send_entry` returns an
    // `AutoWake`; dropping it wakes the parked submitter.
    if !sender.receiver_alive() {
        counters.complete_orphaned.add(1);
    }
    let _wake = sender.send_entry(op);
}
