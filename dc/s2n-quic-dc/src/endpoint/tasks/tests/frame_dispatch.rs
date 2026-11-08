// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract tests for the `frame_dispatch` task.
//!
//! The frame dispatch pipeline routes frame submissions from writers to send workers.
//! It performs priority routing (high-priority frames before low), batching by path secret
//! (frames for the same peer are coalesced), and pick-two load balancing across workers.
//! These tests verify end-to-end behavior of the two cooperating subtasks.

use super::helpers::{
    event_count, test_frame, test_frame_with_payload, test_map_with_subscriber,
    TestReceiverExt as _,
};
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
    setup_with_map(num_workers, pacing, None)
}

/// Like [`setup_with_pacing`] but with the path secret map that the routers publish their events
/// through, where one is given.
fn setup_with_map(
    num_workers: usize,
    pacing: SendPacing,
    path_secret_map: Option<crate::path::secret::Map>,
) -> (SubmissionSender, Vec<WorkerRx>) {
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
        path_secret_map,
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

/// With an interface chosen for the highest-priority traffic, each batch whose frames are all of
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
                    arrivals
                        .lock()
                        .unwrap()
                        .push((worker, highest, batch.len()));
                }
            }
            .primary()
            .spawn();
        }
    });

    let arrivals = arrivals.lock().unwrap();
    let frames: usize = arrivals.iter().map(|arrival| arrival.2).sum();
    assert_eq!(frames, 2 * PINGS, "every frame arrived");
    let pings: usize = arrivals
        .iter()
        .filter(|arrival| arrival.1)
        .map(|arrival| arrival.2)
        .sum();
    assert_eq!(
        pings, PINGS,
        "each ping is in a batch of the highest priority level"
    );
    for &(worker, highest, _) in arrivals.iter() {
        assert!(
            !highest || worker >= 2,
            "worker {worker} on the slow interface got a ping"
        );
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
    assert!(
        !mixed.holds_highest_priority(),
        "a ping beside a bulk frame"
    );

    let bulk = FrameBatch::single(test_frame_with_payload(&pse, 1000));
    assert!(!bulk.holds_highest_priority(), "a bulk frame");
}

/// With an interface chosen for the highest-priority traffic, frame dispatch batches the frames
/// of the highest priority level apart from the bulk frames of the same peer, so that each such
/// frame goes in a batch of that level to a worker of the chosen interface.
#[test]
fn highest_priority_frames_beside_bulk_frames_go_to_the_chosen_interface() {
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
            // Each submission holds a bulk frame and a ping, both for the same peer.
            let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
                .socket_sender_count(4)
                .build(None);
            for _ in 0..PINGS {
                let mut input = PriorityInput::default();
                input.push(test_frame_with_payload(&pse, 1000));
                let mut ping = test_frame(&pse);
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
                    arrivals
                        .lock()
                        .unwrap()
                        .push((worker, highest, batch.len()));
                }
            }
            .primary()
            .spawn();
        }
    });

    let arrivals = arrivals.lock().unwrap();
    let frames: usize = arrivals.iter().map(|arrival| arrival.2).sum();
    assert_eq!(frames, 2 * PINGS, "every frame arrived");
    let pings: usize = arrivals
        .iter()
        .filter(|arrival| arrival.1)
        .map(|arrival| arrival.2)
        .sum();
    assert_eq!(pings, PINGS, "a ping was batched beside a bulk frame");
    for &(worker, highest, _) in arrivals.iter() {
        assert!(
            !highest || worker >= 2,
            "worker {worker} on the slow interface got a ping"
        );
    }
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
    let (frame_tx, frame_rx) = frame::submission_channel(1);
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

    // A worker's sender takes the first frame's batch, which releases its bytes.
    window.release_peer(Some(PeerKey::of(&pse)), costs[0], false);
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

// -- Frames that pass a refused frame -------------------------------------------------------

use crate::endpoint::combinator::PeerKey;

/// Takes the frames of `lanes`, the first lane's first, and returns each frame's peer and send
/// credit.
fn take_peers(lanes: &mut [Lane]) -> Vec<(PeerKey, u64)> {
    let mut frames = crate::intrusive::Queue::<frame::Frame>::new();
    for lane in lanes {
        lane.drain_into(&mut frames);
    }
    frames
        .iter()
        .map(|frame| (PeerKey::of(&frame.path_secret_entry), frame.flow_credits))
        .collect()
}

/// Returns a path secret entry of a peer at `addr`.
fn peer_entry(addr: &str) -> std::sync::Arc<crate::path::secret::map::Entry> {
    crate::path::secret::map::Entry::builder(addr.parse().unwrap())
        .socket_sender_count(1)
        .build(None)
}

/// Returns a ping of `pse`, a frame without send credit of the highest priority level.
fn ping(
    pse: &std::sync::Arc<crate::path::secret::map::Entry>,
) -> crate::intrusive::Entry<frame::Frame> {
    let mut frame = test_frame(pse);
    frame.header = frame::Header::Ping;
    frame
}

/// Returns a control frame of `pse` with `payload_size` zero bytes of payload: a frame without
/// send credit of the control level, whose byte cost grows with its payload.
fn control_frame(
    pse: &std::sync::Arc<crate::path::secret::map::Entry>,
    payload_size: usize,
) -> crate::intrusive::Entry<frame::Frame> {
    let mut frame = test_frame_with_payload(pse, payload_size);
    frame.header = frame::Header::QueueControl {
        queue_pair: crate::packet::datagram::QueuePair {
            source_queue_id: s2n_quic_core::varint::VarInt::from_u8(1),
            dest_queue_id: s2n_quic_core::varint::VarInt::from_u8(2),
        },
        binding_id: s2n_quic_core::varint::VarInt::from_u8(1),
    };
    frame
}

/// A frame that the window refuses holds back the later frames of its peer, in its lane and in
/// the later lanes, in their order, and the frames of another peer pass it, with send credit
/// and without.
#[test]
fn the_priority_router_moves_other_peers_frames_past_a_refused_frame() {
    use core::task::Poll;

    let (a, b) = (peer_entry("127.0.0.1:4433"), peer_entry("127.0.0.2:4433"));
    let (peer_a, peer_b) = (PeerKey::of(&a), PeerKey::of(&b));
    let window = crate::endpoint::combinator::CreditlessWindow::default();
    let (mut frame_tx, mut router, mut lanes) = priority_router(window.clone());

    // The control lane holds three frames of the first peer, each of its own byte cost, with a
    // frame of the second peer after the first two, and the last lane holds a frame with send
    // credit of each peer.
    let [a1, a2, a3] = [10, 20, 30].map(|payload| control_frame(&a, payload));
    let costs = [&a1, &a2, &a3].map(|frame| frame.byte_cost());
    let b1 = control_frame(&b, 0);
    let b_cost = b1.byte_cost();
    let mut input = PriorityInput::default();
    input.push(a1);
    input.push(a2);
    input.push(b1);
    input.push(a3);
    for (pse, credits) in [(&a, 4096), (&b, 2048)] {
        let mut credited = test_frame_with_payload(pse, 3000);
        credited.flow_credits = credits;
        input.push(credited);
    }
    frame_tx.send_batch(input).unwrap();

    // The window's limit is 0. It admits the first peer's first frame while it holds nothing,
    // and the second peer's frame while it holds none of that peer's bytes, and refuses the
    // first peer's second frame, which holds back the first peer's later frames.
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(
        take_peers(&mut lanes),
        [(peer_a, 0), (peer_b, 0), (peer_b, 2048)]
    );
    assert_eq!(window.held(), costs[0] + b_cost);
    assert_eq!(window.peer_held(peer_a), costs[0]);
    assert_eq!(window.peer_held(peer_b), b_cost);

    // Nothing moves while the window holds the first peer's bytes.
    assert_eq!(poll_router(&mut router), Poll::Pending);
    assert!(take_peers(&mut lanes).is_empty());

    // A worker's sender takes the first peer's batch, which leaves the limit at 0. The window
    // holds the second peer's bytes and none of the first peer's, so it admits the first peer's
    // second frame past the limit, and refuses its third, which still holds back its frame with
    // send credit.
    window.release_peer(Some(peer_a), costs[0], false);
    assert_eq!(window.peer_held(peer_a), 0);
    assert_eq!(window.limit(), 0);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(costs[1], 0)]);
    assert_eq!(window.held(), b_cost + costs[1]);
}

/// The priority router swaps in the frames submitted while a refused frame waits, behind it in
/// its peer's order, and moves a new frame of another peer past it in the same pass. Once the
/// submission channel closes, it moves the frames that wait before it ends.
#[test]
fn the_priority_router_swaps_in_new_frames_behind_a_refused_frame() {
    use core::task::Poll;

    let (a, b) = (peer_entry("127.0.0.1:4433"), peer_entry("127.0.0.2:4433"));
    let (peer_a, peer_b) = (PeerKey::of(&a), PeerKey::of(&b));
    let window = crate::endpoint::combinator::CreditlessWindow::default();
    let (mut frame_tx, mut router, mut lanes) = priority_router(window.clone());

    let [a1, a2, a3] = [10, 20, 30].map(|payload| control_frame(&a, payload));
    let costs = [&a1, &a2, &a3].map(|frame| frame.byte_cost());
    let mut input = PriorityInput::default();
    input.push(a1);
    input.push(a2);
    frame_tx.send_batch(input).unwrap();
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_peers(&mut lanes), [(peer_a, 0)]);

    // The first peer's second frame waits at the window. A frame of each peer comes: the
    // second peer's ping goes in the next pass, and the first peer's third frame waits behind
    // the first peer's refused frame.
    let mut input = PriorityInput::default();
    input.push(a3);
    input.push(ping(&b));
    frame_tx.send_batch(input).unwrap();
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_peers(&mut lanes), [(peer_b, 0)]);

    // The test's clone of the window stands for the batch router's: once it drops and the
    // submission channel closes, the router moves the two frames that wait, in their order,
    // and then ends.
    drop(frame_tx);
    drop(window);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(costs[1], 0), (costs[2], 0)]);
    assert_eq!(poll_router(&mut router), Poll::Ready(None));
}

/// Returns a priority router over a submission channel of `shards` shards, as `priority_router`
/// does over one.
fn sharded_priority_router(
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

/// A swap takes one shard of the submission channel. Where a swap brings in only frames of a
/// peer that the window holds back, the router returns ready, and its next poll swaps the next
/// shard, so that another peer's frame there goes while the window holds the first peer's bytes.
#[test]
fn the_priority_router_swaps_the_next_shard_after_a_swap_of_held_back_frames() {
    use core::task::Poll;

    let (a, b) = (peer_entry("127.0.0.1:4433"), peer_entry("127.0.0.2:4433"));
    let (peer_a, peer_b) = (PeerKey::of(&a), PeerKey::of(&b));
    let ping_cost = ping(&a).byte_cost();
    let window = crate::endpoint::combinator::CreditlessWindow::default();
    let (mut frame_tx, mut router, mut lanes) = sharded_priority_router(window.clone(), 2);

    let mut input = PriorityInput::default();
    input.push(ping(&a));
    frame_tx.send_batch(input).unwrap();
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_peers(&mut lanes), [(peer_a, 0)]);
    assert_eq!(window.peer_held(peer_a), ping_cost);

    // The sender takes the two shards in turn, so the first peer's second ping goes to the
    // shard after the one the router swapped, and the second peer's ping to the other.
    for pse in [&a, &b] {
        let mut input = PriorityInput::default();
        input.push(ping(pse));
        frame_tx.send_batch(input).unwrap();
    }

    // The next swap brings in the first peer's second ping alone, which the window refuses. The
    // router returns ready, and its next poll swaps the other shard and moves the second peer's
    // ping while the window still holds the first peer's bytes.
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert!(take_peers(&mut lanes).is_empty());
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_peers(&mut lanes), [(peer_b, 0)]);
    assert_eq!(window.peer_held(peer_a), ping_cost);
    assert_eq!(window.peer_held(peer_b), ping_cost);

    // Both shards are empty now, and the first peer's second ping waits at the window.
    assert_eq!(poll_router(&mut router), Poll::Pending);
    assert!(take_peers(&mut lanes).is_empty());
}

// -- The frames in front of the window while it refuses a frame -----------------------------

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

/// While the window refuses a frame, the priority router drops each copy of a reset without a
/// completion sender that waits already, so the frames in front of the window don't grow with
/// the copies that a peer's frames bring. A copy submitted after the waiting reset left for its
/// lane waits again.
#[test]
fn the_priority_router_keeps_one_copy_of_a_waiting_reset() {
    use core::task::Poll;

    let pse = peer_entry("127.0.0.1:4433");
    let window = crate::endpoint::combinator::CreditlessWindow::default();
    let (mut frame_tx, mut router, mut lanes) = priority_router(window.clone());

    let holder = ping(&pse);
    let holder_cost = holder.byte_cost();
    submit(&mut frame_tx, [holder]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(holder_cost, 0)]);
    // Each frame holds a reference to its path secret entry, wherever it waits.
    let unheld = Arc::strong_count(&pse);

    // The window holds the ping's bytes, so it refuses each reset of the same peer. The poll
    // that swaps returns ready, and the next poll finds the channel empty.
    let reset_cost = reset_frame(&pse, 2, 1).byte_cost();
    for copies in 1..=4 {
        submit(&mut frame_tx, (0..copies).map(|_| reset_frame(&pse, 2, 1)));
        assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
        assert_eq!(poll_router(&mut router), Poll::Pending);
        assert!(take_lanes(&mut lanes).is_empty());
        assert_eq!(
            Arc::strong_count(&pse),
            unheld + 1,
            "one copy of the reset waits, and the channel doesn't hold a copy"
        );
    }

    // A worker's sender takes the ping's batch, which releases its bytes.
    window.release_peer(Some(PeerKey::of(&pse)), holder_cost, false);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(reset_cost, 0)]);
    assert_eq!(Arc::strong_count(&pse), unheld);

    // The reset holds the window now, and a copy submitted after it left for its lane waits,
    // since it resets the queue again.
    submit(&mut frame_tx, [reset_frame(&pse, 2, 1)]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert!(take_lanes(&mut lanes).is_empty());
    assert_eq!(Arc::strong_count(&pse), unheld + 1, "the later copy waits");
    window.release_peer(Some(PeerKey::of(&pse)), reset_cost, false);
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

    let (pse, other) = (peer_entry("127.0.0.1:4433"), peer_entry("127.0.0.2:4433"));
    assert!(pse.id() != other.id(), "the two peers' credentials differ");
    let window = crate::endpoint::combinator::CreditlessWindow::default();
    let (mut frame_tx, mut router, mut lanes) = priority_router(window.clone());
    submit(&mut frame_tx, [ping(&pse)]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes).len(), 1);
    let unheld = Arc::strong_count(&pse);

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

    // The window holds the first peer's bytes and none of the other peer's, so the other
    // peer's reset goes to its lane, and the first peer's resets wait without the two copies.
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_peers(&mut lanes), [(PeerKey::of(&other), 0)]);
    assert_eq!(Arc::strong_count(&pse), unheld + 5, "five resets of the first peer wait");

    // The producer stops waiting before the frames leave their lanes, so that they may drop with
    // a pending status, and the router then holds the window's only clone, so it moves every
    // frame.
    drop(completions);
    drop(window);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes).len(), 5);
}

/// A swap takes one shard of the submission channel, and the priority router drops the copies
/// of waiting resets in each shard it swaps, so that the swaps while the window refuses a frame
/// leave one reset of each queue in front of the window.
#[test]
fn the_priority_router_drops_the_copies_of_waiting_resets_in_every_shard() {
    use core::task::Poll;

    let pse = peer_entry("127.0.0.1:4433");
    let window = crate::endpoint::combinator::CreditlessWindow::default();
    let (mut frame_tx, mut router, mut lanes) = sharded_priority_router(window.clone(), 4);
    submit(&mut frame_tx, [ping(&pse)]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes).len(), 1);
    let unheld = Arc::strong_count(&pse);

    // Each batch goes into the next of the four shards, and each round submits a reset of each
    // of four queues. Each poll swaps one shard and returns ready, and the poll after the four
    // swaps finds the channel empty.
    for round in 0..3 {
        for queue in 2..6 {
            submit(&mut frame_tx, [reset_frame(&pse, queue, 1)]);
        }
        for _ in 0..4 {
            assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
        }
        assert_eq!(poll_router(&mut router), Poll::Pending);
        assert!(take_lanes(&mut lanes).is_empty());
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

/// The waiting resets forget a reset as it leaves for its lane, and don't keep an empty list for
/// its queue.
#[test]
fn the_waiting_resets_forget_a_reset_that_leaves_for_its_lane() {
    let pse = peer_entry("127.0.0.1:4433");
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

    let holder = ping(&pse);
    let holder_cost = holder.byte_cost();
    submit(&mut frame_tx, [holder]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(holder_cost, 0)]);

    // The server rejects an init of binding 1 on slot 1, which the reset answers, and frees the
    // slot while the reset waits at the window. The poll that swaps the reset in returns ready,
    // and the next poll finds the channel empty.
    let (mut freed_tx, _freed_rx) = crate::queue::freed_batch_channel();
    assert!(state.freed.hold(VarInt::from_u8(1), VarInt::from_u8(1)));
    state.freed.record(VarInt::from_u8(1), &pse, &mut freed_tx);
    let reset = reset_frame(&pse, 2, 1);
    let reset_cost = reset.byte_cost();
    submit(&mut frame_tx, [reset]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(poll_router(&mut router), Poll::Pending);
    assert!(take_lanes(&mut lanes).is_empty());
    assert!(!state.freed.has_pending_work(), "the slot waits on its reset");

    // A worker's sender takes the holder's batch, and the reset leaves for its lane: the slot is
    // recorded as freed, and a later rejected init can hold it again.
    window.release_peer(Some(PeerKey::of(&pse)), holder_cost, false);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(reset_cost, 0)]);
    assert!(state.freed.has_pending_work(), "the slot is recorded as freed");
    assert!(state.freed.hold(VarInt::from_u8(1), VarInt::from_u8(2)));

    drop(frame_tx);
    assert_eq!(poll_router(&mut router), Poll::Ready(None));
}

/// The resets that wait in the priority router as it ends release the slots that their rejected
/// inits held, as a reset that leaves for its lane does, so that the slots are recorded as freed:
/// a reset that waits at the window, and one that waits in the submission channel.
#[test]
fn the_priority_router_releases_the_slots_of_its_waiting_resets_as_it_ends() {
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

    // A frame without send credit takes the window, so that the resets below wait.
    submit(&mut frame_tx, [ping(&pse)]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes).len(), 1);

    // The server rejects an init of binding 1 on slots 1 and 2, and frees both slots while their
    // resets wait: slot 1's at the window, and slot 2's in the submission channel. The poll that
    // swaps slot 1's reset in returns ready, and the next poll finds the channel empty.
    let (mut freed_tx, _freed_rx) = crate::queue::freed_batch_channel();
    for slot in [1, 2] {
        assert!(state.freed.hold(VarInt::from_u8(slot), VarInt::from_u8(1)));
        state
            .freed
            .record(VarInt::from_u8(slot), &pse, &mut freed_tx);
    }
    submit(&mut frame_tx, [reset_frame(&pse, 2, 1)]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(poll_router(&mut router), Poll::Pending);
    let mut reset = reset_frame(&pse, 3, 1);
    if let frame::Header::QueueReset { queue_pair, .. } = &mut reset.header {
        queue_pair.source_queue_id = VarInt::from_u8(2);
    }
    submit(&mut frame_tx, [reset]);
    assert!(
        !state.freed.has_pending_work(),
        "the slots wait on their resets"
    );

    // The router ends before either reset leaves for its lane.
    drop(router);
    let mut freed = crate::bitset::HierarchicalBitSet::new(1);
    state
        .freed
        .take(&mut freed)
        .expect("the slots are recorded as freed");
    assert!(freed.contains(1) && freed.contains(2));
}

/// A reset of a held slot's queue and binding that a producer waits on, or that borrows send
/// credit, doesn't end the slot's hold as it leaves for its lane: only a reset without either,
/// as the endpoint sends one for a rejected init, does. While the hold stays, the slot isn't
/// recorded as freed, and another rejected init can't hold it.
#[test]
fn the_priority_router_keeps_the_hold_of_a_slot_whose_reset_has_a_completion_or_send_credit() {
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

    let holder = ping(&pse);
    let holder_cost = holder.byte_cost();
    submit(&mut frame_tx, [holder]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(holder_cost, 0)]);

    // The server rejects an init of binding 1 on slot 1 and frees the slot, which waits on the
    // reset that the endpoint sends for the init.
    let (mut freed_tx, _freed_rx) = crate::queue::freed_batch_channel();
    assert!(state.freed.hold(VarInt::from_u8(1), VarInt::from_u8(1)));
    state.freed.record(VarInt::from_u8(1), &pse, &mut freed_tx);
    assert!(!state.freed.has_pending_work(), "the slot waits on its reset");

    // A reset of the slot's queue and binding whose producer waits on its completion waits at the
    // window and then leaves for its lane, and the slot stays held. The poll that swaps the reset
    // in returns ready, and the next poll finds the channel empty.
    let completions = frame::failure_completion_channel();
    let mut awaited = reset_frame(&pse, 2, 1);
    awaited.completion = Some(completions.sender());
    let awaited_cost = awaited.byte_cost();
    submit(&mut frame_tx, [awaited]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(poll_router(&mut router), Poll::Pending);
    assert!(take_lanes(&mut lanes).is_empty());
    window.release_peer(Some(PeerKey::of(&pse)), holder_cost, false);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    // The producer stops waiting before the reset leaves its lane, so that it may drop with a
    // pending status.
    drop(completions);
    assert_eq!(take_lanes(&mut lanes), [(awaited_cost, 0)]);
    assert!(
        !state.freed.has_pending_work(),
        "a reset that a producer waits on doesn't record the slot as freed"
    );
    assert!(
        !state.freed.hold(VarInt::from_u8(1), VarInt::from_u8(2)),
        "a reset that a producer waits on doesn't end the hold"
    );

    // A reset of the slot's queue and binding with send credit goes into its lane at once, though
    // the window holds the first reset's bytes, and the slot stays held.
    let mut credited = reset_frame(&pse, 2, 1);
    credited.flow_credits = 1;
    let credited_cost = credited.byte_cost();
    submit(&mut frame_tx, [credited]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(credited_cost, 1)]);
    assert!(
        !state.freed.has_pending_work(),
        "a reset with send credit doesn't record the slot as freed"
    );
    assert!(
        !state.freed.hold(VarInt::from_u8(1), VarInt::from_u8(2)),
        "a reset with send credit doesn't end the hold"
    );

    drop(frame_tx);
    assert_eq!(poll_router(&mut router), Poll::Ready(None));
}

/// A reset that leaves for its lane records the end of its slot's hold, once: a reset that waits at
/// the window doesn't record it, and a reset that leaves while its slot is held for another binding
/// doesn't end that hold or record anything.
#[test]
fn the_priority_router_records_each_hold_that_a_leaving_reset_releases() {
    use crate::endpoint::frame_trace::{resident_slot_holds, HoldOutcome};
    use core::task::Poll;
    use s2n_quic_core::varint::VarInt;

    // The recorder's capture starts off; arm it, as a test process doesn't otherwise.
    crate::testing::init_tracing();

    // An address no other test uses, so that the records of its credentials in the process-wide
    // recorder are this test's alone.
    let pse = crate::path::secret::map::Entry::builder("127.0.0.1:44702".parse().unwrap())
        .socket_sender_count(1)
        .endpoint_type(s2n_quic_core::endpoint::Type::Server)
        .build(None);
    let crate::path::secret::map::entry::QueueState::Server(state) = pse.queue_state() else {
        unreachable!("a server entry has a server queue state");
    };
    let state = state.clone();
    let window = crate::endpoint::combinator::CreditlessWindow::default();
    let (mut frame_tx, mut router, mut lanes) = priority_router(window.clone());

    let holder = ping(&pse);
    let holder_cost = holder.byte_cost();
    submit(&mut frame_tx, [holder]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes), [(holder_cost, 0)]);

    // Slot 1 is held for binding 1, and its reset waits at the window: the hold doesn't end. The
    // poll that swaps the reset in returns ready, and the next poll finds the channel empty.
    assert!(state.freed.hold(VarInt::from_u8(1), VarInt::from_u8(1)));
    submit(&mut frame_tx, [reset_frame(&pse, 2, 1)]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(poll_router(&mut router), Poll::Pending);
    assert!(take_lanes(&mut lanes).is_empty());
    assert!(resident_slot_holds(*pse.id()).is_empty());

    // A worker's sender takes the holder's batch, and the reset leaves for its lane and ends the
    // hold, which is recorded once.
    window.release_peer(Some(PeerKey::of(&pse)), holder_cost, false);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes).len(), 1);
    let released = vec![(1, 1, HoldOutcome::Released as u8)];
    assert_eq!(resident_slot_holds(*pse.id()), released);
    drop(frame_tx);
    assert_eq!(poll_router(&mut router), Poll::Ready(None));

    // Slot 3 is held for binding 2, and a reset of slot 3 for binding 1 leaves: the hold stays,
    // and nothing more is recorded.
    assert!(state.freed.hold(VarInt::from_u8(3), VarInt::from_u8(2)));
    let (mut frame_tx, mut router, mut lanes) =
        priority_router(crate::endpoint::combinator::CreditlessWindow::default());
    let mut reset = reset_frame(&pse, 2, 1);
    if let frame::Header::QueueReset { queue_pair, .. } = &mut reset.header {
        queue_pair.source_queue_id = VarInt::from_u8(3);
    }
    submit(&mut frame_tx, [reset]);
    assert_eq!(poll_router(&mut router), Poll::Ready(Some(())));
    assert_eq!(take_lanes(&mut lanes).len(), 1);
    assert_eq!(resident_slot_holds(*pse.id()), released);
    assert!(
        !state.freed.hold(VarInt::from_u8(3), VarInt::from_u8(4)),
        "the hold of binding 2 stays"
    );
}

// -- The window in each batch router of `frame_dispatch` -----------------------------------

/// Like [`setup_with_pacing`] but over `window`, the window of the frames without send credit,
/// whose limit the test reads through a clone.
fn setup_over(
    num_workers: usize,
    pacing: SendPacing,
    window: crate::endpoint::combinator::CreditlessWindow,
) -> (SubmissionSender, Vec<WorkerRx>) {
    let (frame_tx, frame_rx) = frame::submission_channel(1);

    let mut workers = Vec::with_capacity(num_workers);
    let mut rxs = Vec::with_capacity(num_workers);
    for _ in 0..num_workers {
        let (tx, rx) = unsync::new::<FrameBatch>();
        workers.push(tx);
        rxs.push(rx);
    }

    let mut spawner = Local::new(0);
    tasks::frame_dispatch_over(
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
        None,
        window,
        crate::queue::freed_batch_channel().0,
    );

    (frame_tx, rxs)
}

/// A batch of the highest lane goes at once to the interface for the highest-priority traffic,
/// so it doesn't wait in the batch router, and the window's limit doesn't halve as a worker's
/// sender takes it, as it would for a batch that waited. The bulk frame's batch finds a free
/// interface and doesn't wait either.
#[test]
fn a_highest_priority_batch_that_doesnt_wait_keeps_the_windows_limit() {
    use crate::endpoint::combinator::{CreditlessCost as _, CreditlessWindow};

    const LIMIT: u64 = 1_000_000;
    let _snapshots = crate::testing::without_snapshots();
    let arrivals = Arc::new(Mutex::new(Vec::<(usize, bool, bool, u64)>::new()));
    let arrivals_in_sim = arrivals.clone();

    sim(move || {
        // Worker 0 is on the slow interface, and worker 1 on the fast one, which carries the
        // highest-priority traffic.
        let pacing = SendPacing::new(vec![Rate::new(1.0), Rate::new(4.0)], vec![0, 1])
            .with_highest_priority(1);
        let window = CreditlessWindow::with_limit(LIMIT);
        let (mut frame_tx, rxs) = setup_over(2, pacing, window.clone());

        async move {
            // One submission holds a bulk frame and a ping of the same peer, neither of which
            // borrows send credit, so the priority router moves both in one pass. The ping goes
            // in a batch of the highest lane, and the bulk frame in a batch of the other lanes.
            let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
                .socket_sender_count(2)
                .build(None);
            let mut input = PriorityInput::default();
            input.push(test_frame_with_payload(&pse, 1000));
            let mut ping = test_frame(&pse);
            ping.header = frame::Header::Ping;
            input.push(ping);
            frame_tx.send_batch(input).unwrap();
        }
        .primary()
        .spawn();

        for (worker, mut worker_rx) in rxs.into_iter().enumerate() {
            let arrivals = arrivals_in_sim.clone();
            let window = window.clone();
            async move {
                while let Some(batch) = worker_rx.recv().await {
                    // The worker's sender released the batch's bytes as it took the batch.
                    arrivals.lock().unwrap().push((
                        worker,
                        batch.holds_highest_priority(),
                        batch.waited(),
                        window.limit(),
                    ));
                }
            }
            .primary()
            .spawn();
        }
    });

    let arrivals = arrivals.lock().unwrap();
    assert_eq!(
        arrivals.len(),
        2,
        "the ping and the bulk frame go in two batches"
    );
    assert!(
        arrivals
            .iter()
            .any(|&(worker, highest, _, _)| highest && worker == 1),
        "the ping's batch is of the highest priority level and goes to the fast interface"
    );
    for &(worker, highest, waited, limit) in arrivals.iter() {
        assert!(
            !waited,
            "worker {worker}'s batch (highest: {highest}) didn't wait in the batch router"
        );
        assert_eq!(
            limit, LIMIT,
            "the window's limit after worker {worker}'s batch (highest: {highest})"
        );
    }
}

/// `frame_dispatch` shares one window among its priority router, its batch router and each
/// worker's sender: each batch's bytes leave the window as the batch reaches its worker, and the
/// window's limit grows where the worker could take a batch while the window refuses a frame.
#[test]
fn frame_dispatch_shares_its_window_among_the_routers_and_the_workers_senders() {
    shares_its_window(SendPacing::single(Rate::new(100.0)), 1);
}

/// As `frame_dispatch_shares_its_window_among_the_routers_and_the_workers_senders`, where the
/// workers' send sockets are on two interfaces, which `InterfacePacedPickTwo` paces.
#[test]
fn frame_dispatch_shares_its_window_under_two_interfaces() {
    let rate = Rate::new(100.0);
    shares_its_window(SendPacing::new(vec![rate, rate], vec![0, 1]), 2);
}

/// Runs `frame_dispatch` with `pacing` and a worker for each of `workers` send sockets over a
/// window of the test's, and checks the bytes the window holds and its limit as each batch
/// reaches its worker.
fn shares_its_window(pacing: SendPacing, workers: usize) {
    const FRAMES: usize = 10;
    let _snapshots = crate::testing::without_snapshots();
    // The bytes the window holds and its limit as each batch reaches a worker.
    let arrivals = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
    let arrivals_in_sim = arrivals.clone();

    sim(move || {
        let window = crate::endpoint::combinator::CreditlessWindow::new(
            crate::endpoint::combinator::BATCH_TARGET_BYTES,
        );
        let (mut frame_tx, worker_rxs) = setup_over(workers, pacing.clone(), window.clone());

        // Each frame fills a batch of its own, as in `pacing_delays_frames_beyond_burst`, and
        // doesn't borrow send credit.
        async move {
            let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
                .socket_sender_count(workers)
                .build(None);
            let mut input = PriorityInput::default();
            for _ in 0..FRAMES {
                input.push(test_frame_with_payload(&pse, u16::MAX as usize));
            }
            frame_tx.send_batch(input).unwrap();
        }
        .primary()
        .spawn();

        for mut worker_rx in worker_rxs {
            let arrivals = arrivals_in_sim.clone();
            let window = window.clone();
            async move {
                while let Some(_batch) = worker_rx.recv().await {
                    arrivals
                        .lock()
                        .unwrap()
                        .push((window.held(), window.limit()));
                }
            }
            .primary()
            .spawn();
        }
    });

    let arrivals = arrivals.lock().unwrap();
    assert_eq!(arrivals.len(), FRAMES, "one batch for each frame");
    // The priority router admits the frames into the window that the worker's sender releases:
    // a batch arrives while the next one's bytes are in the window, and none are left once the
    // last batch arrives.
    assert!(
        arrivals.iter().any(|&(held, _)| held > 0),
        "the window didn't hold a batch as another arrived: {arrivals:?}",
    );
    assert_eq!(
        arrivals.last().map(|&(held, _)| held),
        Some(0),
        "{arrivals:?}"
    );
    // The batch router tells the same window where the worker could take a batch that it
    // doesn't have, so the limit grows past its minimum.
    assert!(
        arrivals
            .iter()
            .any(|&(_, limit)| limit > crate::endpoint::combinator::BATCH_TARGET_BYTES),
        "the limit didn't grow: {arrivals:?}",
    );
}

// -- The counted-lost events of each batch router of `frame_dispatch` ----------------------

/// Routes one frame through `frame_dispatch` with a path secret map that publishes its events
/// to a testing subscriber, and returns the subscriber's counts of the events that start and end
/// a sender's count as lost for sharing a lost pair's interface. The frame's entry has four
/// senders and a peer of two interfaces, and sender 0's pair is marked lost.
fn counted_lost_events(pacing: SendPacing) -> (u64, u64) {
    let _snapshots = crate::testing::without_snapshots();
    let (map, subscriber) = test_map_with_subscriber();

    sim(move || {
        let (mut frame_tx, rxs) = setup_with_map(4, pacing, Some(map));
        let (result_tx, mut result_rx) = unsync::new::<FrameBatch>();

        async move {
            // The peer has two addresses, on its interfaces 1 and 2.
            let peer = |last: u8| std::net::SocketAddr::from(([10, 0, 0, last], 4000));
            let pse = crate::path::secret::map::Entry::builder("127.0.0.1:4433".parse().unwrap())
                .socket_sender_count(4)
                .peer_data_addrs(vec![peer(1), peer(2)])
                .peer_data_interfaces(vec![Some(1), Some(2)])
                .build(None);
            assert!(pse.set_sender_lost(crate::endpoint::id::LocalSenderId::from_index(0), true));
            let mut input = PriorityInput::default();
            input.push(test_frame(&pse));
            frame_tx.send_batch(input).unwrap();
        }
        .primary()
        .spawn();

        for mut worker_rx in rxs {
            let mut result_tx = result_tx.clone();
            async move {
                while let Some(batch) = worker_rx.recv().await {
                    let _ = UnboundedSender::send(&mut result_tx, batch);
                }
            }
            .spawn();
        }

        // The router publishes the events as it routes the frame's batch, before a worker
        // takes the batch.
        async move {
            let batch = result_rx.recv().await.unwrap();
            assert_eq!(batch.into_inner().len(), 1);
        }
        .primary()
        .spawn();
    });

    (
        event_count(&subscriber.path_secret_map_sender_counted_lost),
        event_count(&subscriber.path_secret_map_sender_counted_lost_cleared),
    )
}

/// The one-interface router counts sender 2, which sends to the peer interface of sender 0's
/// lost pair, as lost, and publishes the start of that count through the map that
/// `frame_dispatch` takes.
#[test]
fn pick_two_publishes_the_counted_lost_event_through_the_path_secret_map() {
    assert_eq!(counted_lost_events(SendPacing::single(Rate::new(100.0))), (1, 0));
}

/// The interface-paced router counts sender 1, which shares both interfaces of sender 0's lost
/// pair, as lost, and publishes the start of that count through the map that `frame_dispatch`
/// takes.
#[test]
fn interface_paced_pick_two_publishes_the_counted_lost_event_through_the_path_secret_map() {
    // Senders 0 and 1 send on local interface 0, and senders 2 and 3 on local interface 1.
    let pacing = SendPacing::new(vec![Rate::new(100.0), Rate::new(100.0)], vec![0, 0, 1, 1]);
    assert_eq!(counted_lost_events(pacing), (1, 0));
}
