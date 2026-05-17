// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract tests for completion-side endpoint tasks.
//!
//! These cover the two small adapters in the send completion pipeline:
//! - `completion_dispatcher`: forwards completion wake notifications to the waker sink.
//! - `cancelled_drain`: consumes cancelled frames and continues draining.

use super::helpers::{test_entry, test_frame, CollectingSender, TestReceiver, TestReceiverExt as _};
use crate::{
    endpoint::{frame, tasks},
    flow::queue::AutoWake,
    intrusive::Entry,
    socket::channel::ReceiverExt as _,
    testing::{ext::*, sim},
};

/// A frame carrying a completion sender produces one wake notification in the sink.
#[test]
fn completion_dispatcher_forwards_wake_notifications() {
    sim(|| {
        let pse = test_entry();
        let mut frame = test_frame(&pse).into_inner();
        let completion_rx = frame::completion_channel();
        frame.completion = Some(completion_rx.sender());

        let input = TestReceiver::new([Entry::new(frame)]);
        let (sender, collected) = CollectingSender::<AutoWake>::new();
        let rx = tasks::completion_dispatcher(input, sender);
        async move { rx.drain_budgeted(Some(32)).await }
            .primary()
            .spawn();

        async move {
            1.ms().sleep().await;
            assert_eq!(collected.borrow().len(), 1);
        }
        .primary()
        .spawn();
    });
}

/// Frames without a completion sender produce no wake notifications.
#[test]
fn completion_dispatcher_ignores_frames_without_completion_sender() {
    sim(|| {
        let pse = test_entry();
        let frame = test_frame(&pse);

        let input = TestReceiver::new([frame]);
        let (sender, collected) = CollectingSender::<AutoWake>::new();
        let rx = tasks::completion_dispatcher(input, sender);
        async move { rx.drain_budgeted(Some(32)).await }
            .primary()
            .spawn();

        async move {
            1.ms().sleep().await;
            assert!(collected.borrow().is_empty());
        }
        .primary()
        .spawn();
    });
}

/// `cancelled_drain` yields one output item per input frame and then closes.
#[test]
fn cancelled_drain_consumes_all_frames_then_closes() {
    sim(|| {
        let pse = test_entry();
        let frames = [test_frame(&pse), test_frame(&pse)];
        let mut rx = tasks::cancelled_drain(TestReceiver::new(frames));

        async move {
            assert!(rx.recv().await.is_some());
            assert!(rx.recv().await.is_some());
            assert!(rx.recv().await.is_none());
        }
        .primary()
        .spawn();
    });
}
