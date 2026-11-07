// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::{
    byte_vec::ByteVec,
    endpoint::{
        frame::{Header, TransmissionStatus, DEFAULT_TTL},
        id::{Id, LocalSendSocketId, LocalSenderId},
    },
    path::secret::map::Entry as PathSecretEntry,
    socket::channel::{intrusive::unsync, ByteCost},
    time::testing as test_clock_mod,
};
use bytes::{Bytes, BytesMut};
use core::task::Poll;
use s2n_quic_core::varint::VarInt;
use std::{
    cell::RefCell,
    collections::VecDeque,
    future::Future,
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

fn test_clock() -> test_clock_mod::Clock {
    test_clock_mod::Clock::new(std::time::Duration::from_secs(1))
}

fn test_completion_dispatcher<R>(rx: R) -> CompletionDispatcher<R>
where
    R: Receiver<Entry<Frame>>,
{
    let registry = crate::counter::Registry::default();
    let clock = crate::time::DefaultClock::default();
    let reader_metrics = Arc::new(crate::stream::metrics::ReaderMetrics::new(
        &registry,
        "test.reader",
    ));
    let writer_metrics = Arc::new(crate::stream::metrics::WriterMetrics::new(
        &registry,
        "test.writer",
    ));
    let send_credit_pool = crate::sync::Arc::new(crate::credit::Pool::new(
        crate::credit::Config::new(1_000_000),
    ));
    CompletionDispatcher::new(rx, clock, reader_metrics, writer_metrics, send_credit_pool)
}

struct TestItem {
    path_secret_entry: Arc<PathSecretEntry>,
    byte_cost: u64,
    flow_credits: u64,
    highest_priority: bool,
    drop_counter: Arc<AtomicUsize>,
}

impl Drop for TestItem {
    fn drop(&mut self) {
        self.drop_counter.fetch_add(1, Ordering::Relaxed);
    }
}

impl ByteCost for TestItem {
    fn byte_cost(&self) -> u64 {
        self.byte_cost
    }
}

impl PathSecretMapEntry for TestItem {
    fn path_secret_entry(&self) -> &Arc<PathSecretEntry> {
        &self.path_secret_entry
    }
}

impl AssignSender for TestItem {
    fn set_sender_id(&mut self, _id: crate::endpoint::id::LocalSenderId) {}
}

impl FlowCredits for TestItem {
    fn total_flow_credits(&self) -> u64 {
        self.flow_credits
    }
}

impl HighestPriority for TestItem {
    fn holds_highest_priority(&self) -> bool {
        self.highest_priority
    }
}

struct TestSender {
    accept: bool,
    calls: usize,
}

impl UnboundedSender<TestItem> for TestSender {
    fn send(&mut self, value: TestItem) -> Result<(), TestItem> {
        self.calls += 1;
        if self.accept {
            drop(value);
            Ok(())
        } else {
            Err(value)
        }
    }
}

struct TestReceiver<T> {
    values: VecDeque<T>,
    consumed: u64,
}

impl<T> Receiver<T> for TestReceiver<T> {
    fn poll_recv(&mut self, _cx: &mut task::Context<'_>, _budget: &mut Budget) -> Poll<Option<T>> {
        Poll::Ready(self.values.pop_front())
    }

    fn on_consumed(&mut self, bytes: u64) {
        self.consumed += bytes;
    }
}

struct BudgetAwareTestReceiver<T> {
    values: VecDeque<T>,
}

impl<T> Receiver<T> for BudgetAwareTestReceiver<T> {
    fn poll_recv(&mut self, _cx: &mut task::Context<'_>, budget: &mut Budget) -> Poll<Option<T>> {
        if budget.is_exhausted() {
            budget.set_needs_wake();
            return Poll::Pending;
        }

        match self.values.pop_front() {
            Some(value) => {
                budget.consume();
                Poll::Ready(Some(value))
            }
            None => Poll::Ready(None),
        }
    }

    fn on_consumed(&mut self, _bytes: u64) {}
}

fn test_path_secret_entry() -> Arc<PathSecretEntry> {
    let peer: std::net::SocketAddr = "127.0.0.1:4433".parse().unwrap();
    PathSecretEntry::builder(peer)
        .socket_sender_count(2)
        .build(None)
}

fn new_test_item(
    path_secret_entry: Arc<PathSecretEntry>,
    drop_counter: Arc<AtomicUsize>,
) -> TestItem {
    TestItem {
        path_secret_entry,
        byte_cost: 123,
        flow_credits: 0,
        highest_priority: false,
        drop_counter,
    }
}

fn new_test_frame(path_secret_entry: Arc<PathSecretEntry>, payload_len: usize) -> Entry<Frame> {
    new_test_frame_with_header(
        path_secret_entry,
        payload_len,
        Header::QueueControl {
            queue_pair: crate::packet::datagram::QueuePair {
                source_queue_id: VarInt::from_u8(0),
                dest_queue_id: VarInt::from_u8(1),
            },
            binding_id: VarInt::from_u8(0),
        },
    )
}

fn new_test_frame_with_header(
    path_secret_entry: Arc<PathSecretEntry>,
    payload_len: usize,
    header: Header,
) -> Entry<Frame> {
    let mut payload = ByteVec::new();
    if payload_len > 0 {
        payload.push_back(Bytes::from(vec![0u8; payload_len]));
    }

    Entry::new(Frame {
        header,
        payload,
        path_secret_entry,
        completion: None,
        status: TransmissionStatus::Pending,
        ttl: DEFAULT_TTL,
        enqueued_at: None,
        flow_credits: 0,
    })
}

fn drive_completion_dispatcher(
    dispatcher: &mut CompletionDispatcher<TestReceiver<Entry<Frame>>>,
    budget_capacity: usize,
) -> Poll<Option<crate::queue::AutoWake>> {
    with_noop_context(|cx| {
        let mut budget = Budget::new(budget_capacity);
        dispatcher.poll_recv(cx, &mut budget)
    })
}

fn with_noop_context<R>(f: impl FnOnce(&mut task::Context<'_>) -> R) -> R {
    let waker = s2n_quic_core::task::waker::noop();
    let mut cx = task::Context::from_waker(&waker);
    f(&mut cx)
}

// ── MappedSender tests ─────────────────────────────────────────────────────

#[derive(Debug)]
struct MappedItem {
    sender_id: LocalSenderId,
    value: usize,
}

impl HasId<LocalSenderId> for MappedItem {
    fn id(&self) -> LocalSenderId {
        self.sender_id
    }
}

struct MappedItemSender {
    sink: Rc<RefCell<Vec<usize>>>,
}

impl UnboundedSender<MappedItem> for MappedItemSender {
    fn send(&mut self, value: MappedItem) -> Result<(), MappedItem> {
        self.sink.borrow_mut().push(value.value);
        Ok(())
    }
}

#[test]
fn mapped_sender_routes_items_through_id_map() {
    let sink0 = Rc::new(RefCell::new(Vec::new()));
    let sink1 = Rc::new(RefCell::new(Vec::new()));

    let senders: crate::endpoint::id::IdMap<LocalSendSocketId, MappedItemSender> = [
        (
            LocalSendSocketId::new(0),
            MappedItemSender {
                sink: sink0.clone(),
            },
        ),
        (
            LocalSendSocketId::new(1),
            MappedItemSender {
                sink: sink1.clone(),
            },
        ),
    ]
    .into_iter()
    .collect();

    let mut sender_idx_to_local: crate::endpoint::id::IdMap<LocalSenderId, LocalSendSocketId> =
        crate::endpoint::id::IdMap::new(2, LocalSendSocketId::new(usize::MAX));
    sender_idx_to_local[LocalSenderId::from_index(0)] = LocalSendSocketId::new(1);
    sender_idx_to_local[LocalSenderId::from_index(1)] = LocalSendSocketId::new(0);

    let mut tx = MappedSender::new(senders, sender_idx_to_local);
    tx.send(MappedItem {
        sender_id: LocalSenderId::from_index(0),
        value: 10,
    })
    .unwrap();
    tx.send(MappedItem {
        sender_id: LocalSenderId::from_index(1),
        value: 20,
    })
    .unwrap();

    assert_eq!(&*sink0.borrow(), &[20]);
    assert_eq!(&*sink1.borrow(), &[10]);
}

// ── PickTwo tests ─────────────────────────────────────────────────────────

fn try_send_pick_two(
    value: TestItem,
    senders: &mut Vec<TestSender>,
    rng: &mut crate::xorshift::Rng,
) -> Result<(), TestItem> {
    try_send_pick_two_with_rr(value, senders, rng, &mut 0)
}

fn try_send_pick_two_with_rr(
    value: TestItem,
    senders: &mut Vec<TestSender>,
    rng: &mut crate::xorshift::Rng,
    round_robin_idx: &mut usize,
) -> Result<(), TestItem> {
    let registry = crate::counter::Registry::default();
    let pick_counters: Vec<_> = (0..senders.len())
        .map(|i| registry.register_nominal("pick_two.chosen", format_args!("send.{i}")))
        .collect();
    let rejected_counters: Vec<_> = (0..senders.len())
        .map(|i| {
            registry.register_nominal_summary(
                "pick_two.rejected",
                format_args!("send.{i}"),
                crate::counter::Unit::Microsecond,
            )
        })
        .collect();
    let score_delta =
        registry.register_summary("pick_two.score_delta", crate::counter::Unit::Microsecond);
    let pick_counters_map: crate::endpoint::id::IdMap<crate::endpoint::id::LocalSenderId, _> =
        pick_counters.into();
    let rejected_counters_map: crate::endpoint::id::IdMap<crate::endpoint::id::LocalSenderId, _> =
        rejected_counters.into();
    let mut senders_map: crate::endpoint::id::IdMap<crate::endpoint::id::LocalSenderId, _> =
        std::mem::take(senders).into();
    let result = PickTwo::<TestItem, TestReceiver<TestItem>, TestSender>::try_send_pick_two(
        value,
        &mut senders_map,
        rng,
        round_robin_idx,
        &pick_counters_map,
        &rejected_counters_map,
        &score_delta,
    );
    *senders = senders_map.into_iter().map(|(_, v)| v).collect();
    result
}

#[test]
fn selected_sender_receives_item() {
    let item = new_test_item(test_path_secret_entry(), Arc::new(AtomicUsize::new(0)));
    let mut senders = vec![
        TestSender {
            accept: true,
            calls: 0,
        },
        TestSender {
            accept: true,
            calls: 0,
        },
    ];
    let result = try_send_pick_two(item, &mut senders, &mut crate::xorshift::Rng::new());
    assert!(result.is_ok());
    assert_eq!(senders[0].calls + senders[1].calls, 1);
}

#[test]
fn sender_error_returns_value() {
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let item = new_test_item(test_path_secret_entry(), drop_counter.clone());
    let mut senders = vec![
        TestSender {
            accept: false,
            calls: 0,
        },
        TestSender {
            accept: false,
            calls: 0,
        },
    ];
    let result = try_send_pick_two(item, &mut senders, &mut crate::xorshift::Rng::new());
    assert!(result.is_err());
    assert_eq!(senders[0].calls + senders[1].calls, 1);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 0);

    drop(result);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 1);
}

/// Set sender `idx`'s published load score to exactly `score_nanos` by feeding a base timestamp
/// of that many nanoseconds with zero queued bytes (so the queue-drain term is zero and the stored
/// score equals the base).
///
/// `score_nanos` must be a whole number of microseconds: the core `Timestamp` stores microseconds
/// internally and truncates sub-microsecond nanos (see `score_as_u64`), so a value like `1_500` ns
/// would round down to `1_000` ns. Callers here pass millisecond-scale gaps, so this is exact.
fn set_load_score(entry: &Arc<PathSecretEntry>, idx: usize, score_nanos: u64) {
    debug_assert_eq!(
        score_nanos % 1_000,
        0,
        "score resolution is one microsecond"
    );
    let base = unsafe {
        s2n_quic_core::time::Timestamp::from_duration(core::time::Duration::from_nanos(score_nanos))
    };
    entry.update_sender_load_score(
        LocalSenderId::from_index(idx),
        base,
        0,
        s2n_quic_core::recovery::bandwidth::Bandwidth::new(
            1_000,
            core::time::Duration::from_millis(1),
        ),
    );
}

/// Run `iters` pick-two routings between two senders with the given load-score gap and return the
/// number of times the higher-scored (worse) sender was chosen. Sender 0 is the better candidate
/// (score 0); sender 1 is worse (score `gap_nanos`).
fn count_worse_picks(gap_nanos: u64, iters: usize, seed: u64) -> usize {
    let entry = test_path_secret_entry();
    set_load_score(&entry, 0, 0);
    set_load_score(&entry, 1, gap_nanos);

    let mut rng = crate::xorshift::Rng::with_seed(seed);
    let mut rr = 0;
    let mut worse = 0;
    for _ in 0..iters {
        let mut senders = vec![
            TestSender {
                accept: true,
                calls: 0,
            },
            TestSender {
                accept: true,
                calls: 0,
            },
        ];
        let item = new_test_item(entry.clone(), Arc::new(AtomicUsize::new(0)));
        assert!(try_send_pick_two_with_rr(item, &mut senders, &mut rng, &mut rr).is_ok());
        // Sender 1 is the worse (higher-score) candidate.
        worse += senders[1].calls;
    }
    worse
}

/// `PICK_TWO_FLOOR_LN_RATIO` is a hand-pinned literal because `f64::ln` is not const-stable. This
/// guards it against drifting out of sync with the floor — change the floor without updating the
/// literal and this fails instead of silently skewing the curve.
#[test]
fn pick_two_worse_probability_floor_ratio_literal() {
    let expected = ((1.0 - PICK_TWO_WORSE_FLOOR) / PICK_TWO_WORSE_FLOOR).ln();
    assert!(
        (PICK_TWO_FLOOR_LN_RATIO - expected).abs() < 1e-12,
        "PICK_TWO_FLOOR_LN_RATIO={PICK_TWO_FLOOR_LN_RATIO} is stale; expected ln((1-floor)/floor)={expected}"
    );
}

#[test]
fn pick_two_worse_probability_curve() {
    // Equal scores must be a fair coin flip (no structural bias toward idx1 or idx2).
    assert_eq!(pick_two_worse_probability(0), 0.5);

    // The curve is derived so the logistic meets the floor *exactly* at the worst-case gap — the
    // smooth decay and the clamp join with no knee.
    let worst_case = PICK_TWO_WORST_CASE_GAP_NANOS as u64;
    let p_worst = pick_two_worse_probability(worst_case);
    assert!(
        (p_worst - PICK_TWO_WORSE_FLOOR).abs() < 1e-6,
        "expected logistic to equal the floor {PICK_TWO_WORSE_FLOOR} at the worst-case gap, got {p_worst}"
    );

    // Intermediate gaps follow the derived logistic: ~9% at half the worst-case gap.
    let p_half = pick_two_worse_probability(worst_case / 2);
    assert!(
        (0.08..0.10).contains(&p_half),
        "expected ~0.09 at half the worst-case gap, got {p_half}"
    );

    // Probability is monotonically non-increasing as the gap widens.
    assert!(pick_two_worse_probability(2 * worst_case) <= p_worst);

    // A huge gap clamps to the floor rather than collapsing to zero — this is what prevents a
    // structurally-worse target from ever being fully starved.
    assert_eq!(pick_two_worse_probability(u64::MAX), PICK_TWO_WORSE_FLOOR);
}

/// Regression test for the starvation problem with the old deterministic `score1 <= score2` rule:
/// a sender that is consistently worse received *zero* traffic. With the probabilistic rule it must
/// keep getting a probe trickle at roughly the floor rate, never zero.
#[test]
fn consistently_worse_sender_is_not_starved() {
    const ITERS: usize = 20_000;
    // A gap far larger than the worst-case gap: the logistic term is effectively zero here, so any
    // traffic the worse sender receives comes from the floor. The old deterministic rule routed 0.
    let worse = count_worse_picks(
        1_000 * PICK_TWO_WORST_CASE_GAP_NANOS as u64,
        ITERS,
        0x5eed_1234,
    );

    let share = worse as f64 / ITERS as f64;
    assert!(
        worse > 0,
        "consistently-worse sender was fully starved (the bug this fix addresses)"
    );
    // Should hover around the floor (1%). Allow a statistical band around it.
    assert!(
        (0.005..0.015).contains(&share),
        "worse-sender share {share} should be near the {PICK_TWO_WORSE_FLOOR} floor"
    );
}

/// With equal load scores the two candidates must split traffic roughly evenly.
#[test]
fn equal_scores_split_evenly() {
    const ITERS: usize = 20_000;
    let worse = count_worse_picks(0, ITERS, 0xabcd_9876);
    let share = worse as f64 / ITERS as f64;
    assert!(
        (0.45..0.55).contains(&share),
        "equal-score split {share} should be ~0.5"
    );
}

#[test]
fn pick_two_drops_unsent_entry_on_shutdown() {
    const CAP: u64 = 1_000_000;
    const CREDIT: u64 = 4096;

    let drop_counter = Arc::new(AtomicUsize::new(0));
    // The dropped item carries borrowed send-pool credit; PickTwo must return it on the
    // undeliverable path or it leaks (Frame has no Drop that releases flow_credits).
    let pool = crate::sync::Arc::new(crate::credit::Pool::new(crate::credit::Config::new(CAP)));
    let mut item = new_test_item(test_path_secret_entry(), drop_counter.clone());
    item.flow_credits = CREDIT;
    let rx = TestReceiver {
        values: [item].into(),
        consumed: 0,
    };
    let senders = vec![
        TestSender {
            accept: false,
            calls: 0,
        },
        TestSender {
            accept: false,
            calls: 0,
        },
    ];
    let registry = crate::counter::Registry::default();
    let pick_two = PickTwo::new(
        rx,
        senders.into(),
        crate::xorshift::Rng::new(),
        &registry,
        pool.clone(),
    );
    let mut fut = core::pin::pin!(crate::socket::channel::ReceiverExt::drain_budgeted(
        pick_two, None
    ));
    let result = with_noop_context(|cx| fut.as_mut().poll(cx));
    assert_eq!(result, Poll::Ready(()));
    assert_eq!(drop_counter.load(Ordering::Relaxed), 1);
    // The batch's borrowed credit was released back to the pool when it was dropped. The pool
    // started full (CAP) and the item's `flow_credits` were fabricated (not drained from the
    // pool), so a release shows up as the balance rising to `CAP + CREDIT`. The point is that
    // a release happened at all — the undeliverable-drop path no longer leaks.
    assert_eq!(
        pool.debug_available() as u64 + pool.debug_returned(),
        CAP + CREDIT,
        "PickTwo did not release flow_credits on the undeliverable-batch drop path",
    );
}

/// Regression test: PickTwo must call `on_consumed` on the inner receiver after successfully
/// dispatching an item.  Before the fix, `on_consumed` was never called, so the `Paced`
/// combinator's token bucket was never consumed and pacing was completely bypassed.
#[test]
fn pick_two_propagates_on_consumed() {
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let item = new_test_item(test_path_secret_entry(), drop_counter);
    let expected_byte_cost = item.byte_cost();
    let rx = TestReceiver {
        values: VecDeque::from([item]),
        consumed: 0,
    };
    let senders = vec![
        TestSender {
            accept: true,
            calls: 0,
        },
        TestSender {
            accept: true,
            calls: 0,
        },
    ];
    let registry = crate::counter::Registry::default();
    let pool = crate::sync::Arc::new(crate::credit::Pool::new(crate::credit::Config::new(
        1_000_000,
    )));
    let mut pick_two = PickTwo::new(
        rx,
        senders.into(),
        crate::xorshift::Rng::new(),
        &registry,
        pool,
    );

    let result = with_noop_context(|cx| {
        let mut budget = Budget::new(usize::MAX);
        pick_two.poll_recv(cx, &mut budget)
    });
    assert_eq!(result, Poll::Ready(Some(())));

    // The inner receiver's on_consumed must be called with the item's byte_cost so that
    // upstream Paced combinators advance their token buckets.
    assert_eq!(
        pick_two.rx.consumed, expected_byte_cost,
        "PickTwo must propagate on_consumed(byte_cost) to inner receiver after dispatch"
    );
}

#[test]
fn completion_dispatcher_filters_non_failures_for_failure_only_subscriptions() {
    let path_secret_entry = test_path_secret_entry();
    let mut completion_rx = frame::failure_completion_channel();

    let mut frame = new_test_frame(path_secret_entry, 1).into_inner();
    frame.status = TransmissionStatus::Acknowledged;
    frame.completion = Some(completion_rx.sender());

    let rx = TestReceiver {
        values: [Entry::new(frame)].into(),
        consumed: 0,
    };
    let mut dispatcher = test_completion_dispatcher(rx);
    let _ = drive_completion_dispatcher(&mut dispatcher, usize::MAX);

    with_noop_context(|cx| {
        let result = completion_rx.poll_swap(cx);
        assert!(
            matches!(result, Poll::Pending),
            "acknowledged frame should be filtered before notification"
        );
    });
}

#[test]
fn completion_dispatcher_notifies_failures_for_failure_only_subscriptions() {
    let path_secret_entry = test_path_secret_entry();
    let mut completion_rx = frame::failure_completion_channel();

    let mut frame = new_test_frame(path_secret_entry, 1).into_inner();
    frame.status = TransmissionStatus::Failed(frame::FailureReason::TransmissionError);
    frame.completion = Some(completion_rx.sender());

    let rx = TestReceiver {
        values: [Entry::new(frame)].into(),
        consumed: 0,
    };
    let mut dispatcher = test_completion_dispatcher(rx);
    let _ = drive_completion_dispatcher(&mut dispatcher, usize::MAX);

    with_noop_context(|cx| match completion_rx.poll_swap(cx) {
        Poll::Ready(Some(queue)) => {
            assert_eq!(queue.len(), 1);
            assert!(matches!(
                queue.front().unwrap().status,
                TransmissionStatus::Failed(frame::FailureReason::TransmissionError)
            ));
        }
        other => panic!("expected failed completion notification, got {other:?}"),
    });
}

#[test]
fn completion_dispatcher_polls_past_filtered_frames_in_same_poll() {
    let path_secret_entry = test_path_secret_entry();
    let mut completion_rx = frame::failure_completion_channel();
    let sender = completion_rx.sender();

    let mut acknowledged = new_test_frame(path_secret_entry.clone(), 1).into_inner();
    acknowledged.status = TransmissionStatus::Acknowledged;
    acknowledged.completion = Some(sender.clone());

    let mut failed = new_test_frame(path_secret_entry, 1).into_inner();
    failed.status = TransmissionStatus::Failed(frame::FailureReason::TransmissionError);
    failed.completion = Some(sender);

    let rx = TestReceiver {
        values: [Entry::new(acknowledged), Entry::new(failed)].into(),
        consumed: 0,
    };
    let mut dispatcher = test_completion_dispatcher(rx);

    let _ = drive_completion_dispatcher(&mut dispatcher, usize::MAX);

    with_noop_context(|cx| match completion_rx.poll_swap(cx) {
        Poll::Ready(Some(queue)) => {
            assert_eq!(queue.len(), 1);
            assert!(matches!(
                queue.front().unwrap().status,
                TransmissionStatus::Failed(frame::FailureReason::TransmissionError)
            ));
        }
        other => panic!("expected failed completion notification, got {other:?}"),
    });
}

#[test]
fn completion_dispatcher_returns_pending_when_budget_exhausted_while_filtering() {
    let path_secret_entry = test_path_secret_entry();
    let completion_rx = frame::failure_completion_channel();

    let mut frame = new_test_frame(path_secret_entry, 1).into_inner();
    frame.status = TransmissionStatus::Acknowledged;
    frame.completion = Some(completion_rx.sender());

    let rx = BudgetAwareTestReceiver {
        values: [Entry::new(frame)].into(),
    };
    let mut dispatcher = test_completion_dispatcher(rx);

    with_noop_context(|cx| {
        let mut budget = Budget::new(1);
        let result = dispatcher.poll_recv(cx, &mut budget);
        assert!(matches!(result, Poll::Pending));
        assert!(budget.take_needs_wake());
    });
}

// ── BatchFramesByPathSecret tests ─────────────────────────────────────────

#[test]
fn frame_batch_tracks_byte_costs_per_priority() {
    let path = test_path_secret_entry();
    let first = new_test_frame(path.clone(), 16);
    let first_cost = first.byte_cost();
    let mut batch = FrameBatch::new(first);

    let data = new_test_frame_with_header(
        path.clone(),
        24,
        Header::QueueData {
            queue_pair: crate::packet::datagram::QueuePair {
                source_queue_id: VarInt::from_u8(0),
                dest_queue_id: VarInt::from_u8(1),
            },
            binding_id: VarInt::from_u8(0),
            offset: VarInt::ZERO,
            largest_offset: VarInt::ZERO,
            is_fin: false,
            blocked: false,
            dest_acceptor_id: None,
            priority: crate::credit::Priority::default(),
        },
    );
    let data_cost = data.byte_cost();
    batch.push_with_cost(data, data_cost);

    let reset = new_test_frame_with_header(
        path,
        0,
        Header::QueueReset {
            queue_pair: crate::packet::datagram::QueuePair {
                source_queue_id: VarInt::from_u8(1),
                dest_queue_id: VarInt::from_u8(1),
            },
            binding_id: VarInt::from_u8(0),
            reset_target: crate::packet::datagram::ResetTarget::Both,
            error_code: VarInt::from_u8(7),
            init: None,
        },
    );
    let reset_cost = reset.byte_cost();
    batch.push_with_cost(reset, reset_cost);

    assert_eq!(
        batch.byte_cost(),
        MAX_FRAME_BATCH_PACKET_OVERHEAD + first_cost + data_cost + reset_cost
    );

    let (queues, costs) = batch.into_queues();
    assert_eq!(
        costs[Priority::QueueControl.as_index()],
        MAX_FRAME_BATCH_PACKET_OVERHEAD + first_cost
    );
    assert_eq!(costs[Priority::QueueData.as_index()], data_cost);
    assert_eq!(costs[Priority::QueueReset.as_index()], reset_cost);
    assert_eq!(queues[Priority::QueueControl.as_index()].len(), 1);
    assert_eq!(queues[Priority::QueueData.as_index()].len(), 1);
    assert_eq!(queues[Priority::QueueReset.as_index()].len(), 1);
}

#[test]
fn batch_frames_groups_by_same_path_secret() {
    let path_a = test_path_secret_entry();
    let path_b = test_path_secret_entry();
    path_a.update_max_datagram_size(4_096);
    path_b.update_max_datagram_size(4_096);

    let rx = TestReceiver {
        values: VecDeque::from([
            new_test_frame(path_a.clone(), 16),
            new_test_frame(path_a.clone(), 16),
            new_test_frame(path_b.clone(), 16),
        ]),
        consumed: 0,
    };
    let mut batcher = BatchFramesByPathSecret::new(rx, &test_clock(), Rate::new(10.0));

    let first = with_noop_context(|cx| batcher.poll_recv(cx, &mut Budget::new(usize::MAX)));
    let Poll::Ready(Some(first)) = first else {
        panic!("expected first batch");
    };
    assert_eq!(first.len(), 2);
    assert!(Arc::ptr_eq(first.path_secret_entry(), &path_a));

    let second = with_noop_context(|cx| batcher.poll_recv(cx, &mut Budget::new(usize::MAX)));
    let Poll::Ready(Some(second)) = second else {
        panic!("expected second batch");
    };
    assert_eq!(second.len(), 1);
    assert!(Arc::ptr_eq(second.path_secret_entry(), &path_b));
}

#[test]
fn batch_frames_enforces_datagram_byte_budget() {
    let path = test_path_secret_entry();
    // target_bytes = u16::MAX - 3000 ≈ 62535. Use frames large enough to exceed it.
    let frame_size = 40_000;

    let rx = TestReceiver {
        values: VecDeque::from([
            new_test_frame(path.clone(), frame_size),
            new_test_frame(path.clone(), frame_size),
            new_test_frame(path.clone(), frame_size),
        ]),
        consumed: 0,
    };
    let mut batcher = BatchFramesByPathSecret::new(rx, &test_clock(), Rate::new(10.0));

    let target_bytes = u16::MAX as u64 - 3000;

    let first = with_noop_context(|cx| batcher.poll_recv(cx, &mut Budget::new(usize::MAX)));
    let Poll::Ready(Some(first)) = first else {
        panic!("expected first batch");
    };
    // First frame + overhead exceeds target, so only one frame per batch.
    assert_eq!(first.len(), 1);
    assert!(first.byte_cost() <= target_bytes);

    let second = with_noop_context(|cx| batcher.poll_recv(cx, &mut Budget::new(usize::MAX)));
    let Poll::Ready(Some(second)) = second else {
        panic!("expected second batch");
    };
    assert_eq!(second.len(), 1);

    let third = with_noop_context(|cx| batcher.poll_recv(cx, &mut Budget::new(usize::MAX)));
    let Poll::Ready(Some(third)) = third else {
        panic!("expected third batch");
    };
    assert_eq!(third.len(), 1);
}

#[test]
fn batch_frames_forwards_on_consumed() {
    let path = test_path_secret_entry();
    let rx = TestReceiver {
        values: VecDeque::from([new_test_frame(path, 0)]),
        consumed: 0,
    };
    let mut batcher = BatchFramesByPathSecret::new(rx, &test_clock(), Rate::new(10.0));

    batcher.on_consumed(321);
    assert_eq!(batcher.inner.consumed, 321);
}

#[test]
fn ack_processor_drops_message_with_out_of_range_sender_idx() {
    const OUT_OF_RANGE_SENDER_ID: u64 = 42; // total_sender_ids is 1, so any value > 0 is invalid.

    let registry = crate::counter::Registry::default();
    let send_caches: crate::endpoint::id::IdMap<crate::endpoint::id::LocalSendSocketId, _> =
        vec![Rc::new(RefCell::new(send::Cache::new(
            &registry,
            crate::endpoint::id::LocalSenderId::from_index(0),
        )))]
        .into();
    let sender_idx_to_local = crate::endpoint::id::IdMap::<
        crate::endpoint::id::LocalSenderId,
        crate::endpoint::id::LocalSendSocketId,
    >::new(1, crate::endpoint::id::LocalSendSocketId::new(0));
    let (frame_tx, _frame_rx) = frame::submission_channel(1);
    let (tx_wheel_tx, _tx_wheel_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
    let (pto_wheel_tx, _pto_wheel_rx) = unsync::new_with_adapter::<send::PtoWheelAdapter>();
    let (idle_wheel_tx, _idle_wheel_rx) = unsync::new_with_adapter::<send::IdleWheelAdapter>();
    let path_secret_entry = test_path_secret_entry();

    let ack_rx = TestReceiver {
        values: VecDeque::from([Entry::new(msg::Sender::ReceivedAck {
            local_sender_id: crate::endpoint::id::LocalSenderId::new(
                VarInt::new(OUT_OF_RANGE_SENDER_ID).expect("valid varint"),
            ),
            path_secret_entry,
            payload: BytesMut::new(),
            ack_delay: core::time::Duration::ZERO,
            largest_acknowledged: VarInt::ZERO,
            ack_range: VarInt::ZERO,
            ecn_counts: Default::default(),
        })]),
        consumed: 0,
    };

    let (ack_completions_tx, _ack_completions_rx) = unsync::new::<msg::Sender>();
    let ack_completions_tx = ack_completions_tx.into_list_sender();
    let processor = AckProcessor::new(
        ack_rx,
        send_caches,
        sender_idx_to_local,
        1,
        crate::time::bach::Clock::default(),
        crate::xorshift::Rng::new(),
        frame_tx,
        frame::PriorityInput::default(),
        frame::PriorityInput::default(),
        ack_completions_tx,
        registry.register("!send.invalid_sender_idx"),
    );
    let rx = crate::socket::channel::Flatten::new(processor);
    let (immediate_tx, _immediate_rx) = unsync::new_with_adapter::<send::TxImmediateAdapter>();
    let mut router = crate::stream::endpoint::send::WheelRouter::new(
        rx,
        immediate_tx,
        tx_wheel_tx,
        pto_wheel_tx,
        idle_wheel_tx,
    );

    // The invalid sender_idx message is consumed (Flatten skips the None),
    // then the input is exhausted so the channel closes.
    let result = with_noop_context(|cx| router.poll_recv(cx, &mut Budget::new(usize::MAX)));
    assert_eq!(result, Poll::Ready(None));
}

// -- InterfacePacedPickTwo tests ---------------------------------------------

type Sends = Rc<RefCell<Vec<(crate::time::precision::Timestamp, u64)>>>;

/// A sender that records the time each item reached it and the item's byte cost.
struct TimedSender {
    clock: test_clock_mod::Clock,
    sends: Sends,
}

impl UnboundedSender<TestItem> for TimedSender {
    fn send(&mut self, value: TestItem) -> Result<(), TestItem> {
        let now = crate::time::precision::Clock::now(&self.clock);
        self.sends.borrow_mut().push((now, value.byte_cost()));
        Ok(())
    }
}

fn timed_senders(
    clock: &test_clock_mod::Clock,
    sends: &[&Sends],
) -> crate::endpoint::id::IdMap<LocalSenderId, TimedSender> {
    sends
        .iter()
        .map(|sends| TimedSender {
            clock: clock.clone(),
            sends: Rc::clone(sends),
        })
        .collect::<Vec<_>>()
        .into()
}

fn test_items(
    entry: &Arc<PathSecretEntry>,
    count: usize,
    byte_cost: u64,
    drop_counter: &Arc<AtomicUsize>,
) -> VecDeque<TestItem> {
    (0..count)
        .map(|_| {
            let mut item = new_test_item(entry.clone(), drop_counter.clone());
            item.byte_cost = byte_cost;
            item
        })
        .collect()
}

fn test_pool() -> crate::sync::Arc<crate::credit::Pool> {
    crate::sync::Arc::new(crate::credit::Pool::new(crate::credit::Config::new(
        1_000_000,
    )))
}

/// Polls `router` until it has nothing to do now: it returns `Pending`, or `Ready(None)` once
/// its input is closed and it holds no batch.
fn poll_until_idle<R: Receiver<()>>(router: &mut R) -> Poll<Option<()>> {
    with_noop_context(|cx| loop {
        match router.poll_recv(cx, &mut Budget::new(usize::MAX)) {
            Poll::Ready(Some(())) => {}
            other => return other,
        }
    })
}

/// Checks that each of `sends`, a time and a byte count, came once the bytes sent before it fit
/// in `rate` and its burst since `start`, with 2 us allowed for timer rounding.
fn assert_within_rate(
    sends: &[(crate::time::precision::Timestamp, u64)],
    rate: Rate,
    start: crate::time::precision::Timestamp,
) {
    let mut sent_before = 0;
    for &(at, bytes) in sends {
        let allowed = at.nanos_since(start) + rate.burst_nanos() + 2_000;
        assert!(
            rate.nanos_for_bytes(sent_before) <= allowed,
            "a send at {at:?} after {sent_before} bytes came early for {rate:?}",
        );
        sent_before += bytes;
    }
}

#[test]
fn interface_paced_pick_two_paces_each_interface_to_its_rate() {
    let slow = Rate::new(1.0);
    let fast = Rate::new(4.0);
    let clock = test_clock();
    let start = crate::time::precision::Clock::now(&clock);
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let rx = TestReceiver {
        values: test_items(&test_path_secret_entry(), 200, 100_000, &drop_counter),
        consumed: 0,
    };
    let slow_sends = Sends::default();
    let fast_sends = Sends::default();
    let pacing = SendPacing::new(vec![slow, fast], vec![0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&slow_sends, &fast_sends]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );

    // 20 ms of the test clock, in steps of 10 us.
    for _ in 0..2_000 {
        assert_eq!(poll_until_idle(&mut router), Poll::Pending);
        clock.advance(std::time::Duration::from_micros(10));
    }

    let slow_sends = slow_sends.borrow();
    let fast_sends = fast_sends.borrow();
    assert_within_rate(&slow_sends, slow, start);
    assert_within_rate(&fast_sends, fast, start);
    // A batch of 100,000 bytes takes 800 us at 1 Gbps and 200 us at 4 Gbps, so in 20 ms the slow
    // interface sends about 25 batches and the fast one about 100.
    let slow_count = slow_sends.len();
    let fast_count = fast_sends.len();
    assert!(slow_count >= 20, "slow: {slow_count} batches");
    assert!(
        fast_count >= 3 * slow_count,
        "fast: {fast_count} batches, slow: {slow_count}",
    );
}

#[test]
fn interface_paced_pick_two_delivers_held_batches_after_upstream_closes() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let rx = TestReceiver {
        values: test_items(&test_path_secret_entry(), 3, 100_000, &drop_counter),
        consumed: 0,
    };
    let sends = Sends::default();
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&sends, &sends]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );

    // Each batch costs more than a burst: two go at once, one on each interface, and the third
    // waits until its interface has paid for the batch it sent.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(sends.borrow().len(), 2);

    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(sends.borrow().len(), 3);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 3);
}

#[test]
fn interface_paced_pick_two_returns_credit_when_the_worker_is_gone() {
    const CAP: u64 = 1_000_000;
    const CREDIT: u64 = 4096;

    let drop_counter = Arc::new(AtomicUsize::new(0));
    let pool = crate::sync::Arc::new(crate::credit::Pool::new(crate::credit::Config::new(CAP)));
    let mut item = new_test_item(test_path_secret_entry(), drop_counter.clone());
    item.flow_credits = CREDIT;
    let rx = TestReceiver {
        values: [item].into(),
        consumed: 0,
    };
    let senders = vec![
        TestSender {
            accept: false,
            calls: 0,
        },
        TestSender {
            accept: false,
            calls: 0,
        },
    ];
    let rate = Rate::new(1.0);
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        senders.into(),
        crate::xorshift::Rng::new(),
        test_clock(),
        &pacing,
        &registry,
        pool.clone(),
    );

    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(drop_counter.load(Ordering::Relaxed), 1);
    // The item's credit wasn't drawn from the pool, so its return shows as a balance of
    // `CAP + CREDIT`.
    assert_eq!(
        pool.debug_available() as u64 + pool.debug_returned(),
        CAP + CREDIT,
        "the router didn't return the batch's send credit to the pool",
    );
}

#[test]
fn send_pacing_paces_each_interface_that_holds_a_send_socket() {
    use crate::socket::interface;

    let overall = Rate::new(25.0);
    let slow = Rate::new(1.0);
    let fast = Rate::new(4.0);
    let addrs: [std::net::SocketAddr; 4] = [
        "127.0.0.1:4000".parse().unwrap(),
        "127.0.0.1:4001".parse().unwrap(),
        "127.0.0.1:4002".parse().unwrap(),
        "127.0.0.1:4003".parse().unwrap(),
    ];
    let pacing = |configs: &[interface::Config]| {
        let table = interface::Table::resolve(configs, overall, &addrs, &addrs, &interface::System);
        SendPacing::from_interfaces(&table, overall)
    };

    // Without interfaces, one rate paces every send socket, as before.
    assert_eq!(pacing(&[]), SendPacing::single(overall));

    // Sockets 0 and 2 on the slow interface, 1 on the fast one, and 3, which neither lists, on
    // the default interface.
    let split = pacing(&[
        interface::Config {
            send_rate: Some(slow),
            send_sockets: vec![0, 2],
            ..Default::default()
        },
        interface::Config {
            send_rate: Some(fast),
            send_sockets: vec![1],
            ..Default::default()
        },
    ]);
    let expected = SendPacing::new(vec![overall, slow, fast], vec![1, 2, 1, 0]);
    assert_eq!(split, expected);
    assert_eq!(split.interface_count(), 3);
    assert_eq!(format!("{:?}", split.total_rate()), "30.0Gbps");

    // Where every send socket is on one interface, that interface's rate paces them all.
    let one = pacing(&[
        interface::Config {
            send_rate: Some(slow),
            ..Default::default()
        },
        interface::Config {
            send_rate: Some(fast),
            send_sockets: vec![0, 1, 2, 3],
            ..Default::default()
        },
    ]);
    assert_eq!(one, SendPacing::single(fast));
    assert_eq!(one.interface_count(), 1);
}

#[test]
fn interface_paced_pick_two_routes_highest_priority_batches_to_the_chosen_interface() {
    let rate = Rate::new(100.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(4)
        .build(None);
    // A highest-priority item costs 1 byte and any other item 2, so that each sender's record
    // tells them apart.
    let item = |highest_priority: bool| {
        let mut item = new_test_item(entry.clone(), drop_counter.clone());
        item.byte_cost = if highest_priority { 1 } else { 2 };
        item.highest_priority = highest_priority;
        item
    };
    let values: VecDeque<TestItem> = (0..10)
        .map(|_| item(true))
        .chain((0..20).map(|_| item(false)))
        .chain((0..10).map(|_| item(true)))
        .collect();
    let rx = TestReceiver {
        values,
        consumed: 0,
    };
    let sends: [Sends; 4] = Default::default();
    // Senders 0 and 1 are on the first interface, and 2 and 3 on the second, which carries the
    // highest-priority traffic.
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 0, 1, 1]).with_highest_priority(1);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&sends[0], &sends[1], &sends[2], &sends[3]]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );

    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));

    let count = |sender: usize, bytes: u64| {
        sends[sender]
            .borrow()
            .iter()
            .filter(|&&(_, cost)| cost == bytes)
            .count()
    };
    // Every highest-priority item went to a sender of the second interface.
    assert_eq!(count(0, 1) + count(1, 1), 0);
    assert_eq!(count(2, 1) + count(3, 1), 20);
    // The other items went to the senders of both interfaces.
    assert_eq!((0..4).map(|sender| count(sender, 2)).sum::<usize>(), 20);
    assert!(
        count(0, 2) + count(1, 2) > 0,
        "the first interface didn't get an item"
    );
}

#[test]
fn send_pacing_routes_highest_priority_batches_to_the_chosen_interface() {
    use crate::socket::interface;

    let overall = Rate::new(25.0);
    let slow = Rate::new(1.0);
    let fast = Rate::new(4.0);
    let addrs: [std::net::SocketAddr; 3] = [
        "127.0.0.1:4000".parse().unwrap(),
        "127.0.0.1:4001".parse().unwrap(),
        "127.0.0.1:4002".parse().unwrap(),
    ];
    let pacing = |configs: &[interface::Config]| {
        let table = interface::Table::resolve(configs, overall, &addrs, &addrs, &interface::System);
        SendPacing::from_interfaces(&table, overall)
    };
    let slow_interface = interface::Config {
        send_rate: Some(slow),
        send_sockets: vec![0],
        ..Default::default()
    };
    let fast_interface = interface::Config {
        send_rate: Some(fast),
        send_sockets: vec![1, 2],
        ..Default::default()
    };

    // The fast interface carries the highest-priority traffic.
    let chosen = pacing(&[
        slow_interface.clone(),
        interface::Config {
            carries_highest_priority: true,
            ..fast_interface.clone()
        },
    ]);
    assert_eq!(
        chosen,
        SendPacing::new(vec![slow, fast], vec![0, 1, 1]).with_highest_priority(1)
    );

    // An interface for the highest-priority traffic that doesn't hold a send socket doesn't count.
    let unused = pacing(&[
        slow_interface,
        fast_interface,
        interface::Config {
            carries_highest_priority: true,
            ..Default::default()
        },
    ]);
    assert_eq!(unused, SendPacing::new(vec![slow, fast], vec![0, 1, 1]));

    // With one interface that holds every send socket, the setting has no effect.
    let one = pacing(&[interface::Config {
        send_rate: Some(fast),
        send_sockets: vec![0, 1, 2],
        carries_highest_priority: true,
        ..Default::default()
    }]);
    assert_eq!(one, SendPacing::single(fast));
}

/// A receiver of the items a test adds between polls: it returns `Pending` while it holds none.
struct SharedReceiver<T> {
    values: Rc<RefCell<VecDeque<T>>>,
}

impl<T> Receiver<T> for SharedReceiver<T> {
    fn poll_recv(&mut self, _cx: &mut task::Context<'_>, _budget: &mut Budget) -> Poll<Option<T>> {
        match self.values.borrow_mut().pop_front() {
            Some(value) => Poll::Ready(Some(value)),
            None => Poll::Pending,
        }
    }

    fn on_consumed(&mut self, _bytes: u64) {}
}

/// Each frame batch a sender got: whether its frames are all of the highest priority level, and
/// how many frames it holds.
type BatchArrivals = Rc<RefCell<Vec<(bool, usize)>>>;

/// A sender that records each frame batch it gets.
struct BatchSender {
    arrivals: BatchArrivals,
}

impl UnboundedSender<FrameBatch> for BatchSender {
    fn send(&mut self, value: FrameBatch) -> Result<(), FrameBatch> {
        let arrival = (value.holds_highest_priority(), value.len());
        self.arrivals.borrow_mut().push(arrival);
        Ok(())
    }
}

/// A `QueueData` header, whose frames are of the lowest priority level.
fn queue_data_header() -> Header {
    Header::QueueData {
        queue_pair: crate::packet::datagram::QueuePair {
            source_queue_id: VarInt::from_u8(0),
            dest_queue_id: VarInt::from_u8(1),
        },
        binding_id: VarInt::from_u8(0),
        offset: VarInt::ZERO,
        largest_offset: VarInt::ZERO,
        is_fin: false,
        blocked: false,
        dest_acceptor_id: None,
        priority: crate::credit::Priority::default(),
    }
}

#[test]
fn interface_paced_pick_two_charges_highest_priority_batches_and_paces_mixed_ones() {
    const PINGS: usize = 3;
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let entry = test_path_secret_entry();
    let ping = || new_test_frame_with_header(entry.clone(), 0, Header::Ping);
    let data = |len: usize| new_test_frame_with_header(entry.clone(), len, queue_data_header());
    // A bulk batch costs more than a burst at `rate`, so it puts its interface's bucket in debt.
    let bulk = || FrameBatch::single(data(100_000));

    let input = Rc::new(RefCell::new(VecDeque::new()));
    let arrivals: [BatchArrivals; 2] = Default::default();
    let senders: Vec<BatchSender> = arrivals
        .iter()
        .map(|arrivals| BatchSender {
            arrivals: Rc::clone(arrivals),
        })
        .collect();
    // Sender 0 is on the first interface, and sender 1 on the second, which carries the
    // highest-priority traffic.
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]).with_highest_priority(1);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        SharedReceiver {
            values: Rc::clone(&input),
        },
        senders.into(),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );
    let arrived = |sender: usize, batch: (bool, usize)| {
        arrivals[sender]
            .borrow()
            .iter()
            .filter(|&&arrival| arrival == batch)
            .count()
    };

    // Put the bucket of each interface in debt with bulk batches, one at a time and without
    // moving the clock. The first goes to either interface, and the second most often to the
    // other one; where the second goes to the same one, it waits there, and the third goes to
    // the other one, the only interface that doesn't hold a batch.
    let mut bulk_batches = 0;
    while router.debt_nanos(0) == 0 || router.debt_nanos(1) == 0 {
        assert!(
            bulk_batches < 3,
            "three bulk batches didn't put both interfaces in debt"
        );
        input.borrow_mut().push_back(bulk());
        bulk_batches += 1;
        assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    }

    // A batch of pings goes at once to the chosen interface, though its bucket holds a debt, and
    // its cost is charged to that bucket alone.
    let mut pings = FrameBatch::single(ping());
    for _ in 1..PINGS {
        pings.push(ping());
    }
    assert!(pings.holds_highest_priority());
    let cost = rate.nanos_for_bytes(pings.byte_cost());
    let debts = [router.debt_nanos(0), router.debt_nanos(1)];
    input.borrow_mut().push_back(pings);
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(
        arrived(1, (true, PINGS)),
        1,
        "the pings didn't reach the chosen interface"
    );
    assert_eq!(
        arrived(0, (true, PINGS)),
        0,
        "the pings reached the other interface"
    );
    assert_eq!(router.debt_nanos(1), debts[1] + cost);
    assert_eq!(router.debt_nanos(0), debts[0]);

    // A ping beside a `QueueData` frame isn't a batch of the highest priority level: it waits on
    // its interface until that interface's timer fires, once the bucket has paid its debt.
    let mut mixed = FrameBatch::single(ping());
    mixed.push(data(1_000));
    assert!(!mixed.holds_highest_priority());
    input.borrow_mut().push_back(mixed);
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    let mixed_arrivals = || arrived(0, (false, 2)) + arrived(1, (false, 2));
    assert_eq!(mixed_arrivals(), 0, "the mixed batch went at once");

    // Each interface's timer fires once the clock reaches its debt.
    let debts = [router.debt_nanos(0), router.debt_nanos(1)];
    let (first, last) = (debts[0].min(debts[1]), debts[0].max(debts[1]));
    clock.advance(std::time::Duration::from_nanos(first - 1));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(
        mixed_arrivals(),
        0,
        "the mixed batch went before its interface's timer"
    );
    clock.advance(std::time::Duration::from_nanos(last - first + 1));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(
        mixed_arrivals(),
        1,
        "the mixed batch didn't go once its interface's timer fired"
    );
}

/// A receiver of test items that adds the bytes each `on_consumed` call reports to a count the
/// test reads while the router holds the receiver.
struct ConsumedReceiver {
    inner: TestReceiver<TestItem>,
    consumed: Rc<std::cell::Cell<u64>>,
}

impl Receiver<TestItem> for ConsumedReceiver {
    fn poll_recv(
        &mut self,
        cx: &mut task::Context<'_>,
        budget: &mut Budget,
    ) -> Poll<Option<TestItem>> {
        self.inner.poll_recv(cx, budget)
    }

    fn on_consumed(&mut self, bytes: u64) {
        self.consumed.set(self.consumed.get() + bytes);
    }
}

/// A sender that takes `accepts` items and then refuses each item, as a worker that has gone.
struct GoneAfter {
    accepts: usize,
}

impl UnboundedSender<TestItem> for GoneAfter {
    fn send(&mut self, value: TestItem) -> Result<(), TestItem> {
        if self.accepts == 0 {
            return Err(value);
        }
        self.accepts -= 1;
        drop(value);
        Ok(())
    }
}

#[test]
fn interface_paced_pick_two_sends_a_highest_priority_batch_while_every_interface_holds_one() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let entry = test_path_secret_entry();
    // Four bulk batches, each of more bytes than a burst, and then a batch of the highest
    // priority level of 1 byte.
    let mut values = test_items(&entry, 4, 100_000, &drop_counter);
    let mut highest = new_test_item(entry.clone(), drop_counter.clone());
    highest.byte_cost = 1;
    highest.highest_priority = true;
    values.push_back(highest);
    let rx = TestReceiver {
        values,
        consumed: 0,
    };
    let sends: [Sends; 2] = Default::default();
    // The second interface carries the highest-priority traffic.
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]).with_highest_priority(1);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&sends[0], &sends[1]]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );
    let costs = |sender: usize| -> Vec<u64> {
        sends[sender]
            .borrow()
            .iter()
            .map(|&(_, cost)| cost)
            .collect()
    };

    // Each interface sends one bulk batch at once and holds the next until its bucket has paid
    // for the first, so that every interface holds a batch. The highest-priority batch behind
    // them still reaches the second interface's worker at once, before either held batch goes.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(0), [100_000]);
    assert_eq!(costs(1), [100_000, 1]);

    // Once the buckets have paid, the held batches go.
    clock.advance(std::time::Duration::from_millis(2));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(costs(0), [100_000, 100_000]);
    assert_eq!(costs(1), [100_000, 1, 100_000]);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 5);
}

#[test]
fn interface_paced_pick_two_keeps_a_batch_read_while_every_interface_holds_one() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let entry = test_path_secret_entry();
    // Four bulk batches fill both interfaces, and the two after them cost 1 and 2 bytes more,
    // so that the records tell them apart.
    let mut values = test_items(&entry, 4, 100_000, &drop_counter);
    values.extend(test_items(&entry, 1, 100_001, &drop_counter));
    values.extend(test_items(&entry, 1, 100_002, &drop_counter));
    let consumed = Rc::new(std::cell::Cell::new(0));
    let rx = ConsumedReceiver {
        inner: TestReceiver {
            values,
            consumed: 0,
        },
        consumed: consumed.clone(),
    };
    let sends: [Sends; 2] = Default::default();
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]).with_highest_priority(1);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&sends[0], &sends[1]]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );
    let sent = |cost: u64| {
        sends
            .iter()
            .map(|sends| sends.borrow().iter().filter(|&&(_, c)| c == cost).count())
            .sum::<usize>()
    };

    // While every interface holds a batch, the router reads the fifth batch and keeps it, and
    // doesn't read the sixth. It tells upstream of each batch it read.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(sent(100_000), 2);
    assert_eq!(sent(100_001), 0);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 2);
    assert_eq!(consumed.get(), 4 * 100_000 + 100_001);

    // Once the buckets have paid, the held batches go, and the router routes the kept batch and
    // then reads the sixth. Each interface holds one of them until it has paid again.
    clock.advance(std::time::Duration::from_millis(2));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(sent(100_000), 4);
    assert_eq!(sent(100_001) + sent(100_002), 0);
    assert_eq!(consumed.get(), 4 * 100_000 + 100_001 + 100_002);

    clock.advance(std::time::Duration::from_millis(2));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(sent(100_001), 1);
    assert_eq!(sent(100_002), 1);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 6);
    assert_eq!(consumed.get(), 4 * 100_000 + 100_001 + 100_002);
}

#[test]
fn interface_paced_pick_two_returns_the_credit_of_a_kept_batch_when_the_worker_is_gone() {
    const CAP: u64 = 1_000_000;
    const CREDIT: u64 = 4096;

    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let pool = crate::sync::Arc::new(crate::credit::Pool::new(crate::credit::Config::new(CAP)));
    let mut values = test_items(&test_path_secret_entry(), 5, 100_000, &drop_counter);
    for item in &mut values {
        item.flow_credits = CREDIT;
    }
    let rx = TestReceiver {
        values,
        consumed: 0,
    };
    // Each worker takes one batch and is gone after it.
    let senders = vec![GoneAfter { accepts: 1 }, GoneAfter { accepts: 1 }];
    let rate = Rate::new(1.0);
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]).with_highest_priority(1);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        senders.into(),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        pool.clone(),
    );

    // Each interface sends a batch and holds one, and the router keeps the fifth.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 2);

    // The first held batch to go finds its worker gone: the router drops it, the other held
    // batch and the kept batch, and returns the credit of the three to the pool.
    clock.advance(std::time::Duration::from_millis(2));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(drop_counter.load(Ordering::Relaxed), 5);
    // The items' credit wasn't drawn from the pool, so the return of three shows as a balance
    // of `CAP + 3 * CREDIT`.
    assert_eq!(
        pool.debug_available() as u64 + pool.debug_returned(),
        CAP + 3 * CREDIT,
        "the router didn't return the send credit of each batch it held or kept",
    );
}

#[test]
fn interface_paced_pick_two_without_a_chosen_interface_waits_for_a_free_interface() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let entry = test_path_secret_entry();
    let mut values = test_items(&entry, 4, 100_000, &drop_counter);
    let mut highest = new_test_item(entry.clone(), drop_counter.clone());
    highest.byte_cost = 1;
    highest.highest_priority = true;
    values.push_back(highest);
    let consumed = Rc::new(std::cell::Cell::new(0));
    let rx = ConsumedReceiver {
        inner: TestReceiver {
            values,
            consumed: 0,
        },
        consumed: consumed.clone(),
    };
    let sends: [Sends; 2] = Default::default();
    // Without an interface for the highest-priority traffic, the fifth batch goes as any other.
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&sends[0], &sends[1]]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );
    let sent = || {
        sends
            .iter()
            .map(|sends| sends.borrow().len())
            .sum::<usize>()
    };

    // While every interface holds a batch, the router doesn't read the fifth.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(sent(), 2);
    assert_eq!(consumed.get(), 4 * 100_000);

    // Once the buckets have paid, the held batches go, and the router reads the fifth, which
    // waits until its interface has paid again.
    clock.advance(std::time::Duration::from_millis(2));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(sent(), 4);
    assert_eq!(consumed.get(), 4 * 100_000 + 1);

    clock.advance(std::time::Duration::from_millis(2));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(sent(), 5);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 5);
}

#[test]
fn interface_paced_pick_two_doesnt_count_a_batch_whose_route_fails() {
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let values = test_items(&test_path_secret_entry(), 1, 100_000, &drop_counter);
    let consumed = Rc::new(std::cell::Cell::new(0));
    let rx = ConsumedReceiver {
        inner: TestReceiver {
            values,
            consumed: 0,
        },
        consumed: consumed.clone(),
    };
    // Both workers are gone, so the route of the one batch fails.
    let senders = vec![GoneAfter { accepts: 0 }, GoneAfter { accepts: 0 }];
    let rate = Rate::new(1.0);
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]).with_highest_priority(1);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        senders.into(),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );

    // The router reads the batch and finds its worker gone: it drops the batch, and doesn't
    // tell upstream that the batch was consumed.
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(drop_counter.load(Ordering::Relaxed), 1);
    assert_eq!(
        consumed.get(),
        0,
        "the router counted a batch whose route failed"
    );
}

/// A receiver of test items that the test feeds and closes while the router holds a clone of it.
/// It answers `Pending` while it doesn't hold an item and the test hasn't closed it, and adds the
/// bytes each `on_consumed` call reports to a count.
#[derive(Clone, Default)]
struct ClosableReceiver {
    values: Rc<RefCell<VecDeque<TestItem>>>,
    closed: Rc<std::cell::Cell<bool>>,
    consumed: Rc<std::cell::Cell<u64>>,
}

impl Receiver<TestItem> for ClosableReceiver {
    fn poll_recv(
        &mut self,
        _cx: &mut task::Context<'_>,
        _budget: &mut Budget,
    ) -> Poll<Option<TestItem>> {
        match self.values.borrow_mut().pop_front() {
            Some(value) => Poll::Ready(Some(value)),
            None if self.closed.get() => Poll::Ready(None),
            None => Poll::Pending,
        }
    }

    fn on_consumed(&mut self, bytes: u64) {
        self.consumed.set(self.consumed.get() + bytes);
    }
}

#[test]
fn interface_paced_pick_two_sends_its_highest_priority_input_ahead_of_a_waiting_bulk_batch() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let entry = test_path_secret_entry();
    // Five bulk batches, each of more bytes than a burst, in the bulk input, which closes after
    // them, and an input for the highest priority level, which the test feeds later. The bulk
    // batches cost 100,000 to 100,004 bytes, in the order the bulk input holds them, so that the
    // records tell them apart.
    let rx = ClosableReceiver::default();
    for byte_cost in 100_000..100_005 {
        let items = test_items(&entry, 1, byte_cost, &drop_counter);
        rx.values.borrow_mut().extend(items);
    }
    rx.closed.set(true);
    let highest_rx = ClosableReceiver::default();
    let sends: [Sends; 2] = Default::default();
    // The second interface carries the highest-priority traffic.
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]).with_highest_priority(1);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx.clone(),
        timed_senders(&clock, &[&sends[0], &sends[1]]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    )
    .with_highest_priority_input(highest_rx.clone());
    let costs = |sender: usize| -> Vec<u64> {
        sends[sender]
            .borrow()
            .iter()
            .map(|&(_, cost)| cost)
            .collect()
    };
    // The byte costs of the bulk batches an interface has sent, in the order it sent them.
    let bulk = |sender: usize| -> Vec<u64> {
        costs(sender)
            .into_iter()
            .filter(|&cost| cost >= 100_000)
            .collect()
    };
    // Checks that each interface has sent the bulk batches it took in the order the bulk input
    // held them, and returns the byte costs of the bulk batches sent, in increasing order.
    let bulk_sent = || -> Vec<u64> {
        let mut sent = Vec::new();
        for sender in 0..sends.len() {
            let taken = bulk(sender);
            assert!(
                taken.windows(2).all(|pair| pair[0] < pair[1]),
                "interface {sender} sent its bulk batches out of order: {taken:?}"
            );
            sent.extend(taken);
        }
        sent.sort_unstable();
        sent
    };

    // Each interface sends one bulk batch and holds the next, so that every interface holds a
    // batch. The router doesn't read the fifth bulk batch, which waits in the bulk input.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(bulk_sent().len(), 2);
    assert_eq!(rx.values.borrow().len(), 1);
    assert_eq!(rx.values.borrow()[0].byte_cost, 100_004);
    assert_eq!(rx.consumed.get(), 100_000 + 100_001 + 100_002 + 100_003);

    // Two batches reach the input of the highest priority level, the second one not marked as of
    // that level. Each goes at once to the second interface's worker, ahead of the bulk batch
    // that waits and of the held ones.
    let mut highest = new_test_item(entry.clone(), drop_counter.clone());
    highest.byte_cost = 1;
    highest.highest_priority = true;
    let mut unmarked = new_test_item(entry.clone(), drop_counter.clone());
    unmarked.byte_cost = 2;
    highest_rx.values.borrow_mut().push_back(highest);
    highest_rx.values.borrow_mut().push_back(unmarked);
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(bulk(1).len(), 1);
    assert_eq!(costs(1)[1..], [1, 2]);
    assert_eq!(rx.values.borrow().len(), 1);
    assert_eq!(highest_rx.consumed.get(), 1 + 2);

    // Once the buckets have paid, the held batches go, and the router reads the fifth bulk
    // batch, which waits until its interface has paid again. Each interface has sent its bulk
    // batches in the order the bulk input held them.
    clock.advance(std::time::Duration::from_millis(2));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(bulk_sent(), [100_000, 100_001, 100_002, 100_003]);
    assert_eq!(
        rx.consumed.get(),
        100_000 + 100_001 + 100_002 + 100_003 + 100_004
    );

    // The bulk input is closed and empty, and the router goes on while the input of the highest
    // priority level is open.
    clock.advance(std::time::Duration::from_millis(2));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(bulk_sent(), [100_000, 100_001, 100_002, 100_003, 100_004]);

    highest_rx.closed.set(true);
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(drop_counter.load(Ordering::Relaxed), 7);
}

#[test]
fn interface_paced_pick_two_without_a_chosen_interface_routes_the_highest_input_as_any_other() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let entry = test_path_secret_entry();
    let rx = ClosableReceiver::default();
    rx.values
        .borrow_mut()
        .extend(test_items(&entry, 4, 100_000, &drop_counter));
    rx.closed.set(true);
    let highest_rx = ClosableReceiver::default();
    let sends: [Sends; 2] = Default::default();
    // Without an interface for the highest-priority traffic, the batches of that level's input
    // go as any other.
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx.clone(),
        timed_senders(&clock, &[&sends[0], &sends[1]]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    )
    .with_highest_priority_input(highest_rx.clone());
    let sent = || {
        sends
            .iter()
            .map(|sends| sends.borrow().len())
            .sum::<usize>()
    };

    // Each interface sends one bulk batch and holds the next.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(sent(), 2);

    // A batch of the highest priority level waits in its input while every interface holds a
    // batch.
    let mut highest = new_test_item(entry.clone(), drop_counter.clone());
    highest.byte_cost = 1;
    highest.highest_priority = true;
    highest_rx.values.borrow_mut().push_back(highest);
    highest_rx.closed.set(true);
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(sent(), 2);
    assert_eq!(highest_rx.values.borrow().len(), 1);

    // Once the buckets have paid, the held batches go, and the router reads the batch of the
    // highest priority level, which waits until its interface has paid again.
    clock.advance(std::time::Duration::from_millis(2));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(sent(), 4);
    assert!(highest_rx.values.borrow().is_empty());
    assert_eq!(highest_rx.consumed.get(), 1);

    clock.advance(std::time::Duration::from_millis(2));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(sent(), 5);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 5);
}

#[test]
fn interface_paced_pick_two_reads_ahead_again_once_its_highest_priority_input_closes() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let entry = test_path_secret_entry();
    // The bulk input holds four bulk batches, each of more bytes than a burst, a 1-byte batch of
    // the highest priority level and a fifth bulk batch, and closes after them. The input of the
    // highest priority level stays empty until the test closes it.
    let rx = ClosableReceiver::default();
    rx.values
        .borrow_mut()
        .extend(test_items(&entry, 4, 100_000, &drop_counter));
    let mut highest = new_test_item(entry.clone(), drop_counter.clone());
    highest.byte_cost = 1;
    highest.highest_priority = true;
    rx.values.borrow_mut().push_back(highest);
    rx.values
        .borrow_mut()
        .extend(test_items(&entry, 1, 100_000, &drop_counter));
    rx.closed.set(true);
    let highest_rx = ClosableReceiver::default();
    let sends: [Sends; 2] = Default::default();
    // The second interface carries the highest-priority traffic.
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]).with_highest_priority(1);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx.clone(),
        timed_senders(&clock, &[&sends[0], &sends[1]]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    )
    .with_highest_priority_input(highest_rx.clone());
    let costs = |sender: usize| -> Vec<u64> {
        sends[sender]
            .borrow()
            .iter()
            .map(|&(_, cost)| cost)
            .collect()
    };
    let sent = |cost: u64| {
        sends
            .iter()
            .map(|sends| sends.borrow().iter().filter(|&&(_, c)| c == cost).count())
            .sum::<usize>()
    };

    // Each interface sends one bulk batch and holds the next. While the input of the highest
    // priority level is open, the router doesn't read the bulk input ahead, so the 1-byte batch
    // and the fifth bulk batch wait there.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(sent(100_000), 2);
    assert_eq!(costs(1), [100_000]);
    assert_eq!(rx.values.borrow().len(), 2);
    assert_eq!(rx.consumed.get(), 4 * 100_000);

    // Once that input closes, the router reads the bulk input ahead again while every interface
    // holds a batch: the 1-byte batch goes at once to the second interface's worker, and the
    // router keeps the fifth bulk batch.
    highest_rx.closed.set(true);
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(1), [100_000, 1]);
    assert_eq!(sent(100_000), 2);
    assert!(rx.values.borrow().is_empty());
    assert_eq!(rx.consumed.get(), 4 * 100_000 + 1 + 100_000);

    // Once the buckets have paid, the held batches go, and the router routes the fifth bulk
    // batch, which waits until its interface has paid again.
    clock.advance(std::time::Duration::from_millis(2));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(sent(100_000), 4);

    clock.advance(std::time::Duration::from_millis(2));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(sent(100_000), 5);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 6);
}

/// Routes `items` items of `entry` with pick-two over one test sender for each of the entry's
/// sender slots, and returns how many items each sender got.
fn pick_two_calls(entry: &Arc<PathSecretEntry>, items: usize, seed: u64) -> Vec<usize> {
    let mut senders: Vec<TestSender> = (0..entry.socket_sender_count())
        .map(|_| TestSender {
            accept: true,
            calls: 0,
        })
        .collect();
    let mut rng = crate::xorshift::Rng::with_seed(seed);
    let mut rr = 0;
    for _ in 0..items {
        let item = new_test_item(entry.clone(), Arc::new(AtomicUsize::new(0)));
        assert!(try_send_pick_two_with_rr(item, &mut senders, &mut rng, &mut rr).is_ok());
    }
    senders.iter().map(|sender| sender.calls).collect()
}

#[test]
fn pick_two_skips_lost_senders_while_another_delivers() {
    let entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(4)
        .build(None);
    let sender = LocalSenderId::from_index;
    assert!(entry.set_sender_lost(sender(1), true));
    assert!(entry.set_sender_lost(sender(3), true));

    let calls = pick_two_calls(&entry, 100, 1);
    assert_eq!(
        calls[1] + calls[3],
        0,
        "a lost sender got an item: {calls:?}"
    );
    assert_eq!(calls[0] + calls[2], 100);
    assert!(
        calls[0] > 0 && calls[2] > 0,
        "a sender that delivers didn't get an item: {calls:?}"
    );

    // Once the pair of sender 1 delivers again, the router chooses it again.
    assert!(entry.set_sender_lost(sender(1), false));
    let calls = pick_two_calls(&entry, 100, 2);
    assert_eq!(calls[3], 0, "a lost sender got an item: {calls:?}");
    assert!(calls[1] > 0, "sender 1 didn't get an item: {calls:?}");
}

#[test]
fn pick_two_spreads_items_over_every_sender_while_every_pair_is_lost() {
    let entry = test_path_secret_entry();
    for idx in 0..2 {
        assert!(entry.set_sender_lost(LocalSenderId::from_index(idx), true));
    }

    // Where no pair delivers, the router chooses among every sender, as it does with no lost
    // pair.
    let calls = pick_two_calls(&entry, 100, 3);
    assert_eq!(calls.iter().sum::<usize>(), 100);
    assert!(
        calls.iter().all(|&calls| calls > 0),
        "a sender didn't get an item: {calls:?}"
    );
}

/// Returns `highest` items of `entry` that hold a frame of the highest priority level, and then
/// `other` items that don't. A highest-priority item costs 1 byte and any other item 2, so that
/// each sender's record tells them apart.
fn priority_items(
    entry: &Arc<PathSecretEntry>,
    drop_counter: &Arc<AtomicUsize>,
    highest: usize,
    other: usize,
) -> VecDeque<TestItem> {
    let item = |highest_priority: bool| {
        let mut item = new_test_item(entry.clone(), drop_counter.clone());
        item.byte_cost = if highest_priority { 1 } else { 2 };
        item.highest_priority = highest_priority;
        item
    };
    (0..highest)
        .map(|_| item(true))
        .chain((0..other).map(|_| item(false)))
        .collect()
}

/// Routes `values` with an interface-paced pick-two over four senders, 0 and 1 on the first
/// interface and 2 and 3 on the second, which carries the highest-priority traffic. Returns, for
/// each sender, how many highest-priority items and how many other items it got.
fn route_by_interface(values: VecDeque<TestItem>) -> [[usize; 2]; 4] {
    let rate = Rate::new(100.0);
    let clock = test_clock();
    let sends: [Sends; 4] = Default::default();
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 0, 1, 1]).with_highest_priority(1);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        TestReceiver {
            values,
            consumed: 0,
        },
        timed_senders(&clock, &[&sends[0], &sends[1], &sends[2], &sends[3]]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );

    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));

    let count = |sender: usize, bytes: u64| {
        sends[sender]
            .borrow()
            .iter()
            .filter(|&&(_, cost)| cost == bytes)
            .count()
    };
    [0, 1, 2, 3].map(|sender| [count(sender, 1), count(sender, 2)])
}

#[test]
fn interface_paced_pick_two_skips_lost_pairs_while_another_delivers() {
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(4)
        .build(None);
    let sender = LocalSenderId::from_index;
    let items = || priority_items(&entry, &drop_counter, 20, 20);

    // While the pair of sender 0 is lost, the other items go to senders 1 to 3, and the
    // highest-priority items stay on the second interface.
    assert!(entry.set_sender_lost(sender(0), true));
    let counts = route_by_interface(items());
    assert_eq!(counts[0], [0, 0], "sender 0 got an item: {counts:?}");
    assert_eq!(counts[2][0] + counts[3][0], 20);
    assert_eq!(counts[1][1] + counts[2][1] + counts[3][1], 20);

    // While every pair of the second interface is lost too, every item goes to sender 1, the
    // highest-priority items among them.
    assert!(entry.set_sender_lost(sender(2), true));
    assert!(entry.set_sender_lost(sender(3), true));
    assert_eq!(
        route_by_interface(items()),
        [[0, 0], [20, 20], [0, 0], [0, 0]]
    );

    // Once the pairs of the second interface deliver again, the highest-priority items go back
    // to it.
    assert!(entry.set_sender_lost(sender(2), false));
    assert!(entry.set_sender_lost(sender(3), false));
    let counts = route_by_interface(items());
    assert_eq!(counts[0], [0, 0], "sender 0 got an item: {counts:?}");
    assert_eq!(counts[2][0] + counts[3][0], 20);
}

#[test]
fn interface_paced_pick_two_chooses_by_one_read_of_the_lost_marks() {
    let rate = Rate::new(100.0);
    let clock = test_clock();
    let sends: [Sends; 4] = Default::default();
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 0, 1, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        TestReceiver {
            values: VecDeque::new(),
            consumed: 0,
        },
        timed_senders(&clock, &[&sends[0], &sends[1], &sends[2], &sends[3]]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );
    let entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(4)
        .build(None);
    let sender = LocalSenderId::from_index;
    assert!(entry.set_sender_lost(sender(0), true));

    // The router reads the marks while sender 0 alone is lost. Senders 1 and 2 are marked lost
    // after that read and before the router chooses: the router chooses by the marks it read,
    // among senders 1 to 3, and doesn't panic.
    let mut chosen = [0usize; 4];
    for _ in 0..30 {
        let choice = router
            .choose_sender_after_read(&entry, false, || {
                assert!(entry.set_sender_lost(sender(1), true));
                assert!(entry.set_sender_lost(sender(2), true));
            })
            .expect("a sender the router read as delivering is free");
        chosen[choice.as_usize()] += 1;
        assert!(entry.set_sender_lost(sender(1), false));
        assert!(entry.set_sender_lost(sender(2), false));
    }
    assert_eq!(
        chosen[0], 0,
        "the router chose the lost sender 0: {chosen:?}"
    );
    assert!(
        chosen[1] + chosen[2] > 0,
        "the router read the marks again after the first read: {chosen:?}",
    );
}

/// A receiver of `values` that adds the bytes of each batch its router consumed to `consumed`,
/// which the test reads while the router holds the receiver.
struct SharedTestReceiver {
    values: VecDeque<TestItem>,
    consumed: Rc<std::cell::Cell<u64>>,
}

impl Receiver<TestItem> for SharedTestReceiver {
    fn poll_recv(
        &mut self,
        _cx: &mut task::Context<'_>,
        _budget: &mut Budget,
    ) -> Poll<Option<TestItem>> {
        Poll::Ready(self.values.pop_front())
    }

    fn on_consumed(&mut self, bytes: u64) {
        self.consumed.set(self.consumed.get() + bytes);
    }
}

#[test]
fn interface_paced_pick_two_holds_a_batch_until_a_sender_that_delivers_is_free() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let entry = test_path_secret_entry();
    // The pair of the second interface's sender is lost, so the first interface's sender takes
    // every batch.
    assert!(entry.set_sender_lost(LocalSenderId::from_index(1), true));
    let consumed = Rc::new(std::cell::Cell::new(0));
    let rx = SharedTestReceiver {
        values: test_items(&entry, 3, 100_000, &drop_counter),
        consumed: Rc::clone(&consumed),
    };
    let delivers = Sends::default();
    let lost = Sends::default();
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&delivers, &lost]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );

    // Each batch costs more than a burst: the first goes at once, the second waits on the first
    // interface, and the third waits in the router, though the second interface is free. The
    // router consumed each of them once.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(delivers.borrow().len(), 1);
    assert_eq!(consumed.get(), 300_000);

    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(delivers.borrow().len(), 2);

    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(delivers.borrow().len(), 3);
    assert!(lost.borrow().is_empty(), "the lost sender got a batch");
    assert_eq!(drop_counter.load(Ordering::Relaxed), 3);
    assert_eq!(consumed.get(), 300_000, "the router consumed a batch twice");
}

/// A sender whose receiver is gone once `gone` is set.
struct GoneSender {
    gone: Rc<std::cell::Cell<bool>>,
}

impl UnboundedSender<TestItem> for GoneSender {
    fn send(&mut self, value: TestItem) -> Result<(), TestItem> {
        if self.gone.get() {
            Err(value)
        } else {
            drop(value);
            Ok(())
        }
    }
}

#[test]
fn interface_paced_pick_two_returns_the_credit_of_a_waiting_batch_when_the_worker_is_gone() {
    const CAP: u64 = 1_000_000;
    const CREDIT: u64 = 4096;

    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let pool = crate::sync::Arc::new(crate::credit::Pool::new(crate::credit::Config::new(CAP)));
    let entry = test_path_secret_entry();
    assert!(entry.set_sender_lost(LocalSenderId::from_index(1), true));
    let mut values = test_items(&entry, 3, 100_000, &drop_counter);
    values[2].flow_credits = CREDIT;
    let gone = Rc::new(std::cell::Cell::new(false));
    let senders: Vec<_> = (0..2)
        .map(|_| GoneSender {
            gone: Rc::clone(&gone),
        })
        .collect();
    let rate = Rate::new(1.0);
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        TestReceiver {
            values,
            consumed: 0,
        },
        senders.into(),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        pool.clone(),
    );

    // The first batch goes, the second waits on the first interface, and the third, which holds
    // the credit, waits in the router.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);

    // Once the worker is gone, the router returns the credit of the batch that waits in it too.
    gone.set(true);
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(drop_counter.load(Ordering::Relaxed), 3);
    // The credit wasn't drawn from the pool, so its return shows as a balance of `CAP + CREDIT`.
    assert_eq!(
        pool.debug_available() as u64 + pool.debug_returned(),
        CAP + CREDIT,
        "the router didn't return the waiting batch's send credit to the pool",
    );
}

/// Polls `router` once.
fn poll_once<R: Receiver<()>>(router: &mut R) -> Poll<Option<()>> {
    with_noop_context(|cx| router.poll_recv(cx, &mut Budget::new(usize::MAX)))
}

#[test]
fn interface_paced_pick_two_routes_another_peers_batch_while_a_batch_waits() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // The pair of the second interface's sender is lost for the first peer alone, so the first
    // interface's sender takes each batch of that peer.
    let waits = test_path_secret_entry();
    assert!(waits.set_sender_lost(LocalSenderId::from_index(1), true));
    let other = PathSecretEntry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(2)
        .build(None);
    let mut values = test_items(&waits, 4, 100_000, &drop_counter);
    for (offset, value) in values.iter_mut().enumerate() {
        // Each batch of the first peer costs a byte more than the one before it, so that the
        // sender's record shows their order.
        value.byte_cost += offset as u64;
    }
    values.extend(test_items(&other, 1, 1_000, &drop_counter));
    let consumed = Rc::new(std::cell::Cell::new(0));
    let rx = SharedTestReceiver {
        values,
        consumed: Rc::clone(&consumed),
    };
    let first = Sends::default();
    let second = Sends::default();
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&first, &second]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );
    let costs = |sends: &Sends| {
        sends
            .borrow()
            .iter()
            .map(|&(_, cost)| cost)
            .collect::<Vec<_>>()
    };
    let first_peer_costs = |count: u64| {
        (0..count)
            .map(|offset| 100_000 + offset)
            .collect::<Vec<_>>()
    };

    // The first peer's first batch goes at once, its second waits on the first interface, and
    // its third and fourth wait in the router. The other peer's batch goes at once to the free
    // second interface while the first peer's batches wait. The router consumed each batch.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), first_peer_costs(1));
    assert_eq!(costs(&second), [1_000]);
    assert_eq!(consumed.get(), 401_006);

    // The pacer releases the first peer's second batch, and the router then holds its third on
    // the first interface: each of the two polls reports the batch it moved.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_once(&mut router), Poll::Ready(Some(())));
    assert_eq!(poll_once(&mut router), Poll::Ready(Some(())));
    assert_eq!(poll_once(&mut router), Poll::Pending);
    assert_eq!(costs(&first), first_peer_costs(2));

    // The first peer's batches go on in the order the router read them.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), first_peer_costs(3));
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(costs(&first), first_peer_costs(4));
    assert_eq!(costs(&second), [1_000]);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 5);
    assert_eq!(consumed.get(), 401_006, "the router consumed a batch twice");
}

#[test]
fn interface_paced_pick_two_returns_the_credit_of_each_waiting_peer_when_the_worker_is_gone() {
    const CAP: u64 = 1_000_000;
    const CREDIT: u64 = 4096;

    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let pool = crate::sync::Arc::new(crate::credit::Pool::new(crate::credit::Config::new(CAP)));
    // The pair of the second interface's sender is lost for both peers, so the first
    // interface's sender takes each batch of theirs.
    let first_peer = test_path_secret_entry();
    let second_peer = PathSecretEntry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(2)
        .build(None);
    for entry in [&first_peer, &second_peer] {
        assert!(entry.set_sender_lost(LocalSenderId::from_index(1), true));
    }
    let mut values = test_items(&first_peer, 3, 100_000, &drop_counter);
    values.extend(test_items(&second_peer, 1, 100_000, &drop_counter));
    values[2].flow_credits = CREDIT;
    values[3].flow_credits = CREDIT;
    let consumed = Rc::new(std::cell::Cell::new(0));
    let gone = Rc::new(std::cell::Cell::new(false));
    let senders: Vec<_> = (0..2)
        .map(|_| GoneSender {
            gone: Rc::clone(&gone),
        })
        .collect();
    let rate = Rate::new(1.0);
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        SharedTestReceiver {
            values,
            consumed: Rc::clone(&consumed),
        },
        senders.into(),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        pool.clone(),
    );

    // The first peer's first batch goes, its second waits on the first interface, and its third
    // waits in the router. The router reads the second peer's batch while that one waits, and
    // that batch waits in the router too.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(consumed.get(), 400_000);

    // Once the worker is gone, the router returns the credit of the waiting batch of each peer.
    gone.set(true);
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(drop_counter.load(Ordering::Relaxed), 4);
    // The credit wasn't drawn from the pool, so its return shows as a balance of
    // `CAP + 2 * CREDIT`.
    assert_eq!(
        pool.debug_available() as u64 + pool.debug_returned(),
        CAP + 2 * CREDIT,
        "the router didn't return the credit of each waiting batch to the pool",
    );
    assert_eq!(
        router.waiter_entries(),
        0,
        "the router kept the waiters of its dropped batches",
    );

    // A poll after the end finds that the router holds nothing.
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
}

/// A receiver of `values` that returns `Pending` once it has returned each of them, as an open
/// channel does.
struct OpenTestReceiver {
    values: VecDeque<TestItem>,
}

impl Receiver<TestItem> for OpenTestReceiver {
    fn poll_recv(
        &mut self,
        _cx: &mut task::Context<'_>,
        _budget: &mut Budget,
    ) -> Poll<Option<TestItem>> {
        match self.values.pop_front() {
            Some(value) => Poll::Ready(Some(value)),
            None => Poll::Pending,
        }
    }

    fn on_consumed(&mut self, _bytes: u64) {}
}

#[test]
fn interface_paced_pick_two_ends_when_a_waiting_batch_finds_its_worker_gone() {
    const CAP: u64 = 1_000_000;
    const CREDIT: u64 = 4096;

    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let pool = crate::sync::Arc::new(crate::credit::Pool::new(crate::credit::Config::new(CAP)));
    let entry = test_path_secret_entry();
    let sender = LocalSenderId::from_index;
    assert!(entry.set_sender_lost(sender(1), true));
    let mut values = test_items(&entry, 3, 100_000, &drop_counter);
    values[2].flow_credits = CREDIT;
    let gone = Rc::new(std::cell::Cell::new(false));
    let senders: Vec<_> = (0..2)
        .map(|_| GoneSender {
            gone: Rc::clone(&gone),
        })
        .collect();
    let rate = Rate::new(1.0);
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        OpenTestReceiver { values },
        senders.into(),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        pool.clone(),
    );

    // The first batch goes, the second waits on the first interface, and the third waits in the
    // router. The input stays open.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);

    // The pacer releases the second batch to the first interface's sender.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_once(&mut router), Poll::Ready(Some(())));

    // Once the first interface may send again, the router offers the waiting batch to its
    // sender, whose receiver is gone: the router ends in that poll, though its input is open,
    // and returns the credit of the waiting batch.
    clock.advance(std::time::Duration::from_millis(1));
    gone.set(true);
    assert_eq!(poll_once(&mut router), Poll::Ready(None));
    assert_eq!(drop_counter.load(Ordering::Relaxed), 3);
    assert_eq!(
        pool.debug_available() as u64 + pool.debug_returned(),
        CAP + CREDIT,
        "the router didn't return the waiting batch's send credit to the pool",
    );
}

#[test]
fn interface_paced_pick_two_ends_without_consuming_a_batch_whose_worker_is_gone() {
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let consumed = Rc::new(std::cell::Cell::new(0));
    let rx = SharedTestReceiver {
        values: test_items(&test_path_secret_entry(), 2, 100, &drop_counter),
        consumed: Rc::clone(&consumed),
    };
    let senders = vec![
        TestSender {
            accept: false,
            calls: 0,
        },
        TestSender {
            accept: false,
            calls: 0,
        },
    ];
    let rate = Rate::new(1.0);
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        senders.into(),
        crate::xorshift::Rng::new(),
        test_clock(),
        &pacing,
        &registry,
        test_pool(),
    );

    // The worker of the first batch is gone: the router ends in that poll, without telling
    // upstream that the batch was consumed or reading the second batch.
    assert_eq!(poll_once(&mut router), Poll::Ready(None));
    assert_eq!(consumed.get(), 0);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 1);
}

#[test]
fn interface_paced_pick_two_offers_a_waiting_batch_only_once_an_interface_it_waits_on_frees() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // The pair of the second interface's sender is lost, so the first interface's sender takes
    // each batch, and a batch that waits waits on the first interface alone.
    let entry = test_path_secret_entry();
    let sender = LocalSenderId::from_index;
    assert!(entry.set_sender_lost(sender(1), true));
    let consumed = Rc::new(std::cell::Cell::new(0));
    let rx = SharedTestReceiver {
        values: test_items(&entry, 3, 100_000, &drop_counter),
        consumed: Rc::clone(&consumed),
    };
    let first = Sends::default();
    let second = Sends::default();
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&first, &second]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );

    // The first batch goes at once, the second waits on the first interface, and the third
    // waits in the router.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(first.borrow().len(), 1);

    // The pair of the second interface's sender delivers again, and that interface is free, but
    // the first interface, which the waiting batch waits on, still holds a batch: the poll
    // doesn't offer the waiting batch again, so the second interface's sender doesn't get it.
    assert!(entry.set_sender_lost(sender(1), false));
    assert_eq!(poll_once(&mut router), Poll::Pending);
    assert!(
        second.borrow().is_empty(),
        "the router offered the waiting batch before an interface it waits on freed",
    );
    assert_eq!(first.borrow().len(), 1);

    // Once the first interface frees, the router offers the waiting batch, and each batch goes.
    clock.advance(std::time::Duration::from_millis(1));
    let _ = poll_until_idle(&mut router);
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(first.borrow().len() + second.borrow().len(), 3);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 3);
    assert_eq!(consumed.get(), 300_000);
}

#[test]
fn interface_paced_pick_two_skips_the_interfaces_a_peer_still_waits_on_when_it_waits_again() {
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let sender = LocalSenderId::from_index;
    let peer = |address: &str, lost: &[usize]| {
        let entry = PathSecretEntry::builder(address.parse().unwrap())
            .socket_sender_count(3)
            .build(None);
        for &idx in lost {
            assert!(entry.set_sender_lost(sender(idx), true));
        }
        entry
    };
    // The first peer's batches go to the first interface's sender alone, and the second peer's
    // to the second interface's. The third peer's batches may go to either of those two senders.
    let first_peer = peer("127.0.0.1:4433", &[1, 2]);
    let second_peer = peer("127.0.0.1:4434", &[0, 2]);
    let third_peer = peer("127.0.0.1:4435", &[2]);
    let mut values = test_items(&first_peer, 2, 100_000, &drop_counter);
    values.extend(test_items(&second_peer, 2, 100_000, &drop_counter));
    values.extend(test_items(&third_peer, 2, 100_000, &drop_counter));
    let consumed = Rc::new(std::cell::Cell::new(0));
    let rx = SharedTestReceiver {
        values,
        consumed: Rc::clone(&consumed),
    };
    let sends: [Sends; 3] = Default::default();
    // The second interface sends a hundredth as fast, so it holds its batch while the first
    // interface frees twice.
    let pacing = SendPacing::new(
        vec![Rate::new(1.0), Rate::new(0.01), Rate::new(1.0)],
        vec![0, 1, 2],
    );
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&sends[0], &sends[1], &sends[2]]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );
    let counts = || [0, 1, 2].map(|idx| sends[idx].borrow().len());

    // The first batch of each of the first two peers goes, and its second waits on its
    // interface. The third peer's first batch waits in the router on the first two interfaces,
    // and its second waits behind it.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(counts(), [1, 1, 0]);
    assert_eq!(router.waiter_entries(), 2);

    // The first interface frees: the router sends the first peer's second batch, and then holds
    // the third peer's first batch on that interface. The third peer's second batch waits again
    // on the first interface, and the third peer stays listed once on the second.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_once(&mut router), Poll::Ready(Some(())));
    assert_eq!(poll_once(&mut router), Poll::Ready(Some(())));
    assert_eq!(poll_once(&mut router), Poll::Pending);
    assert_eq!(counts(), [2, 1, 0]);
    assert_eq!(router.waiter_entries(), 2);

    // The first interface frees again: the router sends the third peer's first batch and holds
    // its second on that interface, and the third peer no longer waits.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(counts(), [3, 1, 0]);

    // Once the second interface frees, the router skips the third peer's entry in its waiters,
    // and ends once each batch went.
    clock.advance(std::time::Duration::from_millis(100));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(counts(), [4, 2, 0]);
    assert_eq!(router.waiter_entries(), 0);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 6);
    assert_eq!(consumed.get(), 600_000);
}

#[test]
fn interface_paced_pick_two_offers_a_waiting_highest_priority_batch_as_an_interface_frees() {
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // The second interface carries the highest-priority traffic. The pair of its sender is lost
    // for the first peer, so the first interface's sender takes that peer's highest-priority
    // batches, paced. The pair of the first interface's sender is lost for the second peer, so
    // the second interface's sender takes each batch of that peer.
    let waits = test_path_secret_entry();
    assert!(waits.set_sender_lost(LocalSenderId::from_index(1), true));
    let other = PathSecretEntry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(2)
        .build(None);
    assert!(other.set_sender_lost(LocalSenderId::from_index(0), true));
    // The input holds two highest-priority batches of the first peer, two other batches of the
    // second peer, and a third highest-priority batch of the first peer.
    let mut values = VecDeque::new();
    for (entry, highest_priority) in [
        (&waits, true),
        (&waits, true),
        (&other, false),
        (&other, false),
        (&waits, true),
    ] {
        let mut item = new_test_item(entry.clone(), drop_counter.clone());
        item.byte_cost = 100_000;
        item.highest_priority = highest_priority;
        values.push_back(item);
    }
    let consumed = Rc::new(std::cell::Cell::new(0));
    let rx = SharedTestReceiver {
        values,
        consumed: Rc::clone(&consumed),
    };
    let first = Sends::default();
    let second = Sends::default();
    // The second interface sends a hundredth as fast, so it holds its batch while the first
    // interface frees.
    let pacing = SendPacing::new(vec![Rate::new(1.0), Rate::new(0.01)], vec![0, 1])
        .with_highest_priority(1);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&first, &second]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    );

    // Each peer's first batch goes at once to its interface, and its second waits on that
    // interface. Each interface holds a batch, so the first peer's third batch waits in the
    // router on the first interface.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(first.borrow().len(), 1);
    assert_eq!(second.borrow().len(), 1);
    assert_eq!(router.waiter_entries(), 1);

    // The pair of the second interface's sender delivers to the first peer again, but the first
    // interface, which the waiting batch waits on, still holds a batch: the poll doesn't offer
    // the waiting batch again, so the second interface's sender doesn't get it.
    assert!(waits.set_sender_lost(LocalSenderId::from_index(1), false));
    assert_eq!(poll_once(&mut router), Poll::Pending);
    assert_eq!(
        second.borrow().len(),
        1,
        "the router offered the waiting batch before an interface it waits on freed",
    );

    // The pacer releases the first peer's second batch to the first interface's sender, and the
    // first interface frees.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_once(&mut router), Poll::Ready(Some(())));
    assert_eq!(first.borrow().len(), 2);

    // The router offers the waiting batch, which goes at once to the interface for the
    // highest-priority traffic now that its sender delivers to the first peer.
    assert_eq!(poll_once(&mut router), Poll::Ready(Some(())));
    assert_eq!(
        second.borrow().len(),
        2,
        "the waiting batch didn't go to the interface for the highest-priority traffic",
    );
    assert_eq!(first.borrow().len(), 2);

    // Once the second interface may send again, its held batch goes, and the router ends.
    clock.advance(std::time::Duration::from_secs(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(first.borrow().len(), 2);
    assert_eq!(second.borrow().len(), 3);
    assert_eq!(router.waiter_entries(), 0);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 5);
    assert_eq!(consumed.get(), 500_000);
}

#[test]
fn interface_paced_pick_two_routes_another_peers_highest_priority_batch_while_one_waits() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // The second interface carries the highest-priority traffic. The pair of its sender is lost
    // for the first peer alone, so the first interface's sender takes each batch of that peer,
    // paced.
    let waits = test_path_secret_entry();
    assert!(waits.set_sender_lost(LocalSenderId::from_index(1), true));
    let other = PathSecretEntry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(2)
        .build(None);
    // The input of the highest priority level holds three batches of the first peer, each of
    // more bytes than a burst and a byte more than the one before it, so that the sender's
    // record shows their order, then a batch of the other peer and a fourth batch of the first
    // peer. The bulk input is closed and empty.
    let highest_rx = ClosableReceiver::default();
    for (entry, byte_cost) in [
        (&waits, 100_000),
        (&waits, 100_001),
        (&waits, 100_002),
        (&other, 1_000),
        (&waits, 100_003),
    ] {
        let mut item = new_test_item(entry.clone(), drop_counter.clone());
        item.byte_cost = byte_cost;
        item.highest_priority = true;
        highest_rx.values.borrow_mut().push_back(item);
    }
    let rx = ClosableReceiver::default();
    rx.closed.set(true);
    let first = Sends::default();
    let second = Sends::default();
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]).with_highest_priority(1);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&first, &second]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    )
    .with_highest_priority_input(highest_rx.clone());
    let costs = |sends: &Sends| {
        sends
            .borrow()
            .iter()
            .map(|&(_, cost)| cost)
            .collect::<Vec<_>>()
    };
    let first_peer_costs = |count: u64| {
        (0..count)
            .map(|offset| 100_000 + offset)
            .collect::<Vec<_>>()
    };

    // The first peer's first batch goes at once to the first interface, its second waits on
    // that interface, and its third waits in the router. The router reads on meanwhile: the
    // other peer's batch goes at once to the free sender of the second interface, and the first
    // peer's fourth batch waits behind its third. The router consumed each batch.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), first_peer_costs(1));
    assert_eq!(costs(&second), [1_000]);
    assert!(highest_rx.values.borrow().is_empty());
    assert_eq!(highest_rx.consumed.get(), 401_006);

    // The router doesn't end while the first peer's batches wait in it, though both of its
    // inputs are closed.
    highest_rx.closed.set(true);
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);

    // The first peer's batches go on to the first interface in the order the router read them.
    for count in 2..4 {
        clock.advance(std::time::Duration::from_millis(1));
        assert_eq!(poll_until_idle(&mut router), Poll::Pending);
        assert_eq!(costs(&first), first_peer_costs(count));
    }
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(costs(&first), first_peer_costs(4));
    assert_eq!(costs(&second), [1_000]);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 5);
    assert_eq!(
        highest_rx.consumed.get(),
        401_006,
        "the router consumed a batch twice"
    );
}

#[test]
fn interface_paced_pick_two_returns_the_credit_of_waiting_highest_priority_batches() {
    const CAP: u64 = 1_000_000;
    const CREDIT: u64 = 4096;

    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let pool = crate::sync::Arc::new(crate::credit::Pool::new(crate::credit::Config::new(CAP)));
    // The second interface carries the highest-priority traffic, and the pair of its sender is
    // lost for both peers, so the first interface's sender takes each batch of theirs.
    let first_peer = test_path_secret_entry();
    let second_peer = PathSecretEntry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(2)
        .build(None);
    for entry in [&first_peer, &second_peer] {
        assert!(entry.set_sender_lost(LocalSenderId::from_index(1), true));
    }
    let highest_rx = ClosableReceiver::default();
    {
        let mut values = highest_rx.values.borrow_mut();
        values.extend(test_items(&first_peer, 3, 100_000, &drop_counter));
        values.extend(test_items(&second_peer, 1, 100_000, &drop_counter));
        for value in values.iter_mut() {
            value.highest_priority = true;
        }
        values[2].flow_credits = CREDIT;
        values[3].flow_credits = CREDIT;
    }
    highest_rx.closed.set(true);
    let rx = ClosableReceiver::default();
    rx.closed.set(true);
    let gone = Rc::new(std::cell::Cell::new(false));
    let senders: Vec<_> = (0..2)
        .map(|_| GoneSender {
            gone: Rc::clone(&gone),
        })
        .collect();
    let rate = Rate::new(1.0);
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]).with_highest_priority(1);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        senders.into(),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        pool.clone(),
    )
    .with_highest_priority_input(highest_rx.clone());

    // The first peer's first batch goes, its second waits on the first interface, and its third
    // waits in the router. The router reads the second peer's batch while that one waits, and
    // that batch waits in the router too.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert!(highest_rx.values.borrow().is_empty());
    assert_eq!(highest_rx.consumed.get(), 400_000);

    // Once the worker is gone, the router returns the credit of the waiting batch of each peer.
    gone.set(true);
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(drop_counter.load(Ordering::Relaxed), 4);
    // The credit wasn't drawn from the pool, so its return shows as a balance of
    // `CAP + 2 * CREDIT`.
    assert_eq!(
        pool.debug_available() as u64 + pool.debug_returned(),
        CAP + 2 * CREDIT,
        "the router didn't return the credit of each waiting batch to the pool",
    );
    // The router stays ended, and doesn't offer the batches of a peer it no longer holds.
    assert_eq!(poll_once(&mut router), Poll::Ready(None));
}

#[test]
fn interface_paced_pick_two_sends_a_waiting_highest_input_batch_unpaced_once_its_pair_delivers() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // The second interface carries the highest-priority traffic. The pair of its sender is lost
    // for the first peer alone, so the first interface's sender takes the batches of that peer.
    let waits = test_path_secret_entry();
    assert!(waits.set_sender_lost(LocalSenderId::from_index(1), true));
    let other = PathSecretEntry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(2)
        .build(None);
    // The input of the highest priority level holds two batches of the first peer, each of more
    // bytes than a burst, a third batch of that peer, which isn't marked as of that level, and a
    // batch of the other peer of more bytes than a burst.
    let highest_rx = ClosableReceiver::default();
    for (entry, byte_cost, highest_priority) in [
        (&waits, 100_000, true),
        (&waits, 100_001, true),
        (&waits, 2, false),
        (&other, 200_000, true),
    ] {
        let mut item = new_test_item(entry.clone(), drop_counter.clone());
        item.byte_cost = byte_cost;
        item.highest_priority = highest_priority;
        highest_rx.values.borrow_mut().push_back(item);
    }
    let rx = ClosableReceiver::default();
    rx.closed.set(true);
    let first = Sends::default();
    let second = Sends::default();
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]).with_highest_priority(1);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&first, &second]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    )
    .with_highest_priority_input(highest_rx.clone());
    let costs = |sends: &Sends| {
        sends
            .borrow()
            .iter()
            .map(|&(_, cost)| cost)
            .collect::<Vec<_>>()
    };

    // The first peer's first batch goes to the first interface, its second waits on that
    // interface, and its third waits in the router. The other peer's batch goes at once to the
    // second interface, and puts that interface's bucket in debt.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [100_000]);
    assert_eq!(costs(&second), [200_000]);

    // Once the pair of the second interface's sender delivers to the first peer again, the
    // router sends that peer's waiting batch at once to that sender, though its interface's
    // bucket is in debt: the batch goes as a batch of the highest priority level, as each batch
    // of that input does, though it isn't marked as of that level.
    assert!(waits.set_sender_lost(LocalSenderId::from_index(1), false));
    assert_eq!(poll_once(&mut router), Poll::Ready(Some(())));
    assert_eq!(costs(&second), [200_000, 2]);

    highest_rx.closed.set(true);
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(costs(&first), [100_000, 100_001]);
    assert_eq!(costs(&second), [200_000, 2]);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 4);
}

#[test]
fn interface_paced_pick_two_ends_when_a_waiting_highest_input_batch_finds_its_worker_gone() {
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // The second interface carries the highest-priority traffic. The pair of its sender is lost
    // for the peer, so the first interface's sender takes each batch of that peer, paced.
    let entry = test_path_secret_entry();
    let sender = LocalSenderId::from_index;
    assert!(entry.set_sender_lost(sender(1), true));
    let highest_rx = ClosableReceiver::default();
    {
        let mut values = highest_rx.values.borrow_mut();
        values.extend(test_items(&entry, 3, 100_000, &drop_counter));
        for value in values.iter_mut() {
            value.highest_priority = true;
        }
    }
    let rx = ClosableReceiver::default();
    rx.closed.set(true);
    let gone = Rc::new(std::cell::Cell::new(false));
    let senders: Vec<_> = (0..2)
        .map(|_| GoneSender {
            gone: Rc::clone(&gone),
        })
        .collect();
    let rate = Rate::new(1.0);
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]).with_highest_priority(1);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        senders.into(),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    )
    .with_highest_priority_input(highest_rx.clone());

    // The first batch goes, the second waits on the first interface, and the third waits in the
    // router. The input stays open.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(highest_rx.consumed.get(), 300_000);

    // Once the pair of the second interface's sender delivers again, the waiting batch goes to
    // that sender, whose receiver is gone: the router ends in that poll, and doesn't read the
    // batch the input holds now.
    let mut next = new_test_item(entry.clone(), drop_counter.clone());
    next.highest_priority = true;
    highest_rx.values.borrow_mut().push_back(next);
    assert!(entry.set_sender_lost(sender(1), false));
    gone.set(true);
    assert_eq!(poll_once(&mut router), Poll::Ready(None));
    assert_eq!(highest_rx.values.borrow().len(), 1);
    assert_eq!(highest_rx.consumed.get(), 300_000);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 3);
}

#[test]
fn interface_paced_pick_two_routes_another_peers_highest_input_batch_without_a_chosen_interface() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // The pacing doesn't have an interface for the highest-priority traffic, so the router routes
    // the batches of that level's input as any other, while an interface doesn't hold a batch.
    // The pair of the second interface's sender is lost for the first peer alone, so the first
    // interface's sender takes each batch of that peer, paced.
    let waits = test_path_secret_entry();
    assert!(waits.set_sender_lost(LocalSenderId::from_index(1), true));
    let other = PathSecretEntry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(2)
        .build(None);
    // The input of the highest priority level holds three batches of the first peer, each of
    // more bytes than a burst and a byte more than the one before it, so that the sender's
    // record shows their order, then a batch of the other peer and a fourth batch of the first
    // peer. The bulk input is closed and empty.
    let highest_rx = ClosableReceiver::default();
    for (entry, byte_cost) in [
        (&waits, 100_000),
        (&waits, 100_001),
        (&waits, 100_002),
        (&other, 1_000),
        (&waits, 100_003),
    ] {
        let mut item = new_test_item(entry.clone(), drop_counter.clone());
        item.byte_cost = byte_cost;
        item.highest_priority = true;
        highest_rx.values.borrow_mut().push_back(item);
    }
    let rx = ClosableReceiver::default();
    rx.closed.set(true);
    let first = Sends::default();
    let second = Sends::default();
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&first, &second]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    )
    .with_highest_priority_input(highest_rx.clone());
    let costs = |sends: &Sends| {
        sends
            .borrow()
            .iter()
            .map(|&(_, cost)| cost)
            .collect::<Vec<_>>()
    };
    let first_peer_costs = |count: u64| {
        (0..count)
            .map(|offset| 100_000 + offset)
            .collect::<Vec<_>>()
    };

    // The first peer's first batch goes at once to the first interface, and its second waits on
    // that interface. Its third waits in the router: the second interface is free, but its
    // sender is lost for that peer. The router reads on meanwhile: the other peer's batch goes
    // at once to the free second interface, and the first peer's fourth batch waits behind its
    // third. The router consumed each batch.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), first_peer_costs(1));
    assert_eq!(costs(&second), [1_000]);
    assert!(highest_rx.values.borrow().is_empty());
    assert_eq!(highest_rx.consumed.get(), 401_006);

    // The router doesn't end while the first peer's batches wait in it, though both of its
    // inputs are closed, and they still don't go to the free second interface.
    highest_rx.closed.set(true);
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), first_peer_costs(1));
    assert_eq!(costs(&second), [1_000]);

    // The pacer releases the first peer's second batch, and the router then holds its third on
    // the first interface, which is free again: each of the two polls reports the batch it
    // moved, and the fourth still waits in the router.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_once(&mut router), Poll::Ready(Some(())));
    assert_eq!(poll_once(&mut router), Poll::Ready(Some(())));
    assert_eq!(poll_once(&mut router), Poll::Pending);
    assert_eq!(costs(&first), first_peer_costs(2));

    // The first peer's batches go on to the first interface in the order the router read them,
    // each once that interface is free.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), first_peer_costs(3));
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(costs(&first), first_peer_costs(4));
    assert_eq!(costs(&second), [1_000]);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 5);
    assert_eq!(
        highest_rx.consumed.get(),
        401_006,
        "the router consumed a batch twice"
    );
}

#[test]
fn interface_paced_pick_two_without_a_chosen_interface_routes_a_batch_once_its_mark_clears() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // The pacing doesn't have an interface for the highest-priority traffic, so the router routes
    // the batches of that level's input as any other, while an interface doesn't hold a batch.
    // The pair of the second interface's sender is lost for the first peer, until the test clears
    // that mark.
    let waits = test_path_secret_entry();
    assert!(waits.set_sender_lost(LocalSenderId::from_index(1), true));
    let other = PathSecretEntry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(2)
        .build(None);
    // The input of the highest priority level holds three batches of the first peer, each of
    // more bytes than a burst and a byte more than the one before it, so that the sender's
    // record shows their order, then a batch of the other peer and a fourth batch of the first
    // peer. The bulk input is closed and empty.
    let highest_rx = ClosableReceiver::default();
    for (entry, byte_cost) in [
        (&waits, 100_000),
        (&waits, 100_001),
        (&waits, 100_002),
        (&other, 1_000),
        (&waits, 100_003),
    ] {
        let mut item = new_test_item(entry.clone(), drop_counter.clone());
        item.byte_cost = byte_cost;
        item.highest_priority = true;
        highest_rx.values.borrow_mut().push_back(item);
    }
    let rx = ClosableReceiver::default();
    rx.closed.set(true);
    let first = Sends::default();
    let second = Sends::default();
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&first, &second]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &pacing,
        &registry,
        test_pool(),
    )
    .with_highest_priority_input(highest_rx.clone());
    let costs = |sends: &Sends| {
        sends
            .borrow()
            .iter()
            .map(|&(_, cost)| cost)
            .collect::<Vec<_>>()
    };

    // The first peer's first batch goes at once to the first interface, and its second waits on
    // that interface. Its third and fourth wait in the router, though the second interface is
    // free, since that interface's sender is lost for that peer. The other peer's batch goes at
    // once to the second interface.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [100_000]);
    assert_eq!(costs(&second), [1_000]);
    assert!(highest_rx.values.borrow().is_empty());
    assert_eq!(highest_rx.consumed.get(), 401_006);

    // The input of the highest priority level closes, and the router doesn't end while the first
    // peer's batches wait in it.
    highest_rx.closed.set(true);
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [100_000]);
    assert_eq!(costs(&second), [1_000]);

    // Once the pair of the second interface's sender delivers to the first peer again, that
    // peer's waiting batches go to the free second interface, as any other batch would, though
    // the clock didn't move: the third at once, since that interface's bucket still holds
    // tokens, and the fourth waits on that interface for the tokens the third spent.
    assert!(waits.set_sender_lost(LocalSenderId::from_index(1), false));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [100_000]);
    assert_eq!(costs(&second), [1_000, 100_002]);

    // Once each bucket has paid for the batches sent before, the batch each interface holds goes,
    // and the router ends, since no batch waits in it.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(costs(&first), [100_000, 100_001]);
    assert_eq!(costs(&second), [1_000, 100_002, 100_003]);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 5);
    assert_eq!(
        highest_rx.consumed.get(),
        401_006,
        "the router consumed a batch twice"
    );
}
