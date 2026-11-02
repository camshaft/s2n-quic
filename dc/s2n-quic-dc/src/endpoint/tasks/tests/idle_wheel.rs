// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract tests for the idle wheel drain tasks (send and recv).
//!
//! The idle wheel fires after a peer's negotiated idle timeout elapses without activity.
//! On expiry the callback re-checks PathSecretEntry.last_activity — if still idle, the
//! context is evicted from the cache; if activity occurred, it is rescheduled.

use super::helpers::{test_entry, test_frame, RecvContextBuilder, TestReceiverExt, WakeNowSender};
use crate::{
    endpoint::{
        frame::{self, Frame},
        id::{Id, IdMap, LocalSendSocketId, LocalSenderId},
        inflight::{Packet, TransmissionInfo},
        recv, send, tasks,
    },
    intrusive::{Entry, Queue},
    socket::channel::{intrusive::unsync, ReceiverExt as _, UnboundedSender as _},
    testing::{ext::*, sim},
    time::{bach::Clock, precision},
};
use s2n_quic_core::{packet::number::PacketNumberSpace, time::Clock as _, varint::VarInt};
use std::{cell::RefCell, rc::Rc};

// ── Send idle wheel tests ────────────────────────────────────────────────────

type SetupSend = (
    IdMap<LocalSendSocketId, Rc<RefCell<send::Cache>>>,
    unsync::Sender<send::IdleWheelAdapter>,
    unsync::Receiver<crate::intrusive::EntryAdapter<Frame>>,
    Clock,
    crate::counter::Registry,
);

fn setup_send() -> SetupSend {
    setup_send_with_frame_receiver(true)
}

/// As `setup_send`, where `open` false closes the frame submission channel at once, so that it
/// refuses each probe the drain submits.
fn setup_send_with_frame_receiver(open: bool) -> SetupSend {
    let registry = crate::counter::Registry::default();
    let clock = Clock::default();
    let send_caches: IdMap<LocalSendSocketId, _> = vec![Rc::new(RefCell::new(send::Cache::new(
        &registry,
        LocalSenderId::from_index(0),
    )))]
    .into();
    let sender_idx_to_local =
        IdMap::<LocalSenderId, LocalSendSocketId>::new(1, LocalSendSocketId::new(0));
    let sender_local_addrs =
        IdMap::<LocalSendSocketId, std::net::SocketAddr>::new(1, "127.0.0.1:0".parse().unwrap());

    let (idle_wheel_tx, idle_wheel_rx) = unsync::new_with_adapter::<send::IdleWheelAdapter>();
    let (completed_tx, completed_rx) = unsync::new::<Frame>();
    let (ack_completions_tx, _ack_completions_rx) = unsync::new::<crate::endpoint::msg::Sender>();
    let ack_completions_tx = ack_completions_tx.into_list_sender();
    let (peer_dead_tx, peer_dead_rx) = unsync::new::<tasks::PeerDead>();
    let q_gauge = registry.register_queue_gauge("test.idle_wheel");
    // The drain submits its probes of quiet paths here; nothing delivers them in these
    // tests, as for a peer that doesn't answer.
    let (frame_tx, frame_rx) = frame::submission_channel(1);
    if open {
        async move {
            let _frame_rx = frame_rx;
            core::future::pending::<()>().await
        }
        .spawn();
    } else {
        drop(frame_rx);
    }

    tasks::send_idle_wheel_drain(
        idle_wheel_rx,
        idle_wheel_tx.clone(),
        clock.clone(),
        q_gauge,
        send_caches.clone(),
        sender_idx_to_local,
        sender_local_addrs,
        completed_tx,
        ack_completions_tx,
        peer_dead_tx,
        crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN,
        frame_tx,
        registry.register("idle.send.expired"),
        registry.register("idle.send.rescheduled"),
        registry.register_nominal_timer("idle.send.lifetime", "send.0"),
        32,
        registry.register_nominal_task("task.idle_wheel", "send.0"),
    )
    .spawn();

    let peer_dead_broadcast_task = tasks::peer_dead_broadcast(
        peer_dead_rx,
        WakeNowSender,
        tasks::PeerDeadCounters {
            events: registry.register("test.peer_dead.events"),
            broadcasted: registry.register("test.peer_dead.broadcasted"),
        },
    );
    async move { peer_dead_broadcast_task.drain_budgeted(Some(32)).await }.spawn();

    (send_caches, idle_wheel_tx, completed_rx, clock, registry)
}

#[test]
fn send_idle_wheel_expires_inactive_context() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, mut completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        // Simulate initial activity so is_idle_expired can fire
        pse.touch_activity(precision::Clock::now(&clock));

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();

        // Push a frame so invalidation has something to drain
        ctx.borrow_mut().queues[1].push_back(test_frame(&pse));

        // Schedule into the idle wheel
        {
            let mut c = ctx.borrow_mut();
            let timeout = c.path_secret_entry.idle_timeout();
            c.idle_wheel.target_time = Some(precision::Clock::now(&clock) + timeout);
        }
        let _ = idle_wheel_tx.send(ctx);

        let send_caches = send_caches.clone();
        async move {
            // Default idle_timeout is 60s. Advance past it.
            61.s().sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should be evicted after idle timeout"
            );

            // The drained frame should arrive on completed_rx marked as Failed(PeerDead)
            let frame: Entry<Frame> = TestReceiverExt::recv(&mut completed_rx)
                .await
                .expect("should receive the drained frame on completed_rx");
            assert_eq!(
                frame.status,
                frame::TransmissionStatus::Failed(frame::FailureReason::PeerDead),
                "frame should be marked Failed(PeerDead)"
            );

            // The peer isn't marked dead, so it isn't owed a reset of the frame's stream.
            assert!(
                pse.take_owed_resets().is_empty(),
                "a peer that isn't marked dead isn't owed a reset"
            );
        }
        .primary()
        .spawn();
    });
}

#[test]
fn send_idle_wheel_reschedules_active_context() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();

        // Schedule into the idle wheel
        {
            let mut c = ctx.borrow_mut();
            let timeout = c.path_secret_entry.idle_timeout();
            c.idle_wheel.target_time = Some(precision::Clock::now(&clock) + timeout);
        }
        let _ = idle_wheel_tx.send(ctx.clone());

        let send_caches = send_caches.clone();
        let ctx = ctx.clone();
        async move {
            // Simulate peer activity (ACK received) at T=1s. The idle wheel uses
            // last_peer_activity to anchor the timeout, so the context should survive
            // until last_peer_activity + idle_timeout = 1s + 30s = T=31s.
            1.s().sleep().await;
            ctx.borrow_mut().last_peer_activity = precision::Clock::now(&Clock::default());

            // At T=25s the context should still be alive (expires at T=31s)
            24.s().sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                1,
                "context should still be alive before last_peer_activity + idle_timeout"
            );

            // At T=35s (past 31s) it should be evicted
            10.s().sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should be evicted after last_peer_activity + idle_timeout"
            );
        }
        .primary()
        .spawn();
    });
}

#[test]
fn send_idle_wheel_expires_reader_only_queue_no_reset_without_inflight() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();

        // No inflight packets — this is a naturally idle connection.
        assert!(!ctx.borrow().inflight.has_inflight());

        {
            let mut c = ctx.borrow_mut();
            let timeout = c.path_secret_entry.idle_timeout();
            c.idle_wheel.target_time = Some(precision::Clock::now(&clock) + timeout);
        }
        let _ = idle_wheel_tx.send(ctx);

        let send_caches = send_caches.clone();
        async move {
            61.s().sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should be evicted after idle timeout"
            );

            // Without inflight packets, the peer is not marked dead.
            // The entry's dead_at should remain unset (-1).
            let now = crate::time::DefaultClock::default().now();
            assert!(
                !pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "peer should NOT be marked dead when idle expires without inflight"
            );
        }
        .primary()
        .spawn();
    });
}

/// When a send context goes idle with NO packets in flight, the peer must NOT be
/// marked dead. An empty inflight set means both sides simply stopped talking —
/// this is normal idle, not evidence of a dead peer.
#[test]
fn send_idle_wheel_no_inflight_does_not_mark_dead() {
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();

        // No frames queued, no inflight packets — purely idle.
        assert!(!ctx.borrow().inflight.has_inflight());

        {
            let mut c = ctx.borrow_mut();
            let timeout = c.path_secret_entry.idle_timeout();
            c.idle_wheel.target_time = Some(precision::Clock::now(&clock) + timeout);
        }
        let _ = idle_wheel_tx.send(ctx);

        let send_caches = send_caches.clone();
        let pse = pse.clone();
        async move {
            // Idle timeout is 30s. Sleep just past it so the wheel fires.
            31.s().sleep().await;

            // The context should be evicted (cleanup is fine).
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should be evicted after idle timeout"
            );

            // Check immediately — well within the 30s cooldown window.
            // The peer must NOT be marked dead when no packets were in flight.
            let now = precision::Clock::now(&Clock::default());
            assert!(
                !pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "peer should NOT be marked dead when idle expires with no inflight packets"
            );
        }
        .primary()
        .spawn();
    });
}

/// Opens a stream on the client entry `pse`; it stays open until the returned receivers drop.
fn open_stream(pse: &crate::path::secret::map::Entry) -> impl Sized {
    let crate::path::secret::map::entry::QueueState::Client(state) = pse.queue_state() else {
        panic!("test_entry builds a client entry");
    };
    state
        .alloc_local(VarInt::ZERO)
        .expect("the client state allocates a slot")
}

/// A send context with no unacknowledged packet, on a quiet path with an open stream: the idle
/// wheel probes the peer half an idle timeout into the quiet period, and since the peer doesn't
/// answer, it marks the peer dead at the idle timeout, which resets the stream
/// (`peer_dead_broadcast`) in place of leaving it waiting on a peer it can't reach.
#[test]
fn send_idle_wheel_marks_dead_a_peer_that_doesnt_answer_the_probe_of_an_open_stream() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let stream = open_stream(&pse);

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        assert!(!ctx.borrow().inflight.has_inflight());

        let idle = pse.idle_timeout();
        {
            let mut c = ctx.borrow_mut();
            c.idle_wheel.target_time = Some(precision::Clock::now(&clock) + idle / 2);
        }
        let _ = idle_wheel_tx.send(ctx.clone());

        let send_caches = send_caches.clone();
        let pse = pse.clone();
        async move {
            // Half an idle timeout in, the quiet path is probed and the context kept.
            (idle / 2 + 3.s()).sleep().await;
            assert!(
                ctx.borrow().idle_probe_for.is_some(),
                "the idle wheel should probe a quiet path with an open stream"
            );
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                1,
                "the context should wait for the probe's answer"
            );

            // At the idle timeout, the unanswered probe marks the peer dead.
            (idle / 2).sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should be evicted after idle timeout"
            );
            let now = precision::Clock::now(&Clock::default());
            assert!(
                pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "a peer that doesn't answer the probe of an open stream should be marked dead"
            );
            drop(stream);
        }
        .primary()
        .spawn();
    });
}

/// The same quiet path with an open stream, whose idle check first runs more than a wheel tick
/// after its idle deadline, as after this side was suspended: the peer marks this side dead
/// under the same rule by then, so the idle wheel marks the peer dead at once, instead of probing
/// a peer that could answer though it has reset its end of the stream.
#[test]
fn send_idle_wheel_marks_dead_an_open_stream_whose_idle_check_runs_late() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        let start = precision::Clock::now(&clock);
        pse.touch_activity(start);
        let stream = open_stream(&pse);

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        assert!(!ctx.borrow().inflight.has_inflight());

        let idle = pse.idle_timeout();
        ctx.borrow_mut().idle_wheel.target_time = Some(start + idle);

        let send_caches = send_caches.clone();
        let pse = pse.clone();
        async move {
            // The check is armed for the idle deadline, but reaches the wheel only 3 s after it.
            (idle + 3.s()).sleep().await;
            let _ = idle_wheel_tx.send(ctx.clone());
            3.s().sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should be evicted after idle timeout"
            );
            assert!(
                ctx.borrow().idle_probe_for.is_none(),
                "a path whose idle check runs late isn't probed"
            );
            let now = precision::Clock::now(&Clock::default());
            assert!(
                pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "an open stream whose idle check runs late should mark the peer dead"
            );
            drop(stream);
        }
        .primary()
        .spawn();
    });
}

/// A quiet path whose idle check was armed at the full idle timeout, as where its first stream
/// opened after the check was armed, and which no probe has reached by the deadline: the check
/// runs on time, so the idle wheel probes the peer then, and marks it dead only where the probe
/// goes unanswered for half an idle timeout more.
#[test]
fn send_idle_wheel_probes_an_open_stream_whose_check_was_armed_at_the_full_idle_timeout() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        let start = precision::Clock::now(&clock);
        pse.touch_activity(start);

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        assert!(!ctx.borrow().inflight.has_inflight());

        let idle = pse.idle_timeout();
        ctx.borrow_mut().idle_wheel.target_time = Some(start + idle);
        let _ = idle_wheel_tx.send(ctx.clone());
        let stream = open_stream(&pse);

        let send_caches = send_caches.clone();
        let pse = pse.clone();
        async move {
            // At the idle deadline the quiet path is probed, and the context kept.
            (idle + 3.s()).sleep().await;
            assert!(
                ctx.borrow().idle_probe_for.is_some(),
                "the idle wheel should probe a quiet path with an open stream at its deadline"
            );
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                1,
                "the context should wait for the probe's answer"
            );

            // Half an idle timeout later, the unanswered probe marks the peer dead.
            (idle / 2).sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should be evicted once the probe goes unanswered"
            );
            let now = precision::Clock::now(&Clock::default());
            assert!(
                pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "a peer that doesn't answer the probe should be marked dead"
            );
            drop(stream);
        }
        .primary()
        .spawn();
    });
}

/// A quiet path probed while its stream was open, whose stream closes before the idle timeout:
/// the unanswered probe shows that the peer can't be reached, as an unacknowledged packet does,
/// so the idle wheel marks the peer dead at the idle timeout though no stream is open by then.
#[test]
fn send_idle_wheel_marks_dead_a_peer_that_doesnt_answer_the_probe_after_its_stream_closes() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let stream = open_stream(&pse);

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        assert!(!ctx.borrow().inflight.has_inflight());

        let idle = pse.idle_timeout();
        {
            let mut c = ctx.borrow_mut();
            c.idle_wheel.target_time = Some(precision::Clock::now(&clock) + idle / 2);
        }
        let _ = idle_wheel_tx.send(ctx.clone());

        let send_caches = send_caches.clone();
        let pse = pse.clone();
        async move {
            // Half an idle timeout in, the quiet path is probed; then the stream closes.
            (idle / 2 + 3.s()).sleep().await;
            assert!(
                ctx.borrow().idle_probe_for.is_some(),
                "the idle wheel should probe a quiet path with an open stream"
            );
            drop(stream);
            assert!(!pse.queue_state().has_open_stream());

            // At the idle timeout, the unanswered probe marks the peer dead.
            (idle / 2).sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should be evicted after idle timeout"
            );
            let now = precision::Clock::now(&Clock::default());
            assert!(
                pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "an unanswered probe should mark the peer dead after its stream closes"
            );
        }
        .primary()
        .spawn();
    });
}

/// A probe that the frame submission channel refuses, as once its receiver has closed it: the
/// drain doesn't record a refused probe, and tries it again at the next wheel tick instead of
/// waiting for the idle deadline on a probe that never went out.
#[test]
fn send_idle_wheel_retries_a_probe_the_submission_channel_refuses() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) =
            setup_send_with_frame_receiver(false);

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let stream = open_stream(&pse);

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        assert!(!ctx.borrow().inflight.has_inflight());

        let idle = pse.idle_timeout();
        let start = precision::Clock::now(&clock);
        {
            let mut c = ctx.borrow_mut();
            c.idle_wheel.target_time = Some(start + idle / 2);
        }
        let _ = idle_wheel_tx.send(ctx.clone());

        let send_caches = send_caches.clone();
        async move {
            // Half an idle timeout in, the probe is refused: it isn't recorded, and the context
            // is checked again at the next wheel tick instead of at the idle deadline.
            (idle / 2 + 3.s()).sleep().await;
            assert!(
                ctx.borrow().idle_probe_for.is_none(),
                "a refused probe shouldn't be recorded"
            );
            let deadline = start + idle;
            assert!(
                ctx.borrow()
                    .idle_wheel
                    .target_time
                    .is_some_and(|next_check| next_check.nanos < deadline.nanos),
                "a refused probe should be tried again before the idle deadline"
            );
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                1,
                "the context should be kept while the probe is tried again"
            );
            drop(stream);
        }
        .primary()
        .spawn();
    });
}

/// A send context first arms its idle check half an idle timeout out while its path has an
/// open stream, so that the idle wheel can probe the quiet path before the idle timeout
/// decides, and a full idle timeout out otherwise.
#[test]
fn send_context_with_an_open_stream_arms_its_idle_check_at_half_the_idle_timeout() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, _idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        let idle = pse.idle_timeout();
        let now = precision::Clock::now(&clock);

        let _ = ctx.borrow_mut().wheel_interest(&clock);
        assert_eq!(ctx.borrow().idle_wheel.target_time, Some(now + idle));

        let _stream = open_stream(&pse);
        let _ = ctx.borrow_mut().wheel_interest(&clock);
        assert_eq!(ctx.borrow().idle_wheel.target_time, Some(now + idle / 2));
    });
}

/// When a recv context goes idle, the peer must NOT be marked dead. The recv side
/// has no way to distinguish "peer stopped sending because it has nothing to say"
/// from "peer is actually dead". Only the send side (via unacknowledged inflight
/// packets) has evidence of a dead peer.
#[test]
fn recv_idle_wheel_does_not_mark_dead() {
    sim(|| {
        let (recv_cache, mut idle_wheel_tx, clock, _registry) = setup_recv();

        let ctx = RecvContextBuilder::default().build();
        ctx.borrow()
            .path_entry
            .touch_activity(precision::Clock::now(&clock));
        let pse = ctx.borrow().path_entry.clone();
        let key = {
            let c = ctx.borrow();
            recv::Key {
                id: *c.path_entry.id(),
                remote_sender_id: c.remote_sender_id,
            }
        };
        recv_cache.borrow_mut().senders.insert(key, ctx.clone());
        let _ = idle_wheel_tx.send(ctx);

        let recv_cache = recv_cache.clone();
        async move {
            // Idle timeout is 30s. Sleep just past it so the wheel fires,
            // but stay within the 30s cooldown window to detect the mark.
            31.s().sleep().await;

            // The recv context should be evicted.
            assert!(
                recv_cache.borrow().senders.is_empty(),
                "recv context should be evicted after idle timeout"
            );

            // Check immediately — well within the cooldown window.
            // The peer must NOT be marked dead from the recv side.
            let now = precision::Clock::now(&Clock::default());
            assert!(
                !pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "peer should NOT be marked dead when recv idle expires — \
                 only the send side with inflight packets can determine peer death"
            );
        }
        .primary()
        .spawn();
    });
}

// ── Recv idle wheel tests ────────────────────────────────────────────────────

fn setup_recv() -> (
    Rc<RefCell<recv::Cache>>,
    unsync::Sender<recv::IdleWheelAdapter>,
    Clock,
    crate::counter::Registry,
) {
    let registry = crate::counter::Registry::default();
    let clock = Clock::default();
    let recv_cache = Rc::new(RefCell::new(recv::Cache::new(
        crate::endpoint::id::RecvDispatchWorkerId::new(0),
    )));

    let (idle_wheel_tx, idle_wheel_rx) = unsync::new_with_adapter::<recv::IdleWheelAdapter>();
    let q_gauge = registry.register_queue_gauge("test.recv_idle_wheel");

    tasks::recv_idle_wheel_drain(
        idle_wheel_rx,
        idle_wheel_tx.clone(),
        clock.clone(),
        q_gauge,
        recv_cache.clone(),
        registry.register("idle.recv.expired"),
        registry.register("idle.recv.rescheduled"),
        registry.register_nominal_timer("idle.recv.lifetime", "recv.0"),
        32,
        registry.register_nominal_task("task.recv_idle_wheel", "recv.0"),
    )
    .spawn();

    (recv_cache, idle_wheel_tx, clock, registry)
}

#[test]
fn recv_idle_wheel_expires_inactive_context() {
    sim(|| {
        let (recv_cache, mut idle_wheel_tx, clock, _registry) = setup_recv();

        let ctx = RecvContextBuilder::default().build();
        // Simulate initial activity
        ctx.borrow()
            .path_entry
            .touch_activity(precision::Clock::now(&clock));
        let key = {
            let c = ctx.borrow();
            recv::Key {
                id: *c.path_entry.id(),
                remote_sender_id: c.remote_sender_id,
            }
        };
        recv_cache.borrow_mut().senders.insert(key, ctx.clone());
        let _ = idle_wheel_tx.send(ctx);

        let recv_cache = recv_cache.clone();
        async move {
            31.s().sleep().await;
            assert!(
                recv_cache.borrow().senders.is_empty(),
                "recv context should be evicted after idle timeout"
            );
        }
        .primary()
        .spawn();
    });
}

#[test]
fn recv_idle_wheel_reschedules_active_context() {
    sim(|| {
        let (recv_cache, mut idle_wheel_tx, clock, _registry) = setup_recv();

        let ctx = RecvContextBuilder::default().build();
        let key = {
            let c = ctx.borrow();
            recv::Key {
                id: *c.path_entry.id(),
                remote_sender_id: c.remote_sender_id,
            }
        };
        // Touch activity so last_activity != 0
        ctx.borrow()
            .path_entry
            .touch_activity(precision::Clock::now(&clock));
        recv_cache.borrow_mut().senders.insert(key, ctx.clone());
        let _ = idle_wheel_tx.send(ctx);

        let recv_cache = recv_cache.clone();
        async move {
            // Touch activity at T=1s. The reschedule anchors to last_activity,
            // so the context must survive until last_activity + idle_timeout = 1s + 30s = T=31s.
            1.s().sleep().await;
            let ctx = recv_cache.borrow().senders.values().next().unwrap().clone();
            ctx.borrow()
                .path_entry
                .touch_activity(precision::Clock::now(&Clock::default()));

            // At T=25s the context should still be alive (expires at T=31s)
            24.s().sleep().await;
            assert_eq!(
                recv_cache.borrow().senders.len(),
                1,
                "recv context should still be alive before last_activity + idle_timeout"
            );

            // At T=35s (past 31s) it should be evicted
            10.s().sleep().await;
            assert!(
                recv_cache.borrow().senders.is_empty(),
                "recv context should be evicted after last_activity + idle_timeout"
            );
        }
        .primary()
        .spawn();
    });
}

/// Reproduction: a packet sent shortly before the idle wheel fires (e.g. at t=25s with
/// idle_timeout=30s) should NOT cause the peer to be marked dead. The inflight packet is
/// only 6s old — the peer hasn't had time to respond. Currently, `is_peer_idle` only
/// checks `last_peer_activity` (set at context creation or last ACK), so it incorrectly
/// declares the peer dead despite the recent send.
#[test]
fn send_idle_wheel_defers_death_for_recent_inflight() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();

        // Schedule into the idle wheel: fires at t=30s
        {
            let mut c = ctx.borrow_mut();
            let timeout = c.path_secret_entry.idle_timeout();
            c.idle_wheel.target_time = Some(precision::Clock::now(&clock) + timeout);
        }
        let _ = idle_wheel_tx.send(ctx.clone());

        let send_caches = send_caches.clone();
        let pse = pse.clone();
        async move {
            // At t=25s: simulate a packet being sent (empty→non-empty inflight).
            // The fix resets last_peer_activity to now when transitioning from no
            // inflight to inflight, effectively restarting the idle timeout.
            25.s().sleep().await;
            {
                let mut ctx = ctx.borrow_mut();
                let now = clock.get_time();
                let rtt = ctx.rtt_estimator;
                let cc_info = ctx.cca.on_packet_sent(now, 200, false, &rtt);
                let mut frames = Queue::new();
                frames.push_back(test_frame(&pse));
                let pn = PacketNumberSpace::Initial.new_packet_number(VarInt::ZERO);

                // Simulate what the assembler does after the fix: restart idle on first send.
                if !ctx.inflight.has_inflight() {
                    ctx.last_peer_activity = precision::Clock::now(&clock);
                }

                ctx.inflight.insert(
                    pn,
                    Packet::new(
                        frames,
                        TransmissionInfo {
                            cc_info,
                            time_sent: now,
                            sent_bytes: 200,
                        },
                    ),
                );
            }

            // At t=31s: idle wheel fires (past original 30s target). But with the fix,
            // last_peer_activity was reset to t=25s, so is_peer_idle returns false
            // (only 6s elapsed < 30s timeout). The context is rescheduled.
            6.s().sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                1,
                "context should still be alive — inflight packet is only 6s old"
            );
            let now = precision::Clock::now(&Clock::default());
            assert!(
                !pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "peer should NOT be marked dead when inflight is only 6s old"
            );

            // At t=56s: rescheduled target was t=55s (last_peer_activity=25s + timeout=30s).
            // Now is_peer_idle returns true (31s elapsed >= 30s). Peer marked dead.
            25.s().sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should be evicted after inflight waited idle_timeout"
            );
            let now = precision::Clock::now(&Clock::default());
            assert!(
                pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "peer should be marked dead after inflight waited full idle_timeout"
            );
        }
        .primary()
        .spawn();
    });
}

/// A send context that expires with an unacknowledged packet marks its peer dead and fails the
/// packet's frames. The peer isn't told that its streams were reset on this side, so the path
/// owes it a reset of each stream whose frames failed, once: here two frames of one stream and
/// one of another, beside a Ping, which belongs to no stream.
#[test]
fn send_idle_wheel_owes_a_reset_of_each_stream_whose_frames_it_fails_as_it_marks_the_peer_dead() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, mut completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let idle = pse.idle_timeout();

        // `test_frame` is a QueueData frame of queue pair 1 to 2 and binding 1.
        let data_stream = crate::queue::OwedReset {
            queue_pair: crate::packet::datagram::QueuePair {
                source_queue_id: VarInt::from_u8(1),
                dest_queue_id: VarInt::from_u8(2),
            },
            binding_id: VarInt::from_u8(1),
        };
        let max_data_stream = crate::queue::OwedReset {
            queue_pair: crate::packet::datagram::QueuePair {
                source_queue_id: VarInt::from_u8(3),
                dest_queue_id: VarInt::from_u8(4),
            },
            binding_id: VarInt::from_u8(2),
        };
        let with_header = |header: frame::Header| {
            let mut frame = test_frame(&pse);
            frame.header = header;
            frame
        };

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        {
            let mut ctx = ctx.borrow_mut();
            let now = clock.get_time();
            let rtt = ctx.rtt_estimator;
            let cc_info = ctx.cca.on_packet_sent(now, 200, false, &rtt);
            let mut frames = Queue::new();
            frames.push_back(test_frame(&pse));
            frames.push_back(test_frame(&pse));
            frames.push_back(with_header(frame::Header::QueueMaxData {
                queue_pair: max_data_stream.queue_pair,
                binding_id: max_data_stream.binding_id,
                maximum_data: VarInt::from_u32(1024),
            }));
            frames.push_back(with_header(frame::Header::Ping));
            let pn = PacketNumberSpace::Initial.new_packet_number(VarInt::ZERO);
            ctx.inflight.insert(
                pn,
                Packet::new(
                    frames,
                    TransmissionInfo {
                        cc_info,
                        time_sent: now,
                        sent_bytes: 200,
                    },
                ),
            );
            ctx.idle_wheel.target_time = Some(precision::Clock::now(&clock) + idle);
        }
        let _ = idle_wheel_tx.send(ctx);

        let send_caches = send_caches.clone();
        async move {
            // 3 s past the idle deadline, a margin over the wheel's one-second tick.
            (idle + 3.s()).sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should be evicted after idle timeout"
            );
            let now = precision::Clock::now(&Clock::default());
            assert!(
                pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "a peer that left a packet unacknowledged should be marked dead"
            );

            // Each failed frame still reaches the completions.
            for _ in 0..4 {
                let frame: Entry<Frame> = TestReceiverExt::recv(&mut completed_rx)
                    .await
                    .expect("each failed frame should arrive on completed_rx");
                assert_eq!(
                    frame.status,
                    frame::TransmissionStatus::Failed(frame::FailureReason::PeerDead),
                    "each frame should be marked Failed(PeerDead)"
                );
            }

            // Each of the two streams is owed one reset, and the Ping none.
            let owed = pse.take_owed_resets();
            assert_eq!(owed.len(), 2, "one reset for each stream: {owed:?}");
            assert!(
                owed.contains(&data_stream) && owed.contains(&max_data_stream),
                "{owed:?}"
            );
        }
        .primary()
        .spawn();
    });
}

/// Gossip heartbeats arrive on the recv path and update PathSecretEntry::last_activity.
/// The send idle wheel must treat this as evidence the peer is alive and reschedule
/// rather than expiring the send context.
#[test]
fn send_idle_wheel_kept_alive_by_path_activity() {
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();

        // Schedule into the idle wheel: fires at t=30s
        {
            let mut c = ctx.borrow_mut();
            let timeout = c.path_secret_entry.idle_timeout();
            c.idle_wheel.target_time = Some(precision::Clock::now(&clock) + timeout);
        }
        let _ = idle_wheel_tx.send(ctx.clone());

        let send_caches = send_caches.clone();
        let pse = pse.clone();
        async move {
            // At t=20s: simulate a gossip heartbeat arriving on the recv path.
            // This touches PathSecretEntry::last_activity but NOT the send context's
            // last_peer_activity (no ACK was received).
            20.s().sleep().await;
            pse.touch_activity(precision::Clock::now(&Clock::default()));

            // At t=31s: the idle wheel fires (past the original 30s target).
            // Because path_entry.last_activity was refreshed at t=20s, the send context
            // should be rescheduled, not expired.
            11.s().sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                1,
                "context should still be alive — path activity (gossip) at t=20s keeps it alive"
            );

            // At t=51s: past last_activity(20s) + idle_timeout(30s) = t=50s.
            // Now the context should expire.
            20.s().sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should expire after path activity + idle_timeout"
            );
        }
        .primary()
        .spawn();
    });
}

/// Repeated gossip heartbeats keep a send context alive indefinitely, even without
/// any ACK activity. This simulates the steady-state gossip pattern where peers
/// heartbeat every second.
#[test]
fn send_idle_wheel_survives_indefinitely_with_periodic_path_activity() {
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();

        {
            let mut c = ctx.borrow_mut();
            let timeout = c.path_secret_entry.idle_timeout();
            c.idle_wheel.target_time = Some(precision::Clock::now(&clock) + timeout);
        }
        let _ = idle_wheel_tx.send(ctx.clone());

        let send_caches = send_caches.clone();
        let pse = pse.clone();
        async move {
            // Simulate heartbeats arriving every 10 seconds for 120 seconds (4x the idle timeout).
            // The send context must survive the entire time.
            for _ in 0..12 {
                10.s().sleep().await;
                pse.touch_activity(precision::Clock::now(&Clock::default()));
            }

            // At t=120s: context should still be alive.
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                1,
                "context should survive indefinitely with periodic heartbeats"
            );

            // Stop heartbeating. Wait for idle timeout + margin.
            31.s().sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should expire after heartbeats stop"
            );
        }
        .primary()
        .spawn();
    });
}
