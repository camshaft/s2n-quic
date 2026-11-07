// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract tests for the `ack_burst` task.
//!
//! The ack burst task sits on the recv dispatch worker. When the packet dispatch task
//! schedules an ACK (by pushing a recv::Context into the burst queue), this task encodes
//! the ACK ranges and emits a PendingAck submission to the send worker. These tests verify
//! the encoding/emission contract: contexts with pending ACKs produce submissions, contexts
//! without pending ACKs produce nothing, and already-flushed contexts are not double-submitted.

use super::helpers::{
    event_count, test_map_with_subscriber, RecvContextBuilder, TestReceiver, TestReceiverExt as _,
};
use crate::{
    socket::channel::{intrusive::unsync, ReceiverExt as _},
    stream::endpoint::{msg, tasks},
    testing::{ext::*, sim},
    time::bach::Clock,
};
use s2n_quic_core::{time::Clock as _, varint::VarInt};
use std::{cell::RefCell, rc::Rc};

struct Harness {
    output_rx: crate::socket::channel::intrusive::unsync::Receiver<
        crate::intrusive::EntryAdapter<msg::Sender>,
    >,
}

/// Spawns the ack_burst task with the given contexts and returns a harness
/// for observing the emitted PendingAck submissions.
fn setup(
    contexts: impl IntoIterator<Item = Rc<RefCell<crate::stream::endpoint::recv::Context>>>,
) -> Harness {
    setup_with_map(contexts, None)
}

/// Spawns the ack_burst task as `setup` does, and publishes its events through
/// `path_secret_map` where one is set.
fn setup_with_map(
    contexts: impl IntoIterator<Item = Rc<RefCell<crate::stream::endpoint::recv::Context>>>,
    path_secret_map: Option<crate::path::secret::Map>,
) -> Harness {
    let (sender, output_rx) = unsync::new::<msg::Sender>();
    let input = TestReceiver::new(contexts);
    let counters = crate::endpoint::counters::Dispatch::new(&crate::counter::Registry::default());
    let rx = tasks::ack_burst(
        input,
        sender,
        crate::endpoint::id::RecvDispatchWorkerId::new(0),
        counters,
        path_secret_map,
    );
    async move { rx.drain_budgeted(Some(32)).await }
        .primary()
        .spawn();
    Harness { output_rx }
}

/// Helper: build a context and schedule an ACK on it.
fn scheduled_context() -> Rc<RefCell<crate::stream::endpoint::recv::Context>> {
    let ctx = RecvContextBuilder::default().build();
    {
        let mut c = ctx.borrow_mut();
        let clock = Clock::default();
        let now = clock.get_time();
        c.ack_ranges.on_packet_received(VarInt::from_u8(1), now);
        c.ack_state.on_ack_eliciting().unwrap();
    }
    ctx
}

/// Helper: build a context of a path with four senders, schedule an ACK on it, and mark the
/// interface pair of its sender lost where `lost` is set.
fn scheduled_context_with_senders(
    lost: bool,
) -> Rc<RefCell<crate::stream::endpoint::recv::Context>> {
    let ctx = RecvContextBuilder::default().socket_sender_count(4).build();
    {
        let mut c = ctx.borrow_mut();
        let clock = Clock::default();
        let now = clock.get_time();
        c.ack_ranges.on_packet_received(VarInt::from_u8(1), now);
        c.ack_state.on_ack_eliciting().unwrap();
        let sender = c.local_sender_id;
        assert_eq!(c.path_entry.set_sender_lost(sender, lost), lost);
    }
    ctx
}

/// A context with ack_state=Scheduled and recorded ACK ranges produces a PendingAck submission.
#[test]
fn context_with_pending_acks_emits_submission() {
    sim(|| {
        let Harness { mut output_rx } = setup([scheduled_context()]);

        async move {
            let first = output_rx
                .recv()
                .await
                .expect("expected pending-ack submission");
            assert!(matches!(&*first, msg::Sender::PendingAck(_)));
            assert!(
                output_rx.recv().await.is_none(),
                "expected exactly one submission"
            );
        }
        .primary()
        .spawn();
    });
}

/// A context in Idle state (no ack-eliciting packets received) produces no output.
#[test]
fn context_with_no_pending_acks_emits_nothing() {
    sim(|| {
        let ctx = RecvContextBuilder::default().build();
        let Harness { mut output_rx } = setup([ctx]);

        async move {
            assert!(
                output_rx.recv().await.is_none(),
                "idle context should emit nothing"
            );
        }
        .primary()
        .spawn();
    });
}

/// Each context in the burst queue is processed independently — N scheduled contexts
/// produce N submissions.
#[test]
fn multiple_contexts_each_produce_submission() {
    sim(|| {
        let contexts: Vec<_> = (0..3)
            .map(|i| {
                let ctx = RecvContextBuilder::default()
                    .remote_sender_id(VarInt::new(i).unwrap())
                    .build();
                {
                    let mut c = ctx.borrow_mut();
                    let clock = Clock::default();
                    let now = clock.get_time();
                    c.ack_ranges.on_packet_received(VarInt::from_u8(1), now);
                    c.ack_state.on_ack_eliciting().unwrap();
                }
                ctx
            })
            .collect();

        let Harness { mut output_rx } = setup(contexts);

        async move {
            for _ in 0..3 {
                let item = output_rx
                    .recv()
                    .await
                    .expect("scheduled context should emit submission");
                assert!(matches!(&*item, msg::Sender::PendingAck(_)));
            }
            assert!(
                output_rx.recv().await.is_none(),
                "expected exactly three submissions"
            );
        }
        .primary()
        .spawn();
    });
}

/// A context already in Flushed state (ACK submission already in the send pipeline)
/// does not produce a second submission. The at-most-one-in-flight invariant is preserved.
#[test]
fn flushed_context_does_not_double_submit() {
    sim(|| {
        let ctx = RecvContextBuilder::default().build();
        {
            let mut c = ctx.borrow_mut();
            let clock = Clock::default();
            let now = clock.get_time();
            c.ack_ranges.on_packet_received(VarInt::from_u8(1), now);
            c.ack_state.on_ack_eliciting().unwrap();
            c.ack_state.on_flush().unwrap();
        }

        let Harness { mut output_rx } = setup([ctx]);

        async move {
            assert!(
                output_rx.recv().await.is_none(),
                "flushed context should not be re-submitted"
            );
        }
        .primary()
        .spawn();
    });
}

/// The same context queued twice before the burst task drains should still produce
/// at most one in-flight ACK submission.
#[test]
fn duplicate_context_entry_produces_single_submission() {
    sim(|| {
        let ctx = scheduled_context();
        let Harness { mut output_rx } = setup([ctx.clone(), ctx]);

        async move {
            let first = output_rx
                .recv()
                .await
                .expect("duplicate context should still emit first submission");
            assert!(matches!(&*first, msg::Sender::PendingAck(_)));
            assert!(
                output_rx.recv().await.is_none(),
                "duplicate context should not double-submit"
            );
        }
        .primary()
        .spawn();
    });
}

/// An ACK that the lost mark of its context's sender moves to another sender publishes a
/// `PathSecretMapAcknowledgementMoved` event through the path secret map.
#[test]
fn ack_moved_off_a_lost_sender_publishes_an_event() {
    sim(|| {
        let (map, subscriber) = test_map_with_subscriber();
        let Harness { mut output_rx } =
            setup_with_map([scheduled_context_with_senders(true)], Some(map));

        async move {
            let first = output_rx
                .recv()
                .await
                .expect("expected pending-ack submission");
            let msg::Sender::PendingAck(submission) = &*first else {
                panic!("expected a pending-ack submission");
            };
            let delivering = crate::endpoint::id::LocalSenderId::new(VarInt::from_u8(2));
            assert_eq!(submission.local_sender_id, delivering);
            let moves = event_count(&subscriber.path_secret_map_acknowledgement_moved);
            assert_eq!(moves, 1);
        }
        .primary()
        .spawn();
    });
}

/// An ACK that stays on its context's sender, whose pair delivers, doesn't publish a
/// `PathSecretMapAcknowledgementMoved` event.
#[test]
fn ack_on_a_delivering_sender_is_not_recorded_as_moved() {
    sim(|| {
        let (map, subscriber) = test_map_with_subscriber();
        let Harness { mut output_rx } =
            setup_with_map([scheduled_context_with_senders(false)], Some(map));

        async move {
            let first = output_rx
                .recv()
                .await
                .expect("expected pending-ack submission");
            let msg::Sender::PendingAck(submission) = &*first else {
                panic!("expected a pending-ack submission");
            };
            let sender = crate::endpoint::id::LocalSenderId::new(VarInt::from_u8(1));
            assert_eq!(submission.local_sender_id, sender);
            let moves = event_count(&subscriber.path_secret_map_acknowledgement_moved);
            assert_eq!(moves, 0);
        }
        .primary()
        .spawn();
    });
}
