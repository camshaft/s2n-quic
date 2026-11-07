// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract tests for the idle wheel drain tasks (send and recv).
//!
//! The idle wheel fires after a peer's negotiated idle timeout elapses without activity.
//! On expiry the callback re-checks PathSecretEntry.last_activity — if still idle, the
//! context is evicted from the cache; if activity occurred, it is rescheduled.

use super::helpers::{
    event_count, insert_inflight_packet, test_entry, test_frame, test_map_with_subscriber,
    RecvContextBuilder, TestReceiverExt, WakeNowSender,
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

/// The peers a test's `peer_dead_broadcast` asked a background handshake for, in order.
type Rehandshakes = Rc<RefCell<Vec<std::net::SocketAddr>>>;

fn setup_send() -> SetupSend {
    setup_send_recording(Rehandshakes::default())
}

fn setup_send_recording(rehandshakes: Rehandshakes) -> SetupSend {
    setup_send_with_caches(1, rehandshakes, None)
}

/// Builds the send idle wheel drain over `count` send caches: the cache of sender slot `i`
/// sends on local send socket `i`, for each `i` below `count`. `rehandshakes` records each
/// peer that the test's `peer_dead_broadcast` asks a background handshake for. Where
/// `path_secret_map` is given, each cache publishes the lost-pair events of its contexts
/// through it.
fn setup_send_with_caches(
    count: usize,
    rehandshakes: Rehandshakes,
    path_secret_map: Option<crate::path::secret::Map>,
) -> SetupSend {
    let registry = crate::counter::Registry::default();
    let clock = Clock::default();
    let send_caches: IdMap<LocalSendSocketId, _> = (0..count)
        .map(|idx| {
            let mut cache = send::Cache::new(&registry, LocalSenderId::from_index(idx));
            if let Some(map) = &path_secret_map {
                cache = cache.with_path_secret_map(map.clone());
            }
            cache
        })
        .map(|cache| Rc::new(RefCell::new(cache)))
        .collect::<Vec<_>>()
        .into();
    let sender_idx_to_local: IdMap<LocalSenderId, LocalSendSocketId> = (0..count)
        .map(LocalSendSocketId::new)
        .collect::<Vec<_>>()
        .into();
    let sender_local_addrs = IdMap::<LocalSendSocketId, std::net::SocketAddr>::new(
        count,
        "127.0.0.1:0".parse().unwrap(),
    );

    let (idle_wheel_tx, idle_wheel_rx) = unsync::new_with_adapter::<send::IdleWheelAdapter>();
    let (completed_tx, completed_rx) = unsync::new::<Frame>();
    let (ack_completions_tx, _ack_completions_rx) = unsync::new::<crate::endpoint::msg::Sender>();
    let ack_completions_tx = ack_completions_tx.into_list_sender();
    let (peer_dead_tx, peer_dead_rx) = unsync::new::<tasks::PeerDead>();
    let q_gauge = registry.register_queue_gauge("test.idle_wheel");

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
        move |peer| rehandshakes.borrow_mut().push(peer),
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

/// A peer marked dead on a client path, with a packet it never acknowledged, gets one background
/// handshake asked for, from `peer_dead_broadcast`.
#[test]
fn send_idle_wheel_dead_mark_requests_a_rehandshake() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let rehandshakes = Rehandshakes::default();
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) =
            setup_send_recording(rehandshakes.clone());

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();

        // A sent packet that the peer never acknowledges, so the idle expiry marks the peer
        // dead.
        {
            let mut ctx = ctx.borrow_mut();
            let now = clock.get_time();
            let rtt = ctx.rtt_estimator;
            let cc_info = ctx.cca.on_packet_sent(now, 200, false, &rtt);
            let mut frames = Queue::new();
            frames.push_back(test_frame(&pse));
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
            let timeout = ctx.path_secret_entry.idle_timeout();
            ctx.idle_wheel.target_time = Some(precision::Clock::now(&clock) + timeout);
        }
        let _ = idle_wheel_tx.send(ctx);

        async move {
            // Sleep past the idle timeout, so the wheel expires the context and marks the peer
            // dead, and the broadcast task reads the mark.
            (pse.idle_timeout() + 2.s()).sleep().await;

            let now = precision::Clock::now(&Clock::default());
            assert!(
                pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "the peer is marked dead after its packet waited an idle timeout"
            );
            assert_eq!(
                rehandshakes.borrow().as_slice(),
                &[*pse.peer()],
                "the dead mark asks for one background handshake with the peer"
            );
        }
        .primary()
        .spawn();
    });
}

/// A peer marked dead on a server path, with a packet it never acknowledged, has its streams
/// reset and doesn't get a background handshake: only a client handshakes.
#[test]
fn send_idle_wheel_dead_mark_on_a_server_path_doesnt_request_a_rehandshake() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let rehandshakes = Rehandshakes::default();
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) =
            setup_send_recording(rehandshakes.clone());

        let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
            .socket_sender_count(8)
            .endpoint_type(s2n_quic_core::endpoint::Type::Server)
            .build(None);
        pse.touch_activity(precision::Clock::now(&clock));

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();

        // A sent packet that the peer never acknowledges, so the idle expiry marks the peer
        // dead.
        {
            let mut ctx = ctx.borrow_mut();
            let now = clock.get_time();
            let rtt = ctx.rtt_estimator;
            let cc_info = ctx.cca.on_packet_sent(now, 200, false, &rtt);
            let mut frames = Queue::new();
            frames.push_back(test_frame(&pse));
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
            let timeout = ctx.path_secret_entry.idle_timeout();
            ctx.idle_wheel.target_time = Some(precision::Clock::now(&clock) + timeout);
        }
        let _ = idle_wheel_tx.send(ctx);

        async move {
            // Sleep past the idle timeout, so the wheel expires the context and marks the peer
            // dead, and the broadcast task reads the mark.
            (pse.idle_timeout() + 2.s()).sleep().await;

            let now = precision::Clock::now(&Clock::default());
            assert!(
                pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "the peer is marked dead after its packet waited an idle timeout"
            );
            assert!(
                rehandshakes.borrow().is_empty(),
                "a dead mark on a server path doesn't ask for a handshake"
            );
        }
        .primary()
        .spawn();
    });
}

/// A send context that expires with no unacknowledged packet doesn't mark the peer dead, so it
/// doesn't ask for a background handshake either.
#[test]
fn send_idle_wheel_expiry_without_inflight_doesnt_request_a_rehandshake() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let rehandshakes = Rehandshakes::default();
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) =
            setup_send_recording(rehandshakes.clone());

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        assert!(!ctx.borrow().inflight.has_inflight());

        {
            let mut c = ctx.borrow_mut();
            let timeout = c.path_secret_entry.idle_timeout();
            c.idle_wheel.target_time = Some(precision::Clock::now(&clock) + timeout);
        }
        let _ = idle_wheel_tx.send(ctx);

        let send_caches = send_caches.clone();
        async move {
            (pse.idle_timeout() + 2.s()).sleep().await;

            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "the context expires after its idle timeout"
            );
            assert!(
                rehandshakes.borrow().is_empty(),
                "an expiry with no unacknowledged packet doesn't ask for a handshake"
            );
        }
        .primary()
        .spawn();
    });
}

/// A lost pair marks the peer dead at its idle expiry where the peer hasn't sent an
/// authenticated packet on any interface since the pair was marked lost, though the context
/// doesn't wait for an acknowledgement and the peer's other sender slots aren't marked lost.
#[test]
fn send_idle_wheel_marks_dead_a_peer_silent_since_its_pair_was_lost() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        assert!(!ctx.borrow().inflight.has_inflight());

        {
            let mut c = ctx.borrow_mut();
            let timeout = c.path_secret_entry.idle_timeout();
            c.idle_wheel.target_time = Some(precision::Clock::now(&clock) + timeout);
        }
        let _ = idle_wheel_tx.send(ctx.clone());

        let send_caches = send_caches.clone();
        let pse = pse.clone();
        async move {
            // At t=1s the pair is marked lost, after the peer's last packet at t=0.
            1.s().sleep().await;
            ctx.borrow_mut()
                .mark_path_lost(precision::Clock::now(&clock));
            assert!(
                pse.has_delivering_sender(),
                "the peer's other sender slots aren't marked lost"
            );

            // At t=31s the context has expired, and the peer stayed silent since the mark.
            30.s().sleep().await;
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
                "a peer silent since its pair was marked lost is marked dead"
            );
        }
        .primary()
        .spawn();
    });
}

/// A lost pair doesn't mark the peer dead at its idle expiry where the peer sent an
/// authenticated packet after the pair was marked lost, while no packet waits for an ack.
#[test]
fn send_idle_wheel_keeps_alive_a_peer_heard_from_since_its_pair_was_lost() {
    let _guard = crate::testing::without_snapshots();
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
            1.s().sleep().await;
            ctx.borrow_mut()
                .mark_path_lost(precision::Clock::now(&clock));

            // At t=2s the peer sends a packet on another interface.
            1.s().sleep().await;
            pse.touch_activity(precision::Clock::now(&clock));

            // At t=34s the context has expired: the peer's packet at t=2s moved its expiry
            // to t=32s.
            32.s().sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should expire after path activity + idle_timeout"
            );
            let now = precision::Clock::now(&Clock::default());
            assert!(
                !pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "a peer heard from since its pair was marked lost isn't marked dead"
            );
        }
        .primary()
        .spawn();
    });
}

/// A lost pair doesn't mark the peer dead at its idle expiry where the peer's last
/// authenticated packet came at the same instant as the pair's lost mark.
#[test]
fn send_idle_wheel_keeps_alive_a_peer_heard_from_at_the_instant_its_pair_was_lost() {
    let _guard = crate::testing::without_snapshots();
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
            // At t=1s the peer sends a packet, and the pair is marked lost at the same instant.
            1.s().sleep().await;
            let now = precision::Clock::now(&clock);
            pse.touch_activity(now);
            ctx.borrow_mut().mark_path_lost(now);
            assert_eq!(
                ctx.borrow().lost_at.map(|at| at.nanos),
                Some(pse.last_activity().nanos),
                "the peer's packet and the lost mark share one instant"
            );

            // At t=33s the context has expired: the peer's packet at t=1s moved its expiry
            // to t=31s.
            32.s().sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should expire after path activity + idle_timeout"
            );
            let now = precision::Clock::now(&Clock::default());
            assert!(
                !pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "a peer heard from at the instant its pair was marked lost isn't marked dead"
            );
        }
        .primary()
        .spawn();
    });
}

/// Two lost pairs of one peer whose contexts expire at the same wheel tick mark the peer
/// dead, though the first expiry's invalidation clears its pair's lost mark before the
/// second expiry runs.
#[test]
fn send_idle_wheel_marks_dead_a_peer_whose_lost_pairs_expire_at_the_same_tick() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) =
            setup_send_with_caches(2, Rehandshakes::default(), None);

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));

        let contexts: Vec<_> = (0..2)
            .map(|idx| {
                send_caches[LocalSendSocketId::new(idx)]
                    .borrow_mut()
                    .get_or_insert(&pse, &clock)
                    .unwrap()
            })
            .collect();
        let target = precision::Clock::now(&clock) + pse.idle_timeout();
        for ctx in &contexts {
            ctx.borrow_mut().idle_wheel.target_time = Some(target);
            let _ = idle_wheel_tx.send(ctx.clone());
        }

        let send_caches = send_caches.clone();
        let pse = pse.clone();
        async move {
            // At t=1s both pairs are marked lost, after the peer's last packet at t=0.
            1.s().sleep().await;
            for ctx in &contexts {
                ctx.borrow_mut()
                    .mark_path_lost(precision::Clock::now(&clock));
            }

            // At t=31s both contexts have expired at the same tick.
            30.s().sleep().await;
            for idx in 0..2 {
                assert_eq!(
                    send_caches[LocalSendSocketId::new(idx)]
                        .borrow()
                        .context_count(),
                    0,
                    "each lost context should be evicted after idle timeout"
                );
            }
            let now = precision::Clock::now(&Clock::default());
            assert!(
                pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "the lost pairs that expire at the same tick mark the peer dead"
            );
        }
        .primary()
        .spawn();
    });
}

/// A lost pair's idle expiry, with a packet the peer never acknowledged, doesn't mark the
/// peer dead while another pair of the peer delivers, and records the skipped dead mark as an
/// event and a count.
#[test]
fn send_idle_wheel_records_a_skipped_dead_mark() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let rehandshakes = Rehandshakes::default();
        let (map, subscriber) = test_map_with_subscriber();
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, registry) =
            setup_send_with_caches(1, rehandshakes.clone(), Some(map));

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));

        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        // The lost pair's packet waits for an acknowledgement until the idle expiry.
        {
            let mut ctx = ctx.borrow_mut();
            ctx.set_path_lost(true);
            insert_inflight_packet(&mut ctx, &pse, &clock);
            let timeout = ctx.path_secret_entry.idle_timeout();
            ctx.idle_wheel.target_time = Some(precision::Clock::now(&clock) + timeout);
        }
        assert!(
            pse.has_delivering_sender(),
            "the peer's other pairs still deliver"
        );
        let _ = idle_wheel_tx.send(ctx);

        async move {
            // Sleep past the idle timeout, so the wheel expires the context.
            (pse.idle_timeout() + 2.s()).sleep().await;

            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "the context expires after its idle timeout"
            );
            let now = precision::Clock::now(&Clock::default());
            assert!(
                !pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "a lost pair doesn't mark the peer dead while another pair delivers"
            );
            assert!(
                rehandshakes.borrow().is_empty(),
                "a skipped dead mark doesn't ask for a handshake"
            );
            assert_eq!(
                event_count(&subscriber.path_secret_map_dead_mark_skipped),
                1
            );

            let line = registry
                .take_current_metrics_line()
                .expect("metrics were recorded");
            assert!(
                line.contains("send.dead_mark_skipped=1 Variant|send.0"),
                "got: {line}"
            );
        }
        .primary()
        .spawn();
    });
}

#[test]
fn send_idle_wheel_silence_doesnt_renew_a_dead_mark() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();
        let cooldown = crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN;

        // At t=0 the peer's last packet arrives, and then the peer is marked dead.
        let pse = test_entry();
        let now = precision::Clock::now(&clock);
        pse.touch_activity(now);
        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        assert!(pse.mark_dead_if_cooldown_elapsed(now, cooldown));

        {
            let mut c = ctx.borrow_mut();
            let timeout = c.path_secret_entry.idle_timeout();
            // The context expires once the cooldown has passed since the dead mark.
            c.idle_wheel.target_time = Some(now + timeout + 5.s());
        }
        let _ = idle_wheel_tx.send(ctx.clone());

        let send_caches = send_caches.clone();
        let pse = pse.clone();
        async move {
            // At t=1s the pair is marked lost, after the peer's last packet at t=0.
            1.s().sleep().await;
            ctx.borrow_mut()
                .mark_path_lost(precision::Clock::now(&clock));

            // At t=36s the context has expired, and the peer stayed silent since the mark.
            35.s().sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                0,
                "context should be evicted after idle timeout"
            );
            let now = precision::Clock::now(&Clock::default());
            assert!(
                !pse.is_dead_during_cooldown(now, cooldown),
                "the expiry of a pair lost after the dead mark doesn't renew the mark"
            );
            assert!(pse.is_marked_dead(), "the mark stays until the peer is heard from");
        }
        .primary()
        .spawn();
    });
}

/// At a send context's idle expiry, a packet that waits for an acknowledgement on a pair that
/// isn't marked lost renews a dead mark the entry holds, once the dead-peer cooldown has passed
/// since the mark.
#[test]
fn send_idle_wheel_unacknowledged_packet_renews_a_dead_mark() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let (send_caches, mut idle_wheel_tx, _completed_rx, clock, _registry) = setup_send();
        let cooldown = crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN;

        // At t=0 the peer's last packet arrives, and then the peer is marked dead.
        let pse = test_entry();
        let now = precision::Clock::now(&clock);
        pse.touch_activity(now);
        let ctx = send_caches[LocalSendSocketId::new(0)]
            .borrow_mut()
            .get_or_insert(&pse, &clock)
            .unwrap();
        assert!(pse.mark_dead_if_cooldown_elapsed(now, cooldown));

        // The context's packet waits for an acknowledgement until the idle expiry, and its
        // pair isn't marked lost.
        {
            let mut ctx = ctx.borrow_mut();
            insert_inflight_packet(&mut ctx, &pse, &clock);
            let timeout = ctx.path_secret_entry.idle_timeout();
            // The context expires once the cooldown has passed since the dead mark.
            ctx.idle_wheel.target_time = Some(now + timeout + 5.s());
        }
        let _ = idle_wheel_tx.send(ctx);

        async move {
            // At t=33s the cooldown has passed since the dead mark, and the context hasn't
            // expired yet.
            33.s().sleep().await;
            assert_eq!(
                send_caches[LocalSendSocketId::new(0)]
                    .borrow()
                    .context_count(),
                1,
                "the context expires at t=35s"
            );
            let now = precision::Clock::now(&Clock::default());
            assert!(
                !pse.is_dead_during_cooldown(now, cooldown),
                "the cooldown has passed since the dead mark at t=0"
            );
            assert!(pse.is_marked_dead(), "the entry still holds its dead mark");

            // At t=36s the context has expired with its packet unacknowledged.
            3.s().sleep().await;
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
                "the expiry with an unacknowledged packet renews the dead mark"
            );
        }
        .primary()
        .spawn();
    });
}
