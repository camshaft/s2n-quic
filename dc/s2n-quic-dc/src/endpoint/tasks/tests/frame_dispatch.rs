// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract tests for the `frame_dispatch` task.
//!
//! The frame dispatch pipeline routes frame submissions from writers to send workers.
//! It performs priority routing (high-priority frames before low), batching by path secret
//! (frames for the same peer are coalesced), and pick-two load balancing across workers.
//! These tests verify end-to-end behavior of the two cooperating subtasks.

use super::helpers::{test_frame, test_frame_with_payload, TestReceiverExt as _};
use crate::{
    endpoint::{
        combinator::{FrameBatch, HighestPriority, SendPacing},
        frame::{self, PriorityInput, SubmissionSender},
        tasks, Budgets,
    },
    intrusive::EntryAdapter,
    runtime::bach::Local,
    socket::{
        channel::{intrusive::unsync, ByteCost, UnboundedSender},
        rate::Rate,
    },
    testing::{ext::*, sim},
    time::bach::Clock,
};
use std::sync::{Arc, Mutex};

type WorkerRx = unsync::Receiver<EntryAdapter<FrameBatch>>;

/// Spawns the frame_dispatch pipeline and returns a SubmissionSender for feeding frames.
/// Each element of the returned Vec is a receiver for one worker.
fn setup(num_workers: usize) -> (SubmissionSender, Vec<WorkerRx>) {
    setup_with_rates(num_workers, Rate::new(100.0))
}

/// Like [`setup`] but with a configurable overall send rate.
fn setup_with_rates(num_workers: usize, overall_rate: Rate) -> (SubmissionSender, Vec<WorkerRx>) {
    setup_with_pacing(num_workers, SendPacing::single(overall_rate))
}

/// Like [`setup`] but with a configurable pacing of the workers' send sockets.
fn setup_with_pacing(num_workers: usize, pacing: SendPacing) -> (SubmissionSender, Vec<WorkerRx>) {
    let (frame_tx, frame_rx) = frame::submission_channel(1);

    let mut workers = Vec::with_capacity(num_workers);
    let mut rxs = Vec::with_capacity(num_workers);
    for _ in 0..num_workers {
        let (tx, rx) = unsync::new::<FrameBatch>();
        workers.push(tx);
        rxs.push(rx);
    }

    let mut spawner = Local::new(0);
    tasks::frame_dispatch(
        &mut spawner,
        frame_rx,
        workers.into(),
        crate::xorshift::Rng::new(),
        Clock::default(),
        pacing,
        Budgets::default(),
        crate::counter::Registry::default(),
        crate::sync::Arc::new(crate::credit::Pool::new(crate::credit::Config::new(
            1_000_000,
        ))),
        crate::queue::freed_batch_channel().0,
    );

    (frame_tx, rxs)
}

/// A single frame submitted through the channel arrives at the worker as a FrameBatch.
#[test]
fn single_frame_arrives_at_worker() {
    sim(|| {
        let (mut frame_tx, mut rxs) = setup(1);
        let mut worker_rx = rxs.pop().unwrap();

        async move {
            let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
                .socket_sender_count(1)
                .build(None);
            let mut input = PriorityInput::default();
            input.push(test_frame(&pse));
            frame_tx.send_batch(input).unwrap();
        }
        .primary()
        .spawn();

        async move {
            let batch = worker_rx.recv().await.unwrap();
            assert_eq!(batch.into_inner().len(), 1);
        }
        .primary()
        .spawn();
    });
}

/// Multiple frames for the same peer are batched together into one FrameBatch.
#[test]
fn same_peer_frames_batched() {
    sim(|| {
        let (mut frame_tx, mut rxs) = setup(1);
        let mut worker_rx = rxs.pop().unwrap();

        async move {
            let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
                .socket_sender_count(1)
                .build(None);
            let mut input = PriorityInput::default();
            for _ in 0..5 {
                input.push(test_frame(&pse));
            }
            frame_tx.send_batch(input).unwrap();
        }
        .primary()
        .spawn();

        async move {
            let batch = worker_rx.recv().await.unwrap();
            assert_eq!(batch.into_inner().len(), 5);
        }
        .primary()
        .spawn();
    });
}

/// With two workers, a frame is delivered to exactly one of them.
#[test]
fn multiple_workers_receive_frames() {
    sim(|| {
        let (mut frame_tx, rxs) = setup(2);
        let (result_tx, mut result_rx) = unsync::new::<FrameBatch>();

        // Submit one frame
        async move {
            let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
                .socket_sender_count(2)
                .build(None);
            let mut input = PriorityInput::default();
            input.push(test_frame(&pse));
            frame_tx.send_batch(input).unwrap();
        }
        .primary()
        .spawn();

        // Worker forwards to result channel
        for mut worker_rx in rxs {
            let mut result_tx = result_tx.clone();
            async move {
                while let Some(batch) = worker_rx.recv().await {
                    let _ = UnboundedSender::send(&mut result_tx, batch);
                }
            }
            .spawn();
        }

        // Primary: wait for exactly one batch from either worker
        async move {
            let batch = result_rx.recv().await.unwrap();
            assert_eq!(batch.into_inner().len(), 1);
        }
        .primary()
        .spawn();
    });
}

/// Dropping the frame submission sender cascades through the pipeline: Task 1 sees
/// channel close, drops priority senders, Task 2 sees close, drops worker senders,
/// and the worker channel closes.
#[test]
fn sender_drop_shuts_down() {
    sim(|| {
        let (frame_tx, mut rxs) = setup(1);
        let mut worker_rx = rxs.pop().unwrap();

        async move {
            drop(frame_tx);
            let result = worker_rx.recv().await;
            assert!(result.is_none(), "expected worker channel to close");
        }
        .primary()
        .spawn();
    });
}

/// Regression test: the `Paced` combinator in the frame_dispatch pipeline must actually
/// delay transmissions after the burst budget is exhausted.
///
/// Before the fix, `PickTwo::poll_recv` never called `on_consumed` on its inner receiver,
/// so the `Paced` token bucket was never consumed and pacing was completely bypassed —
/// all batches passed through immediately regardless of the configured rate.
///
/// This test submits two oversized frames (each exceeding the burst budget on its own)
/// through the full pipeline and asserts that the second batch arrives at a strictly later
/// simulated time than the first, proving the pacing timer was armed and fired.
#[test]
fn pacing_delays_frames_beyond_burst() {
    sim(|| {
        // Use 100 Gbps: nanos_per_byte = 0.08, burst_nanos = u16::MAX × 0.08 ≈ 5242 ns.
        // A frame of u16::MAX bytes has byte_cost ≈ u16::MAX + metadata ≥ u16::MAX,
        // so its transmission cost slightly exceeds the burst budget, meaning the first
        // frame consumes all burst credit and the second must wait for the pacing timer.
        let overall_rate = Rate::new(100.0);
        let (mut frame_tx, mut rxs) = setup_with_rates(1, overall_rate);
        let mut worker_rx = rxs.pop().unwrap();

        // Submit two oversized frames in one batch so both are visible to the pipeline
        // simultaneously, making the pacing delay the only reason for the second to wait.
        async move {
            let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
                .socket_sender_count(1)
                .build(None);
            // Each frame is larger than the burst budget; BatchFramesByPathSecret will emit
            // them as two separate FrameBatches because the first already fills a batch.
            const OVERSIZED_FRAME_SIZE: usize = u16::MAX as usize;
            let mut input = PriorityInput::default();
            input.push(test_frame_with_payload(&pse, OVERSIZED_FRAME_SIZE));
            input.push(test_frame_with_payload(&pse, OVERSIZED_FRAME_SIZE));
            frame_tx.send_batch(input).unwrap();
        }
        .primary()
        .spawn();

        async move {
            // First batch: burst credit covers it, arrives without delay.
            let t0 = bach::time::Instant::now();
            let _first = worker_rx.recv().await.unwrap();
            let t1 = bach::time::Instant::now();

            // Second batch: burst is exhausted; pacing timer must fire before it arrives.
            let _second = worker_rx.recv().await.unwrap();
            let t2 = bach::time::Instant::now();

            // The first batch should have arrived on the same simulated tick as t0.
            assert_eq!(t0, t1, "first batch should arrive without any pacing delay");

            // The second batch must arrive at a strictly later time — the pacing timer
            // advanced the simulated clock.
            assert!(
                t2 > t1,
                "second batch should be delayed by pacing (t2={t2:?} should be > t1={t1:?}); \
                 if they are equal, PickTwo::poll_recv is not calling on_consumed"
            );
        }
        .primary()
        .spawn();
    });
}

/// With two interfaces, each worker's batches arrive within its interface's rate and burst, and
/// the worker on the fast interface gets more than the slow interface's rate allows.
#[test]
fn two_interfaces_pace_each_worker_to_its_interface_rate() {
    let _snapshots = crate::testing::without_snapshots();
    let slow = Rate::new(1.0);
    let fast = Rate::new(4.0);
    let arrivals = Arc::new(Mutex::new(Vec::<(usize, u64, u64)>::new()));
    let arrivals_in_sim = arrivals.clone();

    sim(move || {
        let pacing = SendPacing::new(vec![slow, fast], vec![0, 1]);
        let (mut frame_tx, rxs) = setup_with_pacing(2, pacing);

        // Each frame fills a batch of its own, as in `pacing_delays_frames_beyond_burst`.
        async move {
            let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
                .socket_sender_count(2)
                .build(None);
            let mut input = PriorityInput::default();
            for _ in 0..40 {
                input.push(test_frame_with_payload(&pse, u16::MAX as usize));
            }
            frame_tx.send_batch(input).unwrap();
        }
        .primary()
        .spawn();

        for (worker, mut worker_rx) in rxs.into_iter().enumerate() {
            let arrivals = arrivals_in_sim.clone();
            async move {
                while let Some(batch) = worker_rx.recv().await {
                    let at = bach::time::Instant::now().elapsed_since_start().as_nanos() as u64;
                    let bytes = batch.byte_cost();
                    arrivals.lock().unwrap().push((worker, at, bytes));
                }
            }
            .primary()
            .spawn();
        }
    });

    let arrivals = arrivals.lock().unwrap();
    assert_eq!(arrivals.len(), 40, "one batch for each frame");
    let mut received = [0u64; 2];
    for (worker, rate) in [(0, slow), (1, fast)] {
        let mut sent_before = 0;
        for &(_, at, bytes) in arrivals.iter().filter(|arrival| arrival.0 == worker) {
            // The batches before this one fit in the interface's rate and burst, with 2 us
            // allowed for timer rounding.
            let allowed = at + rate.burst_nanos() + 2_000;
            assert!(
                rate.nanos_for_bytes(sent_before) <= allowed,
                "worker {worker} got a batch at {at} ns after {sent_before} bytes",
            );
            sent_before += bytes;
        }
        received[worker] = sent_before;
    }
    let [slow_bytes, fast_bytes] = received;
    assert!(slow_bytes > 0, "the slow worker didn't get a batch");
    assert!(
        fast_bytes >= 2 * slow_bytes,
        "the fast worker got {fast_bytes} bytes, the slow one {slow_bytes}",
    );
}

/// With an interface chosen for the highest-priority traffic, each batch that holds a frame of
/// the highest priority level goes to a worker of that interface, and the other batches still go
/// to the workers of both interfaces.
#[test]
fn highest_priority_batches_go_to_the_chosen_interface() {
    const PINGS: usize = 20;
    let _snapshots = crate::testing::without_snapshots();
    let slow = Rate::new(1.0);
    let fast = Rate::new(4.0);
    let arrivals = Arc::new(Mutex::new(Vec::<(usize, bool, usize)>::new()));
    let arrivals_in_sim = arrivals.clone();

    sim(move || {
        // Workers 0 and 1 are on the slow interface, and workers 2 and 3 on the fast one, which
        // carries the highest-priority traffic.
        let pacing = SendPacing::new(vec![slow, fast], vec![0, 0, 1, 1]).with_highest_priority(1);
        let (mut frame_tx, rxs) = setup_with_pacing(4, pacing);

        async move {
            let peer = |addr: &str| {
                crate::path::secret::map::Entry::builder(addr.parse().unwrap())
                    .socket_sender_count(4)
                    .build(None)
            };
            let bulk_peer = peer("127.0.0.1:4433");
            // The pings go to a peer of their own, so that a batch doesn't hold a ping beside a
            // bulk frame.
            let ping_peer = peer("127.0.0.1:4434");
            for _ in 0..PINGS {
                let mut input = PriorityInput::default();
                input.push(test_frame_with_payload(&bulk_peer, u16::MAX as usize));
                let mut ping = test_frame(&ping_peer);
                ping.header = frame::Header::Ping;
                input.push(ping);
                frame_tx.send_batch(input).unwrap();
                bach::time::sleep(core::time::Duration::from_micros(50)).await;
            }
        }
        .primary()
        .spawn();

        for (worker, mut worker_rx) in rxs.into_iter().enumerate() {
            let arrivals = arrivals_in_sim.clone();
            async move {
                while let Some(batch) = worker_rx.recv().await {
                    let highest = batch.holds_highest_priority();
                    arrivals.lock().unwrap().push((worker, highest, batch.len()));
                }
            }
            .primary()
            .spawn();
        }
    });

    let arrivals = arrivals.lock().unwrap();
    let frames: usize = arrivals.iter().map(|arrival| arrival.2).sum();
    assert_eq!(frames, 2 * PINGS, "every frame arrived");
    let pings: usize = arrivals.iter().filter(|arrival| arrival.1).map(|arrival| arrival.2).sum();
    assert_eq!(pings, PINGS, "each ping is in a batch of the highest priority level");
    for &(worker, highest, _) in arrivals.iter() {
        assert!(!highest || worker >= 2, "worker {worker} on the slow interface got a ping");
    }
    let bulk_on = |workers: core::ops::Range<usize>| {
        arrivals
            .iter()
            .any(|&(worker, highest, _)| !highest && workers.contains(&worker))
    };
    assert!(bulk_on(0..2), "the slow interface didn't get a bulk batch");
    assert!(bulk_on(2..4), "the fast interface didn't get a bulk batch");
}

/// A batch is of the highest priority level only where every frame it holds is of that level,
/// so that a bulk frame batched beside a ping is paced with its interface's bucket.
#[test]
fn a_batch_that_mixes_a_ping_with_a_bulk_frame_isnt_of_the_highest_priority() {
    let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(4)
        .build(None);
    let ping = || {
        let mut ping = test_frame(&pse);
        ping.header = frame::Header::Ping;
        ping
    };

    let mut pings = FrameBatch::single(ping());
    pings.push(ping());
    assert!(pings.holds_highest_priority(), "a batch of pings");

    let mut mixed = FrameBatch::single(ping());
    mixed.push(test_frame_with_payload(&pse, 1000));
    assert!(!mixed.holds_highest_priority(), "a ping beside a bulk frame");

    let bulk = FrameBatch::single(test_frame_with_payload(&pse, 1000));
    assert!(!bulk.holds_highest_priority(), "a bulk frame");
}

// -- The window of the frames without send credit ------------------------------------------

type Lane = unsync::Receiver<EntryAdapter<frame::Frame>>;

/// Returns the priority router of `frame_dispatch` over `window`, with the sender of its
/// submission channel and the receiver of each of its lanes.
fn priority_router(
    window: crate::endpoint::combinator::CreditlessWindow,
) -> (
    SubmissionSender,
    impl crate::socket::channel::Receiver<()>,
    Vec<Lane>,
) {
    priority_router_with_shards(window, 1)
}

/// Returns the priority router of `priority_router` over a submission channel of `shards`
/// shards. A sender puts each batch it submits into the shard after its last batch's.
fn priority_router_with_shards(
    window: crate::endpoint::combinator::CreditlessWindow,
    shards: usize,
) -> (
    SubmissionSender,
    impl crate::socket::channel::Receiver<()>,
    Vec<Lane>,
) {
    let (frame_tx, frame_rx) = frame::submission_channel(shards);
    let mut lanes = Vec::new();
    let lane_txs = core::array::from_fn(|_| {
        let (tx, rx) = unsync::new::<frame::Frame>();
        lanes.push(rx);
        tx.into_list_sender()
    });
    let registry = crate::counter::Registry::default();
    let gauges = core::array::from_fn(|i| {
        registry.register_queue_gauge_nominal("q.router_to_batcher", format_args!("p{i}"))
    });
    let router = tasks::FrameReceiver::new(
        frame_rx,
        lane_txs,
        gauges,
        window,
        crate::queue::freed_batch_channel().0,
    );
    (frame_tx, router, lanes)
}

/// Polls `router` once, with a waker that does nothing.
fn poll_router(
    router: &mut impl crate::socket::channel::Receiver<()>,
) -> core::task::Poll<Option<()>> {
    let waker = s2n_quic_core::task::waker::noop();
    let mut cx = core::task::Context::from_waker(&waker);
    let mut budget = crate::socket::channel::Budget::new(usize::MAX);
    router.poll_recv(&mut cx, &mut budget)
}

/// Takes the frames of `lanes`, the first lane's first, and returns each frame's byte cost and
/// send credit.
fn take_lanes(lanes: &mut [Lane]) -> Vec<(u64, u64)> {
    let mut frames = crate::intrusive::Queue::<frame::Frame>::new();
    for lane in lanes {
        lane.drain_into(&mut frames);
    }
    frames
        .iter()
        .map(|frame| (frame.byte_cost(), frame.flow_credits))
        .collect()
}

/// The priority router moves a frame without send credit into its lane only once the window
/// admits its bytes, and keeps each frame after a refused frame, in its lane or a later lane.
#[test]
fn the_priority_router_holds_frames_without_send_credit_at_the_window() {
    use core::task::Poll;

    let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(1)
        .build(None);
    let window = crate::endpoint::combinator::CreditlessWindow::default();
    let (mut frame_tx, mut router, mut lanes) = priority_router(window.clone());

    // Two frames without send credit in the first lane, and a frame with send credit in the
    // last lane.
    let mut first = test_frame(&pse);
    first.header = frame::Header::Ping;
    let mut second = test_frame(&pse);
    second.header = frame::Header::Ping;
    let mut credited = test_frame_with_payload(&pse, 3000);
    credited.flow_credits = 4096;
    let costs = [&first, &second, &credited].map(|frame| frame.byte_cost());
    let mut input = PriorityInput::default();
    input.push(first);
    input.push(second);
    input.push(credited);
    frame_tx.send_batch(input).unwrap();

    // The window's limit starts at 0, so it admits the first frame alone, while it holds
    // nothing, and holds its bytes.
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(costs[0], 0)]);
    assert_eq!(window.held(), costs[0]);

    // The second frame, and the frame with send credit in the later lane, wait at the window.
    assert_eq!(poll_router(&mut router), Poll::Pending);
    assert!(take_lanes(&mut lanes).is_empty());
    assert_eq!(window.round(), 2, "each pass of the priority router starts a round");

    // A worker's sender takes the first frame's batch, which releases its bytes.
    window.release(costs[0], window.round());
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(costs[1], 0), (costs[2], 4096)]);
    assert_eq!(window.held(), costs[1]);

    drop(frame_tx);
    assert_eq!(poll_router(&mut router), Poll::Ready(None));
}

/// The priority router moves every frame once it holds the window's only clone, since no
/// worker's sender would then release their bytes.
#[test]
fn the_priority_router_moves_every_frame_once_the_batch_router_is_gone() {
    use core::task::Poll;

    let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(1)
        .build(None);
    let window = crate::endpoint::combinator::CreditlessWindow::default();
    let (mut frame_tx, mut router, mut lanes) = priority_router(window.clone());
    let mut input = PriorityInput::default();
    for _ in 0..3 {
        input.push(test_frame_with_payload(&pse, 1000));
    }
    frame_tx.send_batch(input).unwrap();
    drop(frame_tx);

    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes).len(), 1);
    assert_eq!(poll_router(&mut router), Poll::Pending);

    // The test's clone of the window stands for the batch router's.
    drop(window);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes).len(), 2);
    assert_eq!(poll_router(&mut router), Poll::Ready(None));
}

// -- The frames in front of the window while it refuses a frame -----------------------------

/// Returns a frame without send credit, which the window admits while it holds nothing, so that
/// the window refuses each frame without send credit after it until the test releases its bytes.
fn holding_frame(
    pse: &std::sync::Arc<crate::path::secret::map::Entry>,
) -> crate::intrusive::Entry<frame::Frame> {
    let mut holder = test_frame(pse);
    holder.header = frame::Header::Ping;
    holder
}

/// Returns a reset of the queue `dest_queue_id` with `error_code`, without a completion sender
/// or send credit, as the endpoint sends one in answer to a peer's frame.
fn reset_frame(
    pse: &std::sync::Arc<crate::path::secret::map::Entry>,
    dest_queue_id: u8,
    error_code: u8,
) -> crate::intrusive::Entry<frame::Frame> {
    use crate::packet::datagram::{QueuePair, ResetTarget};
    use s2n_quic_core::varint::VarInt;

    let mut reset = test_frame(pse);
    reset.header = frame::Header::QueueReset {
        queue_pair: QueuePair {
            source_queue_id: VarInt::from_u8(1),
            dest_queue_id: VarInt::from_u8(dest_queue_id),
        },
        binding_id: VarInt::from_u8(1),
        reset_target: ResetTarget::Both,
        error_code: VarInt::from_u8(error_code),
        init: None,
    };
    reset
}

/// Submits `frames` in one batch, which goes into the sender's next shard.
fn submit(
    frame_tx: &mut SubmissionSender,
    frames: impl IntoIterator<Item = crate::intrusive::Entry<frame::Frame>>,
) {
    let mut input = PriorityInput::default();
    for entry in frames {
        input.push(entry);
    }
    frame_tx.send_batch(input).unwrap();
}

/// Polls `router` once with `budget`, with a waker that does nothing.
fn poll_router_with_budget(
    router: &mut impl crate::socket::channel::Receiver<()>,
    budget: &mut crate::socket::channel::Budget,
) -> core::task::Poll<Option<()>> {
    let waker = s2n_quic_core::task::waker::noop();
    let mut cx = core::task::Context::from_waker(&waker);
    router.poll_recv(&mut cx, budget)
}

/// While the window refuses a frame, each poll of the priority router swaps in the frames
/// submitted since the last poll, and drops each copy of a reset without a completion sender
/// that waits already, so the frames in front of the window don't grow with the copies that a
/// peer's frames bring. A copy submitted after the waiting reset left for its lane waits again.
#[test]
fn the_priority_router_keeps_one_copy_of_a_waiting_reset() {
    use crate::socket::channel::ByteCost as _;
    use core::task::Poll;
    use std::sync::Arc;

    let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(1)
        .build(None);
    let window = crate::endpoint::combinator::CreditlessWindow::default();
    let (mut frame_tx, mut router, mut lanes) = priority_router(window.clone());

    let holder = holding_frame(&pse);
    let holder_cost = holder.byte_cost();
    submit(&mut frame_tx, [holder]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(holder_cost, 0)]);
    // Each frame holds a reference to its path secret entry, wherever it waits.
    let unheld = Arc::strong_count(&pse);

    let reset_cost = reset_frame(&pse, 2, 1).byte_cost();
    for copies in 1..=4 {
        submit(&mut frame_tx, (0..copies).map(|_| reset_frame(&pse, 2, 1)));
        assert_eq!(poll_router(&mut router), Poll::Pending);
        assert!(take_lanes(&mut lanes).is_empty());
        assert_eq!(
            Arc::strong_count(&pse),
            unheld + 1,
            "one copy of the reset waits, and the channel doesn't hold a copy"
        );
    }

    // A worker's sender takes the holder's batch, which releases its bytes.
    window.release(holder_cost, window.round());
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(reset_cost, 0)]);
    assert_eq!(Arc::strong_count(&pse), unheld);

    // The reset holds the window now, and a copy submitted after it left for its lane waits,
    // since it resets the queue again.
    submit(&mut frame_tx, [reset_frame(&pse, 2, 1)]);
    assert_eq!(poll_router(&mut router), Poll::Pending);
    assert_eq!(Arc::strong_count(&pse), unheld + 1, "the later copy waits");
    window.release(reset_cost, window.round());
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(reset_cost, 0)]);

    drop(frame_tx);
    assert_eq!(poll_router(&mut router), Poll::Ready(None));
}

/// The priority router keeps each reset that differs from a waiting one: a reset whose producer
/// waits on its completion, a reset with send credit, a reset of another queue, a reset of the
/// same queue with another error code, and a reset of the same queue of another peer.
#[test]
fn the_priority_router_keeps_each_reset_that_differs_from_a_waiting_one() {
    use core::task::Poll;
    use std::sync::Arc;

    let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(1)
        .build(None);
    let other = crate::path::secret::map::Entry::builder("127.0.0.1:4434".parse().unwrap())
        .socket_sender_count(1)
        .build(None);
    assert!(pse.id() != other.id(), "the two peers' credentials differ");
    let window = crate::endpoint::combinator::CreditlessWindow::default();
    let (mut frame_tx, mut router, mut lanes) = priority_router(window.clone());
    submit(&mut frame_tx, [holding_frame(&pse)]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes).len(), 1);
    let unheld = Arc::strong_count(&pse);
    let other_unheld = Arc::strong_count(&other);

    // The test's receiver stands for a producer that waits on the completion.
    let completions = frame::failure_completion_channel();
    let mut awaited = reset_frame(&pse, 2, 1);
    awaited.completion = Some(completions.sender());
    let mut credited = reset_frame(&pse, 2, 1);
    credited.flow_credits = 64;
    submit(
        &mut frame_tx,
        [
            reset_frame(&pse, 2, 1),
            awaited,
            credited,
            reset_frame(&pse, 3, 1),
            reset_frame(&pse, 2, 7),
            reset_frame(&other, 2, 1),
            // Copies of the waiting resets of queues 3 and 2.
            reset_frame(&pse, 3, 1),
            reset_frame(&pse, 2, 1),
        ],
    );
    assert_eq!(poll_router(&mut router), Poll::Pending);
    assert_eq!(Arc::strong_count(&pse), unheld + 5, "five resets of the first peer wait");
    assert_eq!(Arc::strong_count(&other), other_unheld + 1, "the other peer's reset waits");

    // The producer stops waiting before the frames leave their lanes, so that they may drop with
    // a pending status, and the router then holds the window's only clone, so it moves every
    // frame.
    drop(completions);
    drop(window);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes).len(), 6);
}

/// Each poll of the priority router swaps in every occupied shard of the submission channel,
/// though the window refuses a frame, so that the frames submitted meanwhile don't wait in the
/// channel, where a later copy of a waiting reset would stay.
#[test]
fn the_priority_router_swaps_in_every_shard_while_the_window_refuses_a_frame() {
    use core::task::Poll;
    use std::sync::Arc;

    let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(1)
        .build(None);
    let window = crate::endpoint::combinator::CreditlessWindow::default();
    let (mut frame_tx, mut router, mut lanes) = priority_router_with_shards(window.clone(), 4);
    submit(&mut frame_tx, [holding_frame(&pse)]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes).len(), 1);
    let unheld = Arc::strong_count(&pse);

    // Each batch goes into the next of the four shards, and each round submits a reset of each
    // of four queues.
    for round in 0..3 {
        for queue in 2..6 {
            submit(&mut frame_tx, [reset_frame(&pse, queue, 1)]);
        }
        assert_eq!(poll_router(&mut router), Poll::Pending);
        assert_eq!(
            Arc::strong_count(&pse),
            unheld + 4,
            "round {round}: one reset of each queue waits, and the channel doesn't hold one"
        );
    }

    // The router holds the window's only clone now, so it moves every frame.
    drop(window);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes).len(), 4);
    drop(frame_tx);
    assert_eq!(poll_router(&mut router), Poll::Ready(None));
}

/// A poll of the priority router takes a unit of the budget for each swap, and one unit where it
/// moves waiting frames without a swap, and a poll whose budget runs out before the channel is
/// empty asks to be polled again.
#[test]
fn the_priority_router_takes_the_budget_for_its_swaps_and_moves() {
    use crate::socket::channel::{Budget, ByteCost as _};
    use core::task::Poll;

    let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(1)
        .build(None);
    let window = crate::endpoint::combinator::CreditlessWindow::default();
    let (mut frame_tx, mut router, mut lanes) = priority_router(window.clone());

    // One swap, and the move of the frame it brought, take one unit.
    let holder = holding_frame(&pse);
    let holder_cost = holder.byte_cost();
    submit(&mut frame_tx, [holder]);
    let mut budget = Budget::new(2);
    assert_eq!(poll_router_with_budget(&mut router, &mut budget), Poll::Ready(Some(())));
    assert_eq!(budget.consumed(), 1);
    assert_eq!(take_lanes(&mut lanes), [(holder_cost, 0)]);

    // The swap takes the only unit, and the window refuses the reset it brought.
    submit(&mut frame_tx, [reset_frame(&pse, 2, 1)]);
    let mut budget = Budget::new(1);
    assert_eq!(poll_router_with_budget(&mut router, &mut budget), Poll::Pending);
    assert!(budget.take_needs_wake(), "the channel may hold more frames");

    // The move of the waiting reset takes one unit, without a swap.
    window.release(holder_cost, window.round());
    let mut budget = Budget::new(2);
    assert_eq!(poll_router_with_budget(&mut router, &mut budget), Poll::Ready(Some(())));
    assert_eq!(budget.consumed(), 1);
    assert_eq!(take_lanes(&mut lanes).len(), 1);
}

/// The waiting resets forget a reset as it leaves for its lane, and don't keep an empty list for
/// its queue.
#[test]
fn the_waiting_resets_forget_a_reset_that_leaves_for_its_lane() {
    let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(1)
        .build(None);
    let mut resets = tasks::WaitingResets::default();
    let mut queue = crate::intrusive::Queue::<frame::Frame>::new();
    queue.push_back(reset_frame(&pse, 2, 1));
    queue.push_back(reset_frame(&pse, 2, 1));
    let mut kept = resets.take_new(queue);
    assert_eq!(kept.len(), 1, "the copy is dropped");
    let reset = kept.pop_front().expect("the first reset is kept");
    resets.remove(&reset);
    assert!(resets.waiting.is_empty());
}

/// A reset that leaves for its lane releases the slot that its rejected init held, so that the
/// slot is recorded as freed only then: while the window refuses the reset, the client doesn't
/// learn that the slot is free, and doesn't send another init on it.
#[test]
fn the_priority_router_releases_the_slot_of_a_reset_that_leaves_for_its_lane() {
    use crate::socket::channel::ByteCost as _;
    use core::task::Poll;
    use s2n_quic_core::varint::VarInt;

    let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
        .socket_sender_count(1)
        .endpoint_type(s2n_quic_core::endpoint::Type::Server)
        .build(None);
    let crate::path::secret::map::entry::QueueState::Server(state) = pse.queue_state() else {
        unreachable!("a server entry has a server queue state");
    };
    let state = state.clone();
    let window = crate::endpoint::combinator::CreditlessWindow::default();
    let (mut frame_tx, mut router, mut lanes) = priority_router(window.clone());

    let holder = holding_frame(&pse);
    let holder_cost = holder.byte_cost();
    submit(&mut frame_tx, [holder]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(holder_cost, 0)]);

    // The server rejects an init of binding 1 on slot 1, which the reset answers, and frees the
    // slot while the reset waits at the window.
    let (mut freed_tx, _freed_rx) = crate::queue::freed_batch_channel();
    assert!(state.freed.hold(VarInt::from_u8(1), VarInt::from_u8(1)));
    state.freed.record(VarInt::from_u8(1), &pse, &mut freed_tx);
    let reset = reset_frame(&pse, 2, 1);
    let reset_cost = reset.byte_cost();
    submit(&mut frame_tx, [reset]);
    assert_eq!(poll_router(&mut router), Poll::Pending);
    assert!(take_lanes(&mut lanes).is_empty());
    assert!(!state.freed.has_pending_work(), "the slot waits on its reset");

    // A worker's sender takes the holder's batch, and the reset leaves for its lane: the slot is
    // recorded as freed, and a later rejected init can hold it again.
    window.release(holder_cost, window.round());
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(reset_cost, 0)]);
    assert!(state.freed.has_pending_work(), "the slot is recorded as freed");
    assert!(state.freed.hold(VarInt::from_u8(1), VarInt::from_u8(2)));

    drop(frame_tx);
    assert_eq!(poll_router(&mut router), Poll::Ready(None));
}
