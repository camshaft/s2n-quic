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
    waited: bool,
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
        waited: false,
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

    // While the pair of sender 0 is lost and no sender delivers since, sender 1, on the same
    // local interface, counts as lost too: the other items go to senders 2 and 3, and the
    // highest-priority items stay on the second interface.
    assert!(entry.set_sender_lost(sender(0), true));
    let counts = route_by_interface(items());
    assert_eq!(counts[0], [0, 0], "sender 0 got an item: {counts:?}");
    assert_eq!(
        counts[1],
        [0, 0],
        "sender 1, on the lost pair's interface, got an item: {counts:?}"
    );
    assert_eq!(counts[2][0] + counts[3][0], 20);
    assert_eq!(counts[2][1] + counts[3][1], 20);

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

    // Once the mark of sender 0 clears, though no sender delivers since the loss, sender 1 no
    // longer counts as lost and gets items again.
    assert!(entry.set_sender_lost(sender(0), false));
    let counts = route_by_interface(items());
    assert_ne!(
        counts[1],
        [0, 0],
        "sender 1 didn't get an item once sender 0's mark cleared: {counts:?}"
    );
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

    // The router reads the marks while sender 0 alone is lost, and sender 1, on the same
    // interface, counts as lost too. Senders 1 and 2 are marked lost after that read and before
    // the router chooses: the router chooses by the marks it read, between senders 2 and 3, and
    // doesn't panic.
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
    assert_eq!(chosen[1], 0, "the router chose sender 1: {chosen:?}");
    assert!(
        chosen[2] > 0,
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

    // While the pair of the second interface's sender stays lost, the second interface is free,
    // but the waiting batch doesn't wait on it: the poll doesn't offer the batch to the senders.
    let offers = router.waiting_offers();
    assert_eq!(poll_once(&mut router), Poll::Pending);
    assert_eq!(
        router.waiting_offers(),
        offers,
        "the router offered the waiting batch though no interface it waits on freed",
    );

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
    assert_eq!(
        router.waiting_offers(),
        1,
        "the router didn't offer the waiting batch exactly once",
    );
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
    let pacing =
        SendPacing::new(vec![Rate::new(1.0), Rate::new(0.01)], vec![0, 1]).with_highest_priority(1);
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
    // The router doesn't keep an entry of the first peer once its batches left it.
    assert_eq!(router.highest_waiter_entries(), 0);
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

#[test]
fn interface_paced_pick_two_offers_a_waiting_highest_input_batch_again_only_once_it_may_go() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // The second interface carries the highest-priority traffic. The pair of its sender is lost
    // for the peer, so the first interface's sender takes the batches of that peer, paced.
    let entry = test_path_secret_entry();
    assert!(entry.set_sender_lost(LocalSenderId::from_index(1), true));
    // The input of the highest priority level holds four batches of the peer, each of more bytes
    // than a burst and a byte more than the one before it, so that the senders' records show
    // their order. The bulk input is closed and empty.
    let highest_rx = ClosableReceiver::default();
    for byte_cost in [100_000, 100_001, 100_002, 100_003] {
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

    // The peer's first batch goes at once to the first interface, and its second waits on that
    // interface. Its third and fourth wait in the router: the first interface holds a batch, and
    // the second interface's sender is lost for that peer. The router lists the peer once on the
    // waiters of the first interface and once on the lost peers of the second sender.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [100_000]);
    assert!(costs(&second).is_empty());
    assert_eq!(router.highest_waiter_entries(), 2);

    // A poll that neither frees the first interface nor clears the mark doesn't offer the
    // waiting batches to the senders again.
    let dispatches = router.dispatches();
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(router.dispatches(), dispatches);
    assert_eq!(costs(&first), [100_000]);

    // Once the pacer releases the peer's second batch, the router offers the waiting batches to
    // the senders again: the third waits on the first interface, and the fourth waits in the
    // router again, which still lists the peer once on each of the two lists.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(router.dispatches(), dispatches + 2);
    assert_eq!(costs(&first), [100_000, 100_001]);
    assert_eq!(router.highest_waiter_entries(), 2);

    // Once the pair of the second interface's sender delivers to the peer again, the fourth
    // batch goes at once to that sender, though the clock didn't move.
    assert!(entry.set_sender_lost(LocalSenderId::from_index(1), false));
    assert_eq!(poll_once(&mut router), Poll::Ready(Some(())));
    assert_eq!(costs(&second), [100_003]);

    // The third batch goes once the first interface's bucket has paid for the second, and the
    // router ends, with each entry of the peer gone from its lists.
    highest_rx.closed.set(true);
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(costs(&first), [100_000, 100_001, 100_002]);
    assert_eq!(costs(&second), [100_003]);
    assert_eq!(router.highest_waiter_entries(), 0);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 4);
    assert_eq!(highest_rx.consumed.get(), 400_006);
}

#[test]
fn interface_paced_pick_two_sends_a_waiting_highest_input_batch_while_its_interface_holds_one() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // The second interface carries the highest-priority traffic. The pair of its sender is lost
    // for the first peer, so the first interface's sender takes the batches of that peer, paced.
    // The pair of the first interface's sender is lost for the other peer, whose bulk batches go
    // to the second interface.
    let waits = test_path_secret_entry();
    assert!(waits.set_sender_lost(LocalSenderId::from_index(1), true));
    let bulk = PathSecretEntry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(2)
        .build(None);
    assert!(bulk.set_sender_lost(LocalSenderId::from_index(0), true));
    // The input of the highest priority level holds three batches of the first peer, each of
    // more bytes than a burst and a byte more than the one before it, so that the senders'
    // records show their order. The bulk input holds two batches of the other peer, each of more
    // bytes than a burst, and is closed.
    let highest_rx = ClosableReceiver::default();
    for byte_cost in [100_000, 100_001, 100_002] {
        let mut item = new_test_item(waits.clone(), drop_counter.clone());
        item.byte_cost = byte_cost;
        item.highest_priority = true;
        highest_rx.values.borrow_mut().push_back(item);
    }
    let rx = ClosableReceiver::default();
    *rx.values.borrow_mut() = test_items(&bulk, 2, 200_000, &drop_counter);
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

    // The first peer's first batch goes at once to the first interface, its second waits on that
    // interface, and its third waits in the router. The other peer's first bulk batch goes at
    // once to the second interface, and its second waits on that interface: each interface
    // holds a batch.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [100_000]);
    assert_eq!(costs(&second), [200_000]);

    // Once the pair of the second interface's sender delivers to the first peer again, the
    // router sends that peer's waiting batch at once to that sender, though its interface holds
    // a batch and its bucket is in debt, since that interface carries the highest-priority
    // traffic.
    assert!(waits.set_sender_lost(LocalSenderId::from_index(1), false));
    assert_eq!(poll_once(&mut router), Poll::Ready(Some(())));
    assert_eq!(costs(&second), [200_000, 100_002]);

    // The batch each interface holds goes once its bucket has paid for the batches sent before,
    // and the router ends.
    highest_rx.closed.set(true);
    clock.advance(std::time::Duration::from_millis(2));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(costs(&first), [100_000, 100_001]);
    assert_eq!(costs(&second), [200_000, 100_002, 200_000]);
    assert_eq!(router.highest_waiter_entries(), 0);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 5);
}

#[test]
fn interface_paced_pick_two_lists_a_waiting_highest_input_peer_once_on_each_interface() {
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
    // more bytes than a burst and a byte more than the one before it, then a batch of the other
    // peer and two more batches of the first peer. The bulk input is closed and empty.
    let highest_rx = ClosableReceiver::default();
    for (entry, byte_cost) in [
        (&waits, 100_000),
        (&waits, 100_001),
        (&waits, 100_002),
        (&other, 1_000),
        (&waits, 100_003),
        (&waits, 100_004),
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
    // that interface. Its later batches wait in the router, which lists the peer on the waiters
    // of the first interface and on the lost peers of the second sender. The other peer's batch
    // goes at once to the second interface.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [100_000]);
    assert_eq!(costs(&second), [1_000]);
    assert_eq!(router.highest_waiter_entries(), 2);

    // Once the mark clears, the first peer's third batch goes at once to the free second
    // interface, its fourth waits on that interface, and its fifth waits in the router again.
    // The router lists the peer on the waiters of the second interface, and not a second time on
    // those of the first, where it still waits.
    assert!(waits.set_sender_lost(LocalSenderId::from_index(1), false));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [100_000]);
    assert_eq!(costs(&second), [1_000, 100_002]);
    assert_eq!(router.highest_waiter_entries(), 2);

    // Once each bucket has paid for the batches sent before, the batch each interface holds goes,
    // and the fifth batch then waits on the interface the router chose for it. Once that
    // interface's bucket has paid for the batch before it, the fifth batch goes too, and the
    // router ends, with each entry of the peer gone from its lists.
    highest_rx.closed.set(true);
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(router.highest_waiter_entries(), 0);
    let mut sent = costs(&first);
    sent.extend(costs(&second));
    sent.sort_unstable();
    assert_eq!(sent, [1_000, 100_000, 100_001, 100_002, 100_003, 100_004]);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 6);
    assert_eq!(highest_rx.consumed.get(), 501_010);
}

#[test]
fn interface_paced_pick_two_offers_a_waiting_highest_input_batch_only_on_a_free_interface() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // Three interfaces, each with one sender, and no interface for the highest-priority traffic.
    // The pairs of the second and third senders are lost for the first peer and for the third
    // peer, so that the first sender takes their batches, and those of the first and third
    // senders are lost for the second peer, so that the second sender takes its batches.
    let waits = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(3)
        .build(None);
    let other = PathSecretEntry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(3)
        .build(None);
    let later = PathSecretEntry::builder("127.0.0.1:4435".parse().unwrap())
        .socket_sender_count(3)
        .build(None);
    for (entry, lost) in [(&waits, [1, 2]), (&other, [0, 2]), (&later, [1, 2])] {
        for sender in lost {
            assert!(entry.set_sender_lost(LocalSenderId::from_index(sender), true));
        }
    }
    // The input of the highest priority level holds three batches of the first peer, each of
    // more bytes than a burst and a byte more than the one before it, then two batches of the
    // second peer, each of more bytes than a burst. The bulk input is closed and empty.
    let highest_rx = ClosableReceiver::default();
    for (entry, byte_cost) in [
        (&waits, 100_000),
        (&waits, 100_001),
        (&waits, 100_002),
        (&other, 200_000),
        (&other, 200_001),
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
    let third = Sends::default();
    let pacing = SendPacing::new(vec![rate, rate, rate], vec![0, 1, 2]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&first, &second, &third]),
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

    // The first peer's first batch goes at once to the first interface, its second waits on
    // that interface, and its third waits in the router, which lists the peer on the waiters of
    // the first interface and on the lost peers of the second and third senders. The second
    // peer's first batch goes at once to the second interface, and its second waits on that
    // interface. The third interface is free.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [100_000]);
    assert_eq!(costs(&second), [200_000]);
    assert!(costs(&third).is_empty());
    assert_eq!(router.highest_waiter_entries(), 3);

    // The second sender's pair delivers to the first peer again, but that sender's interface
    // holds a batch, so the router doesn't offer the first peer's batch to the senders.
    let dispatches = router.dispatches();
    assert!(waits.set_sender_lost(LocalSenderId::from_index(1), false));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(router.dispatches(), dispatches);

    // The third sender's pair delivers to the first peer again, and its interface is free: the
    // first peer's waiting batch goes at once to it. The router keeps the peer's entries on the
    // waiters of the first interface and on the lost peers of the second sender.
    assert!(waits.set_sender_lost(LocalSenderId::from_index(2), false));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&third), [100_002]);
    assert_eq!(router.highest_waiter_entries(), 2);

    // A batch of the third peer waits in the router, which lists that peer on the waiters of the
    // first interface behind the first peer's entry, and on the lost peers of the second and
    // third senders.
    let mut item = new_test_item(later.clone(), drop_counter.clone());
    item.byte_cost = 300_000;
    item.highest_priority = true;
    highest_rx.values.borrow_mut().push_back(item);
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(router.highest_waiter_entries(), 5);

    // Once the pacer releases the first peer's second batch, the router skips the first peer's
    // entry on the first interface's waiters and offers the third peer's batch, which waits on
    // that interface for its tokens, in the same poll.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [100_000, 100_001]);
    assert_eq!(router.highest_waiter_entries(), 2);

    // Once each bucket has paid for the batches sent before, the batch each interface holds goes,
    // and the router ends, with each entry gone from its lists.
    highest_rx.closed.set(true);
    clock.advance(std::time::Duration::from_millis(2));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(costs(&first), [100_000, 100_001, 300_000]);
    assert_eq!(costs(&second), [200_000, 200_001]);
    assert_eq!(costs(&third), [100_002]);
    assert_eq!(router.highest_waiter_entries(), 0);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 6);
    assert_eq!(highest_rx.consumed.get(), 1_000_004);
}

#[test]
fn interface_paced_pick_two_empties_its_highest_input_lists_when_its_worker_is_gone() {
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // The second interface carries the highest-priority traffic. The pair of its sender is lost
    // for both peers, so the first interface's sender takes each batch of theirs, paced.
    let first_peer = test_path_secret_entry();
    let second_peer = PathSecretEntry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(2)
        .build(None);
    for entry in [&first_peer, &second_peer] {
        assert!(entry.set_sender_lost(LocalSenderId::from_index(1), true));
    }
    // The input of the highest priority level holds a batch of the first peer of more bytes than
    // a burst, two small batches of that peer and a small batch of the second peer. It stays
    // open.
    let highest_rx = ClosableReceiver::default();
    for (entry, byte_cost) in [
        (&first_peer, 100_000),
        (&first_peer, 2),
        (&first_peer, 3),
        (&second_peer, 4),
    ] {
        let mut item = new_test_item(entry.clone(), drop_counter.clone());
        item.byte_cost = byte_cost;
        item.highest_priority = true;
        highest_rx.values.borrow_mut().push_back(item);
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

    // The first peer's first batch goes, its second waits on the first interface for that
    // interface's tokens, and its third waits in the router, as does the second peer's batch.
    // The router lists both peers on the waiters of the first interface and on the lost peers of
    // the second sender.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(router.highest_waiter_entries(), 4);

    // The pacer releases the first peer's second batch, which leaves tokens in the first
    // interface's bucket.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_once(&mut router), Poll::Ready(Some(())));

    // The router offers the first peer's third batch to the freed first interface, whose
    // sender's receiver is gone now: the router ends in that poll, though its input is open, and
    // empties both lists.
    gone.set(true);
    assert_eq!(poll_once(&mut router), Poll::Ready(None));
    assert_eq!(router.highest_waiter_entries(), 0);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 4);
}

#[test]
fn interface_paced_pick_two_lists_a_highest_input_peer_that_waits_again_once_on_each_list() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // Three interfaces, each with one sender, and no interface for the highest-priority traffic.
    // The pairs of the second and third senders are lost for the first peer, so that the first
    // sender takes its batches, and those of the first and third senders are lost for the second
    // peer, so that the second sender takes its batches.
    let waits = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(3)
        .build(None);
    let other = PathSecretEntry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(3)
        .build(None);
    for (entry, lost) in [(&waits, [1, 2]), (&other, [0, 2])] {
        for sender in lost {
            assert!(entry.set_sender_lost(LocalSenderId::from_index(sender), true));
        }
    }
    // The input of the highest priority level holds three batches of the first peer: one of
    // more bytes than a burst, one of many bursts and one more of more bytes than a burst. Then
    // it holds two batches of the second peer, each of more bytes than a burst. The bulk input
    // is closed and empty.
    let highest_rx = ClosableReceiver::default();
    for (entry, byte_cost) in [
        (&waits, 100_000),
        (&waits, 2_000_000),
        (&waits, 100_002),
        (&other, 300_000),
        (&other, 300_001),
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
    let third = Sends::default();
    let pacing = SendPacing::new(vec![rate, rate, rate], vec![0, 1, 2]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&first, &second, &third]),
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

    // The first peer's first batch goes at once to the first interface, its second waits on
    // that interface, and its third waits in the router, which lists the peer on the waiters of
    // the first interface and on the lost peers of the second and third senders. The second
    // peer's first batch goes at once to the second interface, and its second waits on that
    // interface. The third interface is free.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [100_000]);
    assert_eq!(costs(&second), [300_000]);
    assert!(costs(&third).is_empty());
    assert_eq!(router.highest_waiter_entries(), 3);

    // Once the pacer releases the first peer's second batch, the router offers the peer's third
    // batch to the freed first interface, which holds it for its tokens, and the peer's batches
    // leave the router. The free third sender's lost peers drop the peer's entry, and its entry
    // on the lost peers of the second sender stays, since that sender's interface holds a batch.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [100_000, 2_000_000]);
    assert_eq!(router.highest_waiter_entries(), 1);

    // A fourth batch of the first peer waits in the router. The router lists the peer on the
    // waiters of the first interface and on the lost peers of the third sender, and the peer's
    // entry of the earlier wait on the lost peers of the second sender stands for this wait, so
    // that each list holds the peer once.
    let dispatches = router.dispatches();
    let mut item = new_test_item(waits.clone(), drop_counter.clone());
    item.byte_cost = 100_003;
    item.highest_priority = true;
    highest_rx.values.borrow_mut().push_back(item);
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(router.dispatches(), dispatches + 1);
    assert_eq!(router.highest_waiter_entries(), 3);

    // The second sender's pair delivers to the first peer again while that sender's interface
    // holds a batch: the router doesn't offer the peer's batch to the senders.
    assert!(waits.set_sender_lost(LocalSenderId::from_index(1), false));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(router.dispatches(), dispatches + 1);

    // Once the pacer releases the second peer's second batch, the router reads the cleared mark
    // of the second sender for this wait of the first peer, and offers the peer's batch, which
    // the freed second interface holds for its tokens. The free third sender's lost peers drop
    // the peer's entry, and its entry on the waiters of the first interface, which holds a
    // batch, stays.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(router.dispatches(), dispatches + 2);
    assert_eq!(router.highest_waiter_entries(), 1);

    // The second interface sends the first peer's fourth batch once its bucket has paid for the
    // batch before it.
    clock.advance(std::time::Duration::from_millis(3));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [100_000, 2_000_000]);
    assert_eq!(costs(&second), [300_000, 300_001, 100_003]);

    // Once the first interface's bucket has paid for the first peer's second batch, the batch
    // that interface holds goes, and the router ends, with the peer's entry gone from the first
    // interface's waiters.
    highest_rx.closed.set(true);
    clock.advance(std::time::Duration::from_millis(20));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(costs(&first), [100_000, 2_000_000, 100_002]);
    assert_eq!(costs(&second), [300_000, 300_001, 100_003]);
    assert!(costs(&third).is_empty());
    assert_eq!(router.highest_waiter_entries(), 0);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 6);
    assert_eq!(highest_rx.consumed.get(), 2_900_006);
}

#[test]
fn interface_paced_pick_two_skips_an_entry_of_an_earlier_wait_on_a_freed_interface() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // Three interfaces, each with one sender, and no interface for the highest-priority traffic.
    // The pair of the third sender is lost for the first peer, so that the first and second
    // senders take its batches, and those of the second and third senders are lost for the
    // second peer, so that the first sender takes its batches.
    let waits = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(3)
        .build(None);
    let other = PathSecretEntry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(3)
        .build(None);
    assert!(waits.set_sender_lost(LocalSenderId::from_index(2), true));
    for sender in [1, 2] {
        assert!(other.set_sender_lost(LocalSenderId::from_index(sender), true));
    }
    // The input of the highest priority level holds two batches of the second peer, each of
    // many bursts, then three batches of the first peer: one of more bytes than a burst, one of
    // many bursts and one more of more bytes than a burst. The bulk input is closed and empty.
    let highest_rx = ClosableReceiver::default();
    for (entry, byte_cost) in [
        (&other, 1_000_000),
        (&other, 1_000_001),
        (&waits, 100_000),
        (&waits, 2_000_000),
        (&waits, 100_002),
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
    let third = Sends::default();
    let pacing = SendPacing::new(vec![rate, rate, rate], vec![0, 1, 2]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&first, &second, &third]),
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

    // The second peer's first batch goes at once to the first interface, and its second waits
    // on that interface. The first peer's first batch goes at once to the second interface, its
    // second waits on that interface, and its third waits in the router, which lists the peer
    // on the waiters of the first and second interfaces and on the lost peers of the third
    // sender.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [1_000_000]);
    assert_eq!(costs(&second), [100_000]);
    assert!(costs(&third).is_empty());
    assert_eq!(router.highest_waiter_entries(), 3);

    // Once the pacer releases the first peer's second batch, the router offers the peer's third
    // batch to the freed second interface, which holds it for its tokens, and the peer's batches
    // leave the router. The free third sender's lost peers drop the peer's entry, and its entry
    // on the waiters of the first interface stays, since that interface holds a batch.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&second), [100_000, 2_000_000]);
    assert_eq!(router.highest_waiter_entries(), 1);

    // The first sender's pair is lost for the first peer now, and a fourth batch of that peer
    // waits in the router, on the lost peers of the first and third senders and on the waiters
    // of the second interface. The peer's entry of the earlier wait on the waiters of the first
    // interface stays.
    assert!(waits.set_sender_lost(LocalSenderId::from_index(0), true));
    let mut item = new_test_item(waits.clone(), drop_counter.clone());
    item.byte_cost = 100_003;
    item.highest_priority = true;
    highest_rx.values.borrow_mut().push_back(item);
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(router.highest_waiter_entries(), 4);

    // Once the pacer releases the second peer's second batch, the router takes the first peer's
    // entry of the earlier wait from the waiters of the freed first interface, and doesn't offer
    // the peer's batch, which the sender of that interface may not take.
    let dispatches = router.dispatches();
    clock.advance(std::time::Duration::from_millis(7));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [1_000_000, 1_000_001]);
    assert_eq!(router.dispatches(), dispatches);
    assert_eq!(router.highest_waiter_entries(), 3);

    // Once the pacer releases the first peer's third batch, the router offers the peer's fourth
    // batch to the freed second interface, which holds it for its tokens, and the free first and
    // third senders' lost peers drop the peer's entries.
    highest_rx.closed.set(true);
    clock.advance(std::time::Duration::from_millis(10));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(router.dispatches(), dispatches + 1);
    assert_eq!(router.highest_waiter_entries(), 0);

    // Once the second interface's bucket has paid for the batch before it, the batch that
    // interface holds goes, and the router ends.
    clock.advance(std::time::Duration::from_millis(10));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(costs(&first), [1_000_000, 1_000_001]);
    assert_eq!(costs(&second), [100_000, 2_000_000, 100_002, 100_003]);
    assert!(costs(&third).is_empty());
    assert_eq!(router.highest_waiter_entries(), 0);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 6);
    assert_eq!(highest_rx.consumed.get(), 4_300_006);
}

#[test]
fn interface_paced_pick_two_skips_an_entry_left_when_a_waiting_peer_waits_again_on_other_lists() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // Three interfaces, each with one sender, and no interface for the highest-priority traffic.
    // The pair of the third sender is lost for the first peer, so that the first and second
    // senders take its batches, and those of the second and third senders are lost for the
    // second peer, so that the first sender takes its batches.
    let waits = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(3)
        .build(None);
    let other = PathSecretEntry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(3)
        .build(None);
    assert!(waits.set_sender_lost(LocalSenderId::from_index(2), true));
    for sender in [1, 2] {
        assert!(other.set_sender_lost(LocalSenderId::from_index(sender), true));
    }
    // The input of the highest priority level holds two batches of the second peer, each of
    // many bursts, then four batches of the first peer: one of more bytes than a burst, one of
    // many bursts and two more of more bytes than a burst. The bulk input is closed and empty.
    let highest_rx = ClosableReceiver::default();
    for (entry, byte_cost) in [
        (&other, 1_000_000),
        (&other, 1_000_001),
        (&waits, 100_000),
        (&waits, 2_000_000),
        (&waits, 100_002),
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
    let third = Sends::default();
    let pacing = SendPacing::new(vec![rate, rate, rate], vec![0, 1, 2]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&first, &second, &third]),
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

    // The second peer's first batch goes at once to the first interface, and its second waits
    // on that interface. The first peer's first batch goes at once to the second interface, its
    // second waits on that interface, and its third and fourth wait in the router, which lists
    // the peer on the waiters of the first and second interfaces and on the lost peers of the
    // third sender.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [1_000_000]);
    assert_eq!(costs(&second), [100_000]);
    assert!(costs(&third).is_empty());
    assert_eq!(router.highest_waiter_entries(), 3);

    // The first sender's pair is lost for the first peer now. Once the pacer releases the
    // peer's second batch, the router offers the peer's third batch to the freed second
    // interface, which holds it for its tokens. No sender may take the peer's fourth batch,
    // which begins a new wait, on the waiters of the second interface and on the lost peers of
    // the first and third senders. The peer's entry of the earlier wait on the waiters of the
    // first interface stays.
    assert!(waits.set_sender_lost(LocalSenderId::from_index(0), true));
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&second), [100_000, 2_000_000]);
    assert_eq!(router.highest_waiter_entries(), 4);

    // Once the pacer releases the second peer's second batch, the router takes the first peer's
    // entry of the earlier wait from the waiters of the freed first interface, and doesn't offer
    // the peer's batch, which the sender of that interface may not take.
    let dispatches = router.dispatches();
    clock.advance(std::time::Duration::from_millis(7));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(costs(&first), [1_000_000, 1_000_001]);
    assert_eq!(router.dispatches(), dispatches);
    assert_eq!(router.highest_waiter_entries(), 3);

    // Once the pacer releases the first peer's third batch, the router offers the peer's fourth
    // batch to the freed second interface, which holds it for its tokens, and the free first and
    // third senders' lost peers drop the peer's entries.
    highest_rx.closed.set(true);
    clock.advance(std::time::Duration::from_millis(10));
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(router.dispatches(), dispatches + 1);
    assert_eq!(router.highest_waiter_entries(), 0);

    // Once the second interface's bucket has paid for the batch before it, the batch that
    // interface holds goes, and the router ends.
    clock.advance(std::time::Duration::from_millis(10));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(costs(&first), [1_000_000, 1_000_001]);
    assert_eq!(costs(&second), [100_000, 2_000_000, 100_002, 100_003]);
    assert!(costs(&third).is_empty());
    assert_eq!(router.highest_waiter_entries(), 0);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 6);
    assert_eq!(highest_rx.consumed.get(), 4_300_006);
}

#[test]
fn interface_paced_pick_two_keeps_a_waiting_highest_input_batch_while_its_sender_counts_as_lost() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // Two interfaces, the first with two senders and the second with one, and no interface for
    // the highest-priority traffic. The pair of the first sender is lost for the peer, so the
    // router counts the second sender, which shares the first sender's interface, as lost too,
    // and the third sender takes the peer's batches.
    let entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(3)
        .build(None);
    assert!(entry.set_sender_lost(LocalSenderId::from_index(0), true));
    // The input of the highest priority level holds three batches of the peer, each of more
    // bytes than a burst and a byte more than the one before it. The bulk input is closed and
    // empty.
    let highest_rx = ClosableReceiver::default();
    for byte_cost in [100_000, 100_001, 100_002] {
        let mut item = new_test_item(entry.clone(), drop_counter.clone());
        item.byte_cost = byte_cost;
        item.highest_priority = true;
        highest_rx.values.borrow_mut().push_back(item);
    }
    let rx = ClosableReceiver::default();
    rx.closed.set(true);
    let first = Sends::default();
    let second = Sends::default();
    let third = Sends::default();
    let pacing = SendPacing::new(vec![rate, rate], vec![0, 0, 1]);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        rx,
        timed_senders(&clock, &[&first, &second, &third]),
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

    // The peer's first batch goes at once to the third sender, its second waits on the second
    // interface, and its third waits in the router, which lists the peer on the waiters of the
    // second interface and on the lost peers of the first and second senders.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert!(costs(&first).is_empty());
    assert!(costs(&second).is_empty());
    assert_eq!(costs(&third), [100_000]);
    assert_eq!(router.highest_waiter_entries(), 3);

    // The first interface is free, but the marks still count both of its senders as lost: the
    // second sender isn't marked, and shares the first sender's interface. So the router doesn't
    // offer the peer's batch to the senders again.
    let dispatches = router.dispatches();
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(router.dispatches(), dispatches);
    assert_eq!(router.highest_waiter_entries(), 3);

    // Once the first sender's pair delivers to the peer again, neither sender of the first
    // interface counts as lost, and the router offers the peer's batch once, which goes at once
    // to one of them. The peer's entry stays on the waiters of the second interface.
    assert!(entry.set_sender_lost(LocalSenderId::from_index(0), false));
    assert_eq!(poll_once(&mut router), Poll::Ready(Some(())));
    assert_eq!(router.dispatches(), dispatches + 1);
    let mut sent = costs(&first);
    sent.extend(costs(&second));
    assert_eq!(sent, [100_002]);
    assert_eq!(router.highest_waiter_entries(), 1);

    // Once the second interface's bucket has paid for the batch before it, the batch that
    // interface holds goes, and the router ends, with each entry gone from its lists.
    highest_rx.closed.set(true);
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(costs(&third), [100_000, 100_001]);
    assert_eq!(router.highest_waiter_entries(), 0);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 3);
    assert_eq!(highest_rx.consumed.get(), 300_003);
}

// -- CreditlessWindow and WindowSender ------------------------------------------------------

impl CreditlessCost for TestItem {
    fn creditless_bytes(&self) -> u64 {
        if self.flow_credits == 0 {
            self.byte_cost
        } else {
            0
        }
    }

    fn waited(&self) -> bool {
        self.waited
    }

    fn set_waited(&mut self) {
        self.waited = true;
    }
}

/// The window admits a frame while it holds nothing, refuses one past its limit, and grows its
/// limit, once for each refusal, where a worker could take a batch that the batch router doesn't
/// have.
#[test]
fn creditless_window_admits_a_frame_while_empty_and_grows_where_a_worker_could_take_a_batch() {
    let window = CreditlessWindow::new(100);
    // A clone stands for the batch router's, so that the window counts the frames.
    let _router = window.clone();
    // A worker that could take a batch while the window didn't refuse a frame leaves the limit as
    // it is.
    window.starved();
    assert_eq!(window.limit(), 100);
    assert!(
        window.admit(1000),
        "the window refused a frame while it held nothing"
    );
    assert!(
        !window.admit(1),
        "the window admitted a frame past its limit of 100"
    );
    assert_eq!(window.held(), 1000);

    // Before a worker's sender takes a batch, the limit grows by the minimum, once for the
    // refusal.
    window.starved();
    assert_eq!(window.limit(), 200);
    window.starved();
    assert_eq!(window.limit(), 200, "the limit grew twice for one refusal");

    // A batch that didn't wait leaves the limit as it is.
    window.release(1000, false);
    assert_eq!(window.held(), 0);
    assert_eq!(window.limit(), 200);
    assert!(window.admit(1000));
    assert!(!window.admit(1));

    // After that, the limit grows by the bytes of the last batch a worker's sender took.
    window.starved();
    assert_eq!(window.limit(), 1200);
    // A frame the window admits ends the refusal.
    assert!(window.admit(200));
    window.starved();
    assert_eq!(window.limit(), 1200);
    assert_eq!(window.held(), 1200);
}

/// The window halves its limit as a worker's sender takes a batch that waited for a sender, once
/// for the bytes the window held then.
#[test]
fn creditless_window_halves_its_limit_once_for_the_bytes_it_held_as_a_batch_waited() {
    let window = CreditlessWindow::with_limit(1000);
    let _router = window.clone();
    for _ in 0..4 {
        assert!(window.admit(100));
    }

    window.release(100, true);
    assert_eq!(window.limit(), 500);
    // The window held 300 bytes as its limit halved: the batches that release them don't halve
    // it again, whether they waited or not.
    window.release(100, true);
    window.release(100, false);
    window.release(100, true);
    assert_eq!(
        window.limit(),
        500,
        "the limit halved again for the bytes it held"
    );
    assert_eq!(window.held(), 0);

    assert!(window.admit(100));
    window.release(100, true);
    assert_eq!(window.limit(), 250);

    // A release of no bytes leaves the limit as it is.
    window.release(0, true);
    assert_eq!(window.limit(), 250);
}

/// A window made with a minimum starts its limit at the minimum and doesn't halve it below.
#[test]
fn creditless_window_starts_at_its_minimum_and_doesnt_halve_below_it() {
    let window = CreditlessWindow::new(1000);
    let _router = window.clone();
    assert!(window.admit(600));
    assert!(window.admit(400));
    assert!(
        !window.admit(1),
        "the window admitted a frame past its minimum"
    );

    window.release(1000, true);
    assert_eq!(window.limit(), 1000);
}

/// A waker that counts its wakes.
struct CountWaker(AtomicUsize);

impl std::task::Wake for CountWaker {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// A window sender releases a batch's bytes as it hands the batch on, and wakes the receiver
/// that the window refused. Once the last sender drops, the window wakes the receiver again
/// and admits every frame.
#[test]
fn creditless_window_wakes_its_receiver_and_admits_every_frame_once_the_senders_are_gone() {
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let wakes = Arc::new(CountWaker(AtomicUsize::new(0)));
    let waker = std::task::Waker::from(wakes.clone());
    let window = CreditlessWindow::default();
    let mut sender = WindowSender::new(
        TestSender {
            accept: true,
            calls: 0,
        },
        window.clone(),
    );
    let mut item = new_test_item(test_path_secret_entry(), drop_counter.clone());
    item.byte_cost = 1000;

    assert!(window.admit(item.creditless_bytes()));
    assert!(!window.admit(1));
    window.register(&waker);
    assert!(sender.send(item).is_ok());
    assert_eq!(window.held(), 0);
    assert_eq!(wakes.0.load(Ordering::Relaxed), 1);

    window.register(&waker);
    drop(sender);
    assert_eq!(wakes.0.load(Ordering::Relaxed), 2);
    assert!(window.admit(u64::MAX));
    assert!(window.admit(u64::MAX));
    assert_eq!(window.held(), 0);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 1);
}

/// A window sender releases a batch's bytes and wakes the receiver though the worker's
/// receiver refuses the batch, which the sender returns.
#[test]
fn creditless_window_releases_a_batch_that_the_workers_receiver_refuses() {
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let wakes = Arc::new(CountWaker(AtomicUsize::new(0)));
    let waker = std::task::Waker::from(wakes.clone());
    let window = CreditlessWindow::default();
    let mut sender = WindowSender::new(
        TestSender {
            accept: false,
            calls: 0,
        },
        window.clone(),
    );
    let mut item = new_test_item(test_path_secret_entry(), drop_counter.clone());
    item.byte_cost = 1000;

    assert!(window.admit(item.creditless_bytes()));
    assert!(!window.admit(1));
    window.register(&waker);
    let refused = sender
        .send(item)
        .expect_err("the worker's receiver took the batch");
    assert_eq!(refused.byte_cost, 1000);
    assert_eq!(window.held(), 0, "the window kept a refused batch's bytes");
    assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 0);
    drop(refused);
    assert_eq!(drop_counter.load(Ordering::Relaxed), 1);
}

/// The bytes of a waiting peer's batches without send credit stay in the window while the
/// batches wait in the router, and leave it as each batch goes to a worker's sender. The first
/// batch that waited halves the window's limit as it goes, once for the bytes the window holds
/// then.
#[test]
fn interface_paced_pick_two_holds_a_waiting_peers_creditless_bytes_in_the_window() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    // The pair of the second interface's sender is lost for the peer, so the first interface's
    // sender takes each of its batches.
    let entry = test_path_secret_entry();
    assert!(entry.set_sender_lost(LocalSenderId::from_index(1), true));
    let window = CreditlessWindow::with_limit(400_000);
    let delivers = Sends::default();
    let lost = Sends::default();
    // Whether each batch waited in the router, in the order the senders took them.
    let waited = Rc::new(RefCell::new(Vec::new()));
    let senders: IdMap<LocalSenderId, _> = timed_senders(&clock, &[&delivers, &lost])
        .into_iter()
        .map(|(id, sender)| {
            let sender = WaitedSender {
                sender,
                waited: Rc::clone(&waited),
            };
            (id, WindowSender::new(sender, window.clone()))
        })
        .collect();
    // The priority router admitted each batch's frames, none of which borrows send credit.
    let values = test_items(&entry, 4, 100_000, &drop_counter);
    for value in &values {
        assert!(window.admit(value.creditless_bytes()));
    }
    assert_eq!(window.held(), 400_000);
    let consumed = Rc::new(std::cell::Cell::new(0));
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        SharedTestReceiver {
            values,
            consumed: Rc::clone(&consumed),
        },
        senders,
        crate::xorshift::Rng::new(),
        clock.clone(),
        &SendPacing::new(vec![rate, rate], vec![0, 1]),
        &registry,
        test_pool(),
    );

    // The first batch goes at once, the second waits on the first interface, and the third and
    // fourth wait in the router: the window holds the bytes of the three, and refuses a frame
    // past its limit.
    assert_eq!(poll_until_idle(&mut router), Poll::Pending);
    assert_eq!(delivers.borrow().len(), 1);
    // The router read each batch out of its input, so the third and fourth wait in the router.
    assert_eq!(
        consumed.get(),
        400_000,
        "the third and fourth batches didn't leave the input"
    );
    assert_eq!(window.held(), 300_000);
    assert!(!window.admit(100_001));

    // Each batch's bytes leave the window as the batch goes. The second batch waited on its
    // interface's pacer, which leaves the limit as it is, and the third in the router, which
    // halves it.
    for (delivered, held, limit) in [(2, 200_000, 400_000), (3, 100_000, 200_000)] {
        clock.advance(std::time::Duration::from_millis(1));
        assert_eq!(poll_until_idle(&mut router), Poll::Pending);
        assert_eq!(delivers.borrow().len(), delivered);
        assert_eq!(window.held(), held);
        assert_eq!(window.limit(), limit);
    }
    // The fourth batch waited in the router too, but the window held its bytes as the limit
    // halved.
    clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(poll_until_idle(&mut router), Poll::Ready(None));
    assert_eq!(delivers.borrow().len(), 4);
    assert_eq!(window.held(), 0);
    assert_eq!(window.limit(), 200_000);
    assert!(lost.borrow().is_empty());
    assert_eq!(
        *waited.borrow(),
        [false, false, true, true],
        "the router marks the batches that waited in it",
    );
    assert_eq!(drop_counter.load(Ordering::Relaxed), 4);
}

#[test]
fn frame_batch_counts_the_bytes_of_its_frames_without_send_credit() {
    let path = test_path_secret_entry();
    let first = new_test_frame(path.clone(), 16);
    let first_cost = first.byte_cost();
    let mut batch = FrameBatch::new(first);
    // The send credit pool bounds a frame that borrows send credit, so the batch doesn't count
    // its bytes.
    let mut credited = new_test_frame(path, 24);
    credited.flow_credits = 4096;
    let credited_cost = credited.byte_cost();
    batch.push_with_cost(credited, credited_cost);
    assert_eq!(batch.creditless_bytes(), first_cost);

    // A new batch hasn't waited in the router.
    assert!(!batch.waited());

    // A worker's sender reads them through the batch's entry, and the router marks the batch
    // through it.
    let mut entry = Entry::new(batch);
    assert_eq!(entry.creditless_bytes(), first_cost);
    assert!(!entry.waited());
    entry.set_waited();
    assert!(entry.waited());
}

/// The priority router of the window's pacing tests: it moves its items into the batch router's
/// lane, in order, while the window admits their bytes, and stops at the first item the window
/// refuses.
struct WindowLanes {
    window: CreditlessWindow,
    staged: VecDeque<TestItem>,
    moved: Rc<RefCell<VecDeque<TestItem>>>,
}

impl WindowLanes {
    fn new(window: CreditlessWindow, staged: VecDeque<TestItem>) -> Self {
        Self {
            window,
            staged,
            moved: Rc::default(),
        }
    }

    /// Returns the receiver of the lane, which is `Pending` while the lane is empty.
    fn receiver(&self) -> LaneReceiver {
        LaneReceiver {
            moved: Rc::clone(&self.moved),
        }
    }

    /// Moves the staged items into the lane while the window admits their bytes.
    fn pass(&mut self) {
        while let Some(item) = self.staged.front() {
            if !self.window.admit(item.creditless_bytes()) {
                break;
            }
            let item = self
                .staged
                .pop_front()
                .expect("the lanes hold a front item");
            self.moved.borrow_mut().push_back(item);
        }
    }
}

/// The receiver of the lane of a [`WindowLanes`].
struct LaneReceiver {
    moved: Rc<RefCell<VecDeque<TestItem>>>,
}

impl Receiver<TestItem> for LaneReceiver {
    fn poll_recv(
        &mut self,
        _cx: &mut task::Context<'_>,
        _budget: &mut Budget,
    ) -> Poll<Option<TestItem>> {
        match self.moved.borrow_mut().pop_front() {
            Some(item) => Poll::Ready(Some(item)),
            None => Poll::Pending,
        }
    }

    fn on_consumed(&mut self, _bytes: u64) {}
}

/// Runs `router` over 20 ms of `clock`, in steps of 10 us: in each step `lanes` moves the items
/// that the window admits, and `router` sends what it may. Returns the window's limit after each
/// step, and the bytes it holds then.
fn run_with_window<R: Receiver<()>>(
    lanes: &mut WindowLanes,
    router: &mut R,
    clock: &test_clock_mod::Clock,
) -> (Vec<u64>, Vec<u64>) {
    let mut limits = Vec::new();
    let mut held = Vec::new();
    for _ in 0..2_000 {
        lanes.pass();
        assert_eq!(poll_until_idle(router), Poll::Pending);
        clock.advance(std::time::Duration::from_micros(10));
        limits.push(lanes.window.limit());
        held.push(lanes.window.held());
    }
    (limits, held)
}

/// Under one paced interface, the window's limit grows where the pacer finds the batch router
/// without a batch while the window refuses one, and settles above its minimum once the batch
/// router has the next batch each time the pacer asks for one: at about the batch the pacer
/// holds and the next one.
#[test]
fn creditless_window_settles_at_the_batches_one_paced_interface_holds() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let window = CreditlessWindow::new(10_000);
    let mut lanes = WindowLanes::new(
        window.clone(),
        test_items(&test_path_secret_entry(), 200, 100_000, &drop_counter),
    );
    let sends = Sends::default();
    let senders: IdMap<LocalSenderId, _> = timed_senders(&clock, &[&sends, &sends])
        .into_iter()
        .map(|(id, sender)| (id, WindowSender::new(sender, window.clone())))
        .collect();
    let rx = WindowReader::new(lanes.receiver(), window.clone());
    let rx: crate::socket::channel::Paced<_, _, TestItem> =
        crate::socket::channel::Paced::new(rx, clock.clone(), rate);
    let registry = crate::counter::Registry::default();
    let mut router = PickTwo::new(
        rx,
        senders,
        crate::xorshift::Rng::new(),
        &registry,
        test_pool(),
    );

    let (limits, _) = run_with_window(&mut lanes, &mut router, &clock);

    // A batch of 100,000 bytes takes 800 us at 1 Gbps, so the interface sends about 25 in 20 ms.
    let sent = sends.borrow().len();
    assert!(sent >= 20, "{sent} batches");
    // The pacer holds a batch and the lane the next one, beside the minimum, and a pacer that
    // holds a batch doesn't ask for another: the limit stays there.
    let settled = *limits.last().expect("the run has steps");
    assert!(
        (200_000..=310_000).contains(&settled),
        "the limit settled at {settled}",
    );
    assert!(
        limits[1_000..].iter().all(|&limit| limit == settled),
        "the limit changed in the last 10 ms",
    );
    // Nothing marks a batch as one that waited under one interface, so the limit doesn't halve.
    assert!(
        limits.windows(2).all(|pair| pair[0] <= pair[1]),
        "the limit halved",
    );
}

/// Under two paced interfaces, the window's limit settles above its minimum at about the
/// batches that the two pacers hold and the next ones, and each interface sends at its rate.
#[test]
fn creditless_window_settles_at_the_batches_two_paced_interfaces_hold() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let start = crate::time::precision::Clock::now(&clock);
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let window = CreditlessWindow::new(10_000);
    let mut lanes = WindowLanes::new(
        window.clone(),
        test_items(&test_path_secret_entry(), 200, 100_000, &drop_counter),
    );
    let first = Sends::default();
    let second = Sends::default();
    let senders: IdMap<LocalSenderId, _> = timed_senders(&clock, &[&first, &second])
        .into_iter()
        .map(|(id, sender)| (id, WindowSender::new(sender, window.clone())))
        .collect();
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::<TestItem, _, _, _>::new(
        lanes.receiver(),
        senders,
        crate::xorshift::Rng::new(),
        clock.clone(),
        &SendPacing::new(vec![rate, rate], vec![0, 1]),
        &registry,
        test_pool(),
    )
    .with_window(window);

    let (limits, _) = run_with_window(&mut lanes, &mut router, &clock);

    // A batch of 100,000 bytes takes 800 us at 1 Gbps, so each interface sends about 25 in 20 ms.
    for sends in [&first, &second] {
        let sends = sends.borrow();
        assert_within_rate(&sends, rate, start);
        assert!(sends.len() >= 20, "{} batches", sends.len());
    }
    // Each pacer holds a batch and the lane the next one: the limit grows past them. A pacer
    // that holds a batch doesn't ask for another, so the limit doesn't grow past a batch of each
    // pacer, a next batch for each and the minimum, though the frames keep coming.
    let settled = *limits.last().expect("the run has steps");
    assert!(settled >= 300_000, "the limit settled at {settled}");
    let most = limits.iter().copied().max().unwrap_or_default();
    assert!(most <= 410_000, "the limit grew to {most}");
}

/// Under two paced interfaces, where the pair of the second interface's sender is lost for the
/// peer, the first interface's sender takes each of the peer's batches, and those its pacer
/// doesn't hold wait in the router. The free sender of the second interface can't take them, so
/// the window's limit doesn't grow for them while they wait: the limit and the bytes that wait in
/// the router stay bounded, though the frames keep coming.
#[test]
fn creditless_window_stays_bounded_while_a_peers_batches_wait_for_one_interface() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let entry = test_path_secret_entry();
    assert!(entry.set_sender_lost(LocalSenderId::from_index(1), true));
    let window = CreditlessWindow::new(10_000);
    let mut lanes = WindowLanes::new(
        window.clone(),
        test_items(&entry, 200, 100_000, &drop_counter),
    );
    let delivers = Sends::default();
    let lost = Sends::default();
    let senders: IdMap<LocalSenderId, _> = timed_senders(&clock, &[&delivers, &lost])
        .into_iter()
        .map(|(id, sender)| (id, WindowSender::new(sender, window.clone())))
        .collect();
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::<TestItem, _, _, _>::new(
        lanes.receiver(),
        senders,
        crate::xorshift::Rng::new(),
        clock.clone(),
        &SendPacing::new(vec![rate, rate], vec![0, 1]),
        &registry,
        test_pool(),
    )
    .with_window(window);

    let (limits, held) = run_with_window(&mut lanes, &mut router, &clock);

    // A batch of 100,000 bytes takes 800 us at 1 Gbps, so the first interface sends about 25 in
    // 20 ms, and the second doesn't send any.
    let sent = delivers.borrow().len();
    assert!(sent >= 20, "{sent} batches");
    assert!(lost.borrow().is_empty());
    // The first interface's pacer holds a batch, and at most two more wait in the router: the
    // limit grows only once no batch waits, and halves as a batch that waited goes.
    let most_held = held.iter().copied().max().unwrap_or_default();
    assert!(most_held <= 310_000, "the window held {most_held}");
    let most = limits.iter().copied().max().unwrap_or_default();
    assert!(most <= 410_000, "the limit grew to {most}");
}

/// The interface-paced router tells its window that a sender could take a batch where its
/// receiver is `Pending` while budget is left and no batch waits in it, which grows the limit of
/// a window that refused a frame, and doesn't tell it where the budget ran out.
#[test]
fn interface_paced_pick_two_starves_its_window_only_while_budget_is_left() {
    let rate = Rate::new(1.0);
    let clock = test_clock();
    let window = CreditlessWindow::new(100);
    let lanes = WindowLanes::new(window.clone(), VecDeque::new());
    let first = Sends::default();
    let second = Sends::default();
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::<TestItem, _, _, _>::new(
        lanes.receiver(),
        timed_senders(&clock, &[&first, &second]),
        crate::xorshift::Rng::new(),
        clock.clone(),
        &SendPacing::new(vec![rate, rate], vec![0, 1]),
        &registry,
        test_pool(),
    )
    .with_window(window.clone());
    assert!(window.admit(1000));
    assert!(!window.admit(1));

    let noop = s2n_quic_core::task::waker::noop();
    let mut cx = task::Context::from_waker(&noop);
    let mut exhausted = Budget::new(0);
    assert!(router.poll_recv(&mut cx, &mut exhausted).is_pending());
    assert_eq!(
        window.limit(),
        100,
        "the limit grew where the budget ran out"
    );

    let mut budget = Budget::new(1);
    assert!(router.poll_recv(&mut cx, &mut budget).is_pending());
    assert_eq!(window.limit(), 200);
}

/// A worker's sender that records whether each batch waited in the router before it hands the
/// batch on.
struct WaitedSender<S> {
    sender: S,
    waited: Rc<RefCell<Vec<bool>>>,
}

impl<S: UnboundedSender<TestItem>> UnboundedSender<TestItem> for WaitedSender<S> {
    fn send(&mut self, value: TestItem) -> Result<(), TestItem> {
        self.waited.borrow_mut().push(value.waited);
        self.sender.send(value)
    }
}

/// A reader tells the window that a worker could take a batch where its receiver is `Pending`
/// while budget is left, which grows the limit of a window that refused a frame and wakes the
/// priority router, and doesn't tell it where the budget ran out.
#[test]
fn window_reader_starves_the_window_only_while_budget_is_left() {
    let wakes = Arc::new(CountWaker(AtomicUsize::new(0)));
    let waker = std::task::Waker::from(wakes.clone());
    let window = CreditlessWindow::new(100);
    let lanes = WindowLanes::new(window.clone(), VecDeque::new());
    let mut reader = WindowReader::new(lanes.receiver(), window.clone());
    assert!(window.admit(1000));
    assert!(!window.admit(1));
    window.register(&waker);

    let noop = s2n_quic_core::task::waker::noop();
    let mut cx = task::Context::from_waker(&noop);
    let mut exhausted = Budget::new(0);
    assert!(reader.poll_recv(&mut cx, &mut exhausted).is_pending());
    assert_eq!(
        window.limit(),
        100,
        "the limit grew where the budget ran out"
    );
    assert_eq!(wakes.0.load(Ordering::Relaxed), 0);

    let mut budget = Budget::new(1);
    assert!(reader.poll_recv(&mut cx, &mut budget).is_pending());
    assert_eq!(window.limit(), 200);
    assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
}

/// Records the creditless window's events, each as its kind and its three byte fields in the
/// order the event lists them, and the peer of each frame-refused event.
#[derive(Default)]
struct WindowEvents {
    events: std::sync::Mutex<Vec<(&'static str, u64, u64, u64)>>,
    peers: std::sync::Mutex<Vec<std::net::SocketAddr>>,
}

impl WindowEvents {
    /// Returns the events recorded since the last call.
    fn take(&self) -> Vec<(&'static str, u64, u64, u64)> {
        core::mem::take(&mut *self.events.lock().unwrap())
    }

    /// Returns the peers of the frame-refused events recorded since the last call.
    fn take_peers(&self) -> Vec<std::net::SocketAddr> {
        core::mem::take(&mut *self.peers.lock().unwrap())
    }
}

impl crate::event::Subscriber for WindowEvents {
    type ConnectionContext = ();

    fn create_connection_context(
        &self,
        _meta: &crate::event::api::ConnectionMeta,
        _info: &crate::event::api::ConnectionInfo,
    ) -> Self::ConnectionContext {
    }

    fn on_creditless_window_frame_refused(
        &self,
        _meta: &crate::event::api::EndpointMeta,
        event: &crate::event::api::CreditlessWindowFrameRefused,
    ) {
        let entry = (
            "frame_refused",
            event.frame_bytes,
            event.held_bytes,
            event.limit_bytes,
        );
        self.events.lock().unwrap().push(entry);
        let peer = std::net::SocketAddr::from(&event.peer_address);
        self.peers.lock().unwrap().push(peer);
    }

    fn on_creditless_window_limit_grew(
        &self,
        _meta: &crate::event::api::EndpointMeta,
        event: &crate::event::api::CreditlessWindowLimitGrew,
    ) {
        let entry = (
            "limit_grew",
            event.released_bytes,
            event.held_bytes,
            event.limit_bytes,
        );
        self.events.lock().unwrap().push(entry);
    }

    fn on_creditless_window_limit_halved(
        &self,
        _meta: &crate::event::api::EndpointMeta,
        event: &crate::event::api::CreditlessWindowLimitHalved,
    ) {
        let entry = (
            "limit_halved",
            event.held_bytes,
            event.previous_limit_bytes,
            event.limit_bytes,
        );
        self.events.lock().unwrap().push(entry);
    }
}

/// Returns a window whose limit starts at `minimum`, with a path secret map that publishes the
/// window's events to `events`.
fn creditless_window_with_events(
    minimum: u64,
    events: std::sync::Arc<WindowEvents>,
) -> CreditlessWindow {
    let path_secret_map = crate::path::secret::Map::builder()
        .with_signer(crate::path::secret::stateless_reset::Signer::random())
        .with_capacity(4)
        .with_clock(s2n_quic_core::time::NoopClock)
        .with_subscriber(events)
        .build()
        .unwrap();
    CreditlessWindow::with_path_secret_map(minimum, path_secret_map)
}

/// The window records each frame it refuses, with the frame's peer, each growth of its limit and
/// each halving, with the bytes it holds and its limit. It records each halving, also where its
/// minimum keeps the limit where it is, and doesn't record an event where nothing changes or
/// where it refuses bytes given without their frame.
#[test]
fn creditless_window_records_its_refusals_and_limit_changes_as_events() {
    let events = std::sync::Arc::new(WindowEvents::default());
    let window = creditless_window_with_events(64, events.clone());
    // A clone stands for the batch router's, so that the window counts the frames.
    let _router = window.clone();

    // A frame while the window holds nothing doesn't record an event.
    assert!(window.admit(100));
    assert!(events.take().is_empty());
    // A refused frame records its peer, its bytes, the bytes the window holds and the limit.
    let path_secret_entry = test_path_secret_entry();
    let peer = *path_secret_entry.peer();
    let frame = new_test_frame(path_secret_entry, 1);
    let frame_bytes = frame.byte_cost();
    assert!(!window.admit_frame(&frame));
    assert_eq!(events.take(), [("frame_refused", frame_bytes, 100, 64)]);
    assert_eq!(events.take_peers(), [peer]);
    // Bytes refused without their frame don't record an event, since the window doesn't know
    // their peer.
    assert!(!window.admit(1));
    assert!(events.take().is_empty());

    // A worker that could take a batch that the batch router doesn't have, while the window
    // refuses a frame, grows the limit by the minimum before a worker's sender takes a batch,
    // and the event holds the bytes of that last batch, none yet.
    window.starved();
    assert_eq!(window.limit(), 128);
    assert_eq!(events.take(), [("limit_grew", 0, 100, 128)]);

    // A batch that didn't wait, a worker that could take a batch without a refusal and a release
    // of no bytes leave the limit as it is and don't record an event.
    window.release(100, false);
    window.starved();
    window.release(0, true);
    assert_eq!(window.limit(), 128);
    assert!(events.take().is_empty());

    // After that, the limit grows by the bytes of the last batch a worker's sender took.
    assert!(window.admit(100));
    assert!(!window.admit(100));
    window.starved();
    assert_eq!(window.limit(), 228);
    assert_eq!(events.take(), [("limit_grew", 100, 100, 228)]);

    // A batch that waited halves the limit, once for the bytes the window held then.
    assert!(window.admit(60));
    window.release(100, true);
    assert_eq!(window.limit(), 114);
    assert_eq!(events.take(), [("limit_halved", 60, 228, 114)]);
    window.release(60, true);
    assert_eq!(window.limit(), 114);
    assert!(events.take().is_empty());

    // The minimum bounds a halving and then keeps the limit where it is, and each halving still
    // records the event.
    assert!(window.admit(30));
    window.release(30, true);
    assert_eq!(window.limit(), 64);
    assert!(window.admit(10));
    window.release(10, true);
    assert_eq!(window.limit(), 64);
    assert_eq!(
        events.take(),
        [("limit_halved", 0, 114, 64), ("limit_halved", 0, 64, 64)]
    );
    assert_eq!(window.held(), 0);
}

/// While a pair is lost, pick-two chooses among the senders the peer acknowledged after the
/// lost pairs stopped delivering, and among every sender that isn't marked lost where none has
/// an acknowledgement since.
#[test]
fn pick_two_prefers_senders_that_deliver_since_a_pair_was_lost() {
    let entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(4)
        .build(None);
    let sender = LocalSenderId::from_index;
    let at = |nanos| crate::time::precision::Timestamp { nanos };
    assert!(entry.set_sender_lost(sender(0), true));
    entry.record_sender_delivered(sender(1), at(100));
    entry.record_sender_delivered(sender(2), at(300));
    entry.record_sender_delivered(sender(3), at(400));
    entry.set_senders_lost_since(at(200));
    entry.set_senders_preferred_until(at(1_000));

    let calls = pick_two_calls(&entry, 100, 4);
    assert_eq!(
        calls[0] + calls[1],
        0,
        "a sender without an acknowledgement since the loss got an item: {calls:?}"
    );
    assert!(
        calls[2] > 0 && calls[3] > 0,
        "a sender that delivers since the loss didn't get an item: {calls:?}"
    );

    // Where no sender has an acknowledgement since the loss, the router chooses among every
    // sender that isn't marked lost.
    entry.set_senders_lost_since(at(500));
    let calls = pick_two_calls(&entry, 100, 5);
    assert_eq!(calls[0], 0, "a lost sender got an item: {calls:?}");
    assert!(
        calls[1] > 0 && calls[2] > 0 && calls[3] > 0,
        "a sender that isn't marked lost didn't get an item: {calls:?}"
    );
}

/// Once a sender has an acknowledgement after the end of the preference that the lost marks
/// set, pick-two chooses again as it does where no sender delivers since the loss, so that a
/// sender idle at the loss gets items.
#[test]
fn pick_two_ends_the_preference_for_senders_that_deliver_since_a_loss() {
    let entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(4)
        .build(None);
    let sender = LocalSenderId::from_index;
    let at = |nanos| crate::time::precision::Timestamp { nanos };
    assert!(entry.set_sender_lost(sender(0), true));
    entry.set_senders_lost_since(at(200));
    entry.set_senders_preferred_until(at(1_000));
    entry.record_sender_delivered(sender(2), at(300));
    entry.record_sender_delivered(sender(3), at(400));

    let calls = pick_two_calls(&entry, 100, 6);
    assert_eq!(
        calls[0] + calls[1],
        0,
        "a sender without an acknowledgement since the loss got an item: {calls:?}"
    );

    entry.record_sender_delivered(sender(3), at(1_100));
    let calls = pick_two_calls(&entry, 100, 7);
    assert_eq!(calls[0], 0, "a lost sender got an item: {calls:?}");
    assert!(
        calls[1] > 0,
        "the sender idle at the loss didn't get an item after the preference: {calls:?}"
    );
}

/// Once a sender has an acknowledgement after the end of the preference that the lost marks
/// set, the interface-paced router reads the lost marks again, so that the senders of an
/// interface idle at the loss get its batches, and a sender that shares the lost pair's local
/// interface counts as lost.
#[test]
fn interface_paced_pick_two_ends_the_preference_for_senders_that_deliver_since_a_loss() {
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(4)
        .build(None);
    let sender = LocalSenderId::from_index;
    let at = |nanos| crate::time::precision::Timestamp { nanos };
    let items = || priority_items(&entry, &drop_counter, 20, 20);
    assert!(entry.set_sender_lost(sender(0), true));
    entry.set_senders_lost_since(at(200));
    entry.set_senders_preferred_until(at(1_000));
    entry.record_sender_delivered(sender(1), at(300));

    // Within the preference, the idle senders of the second interface count as lost, and
    // every item goes to sender 1.
    assert_eq!(
        route_by_interface(items()),
        [[0, 0], [20, 20], [0, 0], [0, 0]]
    );

    // Once sender 1 has an acknowledgement after the preference's end, the items go back to
    // the second interface: sender 1 shares the lost pair's local interface and counts as lost.
    entry.record_sender_delivered(sender(1), at(1_100));
    let counts = route_by_interface(items());
    assert_eq!(counts[0], [0, 0], "sender 0 got an item: {counts:?}");
    assert_eq!(
        counts[1],
        [0, 0],
        "sender 1 got an item though it shares the lost pair's interface: {counts:?}"
    );
    assert_eq!(
        counts[2][0] + counts[3][0],
        20,
        "the second interface didn't get the highest-priority items: {counts:?}"
    );
    assert_eq!(counts[2][1] + counts[3][1], 20);
}

/// While a pair is lost, pick-two records its score gap and the candidate it rejects, as it
/// does among every sender.
#[test]
fn pick_two_records_its_counters_while_a_pair_is_lost() {
    let entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(3)
        .build(None);
    assert!(entry.set_sender_lost(LocalSenderId::from_index(0), true));
    let registry = crate::counter::Registry::default();
    let pick_counters: crate::endpoint::id::IdMap<LocalSenderId, _> = (0..3)
        .map(|i| registry.register_nominal("pick_two.chosen", format_args!("send.{i}")))
        .collect::<Vec<_>>()
        .into();
    let rejected_counters: crate::endpoint::id::IdMap<LocalSenderId, _> = (0..3)
        .map(|i| {
            registry.register_nominal_summary(
                "pick_two.rejected",
                format_args!("send.{i}"),
                crate::counter::Unit::Microsecond,
            )
        })
        .collect::<Vec<_>>()
        .into();
    let score_delta =
        registry.register_summary("pick_two.score_delta", crate::counter::Unit::Microsecond);
    let mut senders: crate::endpoint::id::IdMap<LocalSenderId, _> = (0..3)
        .map(|_| TestSender {
            accept: true,
            calls: 0,
        })
        .collect::<Vec<_>>()
        .into();
    let mut rng = crate::xorshift::Rng::with_seed(7);
    let mut rr = 0;
    for _ in 0..10 {
        let item = new_test_item(entry.clone(), Arc::new(AtomicUsize::new(0)));
        let result = PickTwo::<TestItem, TestReceiver<TestItem>, TestSender>::try_send_pick_two(
            item,
            &mut senders,
            &mut rng,
            &mut rr,
            &pick_counters,
            &rejected_counters,
            &score_delta,
        );
        assert!(result.is_ok());
    }
    assert_eq!(
        senders[LocalSenderId::from_index(0)].calls,
        0,
        "the lost sender got an item"
    );

    let line = registry
        .take_current_metrics_line()
        .expect("the counters recorded values");
    let recorded = |name: &str| {
        line.split(',')
            .any(|field| field.contains(name) && field.contains('*'))
    };
    assert!(
        recorded("pick_two.score_delta"),
        "the score gap isn't recorded: {line}"
    );
    assert!(
        recorded("pick_two.rejected"),
        "no rejected candidate is recorded: {line}"
    );
}

/// While a pair is lost, the interface-paced router sends each batch to the senders the peer
/// acknowledged after the lost pairs stopped delivering, and not to a sender on the lost
/// pair's interface without such an acknowledgement.
#[test]
fn interface_paced_pick_two_prefers_senders_that_deliver_since_a_pair_was_lost() {
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(4)
        .build(None);
    let sender = LocalSenderId::from_index;
    let at = |nanos| crate::time::precision::Timestamp { nanos };
    assert!(entry.set_sender_lost(sender(0), true));
    entry.record_sender_delivered(sender(1), at(100));
    entry.record_sender_delivered(sender(2), at(300));
    entry.record_sender_delivered(sender(3), at(300));
    entry.set_senders_lost_since(at(200));
    entry.set_senders_preferred_until(at(1_000));

    let counts = route_by_interface(priority_items(&entry, &drop_counter, 20, 20));
    assert_eq!(counts[0], [0, 0], "sender 0 got an item: {counts:?}");
    assert_eq!(
        counts[1],
        [0, 0],
        "sender 1 got an item without an acknowledgement since the loss: {counts:?}"
    );
    assert_eq!(counts[2][0] + counts[3][0], 20);
    assert_eq!(counts[2][1] + counts[3][1], 20);
}

/// Where no sender has an acknowledgement after the lost pairs stopped delivering, the
/// interface-paced router reads each sender's lost mark: every batch goes to the one sender that
/// isn't marked lost, the highest-priority ones among them.
#[test]
fn interface_paced_pick_two_reads_the_lost_marks_where_no_sender_delivers_since_a_loss() {
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(4)
        .build(None);
    let sender = LocalSenderId::from_index;
    let at = |nanos| crate::time::precision::Timestamp { nanos };
    for idx in [0, 2, 3] {
        assert!(entry.set_sender_lost(sender(idx), true));
    }
    entry.record_sender_delivered(sender(1), at(100));
    entry.record_sender_delivered(sender(2), at(300));
    entry.record_sender_delivered(sender(3), at(300));
    entry.set_senders_lost_since(at(500));
    assert!(!entry.has_sender_delivering_since_lost());

    assert_eq!(
        route_by_interface(priority_items(&entry, &drop_counter, 20, 20)),
        [[0, 0], [20, 20], [0, 0], [0, 0]]
    );
}

/// While a pair is lost and no sender delivers since, pick-two leaves out the senders that send
/// to the lost pair's peer interface, since that interface may be down, and chooses them again
/// once they deliver since the loss.
#[test]
fn pick_two_skips_the_senders_on_a_lost_pairs_peer_interface() {
    let peer = |last: u8| std::net::SocketAddr::from(([10, 0, 0, last], 4000));
    let entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(4)
        .peer_data_addrs(vec![peer(1), peer(2)])
        .peer_data_interfaces(vec![Some(1), Some(2)])
        .build(None);
    let sender = LocalSenderId::from_index;

    // Sender i sends to the peer's address i % 2: senders 0 and 2 send to the peer's first
    // interface, and senders 1 and 3 to its second.
    assert!(entry.set_sender_lost(sender(0), true));
    let calls = pick_two_calls(&entry, 100, 6);
    assert_eq!(
        calls[0] + calls[2],
        0,
        "a sender on the lost pair's peer interface got an item: {calls:?}"
    );
    assert_eq!(calls[1] + calls[3], 100);
    assert!(
        calls[1] > 0 && calls[3] > 0,
        "a sender on the other peer interface didn't get an item: {calls:?}"
    );

    // Once senders 1 to 3 deliver since the loss, the router chooses sender 2 again.
    let at = |nanos| crate::time::precision::Timestamp { nanos };
    entry.set_senders_lost_since(at(200));
    entry.set_senders_preferred_until(at(1_000));
    for idx in 1..4 {
        entry.record_sender_delivered(sender(idx), at(300));
    }
    let calls = pick_two_calls(&entry, 100, 7);
    assert_eq!(calls[0], 0, "the lost sender got an item: {calls:?}");
    assert!(
        calls[2] > 0,
        "a sender that delivers since the loss didn't get an item: {calls:?}"
    );
}

/// While a pair is lost and no sender delivers since, the interface-paced router leaves out the
/// senders that share the lost pair's local interface or its peer interface, keeps the senders
/// that share the fewest of them, and chooses sender 1 again once it delivers since the loss.
#[test]
fn interface_paced_pick_two_skips_the_senders_that_share_a_lost_pairs_interfaces() {
    let drop_counter = Arc::new(AtomicUsize::new(0));
    let peer = |last: u8| std::net::SocketAddr::from(([10, 0, 0, last], 4000));
    let sender = LocalSenderId::from_index;

    // The peer has two interfaces, each paired with one local interface: senders 0 and 1 send
    // to the peer's first, and senders 2 and 3 to its second. Sender 1 shares both interfaces
    // of the lost pair and counts as lost too; senders 2 and 3 share neither.
    let paired = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(4)
        .peer_data_addrs(vec![peer(1), peer(2)])
        .peer_data_interfaces(vec![Some(1), Some(2)])
        .build(None);
    assert!(paired.set_sender_lost(sender(0), true));
    let counts = route_by_interface(priority_items(&paired, &drop_counter, 20, 20));
    assert_eq!(counts[0], [0, 0], "sender 0 got an item: {counts:?}");
    assert_eq!(counts[1], [0, 0], "sender 1 got an item: {counts:?}");
    assert_ne!(counts[2], [0, 0], "sender 2 didn't get an item: {counts:?}");
    assert_eq!(counts[2][0] + counts[3][0], 20);
    assert_eq!(counts[2][1] + counts[3][1], 20);

    // The peer has one interface, which every sender sends to. Senders 2 and 3 share only that
    // one with the lost pair, fewer than sender 1 shares, and take every item.
    let one = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(4)
        .peer_data_addrs(vec![peer(1)])
        .peer_data_interfaces(vec![Some(1)])
        .build(None);
    assert!(one.set_sender_lost(sender(0), true));
    let counts = route_by_interface(priority_items(&one, &drop_counter, 20, 20));
    assert_eq!(counts[0], [0, 0], "sender 0 got an item: {counts:?}");
    assert_eq!(counts[1], [0, 0], "sender 1 got an item: {counts:?}");
    assert_eq!(counts[2][0] + counts[3][0], 20);
    assert_eq!(counts[2][1] + counts[3][1], 20);

    // Once senders 1 to 3 deliver since the loss, the router chooses sender 1 again.
    let at = |nanos| crate::time::precision::Timestamp { nanos };
    one.set_senders_lost_since(at(200));
    one.set_senders_preferred_until(at(1_000));
    for idx in 1..4 {
        one.record_sender_delivered(sender(idx), at(300));
    }
    let counts = route_by_interface(priority_items(&one, &drop_counter, 20, 20));
    assert_eq!(counts[0], [0, 0], "sender 0 got an item: {counts:?}");
    assert!(
        counts[1][1] > 0,
        "sender 1 didn't get an item once it delivers since the loss: {counts:?}"
    );
}

/// The interface-paced router places each send socket among the interfaces as the interface
/// table places it for the send workers, so that it reads the peer interface each worker sends
/// to.
#[test]
fn send_pacing_places_each_send_socket_as_the_interface_table_does() {
    use crate::socket::interface;

    let overall = Rate::new(25.0);
    let addrs: Vec<std::net::SocketAddr> = (0..5)
        .map(|port| std::net::SocketAddr::from(([127, 0, 0, 1], 4000 + port)))
        .collect();
    // Socket 2, which no interface lists, is on the default interface, which ranks first.
    let configs = [
        interface::Config {
            send_sockets: vec![0, 3],
            ..Default::default()
        },
        interface::Config {
            send_sockets: vec![1, 4],
            ..Default::default()
        },
    ];
    let table = interface::Table::resolve(&configs, overall, &addrs, &addrs, &interface::System);
    let pacing = SendPacing::from_interfaces(&table, overall);
    let places: Vec<_> = (0..addrs.len())
        .map(|socket| pacing.send_place(socket))
        .collect();
    assert_eq!(table.send_places(), Some(places));
}
