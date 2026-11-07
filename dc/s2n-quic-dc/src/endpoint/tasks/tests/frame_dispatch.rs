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
        combinator::{FrameBatch, SendPacing},
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
