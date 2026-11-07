// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Unit tests for send-side queue accounting and PTO / `ProbeState` logic.

use super::{Context, PendingFrames, ProbeState, Pto, INITIAL_PTO_BACKOFF};
use crate::{
    byte_vec::ByteVec,
    counter::Registry,
    endpoint::{
        combinator::FrameBatch,
        frame::{Frame, Header, TransmissionStatus, DEFAULT_TTL},
        id::Id,
    },
    packet::datagram::QueuePair,
    path::secret::map::Entry as PathSecretEntry,
    socket::channel::ByteCost,
    time::testing::Clock,
};
use bytes::Bytes;
use core::time::Duration;
use s2n_quic_core::recovery::RttEstimator;
use std::sync::Arc;

fn make_clock(millis: u64) -> Clock {
    Clock::new(Duration::from_millis(millis))
}

fn make_rtt(smoothed_millis: u64) -> RttEstimator {
    RttEstimator::new(Duration::from_millis(smoothed_millis))
}

fn make_pending_frames() -> PendingFrames {
    let registry = Registry::default();
    PendingFrames::new(registry.register_queue_gauge("test.pending"))
}

fn make_path_secret_entry() -> Arc<PathSecretEntry> {
    PathSecretEntry::builder("127.0.0.1:9999".parse().unwrap()).build(None)
}

fn make_frame(payload_len: usize) -> crate::intrusive::Entry<Frame> {
    let mut payload = ByteVec::new();
    if payload_len > 0 {
        payload.push_back(Bytes::from(vec![0; payload_len]));
    }

    Frame {
        header: Header::QueueData {
            queue_pair: QueuePair {
                source_queue_id: s2n_quic_core::varint::VarInt::from_u8(1),
                dest_queue_id: s2n_quic_core::varint::VarInt::from_u8(2),
            },
            binding_id: s2n_quic_core::varint::VarInt::from_u8(3),
            offset: s2n_quic_core::varint::VarInt::ZERO,
            largest_offset: s2n_quic_core::varint::VarInt::ZERO,
            is_fin: false,
            blocked: false,
            dest_acceptor_id: None,
            priority: crate::credit::Priority::default(),
        },
        payload,
        path_secret_entry: make_path_secret_entry(),
        completion: None,
        status: TransmissionStatus::Pending,
        ttl: DEFAULT_TTL,
        enqueued_at: None,
        flow_credits: 0,
    }
    .into()
}

#[test]
fn pending_frames_tracks_byte_cost_through_push_pop_and_requeue() {
    let mut pending = make_pending_frames();
    let frame_a = make_frame(8);
    let cost_a = frame_a.byte_cost() as usize;
    let frame_b = make_frame(32);
    let cost_b = frame_b.byte_cost() as usize;

    pending.push_back(frame_a);
    pending.push_back(frame_b);
    assert_eq!(pending.len(), 2);
    assert_eq!(pending.byte_cost(), cost_a + cost_b);

    let popped = pending.pop_front().expect("frame should be present");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending.byte_cost(), cost_b);

    pending.push_front(popped);
    assert_eq!(pending.len(), 2);
    assert_eq!(pending.byte_cost(), cost_a + cost_b);

    let first = pending.pop_front().expect("first frame should be present");
    let second = pending.pop_front().expect("second frame should be present");
    assert_eq!(first.byte_cost() as usize, cost_a);
    assert_eq!(second.byte_cost() as usize, cost_b);
    assert!(pending.is_empty());
    assert_eq!(pending.byte_cost(), 0);
}

#[test]
fn pending_frames_append_queue_uses_supplied_batch_cost_exactly() {
    let mut pending = make_pending_frames();
    let frame_a = make_frame(5);
    let cost_a = frame_a.byte_cost();
    let frame_b = make_frame(11);
    let cost_b = frame_b.byte_cost();

    let mut queue = crate::intrusive::Queue::new();
    queue.push_back(frame_a);
    queue.push_back(frame_b);

    let batch_cost = cost_a + cost_b;
    pending.append_queue(queue, batch_cost);

    assert_eq!(pending.len(), 2);
    assert_eq!(pending.byte_cost(), batch_cost as usize);

    let drained_cost = pending
        .pop_front()
        .into_iter()
        .chain(pending.pop_front())
        .map(|frame| frame.byte_cost() as usize)
        .sum::<usize>();
    assert_eq!(drained_cost, batch_cost as usize);
    assert!(pending.is_empty());
    assert_eq!(pending.byte_cost(), 0);
}

// ── ProbeState transitions ────────────────────────────────────────────────────

#[test]
fn probe_state_initial_is_idle() {
    let state = ProbeState::default();
    assert!(!state.is_requested());
}

#[test]
fn probe_state_request_transitions_to_requested() {
    let mut state = ProbeState::default();
    state.request().unwrap();
    assert!(state.is_requested());
}

#[test]
fn probe_state_on_transmit_clears_request() {
    let mut state = ProbeState::default();
    state.request().unwrap();
    // ProbeTwice → ProbeOnce (still requested)
    state.on_transmit().unwrap();
    assert!(state.is_requested());
    assert!(state.is_probe_once());
    // ProbeOnce → Idle
    state.on_transmit().unwrap();
    assert!(!state.is_requested());
}

#[test]
fn probe_state_on_all_acked_clears_request() {
    let mut state = ProbeState::default();
    state.request().unwrap();
    state.on_all_acked().unwrap();
    assert!(!state.is_requested());
}

#[test]
fn probe_state_double_request_is_noop() {
    let mut state = ProbeState::default();
    state.request().unwrap();
    // Second request is a NoOp (already Requested) — should not panic
    let _ = state.request();
    assert!(state.is_requested());
}

#[test]
fn probe_state_on_transmit_from_idle_is_noop() {
    let mut state = ProbeState::default();
    // on_transmit from Idle is a NoOp — should not panic
    let _ = state.on_transmit();
    assert!(!state.is_requested());
}

// ── Pto::on_timeout backoff progression ──────────────────────────────────────

#[test]
fn pto_initial_state() {
    let pto = Pto::default();
    assert_eq!(pto.backoff, INITIAL_PTO_BACKOFF);
    assert_eq!(pto.firings_remaining, 0);
    assert!(!pto.needs_update);
    assert!(pto.arm_base.is_none());
    assert!(pto.last_sent_time.is_none());
}

#[test]
fn pto_first_timeout_fires_probe() {
    let mut pto = Pto::default();
    assert!(pto.on_timeout(), "first timeout should signal a probe");
    // After doubling: backoff = INITIAL*2 = 2, firings_remaining = 2-1 = 1
    assert_eq!(pto.backoff, 2);
    assert_eq!(pto.firings_remaining, 1);
}

#[test]
fn pto_second_timeout_is_countdown() {
    let mut pto = Pto::default();
    pto.on_timeout(); // fires probe, sets firings_remaining=1
    assert!(
        !pto.on_timeout(),
        "second timeout is a countdown firing, not a probe"
    );
    assert_eq!(pto.firings_remaining, 0);
}

#[test]
fn pto_third_timeout_fires_probe_again() {
    let mut pto = Pto::default();
    pto.on_timeout(); // 1st: probe, backoff→2, firings=1
    pto.on_timeout(); // 2nd: countdown, firings→0
    assert!(pto.on_timeout(), "3rd timeout should fire probe");
    // backoff → 4, firings_remaining → 3
    assert_eq!(pto.backoff, 4);
    assert_eq!(pto.firings_remaining, 3);
}

#[test]
fn pto_backoff_caps_at_max() {
    let mut pto = Pto::default();
    // Drive backoff to max by firing probes many times.
    for _ in 0..200 {
        pto.on_timeout();
        if pto.backoff == Pto::MAX_BACKOFF {
            break;
        }
    }
    assert_eq!(
        pto.backoff,
        Pto::MAX_BACKOFF,
        "backoff should reach MAX_BACKOFF"
    );

    // Confirm it stays at the cap
    for _ in 0..20 {
        if pto.on_timeout() {
            assert_eq!(
                pto.backoff,
                Pto::MAX_BACKOFF,
                "backoff should remain at MAX_BACKOFF"
            );
        }
    }
}

// ── Pto::needs_update path ────────────────────────────────────────────────────

#[test]
fn pto_on_packet_sent_sets_needs_update() {
    let clock = make_clock(100);
    let now = clock.get_time();
    let mut pto = Pto::default();
    pto.on_packet_sent(now);
    assert!(pto.needs_update, "on_packet_sent should set needs_update");
    assert!(pto.last_sent_time.is_some());
    assert!(pto.arm_base.is_none(), "on_packet_sent resets arm_base");
}

#[test]
fn pto_needs_update_suppresses_probe() {
    let clock = make_clock(100);
    let now = clock.get_time();
    let mut pto = Pto::default();
    pto.on_packet_sent(now);
    // on_timeout while needs_update is set should NOT fire a probe
    assert!(
        !pto.on_timeout(),
        "needs_update path should not fire a probe"
    );
    assert!(!pto.needs_update, "needs_update cleared after timeout");
    assert!(
        pto.arm_base.is_none(),
        "arm_base reset in needs_update path"
    );
}

#[test]
fn pto_needs_update_then_probe_fires() {
    let clock = make_clock(100);
    let now = clock.get_time();
    let mut pto = Pto::default();
    pto.on_packet_sent(now);
    pto.on_timeout(); // clears needs_update, no probe
    assert!(
        pto.on_timeout(),
        "after clearing needs_update, probe should fire"
    );
}

// ── Pto::on_ack_received resets state ────────────────────────────────────────

#[test]
fn pto_on_ack_resets_backoff() {
    let mut pto = Pto::default();
    pto.on_timeout(); // probe, backoff → 2
    pto.on_timeout(); // countdown
    pto.on_timeout(); // probe, backoff → 4
    assert_eq!(pto.backoff, 4);

    pto.on_ack_received(true);
    assert_eq!(pto.backoff, INITIAL_PTO_BACKOFF, "ACK should reset backoff");
    assert_eq!(pto.firings_remaining, 0, "ACK should reset countdown");
    assert!(pto.arm_base.is_none(), "ACK should reset arm_base");
}

#[test]
fn pto_on_ack_with_no_remaining_inflight_clears_probe_state() {
    let mut pto = Pto::default();
    pto.probe_state.request().unwrap();
    assert!(pto.probe_state.is_requested());

    pto.on_ack_received(false);
    assert!(
        !pto.probe_state.is_requested(),
        "probe_state should be cleared when all inflight ACKed"
    );
}

#[test]
fn pto_on_ack_with_remaining_inflight_keeps_probe_state() {
    let mut pto = Pto::default();
    pto.probe_state.request().unwrap();

    pto.on_ack_received(true);
    assert!(
        pto.probe_state.is_requested(),
        "probe_state should remain Requested when inflight remains"
    );
}

#[test]
fn pto_on_ack_with_remaining_inflight_sets_needs_update() {
    let mut pto = Pto::default();
    pto.on_ack_received(true);
    assert!(
        pto.needs_update,
        "needs_update should be set when inflight remains after ACK"
    );
}

#[test]
fn pto_on_ack_with_no_inflight_clears_needs_update() {
    let mut pto = Pto::default();
    pto.on_ack_received(false);
    assert!(
        !pto.needs_update,
        "needs_update should be cleared when all inflight ACKed"
    );
}

// ── Pto::is_armed ─────────────────────────────────────────────────────────────

#[test]
fn pto_not_armed_initially() {
    let pto = Pto::default();
    assert!(!pto.is_armed(), "fresh Pto should not be armed");
}

#[test]
fn pto_armed_after_packet_sent() {
    let clock = make_clock(100);
    let mut pto = Pto::default();
    pto.on_packet_sent(clock.get_time());
    assert!(pto.is_armed(), "Pto should be armed after packet sent");
}

#[test]
fn pto_armed_after_next_target_called() {
    let clock = make_clock(100);
    let rtt = make_rtt(10);
    let mut pto = Pto::default();
    pto.on_packet_sent(clock.get_time());
    let target = pto.next_target(&clock, &rtt);
    assert!(target.is_some(), "next_target should return a timestamp");
    assert!(pto.is_armed(), "Pto armed after next_target sets arm_base");
}

// ── Pto::next_target timing ───────────────────────────────────────────────────

#[test]
fn pto_next_target_advances_arm_base() {
    let clock = make_clock(100);
    let rtt = make_rtt(20);
    let mut pto = Pto::default();
    pto.on_packet_sent(clock.get_time());

    let t1 = pto.next_target(&clock, &rtt).unwrap();
    let t2 = pto.next_target(&clock, &rtt).unwrap();
    assert!(
        t2 > t1,
        "successive next_target calls should advance arm_base"
    );
}

#[test]
fn pto_on_packet_sent_resets_arm_base() {
    let clock = make_clock(100);
    let rtt = make_rtt(20);
    let mut pto = Pto::default();
    pto.on_packet_sent(clock.get_time());
    pto.next_target(&clock, &rtt); // sets arm_base

    clock.advance(Duration::from_millis(50));
    pto.on_packet_sent(clock.get_time());
    assert!(pto.arm_base.is_none(), "on_packet_sent resets arm_base");
    assert!(pto.needs_update);
}

#[test]
fn pto_backoff_sequence_matches_expected_probe_count() {
    // Verify the exact sequence of probes and countdowns.
    // Backoff starts at 1. After a probe, it doubles.
    // firings_remaining = new_backoff - 1 countdowns before next probe.
    //
    // Sequence (backoff 1→2→4→8):
    //   timeout 1: probe  (backoff→2, remaining→1)
    //   timeout 2: count  (remaining→0)
    //   timeout 3: probe  (backoff→4, remaining→3)
    //   timeouts 4,5,6: count
    //   timeout 7: probe  (backoff→8, remaining→7)
    let mut pto = Pto::default();
    let expected = [
        true, false, // backoff 2
        true, false, false, false, // backoff 4
        true, false, false, false, false, false, false, false, // backoff 8
    ];
    for (i, &should_probe) in expected.iter().enumerate() {
        let result = pto.on_timeout();
        assert_eq!(
            result,
            should_probe,
            "timeout #{}: expected probe={should_probe} but got {result}",
            i + 1
        );
    }
}

#[test]
fn on_pto_timeout_with_no_work_does_not_request_probe() {
    let registry = Registry::default();
    let (mut ctx, _) = make_context_with_sender_slots(1, &registry);
    let clock = make_clock(100);

    let interest = ctx.on_pto_timeout(&clock);
    assert!(
        !ctx.pto.probe_state.is_requested(),
        "probe state should remain idle when no inflight/pending work exists"
    );
    assert!(
        !interest.transmission,
        "no transmission should be scheduled without inflight/pending work"
    );
}

// ── publish_sender_load_score tests ──────────────────────────────────────────

/// Build a `Context` backed by an entry that has `sender_count` pre-allocated sender slots.
///
/// `peer_data_addrs` is populated so that `Context::new` can resolve the destination address.
fn make_context_with_sender_slots(
    sender_count: usize,
    registry: &Registry,
) -> (Context, Arc<PathSecretEntry>) {
    let peer: std::net::SocketAddr = "127.0.0.1:9999".parse().unwrap();
    let entry = PathSecretEntry::builder(peer)
        .socket_sender_count(sender_count)
        .build(None);
    let ctx = Context::new(
        &entry,
        registry.register_queue_gauge("test.inflight"),
        registry.register_queue_gauge("test.ack"),
        registry.register_queue_gauge("test.pending"),
        crate::endpoint::id::LocalSenderId::from_index(0),
        &crate::time::bach::Clock::default(),
    )
    .expect("Context::new should succeed with peer_data_addrs populated");
    (ctx, entry)
}

/// `push_batch` must call `publish_sender_load_score` so that pick-two sees an up-to-date
/// backlog immediately after frames are enqueued — not only after the next send or ACK.
///
/// The test verifies that the score includes the enqueued frame's drain delay, not merely
/// that a timestamp was written.  It does so by comparing against an empty-queue baseline
/// published at the same instant: if the drain delay were missing, the two scores would
/// be equal.
#[test]
fn push_batch_immediately_refreshes_sender_load_score() {
    let registry = Registry::default();
    let (mut ctx, entry) = make_context_with_sender_slots(1, &registry);
    let clock = make_clock(1000);
    let now: s2n_quic_core::time::Timestamp = clock.get_time().into();

    // Establish a baseline: score with no queued frames at this instant.
    ctx.publish_sender_load_score(now);
    let score_empty_queue =
        entry.sender_load_score(crate::endpoint::id::LocalSenderId::from_index(0));

    // Enqueue a frame with a non-trivial payload.
    let frame = make_frame(512);
    let batch = FrameBatch::single(frame);
    let _ = ctx.push_batch(batch, &clock);

    // push_batch must have refreshed the score and the new score must be strictly
    // higher than the empty-queue baseline — the difference is the drain delay for
    // the enqueued bytes.
    let score_with_frame =
        entry.sender_load_score(crate::endpoint::id::LocalSenderId::from_index(0));
    assert!(
        score_with_frame > score_empty_queue,
        "push_batch should include the enqueued frame's drain delay in the score; \
         empty_queue={score_empty_queue}, with_frame={score_with_frame}"
    );
}

/// A sender whose CCA is cwnd-limited (bytes_in_flight ≥ cwnd) should receive a congestion
/// penalty of one smoothed RTT, making it look more loaded than an uncongested sender with
/// the same queued bytes and the same wall-clock time.
#[test]
fn cwnd_limited_adds_rtt_penalty_to_load_score() {
    let registry = Registry::default();
    let (mut ctx_congested, entry_congested) = make_context_with_sender_slots(1, &registry);
    let (ctx_idle, entry_idle) = make_context_with_sender_slots(1, &registry);

    let clock = make_clock(1000);
    let now: s2n_quic_core::time::Timestamp = clock.get_time().into();

    // Fill up ctx_congested's inflight past cwnd to trigger is_congestion_limited().
    // Send two packets that each exceed half the current cwnd.
    let rtt_clone = ctx_congested.rtt_estimator;
    let cwnd = ctx_congested.cca.congestion_window();
    let pkt_size = ((cwnd / 2).saturating_add(1)).clamp(1, u16::MAX as u32) as u16;
    let _ = ctx_congested
        .cca
        .on_packet_sent(now, pkt_size, true, &rtt_clone);
    let _ = ctx_congested
        .cca
        .on_packet_sent(now, pkt_size, true, &rtt_clone);

    assert!(
        ctx_congested.cca.is_congestion_limited(),
        "ctx_congested should be cwnd-limited after filling inflight beyond cwnd={cwnd}"
    );

    // Publish with identical 'now' and zero queued bytes so the only difference is the penalty.
    ctx_congested.publish_sender_load_score(now);
    ctx_idle.publish_sender_load_score(now);

    let score_congested =
        entry_congested.sender_load_score(crate::endpoint::id::LocalSenderId::from_index(0));
    let score_idle =
        entry_idle.sender_load_score(crate::endpoint::id::LocalSenderId::from_index(0));

    assert!(
        score_congested > score_idle,
        "cwnd-limited sender (score={score_congested}) should have a higher load score \
         than an uncongested sender (score={score_idle})"
    );

    // The gap must be at least one full smoothed RTT (the congestion penalty is exactly
    // one smoothed RTT).  Timestamps are microsecond-granular so allow up to 1 µs of
    // rounding error in the stored score.
    let srtt_ns = ctx_congested.rtt_estimator.smoothed_rtt().as_nanos() as u64;
    let delta = score_congested - score_idle;
    assert!(
        delta + 1000 >= srtt_ns,
        "congestion penalty delta ({delta} ns) should be ≈ one full smoothed_rtt ({srtt_ns} ns)"
    );
}

/// When BBR's pacing gate is active (`earliest_departure_time` > `now`), the load score should
/// use EDT as its base rather than `now`, so pacing-gated senders appear more loaded than idle
/// senders at the same wall-clock time.
///
/// BBR pacing works in bursts: it sets EDT=now on the first send of a new burst (initialising
/// the departure time) and advances EDT by `send_quantum / pacing_rate` on each subsequent
/// burst.  Two back-to-back sends therefore leave EDT strictly in the future.
#[test]
fn edt_floor_raises_score_when_pacing_gated() {
    let registry = Registry::default();
    let (mut ctx_paced, entry_paced) = make_context_with_sender_slots(1, &registry);
    let (ctx_idle, entry_idle) = make_context_with_sender_slots(1, &registry);

    let clock = make_clock(1000);
    let t0: s2n_quic_core::time::Timestamp = clock.get_time().into();

    // Two back-to-back sends of a full quantum push EDT strictly into the future:
    //   send #1 – BBR initialises next_packet_departure_time = t0 + INITIAL_INTERVAL (= t0)
    //   send #2 – BBR advances EDT = max(EDT, t0) + interval = t0 + interval > t0
    let rtt_clone = ctx_paced.rtt_estimator;
    let quantum = ctx_paced.cca.send_quantum().min(u16::MAX as usize) as u16;
    let _ = ctx_paced.cca.on_packet_sent(t0, quantum, true, &rtt_clone);
    let _ = ctx_paced.cca.on_packet_sent(t0, quantum, true, &rtt_clone);

    let edt = ctx_paced
        .cca
        .earliest_departure_time(&t0)
        .expect("BBR should set earliest_departure_time after two on_packet_sent calls");
    assert!(
        edt > t0,
        "EDT should be strictly after t0; two sends should have advanced the pacing window"
    );

    // Choose `now` that is after t0 but still strictly before EDT so the floor kicks in.
    // Use 1 µs — the minimum Timestamp increment — rather than 1 ns (which would be
    // rounded back to t0 due to Timestamp's microsecond granularity).
    let now_before_edt = t0 + Duration::from_micros(1);
    assert!(
        now_before_edt < edt,
        "test setup: now_before_edt must be < edt for the floor to apply"
    );

    // Publish scores from both contexts at the same instant.
    ctx_paced.publish_sender_load_score(now_before_edt);
    ctx_idle.publish_sender_load_score(now_before_edt);

    let score_paced =
        entry_paced.sender_load_score(crate::endpoint::id::LocalSenderId::from_index(0));
    let score_idle =
        entry_idle.sender_load_score(crate::endpoint::id::LocalSenderId::from_index(0));

    // The pacing-gated sender must not look cheaper than the idle sender.
    assert!(
        score_paced >= score_idle,
        "pacing-gated sender (score={score_paced}) should appear at least as loaded as \
         the idle sender (score={score_idle})"
    );

    // The paced score must be rooted at EDT, so it must be ≥ EDT expressed in nanoseconds.
    let edt_ns = unsafe { edt.as_duration().as_nanos() as u64 };
    assert!(
        score_paced >= edt_ns,
        "paced score ({score_paced}) should be ≥ EDT nanoseconds ({edt_ns})"
    );
}

// ── AckRttTracker tests ───────────────────────────────────────────────────────

use super::AckRttTracker;
use s2n_quic_core::varint::VarInt;

fn make_ts(millis: u64) -> s2n_quic_core::time::Timestamp {
    unsafe { s2n_quic_core::time::Timestamp::from_duration(Duration::from_millis(millis)) }
}

fn make_varint(n: u64) -> VarInt {
    VarInt::new(n).unwrap()
}

#[test]
fn ack_rtt_tracker_initially_not_pending() {
    let tracker = AckRttTracker::default();
    assert!(
        tracker.is_pending(),
        "fresh AckRttTracker starts suppressed (sampled=true) to avoid probing before first data send"
    );
    // After clear() (simulating data entering inflight), it becomes available.
    let mut tracker = tracker;
    tracker.clear();
    assert!(
        !tracker.is_pending(),
        "after clear(), tracker should allow probing"
    );
}

#[test]
fn ack_rtt_tracker_pending_after_on_sent() {
    let mut tracker = AckRttTracker::default();
    tracker.on_sent(make_varint(5), make_ts(100));
    assert!(
        tracker.is_pending(),
        "tracker should be pending after on_sent"
    );
}

#[test]
fn ack_rtt_tracker_clear_removes_pending() {
    let mut tracker = AckRttTracker::default();
    tracker.on_sent(make_varint(5), make_ts(100));
    tracker.clear();
    assert!(
        !tracker.is_pending(),
        "tracker should not be pending after clear"
    );
}

/// When only one ack-eliciting ACK-only packet has been sent (stable == latest),
/// ACKing it should return that packet's time_sent and set sampled=true.
#[test]
fn ack_rtt_tracker_single_send_acked() {
    let mut tracker = AckRttTracker::default();
    let sent_time = make_ts(100);
    tracker.on_sent(make_varint(5), sent_time);

    // ACK range [3, 7] covers PN 5.
    let result = tracker.check_range(make_varint(3), make_varint(7));
    assert_eq!(
        result,
        Some(sent_time),
        "should return time_sent when PN covered"
    );
    // sampled=true → is_pending()=true (cooldown prevents re-probe)
    assert!(
        tracker.is_pending(),
        "tracker is still pending (sampled=true) to prevent ACK loop"
    );
    // clear() resets sampled so a new probe can be started after data flows
    tracker.clear();
    assert!(!tracker.is_pending(), "tracker cleared after clear()");
}

/// Latest (fresher) sample should be preferred when both stable and latest are set
/// and the peer ACKs the latest PN.
#[test]
fn ack_rtt_tracker_latest_preferred_over_stable() {
    let mut tracker = AckRttTracker::default();
    let t1 = make_ts(100);
    let t5 = make_ts(105);
    // First send (ack-eliciting) establishes stable; second send updates latest.
    tracker.on_sent(make_varint(1), t1);
    tracker.on_sent(make_varint(5), t5);

    // ACK covers PN 5 (latest) but not PN 1 (stable).
    let result = tracker.check_range(make_varint(5), make_varint(5));
    assert_eq!(result, Some(t5), "latest time_sent should be returned");
    // sampled=true → is_pending()=true (cooldown)
    assert!(
        tracker.is_pending(),
        "sampled=true after consuming latest — re-probe suppressed"
    );
}

/// When latest is lost but stable is ACKed, stable's time_sent is returned and
/// stable is advanced to the value from latest. Loss of latest is handled in on_ack_done.
#[test]
fn ack_rtt_tracker_stable_fallback_when_latest_lost() {
    let mut tracker = AckRttTracker::default();
    let t1 = make_ts(100);
    let t5 = make_ts(105);
    tracker.on_sent(make_varint(1), t1); // stable = (1, t1)
    tracker.on_sent(make_varint(5), t5); // latest = (5, t5)

    // ACK covers PN 1 (stable) but not PN 5 (latest).
    // check_range: stable ACKed → advance stable = latest.take() = (5,t5), sampled=true.
    let result = tracker.check_range(make_varint(1), make_varint(1));
    assert_eq!(result, Some(t1), "stable fallback time_sent returned");
    // stable advanced to (5,t5) (the value latest held), sampled=true → is_pending()=true
    assert!(
        tracker.is_pending(),
        "still pending (sampled + advanced stable)"
    );

    // After all ranges: on_ack_done(6) declares pn=5 lost (6 > 5).
    tracker.on_ack_done(make_varint(6));
    // stable cleared (was 5, lost); sampled still true → is_pending()=true
    assert!(
        tracker.is_pending(),
        "sampled=true keeps pending even after stable is lost"
    );
}

/// When only stable is set and the peer ACKs it, sampled is set.
#[test]
fn ack_rtt_tracker_single_send_stable_acked() {
    let mut tracker = AckRttTracker::default();
    let sent_time = make_ts(200);
    tracker.on_sent(make_varint(10), sent_time);

    let result = tracker.check_range(make_varint(10), make_varint(10));
    assert_eq!(result, Some(sent_time));
    // sampled=true → is_pending()=true
    assert!(tracker.is_pending(), "sampled=true after consuming sample");
}

#[test]
fn ack_rtt_tracker_check_range_no_match_does_not_clear_when_larger_not_acked() {
    let mut tracker = AckRttTracker::default();
    tracker.on_sent(make_varint(10), make_ts(100));

    // ACK range [1, 5] does not cover PN 10; largest_acknowledged=5 < 10.
    let result = tracker.check_range(make_varint(1), make_varint(5));
    assert!(result.is_none(), "no match expected");
    tracker.on_ack_done(make_varint(5));
    assert!(
        tracker.is_pending(),
        "tracker should remain pending when largest_acked < stable_pn"
    );
}

/// Both stable and latest are declared lost via on_ack_done when the peer
/// acknowledges a PN strictly larger than both without covering either.
#[test]
fn ack_rtt_tracker_both_cleared_when_both_lost() {
    let mut tracker = AckRttTracker::default();
    tracker.on_sent(make_varint(3), make_ts(100)); // stable = (3,_)
    tracker.on_sent(make_varint(7), make_ts(105)); // latest = (7,_)

    // ACK range [10, 15] — neither pn=3 nor pn=7 is covered.
    let result = tracker.check_range(make_varint(10), make_varint(15));
    assert!(result.is_none(), "no RTT sample from lost packets");
    // largest=15 > 7 > 3 → on_ack_done declares both lost.
    tracker.on_ack_done(make_varint(15));
    assert!(
        !tracker.is_pending(),
        "both slots cleared by loss detection; sampled NOT set (packets were lost)"
    );
}

#[test]
fn ack_rtt_tracker_returns_none_when_not_pending() {
    let mut tracker = AckRttTracker::default();
    // No pending sample → check_range is a no-op returning None.
    let result = tracker.check_range(make_varint(0), make_varint(100));
    assert!(result.is_none());
}

/// After a sample is consumed (sampled=true), the tracker remains pending until
/// clear() is called. This prevents an ACK loop: the assembler won't make further
/// ACK-only packets ack-eliciting until new data flows through the inflight map.
#[test]
fn ack_rtt_tracker_sampled_prevents_reprobe_until_clear() {
    let mut tracker = AckRttTracker::default();
    tracker.on_sent(make_varint(1), make_ts(100));

    // Consume the sample.
    let _ = tracker.check_range(make_varint(1), make_varint(1));
    assert!(
        tracker.is_pending(),
        "sampled=true → is_pending()=true → assembler will not re-probe"
    );

    // clear() represents data entering the inflight map, which resets the tracker.
    tracker.clear();
    assert!(
        !tracker.is_pending(),
        "after clear(), tracker is ready to probe again"
    );
}

/// on_non_eliciting_sent updates `latest` while a probe is in-flight, giving
/// a fresher sample if the peer's ACK range covers the new PN.
#[test]
fn ack_rtt_tracker_on_non_eliciting_sent_updates_latest() {
    let mut tracker = AckRttTracker::default();
    let t1 = make_ts(100);
    let t2 = make_ts(110);
    tracker.on_sent(make_varint(1), t1); // ack-eliciting: stable=(1,t1), latest=(1,t1)

    // Non-ack-eliciting send while probe is in-flight.
    tracker.on_non_eliciting_sent(make_varint(2), t2); // latest=(2,t2), stable unchanged

    // Peer's ACK range covers PN 2 (the non-eliciting send).
    let result = tracker.check_range(make_varint(2), make_varint(2));
    assert_eq!(result, Some(t2), "fresher sample from non-eliciting send");
}

/// on_non_eliciting_sent is a no-op when no probe is in-flight.
#[test]
fn ack_rtt_tracker_on_non_eliciting_sent_noop_when_no_probe() {
    let mut tracker = AckRttTracker::default();
    tracker.clear(); // simulate data cycle making tracker available
                     // No probe in-flight (stable=None).
    tracker.on_non_eliciting_sent(make_varint(5), make_ts(100));
    assert!(!tracker.is_pending(), "no-op when stable=None");
}

/// Loss detection in on_ack_done does NOT set sampled — the probe was lost so
/// we should be free to probe again without waiting for clear().
#[test]
fn ack_rtt_tracker_loss_does_not_set_sampled() {
    let mut tracker = AckRttTracker::default();
    tracker.on_sent(make_varint(3), make_ts(100)); // stable=(3,_), latest=(3,_)

    // Peer ACKs [10,15] — pn=3 not covered; largest=15 > 3 → declared lost.
    let result = tracker.check_range(make_varint(10), make_varint(15));
    assert!(result.is_none());
    tracker.on_ack_done(make_varint(15));
    assert!(
        !tracker.is_pending(),
        "after loss, not pending — re-probe is allowed"
    );
}

/// `on_ack_done` is called after all ranges, so a tracked PN in a smaller range
/// is not spuriously declared lost when a larger range is processed first.
#[test]
fn ack_rtt_tracker_multi_range_ack_largest_first() {
    let mut tracker = AckRttTracker::default();
    let sent_time = make_ts(100);
    tracker.on_sent(make_varint(3), sent_time); // stable=(3,_)

    // Simulate ACK frame with two ranges delivered largest-first:
    //   range [10,15] — does not cover pn=3
    //   range [1,5]   — covers pn=3
    // check_range does not perform loss detection, so the first range does not
    // clear the tracker. The second range finds pn=3 and returns the sample.
    // on_ack_done sees largest=15 > 3, but stable has already been consumed so
    // it has nothing left to clear.
    let r1 = tracker.check_range(make_varint(10), make_varint(15)); // no match
    assert!(r1.is_none());

    let r2 = tracker.check_range(make_varint(1), make_varint(5)); // covers pn=3
    assert_eq!(
        r2,
        Some(sent_time),
        "second range should still yield sample"
    );

    tracker.on_ack_done(make_varint(15));
    // sampled=true after consuming in r2
    assert!(tracker.is_pending(), "sampled=true after successful probe");
}

// ── Assembler-path scenarios ──────────────────────────────────────────────────
//
// The following tests simulate the exact sequence of AckRttTracker API calls
// that the assembler makes in specific scenarios, verifying that the tracker
// produces correct RTT samples in each case.

/// PTO fires while an RTT probe is already in-flight.
///
/// Scenario (mirrors the assembler's fixed code path):
///   - `on_sent` was called for the first ack-eliciting probe (stable + latest set).
///   - A subsequent ACK-only send is triggered by PTO while `make_ack_eliciting=false`
///     (sampled not yet set, stable is still in-flight).
///   - The assembler calls `on_non_eliciting_sent` to advance `latest` to the PTO PN.
///   - When the peer ACKs the PTO PN, `check_range` returns the fresher timestamp.
///
/// Before the fix the assembler entered the `if is_ack_eliciting` code path and
/// called neither `on_sent` nor `on_non_eliciting_sent`, leaving `latest` stale.
#[test]
fn ack_rtt_tracker_pto_during_inflight_probe_updates_latest() {
    let mut tracker = AckRttTracker::default();
    let t_probe = make_ts(100);
    let t_pto = make_ts(250);

    // First ack-eliciting probe (make_ack_eliciting=true).
    tracker.on_sent(make_varint(1), t_probe);
    assert!(tracker.is_pending(), "probe in-flight");

    // PTO fires; assembler sends ACK-only ack-eliciting packet with PN 5.
    // make_ack_eliciting=false here, so on_non_eliciting_sent is called.
    tracker.on_non_eliciting_sent(make_varint(5), t_pto);

    // Peer ACKs PN 5 (the PTO PN) — latest was advanced, so we get t_pto.
    let result = tracker.check_range(make_varint(5), make_varint(5));
    assert_eq!(
        result,
        Some(t_pto),
        "PTO PN acknowledged: should return the fresher t_pto timestamp"
    );
}

/// PTO fires while `sampled=true` (no probe in-flight).
///
/// After an RTT sample is consumed, `sampled=true` keeps `is_pending()=true`
/// to prevent ACK loops.  If PTO fires during this cooldown, the assembler
/// calls `on_non_eliciting_sent` — but since `stable=None`, it is a no-op.
/// The sampled state is preserved and no spurious new cycle is started.
#[test]
fn ack_rtt_tracker_pto_after_sample_consumed_is_noop() {
    let mut tracker = AckRttTracker::default();
    let t_probe = make_ts(100);
    let t_pto = make_ts(300);

    // Probe sent and acknowledged — sample consumed, sampled=true.
    tracker.on_sent(make_varint(1), t_probe);
    let _ = tracker.check_range(make_varint(1), make_varint(1));
    tracker.on_ack_done(make_varint(10));
    assert!(tracker.is_pending(), "sampled=true prevents re-probe");

    // PTO fires; assembler calls on_non_eliciting_sent.
    // stable=None so this should be a no-op.
    tracker.on_non_eliciting_sent(make_varint(5), t_pto);

    // Tracker should still be pending (sampled=true) and not track PN 5.
    assert!(tracker.is_pending(), "still sampled=true after PTO no-op");
    let result = tracker.check_range(make_varint(5), make_varint(5));
    assert!(
        result.is_none(),
        "PN 5 should not be tracked when stable=None"
    );
}

/// `make_ack_eliciting=true` AND PTO requested simultaneously.
///
/// The assembler sets `is_ack_eliciting` due to both `make_ack_eliciting` and
/// PTO.  The tracker should receive `on_sent` (not `on_non_eliciting_sent`)
/// because this is our own RTT probe.  After the fix, the `on_sent` call lives
/// inside the `if is_ack_eliciting` block and is reached in this case.
#[test]
fn ack_rtt_tracker_make_ack_eliciting_true_records_probe() {
    let mut tracker = AckRttTracker::default();
    let t_send = make_ts(100);

    // Assembler: make_ack_eliciting=true → calls on_sent.
    tracker.on_sent(make_varint(7), t_send);
    assert!(tracker.is_pending(), "probe recorded after on_sent");

    // Peer acknowledges PN 7.
    let result = tracker.check_range(make_varint(7), make_varint(7));
    assert_eq!(
        result,
        Some(t_send),
        "RTT sample should be returned when probe PN is acknowledged"
    );
    tracker.on_ack_done(make_varint(7));
    assert!(
        tracker.is_pending(),
        "sampled=true keeps is_pending after consumption"
    );
}

/// Collects the frames a context moves.
#[derive(Default)]
struct CollectFrames(Vec<crate::intrusive::Entry<Frame>>);

impl crate::socket::channel::UnboundedSender<crate::intrusive::Entry<Frame>> for CollectFrames {
    fn send(
        &mut self,
        value: crate::intrusive::Entry<Frame>,
    ) -> Result<(), crate::intrusive::Entry<Frame>> {
        self.0.push(value);
        Ok(())
    }
}

/// A lost mark records the next packet number, and only an acknowledgement of that packet or
/// a later one clears it, on the context and on the path secret entry alike.
#[test]
fn a_lost_mark_clears_on_an_ack_of_a_packet_sent_after_it() {
    let registry = Registry::default();
    let (mut ctx, entry) = make_context_with_sender_slots(2, &registry);
    let sender = crate::endpoint::id::LocalSenderId::from_index(0);
    ctx.next_packet_number = VarInt::from_u8(5);

    ctx.set_path_lost(true);
    assert_eq!(ctx.lost_from, Some(VarInt::from_u8(5)));
    assert!(entry.is_sender_lost(sender));

    ctx.on_largest_acknowledged(VarInt::from_u8(4));
    assert!(
        ctx.is_path_lost(),
        "an ack of a packet sent before the mark keeps it"
    );
    assert!(entry.is_sender_lost(sender));

    ctx.on_largest_acknowledged(VarInt::from_u8(5));
    assert!(
        !ctx.is_path_lost(),
        "an ack of the first packet sent after the mark clears it"
    );
    assert!(!entry.is_sender_lost(sender));
}

/// A lost pair moves its pending frames, marked pending, while another sender of the path
/// secret entry delivers.
#[test]
fn a_lost_pair_moves_its_pending_frames_while_another_sender_delivers() {
    let registry = Registry::default();
    let (mut ctx, _entry) = make_context_with_sender_slots(2, &registry);
    let clock = make_clock(1000);
    for _ in 0..2 {
        let _ = ctx.push_batch(FrameBatch::single(make_frame(64)), &clock);
    }
    assert!(
        !ctx.should_move_frames(),
        "a pair that delivers keeps its frames"
    );

    ctx.set_path_lost(true);
    assert!(ctx.should_move_frames());

    let mut moved = CollectFrames::default();
    assert_eq!(ctx.move_frames(&mut moved), 2);
    assert_eq!(moved.0.len(), 2);
    for frame in &moved.0 {
        assert!(matches!(frame.status, TransmissionStatus::Pending));
    }
    assert!(!ctx.has_pending_data());
    assert!(!ctx.should_move_frames(), "nothing is left to move");
}

/// A lost pair keeps its frames where no other sender of the path secret entry delivers.
#[test]
fn a_lost_pair_keeps_its_frames_where_no_other_sender_delivers() {
    let registry = Registry::default();
    let (mut ctx, _entry) = make_context_with_sender_slots(1, &registry);
    let clock = make_clock(1000);
    let _ = ctx.push_batch(FrameBatch::single(make_frame(64)), &clock);

    ctx.set_path_lost(true);
    assert!(!ctx.should_move_frames());
    assert!(ctx.has_pending_data());
}

/// A lost pair keeps its probe timeout armed with nothing to acknowledge, and the timeout
/// asks for a probe, which the assembler sends as a Ping.
#[test]
fn a_lost_pair_keeps_probing_with_nothing_to_acknowledge() {
    let registry = Registry::default();
    let (mut ctx, _entry) = make_context_with_sender_slots(2, &registry);
    let clock = make_clock(100);

    assert!(
        !ctx.wheel_interest(&clock).pto,
        "a pair that delivers has nothing to probe"
    );

    ctx.set_path_lost(true);
    assert!(
        ctx.wheel_interest(&clock).pto,
        "a lost pair arms its probe timeout"
    );

    let _ = ctx.on_pto_timeout(&clock);
    assert!(
        ctx.pto.probe_state.is_requested(),
        "the timeout of a lost pair asks for a probe with nothing to acknowledge"
    );
}

/// A pair with nothing to acknowledge isn't marked lost, however long ago the peer last
/// acknowledged a packet on it.
#[test]
fn a_pair_with_nothing_to_acknowledge_is_not_marked_lost() {
    let registry = Registry::default();
    let (mut ctx, _entry) = make_context_with_sender_slots(2, &registry);
    let clock = make_clock(100);
    let _ = ctx.wheel_interest(&clock);
    clock.advance(Duration::from_secs(60));

    assert!(!ctx.has_stopped_delivering(clock.get_time()));
    let _ = ctx.on_pto_timeout(&clock);
    assert!(!ctx.is_path_lost());
}

/// A new context of a sender slot clears the lost mark that an earlier context of the slot
/// left on the path secret entry.
#[test]
fn a_new_context_clears_the_lost_mark_of_its_sender_slot() {
    let registry = Registry::default();
    let (mut ctx, entry) = make_context_with_sender_slots(2, &registry);
    let sender = crate::endpoint::id::LocalSenderId::from_index(0);
    ctx.set_path_lost(true);
    assert!(entry.is_sender_lost(sender));

    let replacement = Context::new(
        &entry,
        registry.register_queue_gauge("test.inflight"),
        registry.register_queue_gauge("test.ack"),
        registry.register_queue_gauge("test.pending"),
        sender,
        &crate::time::bach::Clock::default(),
    )
    .expect("Context::new should succeed with peer_data_addrs populated");
    assert!(!replacement.is_path_lost());
    assert!(!entry.is_sender_lost(sender));
}

/// Builds a send cache of sender slot 0 holding one context, marked lost, for a path secret
/// entry with two sender slots.
fn make_cache_with_a_lost_context() -> (super::Cache, Arc<PathSecretEntry>) {
    let registry = Registry::default();
    let sender = crate::endpoint::id::LocalSenderId::from_index(0);
    let entry = PathSecretEntry::builder("127.0.0.1:9999".parse().unwrap())
        .socket_sender_count(2)
        .build(None);
    let mut cache = super::Cache::new(&registry, sender);
    let ctx = cache
        .get_or_insert(&entry, &crate::time::bach::Clock::default())
        .expect("Context::new should succeed with peer_data_addrs populated");
    ctx.borrow_mut().set_path_lost(true);
    assert!(entry.is_sender_lost(sender));
    (cache, entry)
}

/// A cache that drops a lost pair's context clears the pair's lost mark on the path secret
/// entry, since none of the dropped context's probes can clear it.
#[test]
fn an_invalidated_context_clears_its_lost_mark() {
    let (mut cache, entry) = make_cache_with_a_lost_context();
    let reason = crate::endpoint::frame::FailureReason::PeerDead;
    let mut cancelled = CollectFrames::default();
    let removed = cache.invalidate(entry.id(), reason, &mut cancelled);
    assert!(removed.is_some());
    assert!(!entry.is_sender_lost(crate::endpoint::id::LocalSenderId::from_index(0)));
}

/// A cache that drops a lost pair's context for a rejected key clears the pair's lost mark
/// on the path secret entry too.
#[test]
fn a_context_dropped_for_a_rejected_key_clears_its_lost_mark() {
    let (mut cache, entry) = make_cache_with_a_lost_context();
    let sender = crate::endpoint::id::LocalSenderId::from_index(0);
    let mut retransmit = CollectFrames::default();
    let removed = cache.invalidate_stale_key(entry.id(), sender, VarInt::MAX, &mut retransmit);
    assert!(removed.is_some());
    assert!(!entry.is_sender_lost(sender));
}

/// A lost mark records when it was set, and a delivering mark clears that time.
#[test]
fn a_lost_mark_records_when_it_was_set() {
    let registry = Registry::default();
    let (mut ctx, entry) = make_context_with_sender_slots(2, &registry);
    let sender = crate::endpoint::id::LocalSenderId::from_index(0);
    let clock = make_clock(100);
    let now = clock.get_time();
    assert!(ctx.lost_at.is_none());

    ctx.mark_path_lost(now);
    assert!(ctx.is_path_lost());
    assert!(entry.is_sender_lost(sender));
    assert_eq!(ctx.lost_at.map(|at| at.nanos), Some(now.nanos));

    ctx.set_path_lost(false);
    assert!(ctx.lost_at.is_none(), "a delivering mark clears the time");
    assert!(!entry.is_sender_lost(sender));
}

/// A lost pair's probe backoff stops at a cap, so that the pair is probed within one wheel
/// firing of each persistent congestion period, and grows up to that cap.
#[test]
fn pto_cap_lost_backoff_probes_about_once_a_persistent_congestion_period() {
    let rtt = make_rtt(10);
    let base_period = Pto::base_period(&rtt).as_nanos();
    let threshold = rtt.persistent_congestion_threshold().as_nanos();
    // The cap lies above the initial backoff and below the largest one, so the test reads
    // the cap and not one of those bounds.
    assert!(u128::from(INITIAL_PTO_BACKOFF) * base_period < threshold);
    assert!(u128::from(Pto::MAX_BACKOFF - 1) * base_period >= threshold);

    let mut pto = Pto::default();
    for _ in 0..200 {
        pto.on_timeout();
        pto.cap_lost_backoff(&rtt);
        assert!(
            u128::from(pto.backoff - 1) * base_period < threshold,
            "a lost pair is probed within one wheel firing of a persistent congestion period"
        );
        assert!(pto.firings_remaining < pto.backoff);
    }
    assert!(
        u128::from(pto.backoff) * base_period >= threshold,
        "the backoff grows up to the cap"
    );
}

/// A lost pair's probe timeout caps its probe backoff, so that the first probe after the pair
/// delivers again doesn't wait about as long as the outage lasted.
#[test]
fn a_lost_pair_caps_its_probe_backoff() {
    let registry = Registry::default();
    let (mut ctx, _entry) = make_context_with_sender_slots(2, &registry);
    let clock = make_clock(100);
    let base_period = Pto::base_period(&ctx.rtt_estimator).as_nanos();
    let threshold = ctx
        .rtt_estimator
        .persistent_congestion_threshold()
        .as_nanos();
    assert!(u128::from(Pto::MAX_BACKOFF - 1) * base_period >= threshold);

    ctx.set_path_lost(true);
    for _ in 0..200 {
        let _ = ctx.on_pto_timeout(&clock);
    }
    assert!(
        u128::from(ctx.pto.backoff - 1) * base_period < threshold,
        "a lost pair's backoff stays within one wheel firing of a persistent congestion period"
    );
}
