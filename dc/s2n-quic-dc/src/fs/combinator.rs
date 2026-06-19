// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Storage-specific pipeline stages, composing with the generic `socket::channel` combinators.
//!
//! For M1 the one storage-specific piece is the completion **dispatcher** that drains the channel
//! backends push completed ops into — the analog of the endpoint's `AckProcessor →
//! CompletionDispatcher` path. The dispatcher releases each completed op's borrowed credit back to
//! its device pool (the conservation invariant: acquire-on-submit, release-on-complete) and routes
//! the op to its submitter's completion channel, which is exactly how the QUIC endpoint notifies a
//! writer that its frame is done.
//!
//! The backend→dispatcher channel is a plain [`unsync`] intrusive channel: the lane pushes a
//! completed op with [`Sender::send_entry`](unsync::Sender::send_entry) (no clone, no wrapper) and
//! the dispatcher drains it. The submit-side routing (submission channel → execution lanes) lives in
//! the scheduler itself ([`crate::fs::scheduler`]); the heavier `BatchOpsByDevice`/`PickRing`/
//! `SubmissionBuilder` stages from the design are deferred — the credit pool already provides the
//! fairness the M1 gate asserts.

use crate::{
    fs::{
        counters::Counters,
        device::DeviceTable,
        op::{IoOp, IoStatus},
    },
    intrusive::{Entry, EntryAdapter},
    socket::channel::{intrusive::unsync, Map, ReceiverExt as _},
    sync::Arc,
};

/// The channel a backend lane pushes completed [`IoOp`]s into; the [`completion_dispatcher`] drains
/// it. A plain `unsync` sender (cheap `Rc` clone per lane), so there is no wrapper type, vtable, or
/// per-lane heap box. Distinct from [`op::CompletionSender`](crate::fs::op::CompletionSender), which
/// is the *submitter's* per-op notification channel the dispatcher forwards onto.
pub type CompletionSink = unsync::Sender<EntryAdapter<IoOp>>;

/// The drain side of [`CompletionSink`], owned by the [`completion_dispatcher`] task.
pub type CompletionDrain = unsync::Receiver<EntryAdapter<IoOp>>;

/// Build a backend-completion channel: lanes clone the [`CompletionSink`]; the dispatcher owns the
/// [`CompletionDrain`].
pub fn completion_channel() -> (CompletionSink, CompletionDrain) {
    unsync::new::<IoOp>()
}

/// Budget per poll cycle for the pipeline tasks — how many ops a task processes before yielding.
pub(crate) const DRAIN_BUDGET: usize = 1024;

/// Drains completed ops, releasing each op's credit back to its device pool and routing it to its
/// submitter's completion channel. Built as a `Map` combinator over the completion receiver and
/// driven by `drain_budgeted`, so the budget/self-wake bookkeeping is the channel layer's standard
/// machinery rather than a hand-rolled poll loop.
pub async fn completion_dispatcher(
    rx: CompletionDrain,
    devices: Arc<DeviceTable>,
    counters: Arc<Counters>,
) {
    Map::new(rx, move |entry| dispatch_one(entry, &devices, &counters))
        .drain_budgeted(Some(DRAIN_BUDGET))
        .await;
}

/// Release a completed op's credit and hand it to its submitter.
fn dispatch_one(mut entry: Entry<IoOp>, devices: &DeviceTable, counters: &Counters) {
    // 1. Record the completion's disposition.
    match entry.status {
        IoStatus::Done(n) => {
            counters.complete_ok.add(1);
            counters.complete_bytes.add(n as u64);
        }
        IoStatus::Failed(_) => counters.complete_failed.add(1),
        // The backend always stamps a terminal status before completing; a still-`Pending` op here
        // would be a backend bug.
        IoStatus::Pending => {
            debug_assert!(false, "completion dispatcher saw a still-pending op");
        }
    }

    // 2. Release the borrowed credit back to the op's device pool — exactly once. This is the
    //    release half of the conservation invariant (acquire on submit, release on complete). The
    //    device id was validated at submit time, so it is always in range here.
    //
    //    ORDER IS LOAD-BEARING — do NOT move this below step 3. The credit must be released BEFORE
    //    the route/discard so that a completion whose submitter already vanished (a dropped
    //    `MaterializeStream` / cancelled `submit`, where step 3 silently discards the op on a dead
    //    receiver) still returns its credit to the pool. Releasing after a discard would leak the
    //    credit for every cancelled-mid-flight op. (`materialize_drop_mid_spray_conserves_credit`
    //    is the regression guard.)
    let credits = entry.take_credits();
    if credits > 0 {
        if let Some(device) = devices.get(entry.device) {
            device.pool_for(entry.kind).release(credits);
        }
    }

    // 3. Route the op to its submitter. Take the completion sender out of the op so the op (with its
    //    filled buffer + final status) can be sent on its own completion channel. If there is no
    //    sender (fire-and-forget) the op is simply dropped here.
    let Some(sender) = entry.completion.take() else {
        return;
    };
    // If the submitter already dropped its receiver (e.g. a cancelled read-ahead), the completion is
    // discarded — count it so an unexpectedly high rate is visible. `send_entry` returns an
    // `AutoWake`; dropping it wakes the parked submitter.
    if !sender.receiver_alive() {
        counters.complete_orphaned.add(1);
    }
    let _wake = sender.send_entry(entry);
}
