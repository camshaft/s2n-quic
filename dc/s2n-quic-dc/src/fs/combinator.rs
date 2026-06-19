// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Storage-specific pipeline stages, composing with the generic `socket::channel` combinators.
//!
//! For M1 the two storage-specific pieces are the completion **sink** the backend pushes into and
//! the completion **dispatcher** that drains it — the analog of the endpoint's `AckProcessor →
//! CompletionDispatcher` path. The dispatcher releases each completed op's borrowed credit back to
//! its device pool (the conservation invariant: acquire-on-submit, release-on-complete) and routes
//! the op to its submitter's completion channel, which is exactly how the QUIC endpoint notifies a
//! writer that its frame is done.
//!
//! The submit-side routing (submission channel → execution lanes) lives in the scheduler itself
//! ([`crate::fs::scheduler`]); the heavier `BatchOpsByDevice`/`PickRing`/`SubmissionBuilder` stages
//! from the design are deferred — the credit pool already provides the fairness the M1 gate asserts.

use crate::{
    fs::{
        backend::CompletionSink,
        device::DeviceTable,
        op::IoOp,
    },
    intrusive::Entry,
    socket::channel::{intrusive::unsync, Budget, Receiver as _, UnboundedSender},
    sync::Arc,
};
use core::task::Poll;

/// A [`CompletionSink`] that forwards completed ops into an unsync channel the completion
/// dispatcher drains. One per backend; the backend takes a `boxed_clone` per lane.
#[derive(Clone)]
pub struct ChannelCompletionSink {
    tx: unsync::Sender<crate::intrusive::EntryAdapter<IoOp>>,
}

impl ChannelCompletionSink {
    /// Build a sink and the matching receiver the dispatcher drains.
    pub fn new() -> (Self, unsync::Receiver<crate::intrusive::EntryAdapter<IoOp>>) {
        let (tx, rx) = unsync::new::<IoOp>();
        (Self { tx }, rx)
    }
}

impl CompletionSink for ChannelCompletionSink {
    fn send(&self, op: Entry<IoOp>) {
        // Unbounded, infallible unless the dispatcher is gone; a dropped completion would strand a
        // submitter, but the dispatcher outlives all lanes by construction (scheduler-owned).
        let mut tx = self.tx.clone();
        let _ = UnboundedSender::send(&mut tx, op);
    }

    fn boxed_clone(&self) -> Box<dyn CompletionSink> {
        Box::new(self.clone())
    }
}

/// Drains completed ops, releasing each op's credit back to its device pool and routing it to its
/// submitter's completion channel. Runs as a single endpoint task.
pub async fn completion_dispatcher(
    mut rx: unsync::Receiver<crate::intrusive::EntryAdapter<IoOp>>,
    devices: Arc<DeviceTable>,
) {
    let mut budget = Budget::new(1 << 20);
    core::future::poll_fn(move |cx| {
        budget.reset();
        loop {
            match rx.poll_recv(cx, &mut budget) {
                Poll::Ready(Some(entry)) => {
                    dispatch_one(entry, &devices);
                    if budget.is_exhausted() {
                        // Queue still non-empty but budget spent: the receiver did not register its
                        // waker this poll, so self-wake to re-poll (mirrors the distributor loop).
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                }
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => return Poll::Pending,
            }
        }
    })
    .await;
}

/// Release a completed op's credit and hand it to its submitter.
fn dispatch_one(mut entry: Entry<IoOp>, devices: &DeviceTable) {
    // 1. Release the borrowed credit back to the op's device pool — exactly once. This is the
    //    release half of the conservation invariant (acquire on submit, release on complete). The
    //    device id was validated at submit time, so it is always in range here.
    let credits = entry.take_credits();
    if credits > 0 {
        if let Some(device) = devices.get(entry.device) {
            device.pool_for(entry.kind).release(credits);
        }
    }

    // 2. Route the op to its submitter. Take the completion sender out of the op so the op (with its
    //    filled buffer + final status) can be sent on its own completion channel. If there is no
    //    sender (fire-and-forget) the op is simply dropped here.
    let Some(sender) = entry.completion.take() else {
        return;
    };
    // `send_entry` returns an `AutoWake`; dropping it wakes the parked submitter. On a dropped
    // receiver the completion is silently discarded (the submitter is gone), which is correct.
    let _wake = sender.send_entry(entry);
}
