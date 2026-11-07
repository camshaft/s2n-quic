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
    assert_eq!(calls[1] + calls[3], 0, "a lost sender got an item: {calls:?}");
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
    assert_eq!(chosen[0], 0, "the router chose the lost sender 0: {chosen:?}");
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
    values[1].flow_credits = CREDIT;
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

    // Once the pair of the second interface's sender delivers again, the waiting batch goes to
    // that sender, whose receiver is gone: the router ends in that poll, though its input is
    // open, and returns the credit of the batch held on the first interface.
    assert!(entry.set_sender_lost(sender(1), false));
    gone.set(true);
    assert_eq!(poll_once(&mut router), Poll::Ready(None));
    assert_eq!(drop_counter.load(Ordering::Relaxed), 3);
    assert_eq!(
        pool.debug_available() as u64 + pool.debug_returned(),
        CAP + CREDIT,
        "the router didn't return the held batch's send credit to the pool",
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
    assert!(window.admit(1000), "the window refused a frame while it held nothing");
    assert!(!window.admit(1), "the window admitted a frame past its limit of 100");
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
    assert_eq!(window.limit(), 500, "the limit halved again for the bytes it held");
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
    assert!(!window.admit(1), "the window admitted a frame past its minimum");

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
    let senders: IdMap<LocalSenderId, _> = timed_senders(&clock, &[&delivers, &lost])
        .into_iter()
        .map(|(id, sender)| (id, WindowSender::new(sender, window.clone())))
        .collect();
    // The priority router admitted each batch's frames, none of which borrows send credit.
    let values = test_items(&entry, 4, 100_000, &drop_counter);
    for value in &values {
        assert!(window.admit(value.creditless_bytes()));
    }
    assert_eq!(window.held(), 400_000);
    let registry = crate::counter::Registry::default();
    let mut router = InterfacePacedPickTwo::new(
        TestReceiver {
            values,
            consumed: 0,
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
            let item = self.staged.pop_front().expect("the lanes hold a front item");
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
/// step.
fn run_with_window<R: Receiver<()>>(
    lanes: &mut WindowLanes,
    router: &mut R,
    clock: &test_clock_mod::Clock,
) -> Vec<u64> {
    let mut limits = Vec::new();
    for _ in 0..2_000 {
        lanes.pass();
        assert_eq!(poll_until_idle(router), Poll::Pending);
        clock.advance(std::time::Duration::from_micros(10));
        limits.push(lanes.window.limit());
    }
    limits
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

    let limits = run_with_window(&mut lanes, &mut router, &clock);

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
        WindowReader::new(lanes.receiver(), window.clone()),
        senders,
        crate::xorshift::Rng::new(),
        clock.clone(),
        &SendPacing::new(vec![rate, rate], vec![0, 1]),
        &registry,
        test_pool(),
    );

    let limits = run_with_window(&mut lanes, &mut router, &clock);

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
