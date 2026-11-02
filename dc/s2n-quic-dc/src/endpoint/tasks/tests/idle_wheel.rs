// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract tests for the idle wheel drain tasks (send and recv).
//!
//! The idle wheel fires after a peer's negotiated idle timeout elapses without activity.
//! On expiry the callback re-checks PathSecretEntry.last_activity — if still idle, the
//! context is evicted from the cache; if activity occurred, it is rescheduled.

use super::helpers::{
    test_entry, test_frame, test_waker, RecvContextBuilder, TestReceiverExt, WakeNowSender,
};
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
    let (setup, frame_rx) = setup_send_and_frame_receiver();
    if open {
        async move {
            let _frame_rx = frame_rx;
            core::future::pending::<()>().await
        }
        .spawn();
    } else {
        drop(frame_rx);
    }
    setup
}

/// As `setup_send`, and returns the receiver of the frame submission channel, where the drain
/// submits its probes of quiet paths, so that a test reads each probe it submits.
fn setup_send_and_frame_receiver() -> (SetupSend, frame::SubmissionReceiver) {
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
        tasks::IdleProbeCounters::new(&registry),
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

    ((send_caches, idle_wheel_tx, completed_rx, clock, registry), frame_rx)
}

/// The counts of the drain's probe counters, `tasks::IdleProbeCounters`.
#[derive(Debug, Default, PartialEq, Eq)]
struct ProbeCounts {
    sent: u64,
    refused: u64,
    dead_probe_unanswered: u64,
    dead_open_stream: u64,
}

/// Takes the registry's metrics line, which drains it, and reads the probe counters from it, each
/// 0 where the line leaves the counter out. Each test takes the line once, at its end.
fn take_probe_counts(registry: &crate::counter::Registry) -> ProbeCounts {
    let line = registry.take_current_metrics_line().unwrap_or_default();
    let count = |label: &str| -> u64 {
        line.split([',', ' ', '\n'])
            .find_map(|entry| entry.strip_prefix(label)?.strip_prefix('='))
            .map_or(0, |value| {
                let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().expect("a counter's value is a whole number")
            })
    };
    ProbeCounts {
        sent: count("idle.send.probe_sent"),
        refused: count("idle.send.probe_refused"),
        dead_probe_unanswered: count("idle.send.dead_probe_unanswered"),
        dead_open_stream: count("idle.send.dead_open_stream"),
    }
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
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, registry) = setup_send();

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
            assert_eq!(
                take_probe_counts(&registry),
                ProbeCounts::default(),
                "an idle path with nothing open or unacknowledged isn't probed or marked dead"
            );
        }
        .primary()
        .spawn();
    });
}

/// Opens a stream on the client entry `pse`; it stays open until the returned receivers drop.
fn open_stream(pse: &crate::path::secret::map::Entry) -> crate::queue::AllocResult {
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
        let ((send_caches, mut idle_wheel_tx, _completed_rx, clock, registry), mut frame_rx) =
            setup_send_and_frame_receiver();

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let stream = open_stream(&pse);
        // As the stream's writer does when it is made, so that the dead mark owes the peer a
        // reset of the stream and resets it on this side.
        stream.control.set_peer_queue_id(stream.dest_queue_id);

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

            // The probe is one Ping frame for the peer, on the send worker's frame submission
            // channel, and the stream isn't reset before the idle timeout.
            let (waker, _wakes) = test_waker();
            let mut cx = core::task::Context::from_waker(&waker);
            let mut submitted = frame::PriorityStorage::default();
            assert!(
                matches!(
                    frame_rx.poll_swap(&mut cx, &mut submitted),
                    core::task::Poll::Ready(Some(()))
                ),
                "the probe should go out on the frame submission channel"
            );
            let probes: Vec<&Frame> = submitted.iter().collect();
            assert_eq!(probes.len(), 1, "the quiet period should be probed once");
            assert_eq!(probes[0].header, frame::Header::Ping);
            assert!(
                std::sync::Arc::ptr_eq(&probes[0].path_secret_entry, &pse),
                "the probe should go to the quiet path's peer"
            );
            assert!(
                matches!(stream.stream.try_recv(), Ok(None)),
                "the stream shouldn't be reset before the idle timeout"
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

            // Marking the peer dead resets the open stream with the idle timeout error.
            let reset = stream
                .stream
                .try_recv()
                .expect("a peer-dead reset leaves the stream's sender open")
                .expect("marking the peer dead should reset the open stream");
            assert!(
                matches!(
                    &*reset,
                    crate::endpoint::msg::Stream::Reset { error_code }
                        if *error_code == crate::endpoint::error::IDLE_TIMEOUT
                ),
                "the stream should be reset with the idle timeout error"
            );

            // The on-time check at the idle deadline doesn't probe the quiet period again.
            let mut later = frame::PriorityStorage::default();
            assert!(
                matches!(frame_rx.poll_swap(&mut cx, &mut later), core::task::Poll::Pending),
                "the quiet period should be probed once"
            );
            assert_eq!(
                take_probe_counts(&registry),
                ProbeCounts {
                    sent: 1,
                    refused: 0,
                    dead_probe_unanswered: 1,
                    dead_open_stream: 0,
                },
                "one probe went out, and the peer was marked dead for not answering it"
            );
            drop(stream);
        }
        .primary()
        .spawn();
    });
}

/// The same quiet path with an open stream, whose probe's Ping is still unacknowledged at the
/// idle timeout. The assembler puts a packet that carries Pings alone into the empty inflight map
/// without restarting the idle clock, so the idle timeout of the probed quiet period decides, with
/// `probed` and the unacknowledged Ping together: the dead mark counts as an unanswered probe, not
/// as a dead mark on unacknowledged packets alone.
#[test]
fn send_idle_wheel_counts_an_unanswered_probe_whose_ping_is_still_unacknowledged() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, registry) = setup_send();

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
            // Half an idle timeout in, the quiet path is probed.
            (idle / 2 + 3.s()).sleep().await;
            {
                let ctx = ctx.borrow();
                let last_activity = ctx.last_peer_activity.max(pse.last_activity());
                assert_eq!(
                    ctx.idle_probe_for,
                    Some(last_activity.nanos),
                    "the idle wheel should record its probe of the quiet period"
                );
            }

            // The Ping goes out and isn't acknowledged: its packet, which carries the Ping alone,
            // goes into the empty inflight map, and `last_peer_activity` stays as it was.
            {
                let mut ctx = ctx.borrow_mut();
                let now = clock.get_time();
                let rtt = ctx.rtt_estimator;
                let cc_info = ctx.cca.on_packet_sent(now, 200, false, &rtt);
                let mut frames = Queue::new();
                frames.push_back(Entry::new(Frame {
                    header: frame::Header::Ping,
                    payload: crate::byte_vec::ByteVec::new(),
                    path_secret_entry: pse.clone(),
                    completion: None,
                    status: frame::TransmissionStatus::default(),
                    ttl: frame::DEFAULT_TTL,
                    enqueued_at: None,
                    flow_credits: 0,
                }));
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
                assert!(ctx.inflight.has_inflight());
            }
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
            assert_eq!(
                take_probe_counts(&registry),
                ProbeCounts {
                    sent: 1,
                    refused: 0,
                    dead_probe_unanswered: 1,
                    dead_open_stream: 0,
                },
                "the dead mark counts as an unanswered probe while the Ping is unacknowledged"
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
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, registry) = setup_send();

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
            assert_eq!(
                take_probe_counts(&registry),
                ProbeCounts {
                    sent: 0,
                    refused: 0,
                    dead_probe_unanswered: 0,
                    dead_open_stream: 1,
                },
                "a late check marks the peer dead on the open stream without a probe"
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
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, registry) = setup_send();

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
            assert_eq!(
                take_probe_counts(&registry),
                ProbeCounts {
                    sent: 1,
                    refused: 0,
                    dead_probe_unanswered: 1,
                    dead_open_stream: 0,
                },
                "the probe at the idle deadline went unanswered, so the peer was marked dead"
            );
            drop(stream);
        }
        .primary()
        .spawn();
    });
}

/// A send context armed at the full idle timeout while its path has no open stream, whose first
/// stream opens half an idle timeout later with a packet that begins a new quiet period.
/// `wheel_interest` doesn't arm a context again while the idle wheel holds it, so the wheel first
/// reaches the context at the full idle timeout, half an idle timeout into the new quiet period:
/// the drain finds the peer active and probes it as it reschedules the context. A peer that
/// answers each probe keeps the context; once a probe goes unanswered, the peer is marked dead at
/// the idle timeout.
#[test]
fn send_idle_wheel_probes_and_keeps_a_stream_opened_after_a_full_idle_timeout_arm() {
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
        assert!(!pse.queue_state().has_open_stream());

        let idle = pse.idle_timeout();
        // Each 3 s margin past a deadline below stays within half an idle timeout.
        assert!(
            idle / 2 > 3.s(),
            "the test's margins need an idle timeout over 6 s"
        );

        // With no stream open, the context arms its idle check at the full idle timeout.
        let _ = ctx.borrow_mut().wheel_interest(&clock);
        assert_eq!(ctx.borrow().idle_wheel.target_time, Some(start + idle));
        let _ = idle_wheel_tx.send(ctx.clone());

        let send_caches = send_caches.clone();
        let pse = pse.clone();
        async move {
            // Half an idle timeout in, the path's first stream opens with a packet.
            (idle / 2).sleep().await;
            pse.touch_activity(precision::Clock::now(&clock));
            let quiet_since = pse.last_activity().nanos;
            let stream = open_stream(&pse);
            assert!(pse.queue_state().has_open_stream());

            // The idle wheel holds the context, so it isn't armed again at half the idle
            // timeout: its check stays at the full idle timeout.
            let _ = ctx.borrow_mut().wheel_interest(&clock);
            assert!(ctx.borrow().is_idle_scheduled());
            assert_eq!(ctx.borrow().idle_wheel.target_time, Some(start + idle));
            assert!(ctx.borrow().idle_probe_for.is_none());

            // At the full idle timeout, half an idle timeout into the stream's quiet period, the
            // drain probes the peer and keeps the context.
            (idle / 2 + 3.s()).sleep().await;
            assert_eq!(
                ctx.borrow().idle_probe_for,
                Some(quiet_since),
                "the idle wheel should probe the quiet period that the stream's packet began"
            );
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                1,
                "the context should wait for the probe's answer"
            );
            let now = precision::Clock::now(&clock);
            assert!(
                !pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "a peer whose probe is still out shouldn't be marked dead"
            );

            // The peer answers the probe, which begins a new quiet period.
            let answer = precision::Clock::now(&clock);
            ctx.borrow_mut().last_peer_activity = answer;

            // Past the probed period's idle deadline, the context is kept, and its new quiet
            // period is probed half an idle timeout in.
            (idle / 2 + 3.s()).sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                1,
                "the context should be kept while its peer answers"
            );
            assert_eq!(
                ctx.borrow().idle_probe_for,
                Some(answer.nanos),
                "the idle wheel should probe the quiet period that the answer began"
            );
            let now = precision::Clock::now(&clock);
            assert!(
                !pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "a peer that answers its probe shouldn't be marked dead"
            );

            // The peer doesn't answer this probe: at the idle timeout it is marked dead.
            (idle / 2).sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should be evicted once the probe goes unanswered"
            );
            let now = precision::Clock::now(&clock);
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
/// drain doesn't record a refused probe, and since a closed channel doesn't open again, it waits
/// for the idle deadline instead of trying the probe again at each wheel tick. The check at the
/// deadline tries the probe once more, and once it is refused again, the idle timeout decides:
/// the context is evicted, and its open stream marks the peer dead.
#[test]
fn send_idle_wheel_retries_a_probe_the_submission_channel_refuses() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, registry) =
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
        let pse = pse.clone();
        async move {
            // Half an idle timeout in, the probe is refused: it isn't recorded, and the context
            // is checked again at its idle deadline instead of at the next wheel tick.
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
                    .is_some_and(|next_check| next_check.nanos == deadline.nanos),
                "a refused probe should wait for the idle deadline"
            );
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                1,
                "the context should be kept until its idle deadline"
            );
            let counts = take_probe_counts(&registry);
            assert!(
                counts.refused >= 1,
                "a refused probe should be counted: {counts:?}"
            );
            assert_eq!(
                ProbeCounts {
                    refused: counts.refused,
                    ..ProbeCounts::default()
                },
                counts,
                "a refused probe isn't counted as sent, and the peer isn't marked dead"
            );

            // At the idle deadline, the probe is refused again, and the idle timeout decides at
            // that check: the context isn't checked again a tick later.
            (idle / 2).sleep().await;
            assert!(
                ctx.borrow().idle_probe_for.is_none(),
                "a probe refused at the idle deadline shouldn't be recorded"
            );
            assert!(
                ctx.borrow()
                    .idle_wheel
                    .target_time
                    .is_some_and(|last_check| last_check.nanos == deadline.nanos),
                "a probe refused at the idle deadline shouldn't reschedule the context"
            );
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "the context should be evicted at the idle deadline"
            );
            let now = precision::Clock::now(&Clock::default());
            assert!(
                pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "the open stream should mark the peer dead at the idle deadline"
            );
            drop(stream);
        }
        .primary()
        .spawn();
    });
}

/// The quiet path with an open stream, whose peer is marked dead after the probe went out: at
/// the idle timeout the dead-peer cooldown refuses the idle wheel's dead mark, so that mark
/// counts in neither dead-mark counter, while the probe counts as sent.
#[test]
fn send_idle_wheel_doesnt_count_a_dead_mark_the_cooldown_refuses() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, registry) = setup_send();

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let stream = open_stream(&pse);

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        assert!(!ctx.borrow().inflight.has_inflight());

        let idle = pse.idle_timeout();
        let cooldown = crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN;
        assert!(
            idle / 2 < cooldown,
            "a mark half an idle timeout in should still hold its cooldown at the idle timeout"
        );
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

            // Mark the peer dead now, so that the cooldown holds at the idle timeout.
            let marked_at = precision::Clock::now(&Clock::default());
            assert!(
                pse.mark_dead_if_cooldown_elapsed(marked_at, cooldown),
                "the peer shouldn't be marked dead before this mark"
            );

            // At the idle timeout the unanswered probe would mark the peer dead, and the
            // cooldown refuses that mark.
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
                pse.is_dead_during_cooldown(now, cooldown),
                "the earlier mark's cooldown should still hold"
            );
            assert_eq!(
                take_probe_counts(&registry),
                ProbeCounts {
                    sent: 1,
                    ..ProbeCounts::default()
                },
                "the probe went out, and the refused dead mark counts in neither dead count"
            );

            // Past the earlier mark's cooldown the peer isn't dead: the idle wheel's mark at the
            // idle timeout didn't start a cooldown of its own.
            (cooldown - idle / 2 + 1.s()).sleep().await;
            let now = precision::Clock::now(&Clock::default());
            assert!(
                !pse.is_dead_during_cooldown(now, cooldown),
                "the cooldown should have refused the idle wheel's dead mark"
            );
            drop(stream);
        }
        .primary()
        .spawn();
    });
}

/// A send context that a stale key's invalidation removed from the cache stays on the idle
/// wheel until its idle deadline, and the retransmission of its frames makes a newer context
/// for the same path. The invalidated context's expiry leaves that newer context as it is: the
/// cache keeps it, and its frames aren't failed.
#[test]
fn send_idle_wheel_expiry_of_an_invalidated_context_keeps_its_paths_newer_context() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let id = *pse.id();

        let invalidated = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        let idle = pse.idle_timeout();
        let start = precision::Clock::now(&clock);
        invalidated.borrow_mut().idle_wheel.target_time = Some(start + idle);
        let _ = idle_wheel_tx.send(invalidated.clone());

        let key_id = invalidated.borrow().credentials.key_id;
        let (mut retransmit_tx, _retransmit_rx) = unsync::new::<Frame>();
        let _ = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .invalidate_stale_key(
                &id,
                LocalSenderId::from_index(0),
                key_id,
                &mut retransmit_tx,
            )
            .expect("the cache should hold the context the stale key invalidates");
        assert!(invalidated.borrow().invalidated);

        let newer = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        assert!(!Rc::ptr_eq(&newer, &invalidated));
        newer.borrow_mut().queues[1].push_back(test_frame(&pse));

        let send_caches = send_caches.clone();
        async move {
            (idle + 3.s()).sleep().await;
            // The idle wheel holds the invalidated context until its expiry, and drops it there.
            assert_eq!(
                Rc::strong_count(&invalidated),
                1,
                "the idle wheel should have expired the invalidated context"
            );
            let cache = send_caches[LocalSendSocketId::new(0)].borrow();
            assert_eq!(
                cache.context_count(),
                1,
                "the cache should keep the newer context"
            );
            assert!(
                cache.get(&id).is_some_and(|ctx| Rc::ptr_eq(&ctx, &newer)),
                "the cache should still hold the newer context for the path"
            );
            drop(cache);
            assert!(
                !newer.borrow().invalidated,
                "the newer context shouldn't be invalidated"
            );
            assert_eq!(
                newer.borrow().queues[1].len(),
                1,
                "the newer context's frame shouldn't be failed"
            );
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
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, registry) = setup_send();

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
            assert_eq!(
                take_probe_counts(&registry),
                ProbeCounts::default(),
                "unacknowledged packets alone don't count as a quiet-path dead mark"
            );
        }
        .primary()
        .spawn();
    });
}

/// A send context that expires with an unacknowledged packet marks its peer dead and fails the
/// packet's frames. The peer isn't told that its streams were reset on this side, so the path
/// owes it a reset of each stream whose frames failed, once: here two frames of one stream and
/// one of another, beside a Ping, which belongs to no stream. The wheel counts the two streams
/// on `send.idle.owed_resets`.
#[test]
fn send_idle_wheel_owes_a_reset_of_each_stream_whose_frames_it_fails_as_it_marks_the_peer_dead() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, mut completed_rx, clock, registry) = setup_send();

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

            // The wheel counts each of the two streams once, as the set records it.
            let metrics = registry
                .take_current_metrics_line()
                .expect("the registry is open");
            let recorded = metrics
                .split(',')
                .find_map(|metric| metric.strip_prefix("send.idle.owed_resets="))
                .and_then(|value| value.split(' ').next());
            assert_eq!(recorded, Some("2"), "{metrics}");
        }
        .primary()
        .spawn();
    });
}

/// A send context that expires with an unacknowledged packet marks its peer dead, and the dead
/// mark resets each stream open on the path on this side. The path owes the peer a reset of each
/// open stream too, though the context holds none of its frames: here the packet holds a Ping
/// alone, and one stream is open, as an idle writer's is. The wheel counts the open stream on
/// `send.idle.owed_resets`.
#[test]
fn send_idle_wheel_owes_a_reset_of_each_open_stream_as_it_marks_the_peer_dead() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, mut completed_rx, clock, registry) = setup_send();

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let idle = pse.idle_timeout();

        let crate::path::secret::map::entry::QueueState::Client(state) = pse.queue_state() else {
            panic!("the test entry holds client queue state");
        };
        let open = state
            .alloc_local(VarInt::from_u8(5))
            .expect("a fresh client queue state allocates a slot");
        // As the stream's writer does when it is made.
        open.control.set_peer_queue_id(open.dest_queue_id);
        let open_stream = crate::queue::OwedReset {
            queue_pair: crate::packet::datagram::QueuePair {
                source_queue_id: open.local_queue_id,
                dest_queue_id: open.dest_queue_id,
            },
            binding_id: open.binding_id,
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
            let mut ping = test_frame(&pse);
            ping.header = frame::Header::Ping;
            let mut frames = Queue::new();
            frames.push_back(ping);
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

        async move {
            // 3 s past the idle deadline, a margin over the wheel's one-second tick.
            (idle + 3.s()).sleep().await;
            let now = precision::Clock::now(&Clock::default());
            assert!(
                pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "a peer that left a packet unacknowledged should be marked dead"
            );
            let frame: Entry<Frame> = TestReceiverExt::recv(&mut completed_rx)
                .await
                .expect("the failed Ping should arrive on completed_rx");
            assert_eq!(
                frame.status,
                frame::TransmissionStatus::Failed(frame::FailureReason::PeerDead),
                "the Ping should be marked Failed(PeerDead)"
            );

            // The open stream is owed a reset, though the context held none of its frames.
            assert_eq!(pse.take_owed_resets(), vec![open_stream]);

            // The wheel counts the open stream once, as the set records it.
            let metrics = registry
                .take_current_metrics_line()
                .expect("the registry is open");
            let recorded = metrics
                .split(',')
                .find_map(|metric| metric.strip_prefix("send.idle.owed_resets="))
                .and_then(|value| value.split(' ').next());
            assert_eq!(recorded, Some("1"), "{metrics}");
            drop(open);
        }
        .primary()
        .spawn();
    });
}

/// The dead mark resets on this side only the open streams it owes the peer a reset of: once the
/// send idle wheel marks the peer dead, `peer_dead_broadcast` resets the stream whose writer
/// recorded the peer's queue, and leaves as it is a stream whose slot is bound before its writer
/// is made, which the peer isn't owed a reset of.
#[test]
fn the_dead_mark_resets_each_open_stream_it_owes_a_reset_of_and_no_other() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let idle = pse.idle_timeout();

        let crate::path::secret::map::entry::QueueState::Client(state) = pse.queue_state() else {
            panic!("the test entry holds client queue state");
        };
        let open = state
            .alloc_local(VarInt::from_u8(5))
            .expect("a fresh client queue state allocates a slot");
        // As the stream's writer does when it is made.
        open.control.set_peer_queue_id(open.dest_queue_id);
        let open_stream = crate::queue::OwedReset {
            queue_pair: crate::packet::datagram::QueuePair {
                source_queue_id: open.local_queue_id,
                dest_queue_id: open.dest_queue_id,
            },
            binding_id: open.binding_id,
        };
        // A slot bound before its stream's writer is made, which hasn't recorded the peer's
        // queue yet.
        let unwritten = state
            .alloc_local(VarInt::from_u8(6))
            .expect("the client queue state allocates a second slot");
        let mut dispatch = crate::queue::ClientDispatch::new(state.clone());

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        {
            let mut ctx = ctx.borrow_mut();
            let now = clock.get_time();
            let rtt = ctx.rtt_estimator;
            let cc_info = ctx.cca.on_packet_sent(now, 200, false, &rtt);
            let mut ping = test_frame(&pse);
            ping.header = frame::Header::Ping;
            let mut frames = Queue::new();
            frames.push_back(ping);
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

        async move {
            // 3 s past the idle deadline, a margin over the wheel's one-second tick.
            (idle + 3.s()).sleep().await;
            let now = precision::Clock::now(&Clock::default());
            assert!(
                pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "a peer that left a packet unacknowledged should be marked dead"
            );

            let mut depth = |queue_id| {
                let diag = dispatch.slot_diag(queue_id).expect("the slot exists");
                (diag.stream_depth, diag.control_depth)
            };
            assert_eq!(
                depth(open.local_queue_id),
                (1, 1),
                "the broadcast resets the stream the dead mark owes a reset of"
            );
            assert_eq!(
                depth(unwritten.local_queue_id),
                (0, 0),
                "the broadcast leaves a stream whose writer isn't made yet as it is"
            );
            assert_eq!(pse.take_owed_resets(), vec![open_stream]);
            drop((open, unwritten));
        }
        .primary()
        .spawn();
    });
}

/// The stream a failed frame belongs to, as the reset the send idle wheel owes routes it: each
/// frame of a stream names the stream's queue pair and binding, and a frame of the path or a
/// diagnostic marker names none.
#[test]
fn owed_reset_of_names_the_stream_of_each_stream_frame_and_none_of_any_other_frame() {
    let queue_pair = crate::packet::datagram::QueuePair {
        source_queue_id: VarInt::from_u8(1),
        dest_queue_id: VarInt::from_u8(2),
    };
    let binding_id = VarInt::from_u8(3);
    let stream = crate::queue::OwedReset {
        queue_pair,
        binding_id,
    };

    let stream_frames = [
        frame::Header::QueueData {
            queue_pair,
            binding_id,
            offset: VarInt::ZERO,
            largest_offset: VarInt::ZERO,
            is_fin: false,
            blocked: false,
            dest_acceptor_id: None,
            priority: crate::credit::Priority::default(),
        },
        frame::Header::QueueControl {
            queue_pair,
            binding_id,
        },
        frame::Header::QueueMaxData {
            queue_pair,
            binding_id,
            maximum_data: VarInt::ZERO,
        },
        frame::Header::QueueReset {
            queue_pair,
            binding_id,
            reset_target: crate::packet::datagram::ResetTarget::Both,
            error_code: VarInt::ZERO,
            init: None,
        },
        frame::Header::QueueMsg {
            queue_pair,
            binding_id,
            msg_id: VarInt::ZERO,
            stream_offset: VarInt::ZERO,
            largest_offset: VarInt::ZERO,
            message_size: VarInt::ZERO,
            chunk_size: VarInt::ZERO,
            chunk_index: VarInt::ZERO,
            is_fin: false,
            is_wakeup: false,
            blocked: false,
            dest_acceptor_id: None,
            priority: crate::credit::Priority::default(),
        },
        frame::Header::QueueDataBlocked {
            queue_pair,
            binding_id,
            desired_offset: VarInt::ZERO,
        },
    ];
    for header in &stream_frames {
        assert_eq!(tasks::owed_reset_of(header), Some(stream), "{header:?}");
    }

    let other_frames = [
        frame::Header::QueueFree {
            free_request_id: VarInt::ZERO,
            smallest_queue_id: VarInt::ZERO,
        },
        frame::Header::Ack {
            dest_sender_id: VarInt::ZERO,
            ack_delay: VarInt::ZERO,
            largest_acknowledged: VarInt::ZERO,
            ack_range: VarInt::ZERO,
            ecn_counts: Default::default(),
            is_ack_eliciting: false,
        },
        frame::Header::Ping,
        frame::Header::QueueDbg {
            dump_id: VarInt::ZERO,
            queue_pair,
            binding_id,
        },
    ];
    for header in &other_frames {
        assert_eq!(tasks::owed_reset_of(header), None, "{header:?}");
    }
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

/// A quiet path with an open stream whose idle timeout is under two wheel ticks: the check half
/// an idle timeout in runs within a tick of the idle deadline, so the idle wheel probes the peer
/// there, and checks again a wheel tick later instead of within the same tick, where the check
/// would mark the peer dead before it could answer. A peer that answers the probe is kept and its
/// new quiet period is probed; once it stops answering, the unanswered probe marks it dead.
#[test]
fn send_idle_wheel_waits_a_tick_for_the_probe_answer_with_an_idle_timeout_under_two_ticks() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let idle = core::time::Duration::from_millis(1500);
        let mut params = s2n_quic_core::dc::testing::TEST_APPLICATION_PARAMS;
        params.max_idle_timeout = core::num::NonZeroU32::new(idle.as_millis() as u32);
        let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
            .socket_sender_count(8)
            .params(params)
            .build(None);
        assert_eq!(pse.idle_timeout(), idle);

        let tick = crate::time::wheel::SECOND_GRANULARITY_NANOS;
        let cooldown = crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN;
        async move {
            // Start 0.3 s into a wheel tick. The check armed half an idle timeout out, 0.75 s in,
            // runs at the start of the next tick, 0.7 s in, which is within a tick of the idle
            // deadline 1.5 s in.
            let now = precision::Clock::now(&clock);
            let to_next_tick = core::time::Duration::from_nanos(tick - now.nanos % tick);
            (to_next_tick + core::time::Duration::from_millis(300)).sleep().await;
            let start = precision::Clock::now(&clock);
            pse.touch_activity(start);
            let stream = open_stream(&pse);

            let ctx = send_caches[LocalSendSocketId::new(0)]
                .borrow_mut()
                .get_or_insert(&pse, &clock)
                .unwrap();
            assert!(!ctx.borrow().inflight.has_inflight());
            // As `wheel_interest` arms a context whose path has an open stream.
            ctx.borrow_mut().idle_wheel.target_time = Some(start + idle / 2);
            let _ = idle_wheel_tx.send(ctx.clone());

            // 0.8 s in, the check 0.7 s in has probed the quiet path and waits for the answer.
            core::time::Duration::from_millis(800).sleep().await;
            assert_eq!(
                ctx.borrow().idle_probe_for,
                Some(start.nanos),
                "the check within a tick of the idle deadline should probe the quiet path"
            );
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                1,
                "the context should wait a wheel tick for the probe's answer"
            );
            let answered_at = precision::Clock::now(&clock);
            assert!(
                !pse.is_dead_during_cooldown(answered_at, cooldown),
                "the peer shouldn't be marked dead before it could answer the probe"
            );

            // The peer answers 0.8 s in. The next check, 1.7 s in, keeps the context and probes
            // the new quiet period.
            pse.touch_activity(answered_at);
            let answered = pse.last_activity();
            core::time::Duration::from_millis(1000).sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                1,
                "a peer that answers the probe should be kept"
            );
            assert_eq!(
                ctx.borrow().idle_probe_for,
                Some(answered.nanos),
                "the answer starts a new quiet period, which should be probed in turn"
            );
            let now = precision::Clock::now(&clock);
            assert!(
                !pse.is_dead_during_cooldown(now, cooldown),
                "a peer that answers the probe shouldn't be marked dead"
            );

            // The peer doesn't answer the second probe: the check a tick later, 2.7 s in, marks
            // it dead and evicts the context.
            core::time::Duration::from_millis(1000).sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "the context should be evicted once the probe goes unanswered"
            );
            let now = precision::Clock::now(&clock);
            assert!(
                pse.is_dead_during_cooldown(now, cooldown),
                "a peer that doesn't answer the probe should be marked dead"
            );
            drop(stream);
        }
        .primary()
        .spawn();
    });
}
