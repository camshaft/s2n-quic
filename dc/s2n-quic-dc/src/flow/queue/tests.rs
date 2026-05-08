// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{Allocator, Key};
use crate::intrusive_queue;
use s2n_quic_core::varint::VarInt;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Barrier, Mutex,
    },
    thread,
};

// ── Shared test types ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct TestKey(u64);

impl Key for TestKey {
    type Request = u64;

    #[inline]
    fn validate(&self, params: &Self::Request) -> bool {
        self.0 == *params
    }
}

/// A message that carries the originating queue_id and a monotonic sequence so the
/// receiver can assert both correct routing (queue_id matches the handle) and ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Message {
    queue_id: VarInt,
    key_id: u64,
    seq: u64,
}

type TestAllocator = Allocator<Message, Message, TestKey>;
type TestDispatch = super::Dispatch<Message, Message, TestKey>;
type TestControl = super::Control<Message, Message, TestKey>;
type TestStream = super::Stream<Message, Message, TestKey>;

// ── Deterministic PRNG ───────────────────────────────────────────────────────

/// xorshift64 with multiplicative scramble for fast, reproducible scheduling decisions.
#[inline]
fn next_u64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

// ── Per-thread oracle helpers ────────────────────────────────────────────────

/// A locally-owned active queue tracked by the thread-local oracle.
struct ActiveQueue {
    key_id: u64,
    /// Number of messages sent to this queue (by *any* thread).
    send_seq: u64,
    /// Number of messages received from this queue (always the owner thread).
    recv_seq: u64,
    control: TestControl,
    stream: TestStream,
}

/// Allocate a new queue, send a canary message to each half, and immediately drain both.
/// Verifies the freshly-allocated queue routes correctly.
#[inline]
fn alloc_queue(
    dispatch: &mut TestDispatch,
    next_key: &AtomicU64,
    active_ids: &mut Vec<VarInt>,
    active: &mut BTreeMap<VarInt, ActiveQueue>,
) {
    let key_id = next_key.fetch_add(1, Ordering::Relaxed);
    let key = TestKey(key_id);
    let remote_queue_id = VarInt::new(key_id + 1).unwrap();
    let (control, stream) = dispatch.alloc_or_grow(key, None);

    let queue_id = stream.queue_id();
    assert_eq!(
        queue_id,
        control.queue_id(),
        "stream and control must share the same queue_id"
    );
    assert_eq!(stream.remote_queue_id(), None);
    assert_eq!(control.remote_queue_id(), None);

    // Queue_ids must be globally unique across all currently-active queues.
    assert!(
        !active.contains_key(&queue_id),
        "queue_id {queue_id} allocated twice while still active"
    );

    active.insert(
        queue_id,
        ActiveQueue {
            key_id,
            send_seq: 0,
            recv_seq: 0,
            control,
            stream,
        },
    );
    active_ids.push(queue_id);

    // Canary send: verify routing to a freshly opened queue before any other activity.
    dispatch
        .send_stream(
            queue_id,
            Some(remote_queue_id),
            &key_id,
            intrusive_queue::Entry::new(Message {
                queue_id,
                key_id,
                seq: u64::MAX - 1,
            }),
        )
        .expect("stream dispatch must route to freshly allocated queue");
    dispatch
        .send_control(
            queue_id,
            Some(remote_queue_id),
            &key_id,
            intrusive_queue::Entry::new(Message {
                queue_id,
                key_id,
                seq: u64::MAX,
            }),
        )
        .expect("control dispatch must route to freshly allocated queue");

    let queue = active.get_mut(&queue_id).unwrap();

    // remote_queue_id must have been recorded via the first send
    assert_eq!(queue.stream.remote_queue_id(), Some(remote_queue_id));
    assert_eq!(queue.control.remote_queue_id(), Some(remote_queue_id));

    let msg = queue.stream.try_recv().unwrap().unwrap().into_inner();
    assert_eq!(
        msg,
        Message {
            queue_id,
            key_id,
            seq: u64::MAX - 1
        },
        "stream canary must arrive at the correct queue"
    );
    let msg = queue.control.try_recv().unwrap().unwrap().into_inner();
    assert_eq!(
        msg,
        Message {
            queue_id,
            key_id,
            seq: u64::MAX
        },
        "control canary must arrive at the correct queue"
    );
    assert!(
        queue.stream.try_recv().unwrap().is_none(),
        "stream queue must be empty after draining canary"
    );
    assert!(
        queue.control.try_recv().unwrap().is_none(),
        "control queue must be empty after draining canary"
    );
}

/// Drain all pending messages from a queue, asserting each message carries the correct
/// queue_id and that messages arrive in-order.
#[inline]
fn drain_queue(queue: &mut ActiveQueue) {
    loop {
        match queue.stream.try_recv().unwrap() {
            None => break,
            Some(entry) => {
                let msg = entry.into_inner();
                assert_eq!(
                    msg.queue_id, queue.stream.queue_id(),
                    "stream message must be routed to the correct queue"
                );
                assert_eq!(
                    msg.key_id, queue.key_id,
                    "stream message must carry the queue's own key_id"
                );
                assert_eq!(
                    msg.seq, queue.recv_seq,
                    "stream messages must arrive in-order; got seq={} expected={}",
                    msg.seq, queue.recv_seq,
                );
                queue.recv_seq += 1;
            }
        }
        match queue.control.try_recv().unwrap() {
            None => {}
            Some(entry) => {
                let msg = entry.into_inner();
                assert_eq!(
                    msg.queue_id, queue.control.queue_id(),
                    "control message must be routed to the correct queue"
                );
            }
        }
    }
}

/// Verify that all sent messages were received, then drop both handles.
#[inline]
fn verify_and_drop(
    active_ids: &mut Vec<VarInt>,
    active: &mut BTreeMap<VarInt, ActiveQueue>,
    idx: usize,
) {
    let queue_id = active_ids.swap_remove(idx);
    let mut queue = active.remove(&queue_id).unwrap();

    // Drain any remaining messages before dropping handles.
    // If the queue is not empty the caller was supposed to have consumed everything —
    // this catches messages that were sent but never received.
    drain_queue(&mut queue);

    assert_eq!(
        queue.send_seq, queue.recv_seq,
        "queue {queue_id}: send_seq={} but recv_seq={} — messages were lost",
        queue.send_seq, queue.recv_seq
    );
}

fn remove_random_queue(
    active_ids: &mut Vec<VarInt>,
    active: &mut BTreeMap<VarInt, ActiveQueue>,
    rng: &mut u64,
) {
    if active_ids.is_empty() {
        return;
    }
    let idx = (next_u64(rng) as usize) % active_ids.len();
    verify_and_drop(active_ids, active, idx);
}

// ── Single-thread routing oracle (baseline correctness) ──────────────────────

/// Config for `run_oracle_stress`
#[derive(Clone, Copy)]
struct Config {
    threads: usize,
    iters_per_thread: usize,
    target_active_queues: usize,
    min_retained_queues: usize,
    alloc_burst: usize,
    route_batch: usize,
    free_batch: usize,
}

fn run_oracle_stress(config: Config) {
    crate::testing::init_tracing();

    let allocator = TestAllocator::new();
    let dispatcher = allocator.dispatcher();
    let start = Arc::new(Barrier::new(config.threads));
    let next_key = Arc::new(AtomicU64::new(0));

    thread::scope(|scope| {
        let mut workers = Vec::with_capacity(config.threads);

        for worker in 0..config.threads {
            let mut dispatch = dispatcher.clone();
            let start = start.clone();
            let next_key = next_key.clone();

            workers.push(scope.spawn(move || {
                let mut rng = ((worker as u64) + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                let mut active_ids = Vec::with_capacity(config.target_active_queues);
                let mut active = BTreeMap::<VarInt, ActiveQueue>::new();

                start.wait();

                // Fill up to the target active queue count before stress begins
                for _ in 0..config.target_active_queues {
                    alloc_queue(&mut dispatch, &next_key, &mut active_ids, &mut active);
                }

                for _iter in 0..config.iters_per_thread {
                    // Burst-allocate then immediately send+recv to force descriptor
                    // recycling to happen while other threads still hold old senders.
                    for _ in 0..config.alloc_burst {
                        alloc_queue(&mut dispatch, &next_key, &mut active_ids, &mut active);
                    }

                    // Route a batch of messages and verify immediate delivery
                    for _ in 0..config.route_batch {
                        let idx = (next_u64(&mut rng) as usize) % active_ids.len();
                        let queue_id = active_ids[idx];

                        let queue = active.get_mut(&queue_id).expect("queue in oracle");
                        assert_eq!(queue.stream.queue_id(), queue_id);
                        assert_eq!(queue.control.queue_id(), queue_id);

                        let seq = queue.send_seq;
                        queue.send_seq += 1;

                        let stream_msg = Message {
                            queue_id,
                            key_id: queue.key_id,
                            seq,
                        };
                        let control_msg = Message {
                            queue_id,
                            key_id: queue.key_id,
                            seq: seq ^ u64::MAX,
                        };

                        dispatch
                            .send_stream(
                                queue_id,
                                None,
                                &queue.key_id,
                                intrusive_queue::Entry::new(stream_msg),
                            )
                            .expect("send to an active queue must succeed");
                        dispatch
                            .send_control(
                                queue_id,
                                None,
                                &queue.key_id,
                                intrusive_queue::Entry::new(control_msg),
                            )
                            .expect("send to an active queue must succeed");

                        let queue = active.get_mut(&queue_id).unwrap();
                        let got_stream =
                            queue.stream.try_recv().unwrap().unwrap().into_inner();
                        let got_control =
                            queue.control.try_recv().unwrap().unwrap().into_inner();

                        assert_eq!(
                            got_stream, stream_msg,
                            "stream entry must arrive at the matching queue_id"
                        );
                        assert_eq!(
                            got_control, control_msg,
                            "control entry must arrive at the matching queue_id"
                        );
                        assert!(
                            queue.stream.try_recv().unwrap().is_none(),
                            "stream queue must not have extra messages"
                        );
                        assert!(
                            queue.control.try_recv().unwrap().is_none(),
                            "control queue must not have extra messages"
                        );
                        queue.recv_seq = queue.send_seq; // kept in sync above
                    }

                    // Free a batch, staying above the minimum floor
                    if active_ids.len() > config.min_retained_queues {
                        let removable = active_ids.len() - config.min_retained_queues;
                        let to_free = config.free_batch.min(removable);
                        for _ in 0..to_free {
                            remove_random_queue(&mut active_ids, &mut active, &mut rng);
                        }
                    }

                    // Refill back to target
                    while active_ids.len() < config.target_active_queues {
                        alloc_queue(&mut dispatch, &next_key, &mut active_ids, &mut active);
                    }
                }

                while !active_ids.is_empty() {
                    remove_random_queue(&mut active_ids, &mut active, &mut rng);
                }
            }));
        }

        for w in workers {
            w.join().unwrap();
        }
    });
}

// ── Cross-thread routing oracle ──────────────────────────────────────────────
//
// This test exercises a scenario that the single-thread oracle cannot cover:
// multiple "sender" threads dispatch to queues owned by separate "owner/drain"
// threads, while an "allocator" thread concurrently replaces slots.
//
// Oracle invariant:
//   Every message that is received from a handle must carry the same queue_id
//   and key_id as the handle it arrived on.  This catches any routing bug
//   (a message intended for queue X ending up on queue Y).
//
// No sequence-number ordering is asserted here, because multiple concurrent
// senders to the same queue reserve seq numbers atomically but push to the
// queue in an unspecified order.  Ordering is verified by the single-thread
// oracle (run_oracle_stress).

/// A single shared directory slot.
///
/// `queue_id` and `key_id` are written by the allocator and read (lock-free)
/// by senders so they can route without holding the handle lock.
///
/// The `handles` mutex serialises the allocator's drain+replace and the
/// owner's periodic drain; senders are intentionally lock-free to create
/// real cross-thread routing races.
struct Slot {
    /// queue_id currently installed in this slot (AtomicU64::MAX = empty)
    queue_id: AtomicU64,
    /// key_id that validates sends to `queue_id`
    key_id: AtomicU64,
    /// Protects the live handles; None while the slot is being replaced
    handles: Mutex<Option<(TestStream, TestControl)>>,
}

impl Slot {
    fn new() -> Self {
        Self {
            queue_id: AtomicU64::new(u64::MAX),
            key_id: AtomicU64::new(0),
            handles: Mutex::new(None),
        }
    }
}

/// Drain all messages from a stream handle, asserting routing correctness.
fn drain_stream_assert_routing(
    stream: &mut TestStream,
    expected_queue_id: VarInt,
    expected_key_id: u64,
) {
    loop {
        match stream.try_recv().unwrap() {
            None => break,
            Some(entry) => {
                let msg = entry.into_inner();
                assert_eq!(
                    msg.queue_id, expected_queue_id,
                    "stream message must arrive at the correct queue \
                     (got queue_id={} expected={})",
                    msg.queue_id, expected_queue_id,
                );
                assert_eq!(
                    msg.key_id, expected_key_id,
                    "stream message carries wrong key_id on queue {expected_queue_id} \
                     (got key_id={} expected={})",
                    msg.key_id, expected_key_id,
                );
            }
        }
    }
}

fn run_cross_thread_stress(
    n_slots: usize,
    alloc_iters: usize,
    sender_iters: usize,
    owner_iters: usize,
    n_senders: usize,
    n_owners: usize,
) {
    crate::testing::init_tracing();

    let allocator = Arc::new(TestAllocator::new());
    let next_key = Arc::new(AtomicU64::new(1));

    // Shared directory: a fixed-size ring of slots.
    let slots: Arc<Vec<Slot>> = Arc::new((0..n_slots).map(|_| Slot::new()).collect());

    let start = Arc::new(Barrier::new(1 + n_senders + n_owners));

    thread::scope(|scope| {
        // ── Allocator / replacer thread ─────────────────────────────────────
        //
        // Continuously replaces slots with freshly allocated queues.  Before
        // replacing it drains the old handles to verify routing.
        {
            let allocator = allocator.clone();
            let next_key = next_key.clone();
            let slots = slots.clone();
            let start = start.clone();

            scope.spawn(move || {
                let mut dispatch = allocator.dispatcher();
                let mut rng = 0xDEAD_BEEF_CAFE_1234_u64;

                start.wait();

                for _iter in 0..alloc_iters {
                    let slot_idx = (next_u64(&mut rng) as usize) % n_slots;
                    let slot = &slots[slot_idx];

                    // Alloc the replacement before taking the lock to minimise
                    // the time the slot is held (reducing sender starvation).
                    let key_id = next_key.fetch_add(1, Ordering::Relaxed);
                    let key = TestKey(key_id);
                    let (new_control, new_stream) = dispatch.alloc_or_grow(key, None);
                    let new_qid = new_stream.queue_id();
                    assert_eq!(new_qid, new_control.queue_id());

                    let mut lock = slot.handles.lock().unwrap();

                    // Drain old handles (if present) while we hold the lock so
                    // no new sends can slip in after the drain.
                    if let Some((ref mut old_stream, _)) = *lock {
                        let old_qid =
                            VarInt::new(slot.queue_id.load(Ordering::Relaxed)).unwrap();
                        let old_kid = slot.key_id.load(Ordering::Relaxed);
                        drain_stream_assert_routing(old_stream, old_qid, old_kid);
                    }

                    // Publish the new queue atomically with the handle swap.
                    // Senders that read the atomics after this point will send
                    // to the new queue; senders that already read the old values
                    // will try to send to the now-closing old queue (valid: the
                    // key-validation path handles this correctly).
                    slot.queue_id
                        .store(new_qid.as_u64(), Ordering::Release);
                    slot.key_id.store(key_id, Ordering::Relaxed);
                    *lock = Some((new_stream, new_control));
                    drop(lock);
                }
            });
        }

        // ── Sender threads ───────────────────────────────────────────────────
        //
        // Each sender thread reads (queue_id, key_id) lock-free from a random
        // slot and sends a message.  This races with both the allocator (which
        // may be replacing the slot) and owner threads (which drain).
        for sender_id in 0..n_senders {
            let allocator = allocator.clone();
            let slots = slots.clone();
            let start = start.clone();

            scope.spawn(move || {
                let mut dispatch = allocator.dispatcher();
                let mut rng =
                    ((sender_id as u64) + 1).wrapping_mul(0x6C62_272E_07BB_0142);

                start.wait();

                for _iter in 0..sender_iters {
                    let slot_idx = (next_u64(&mut rng) as usize) % n_slots;
                    let slot = &slots[slot_idx];

                    // Read without locking — intentional race with allocator.
                    let raw_qid = slot.queue_id.load(Ordering::Acquire);
                    let key_id = slot.key_id.load(Ordering::Relaxed);
                    let Some(queue_id) = VarInt::new(raw_qid).ok() else {
                        continue; // slot not yet populated
                    };

                    let msg = Message {
                        queue_id,
                        key_id,
                        seq: 0, // seq not used in the routing oracle
                    };

                    // Send might fail if the queue was just freed — that is fine.
                    let _ = dispatch.send_stream(
                        queue_id,
                        None,
                        &key_id,
                        intrusive_queue::Entry::new(msg),
                    );
                }
            });
        }

        // ── Owner / drain threads ────────────────────────────────────────────
        //
        // Periodically drain a random slot, asserting routing correctness on
        // every message.  This runs concurrently with both the senders and the
        // allocator.
        for owner_id in 0..n_owners {
            let slots = slots.clone();
            let start = start.clone();

            scope.spawn(move || {
                let mut rng =
                    ((owner_id as u64) + 1).wrapping_mul(0x1234_5678_9ABC_DEF0);

                start.wait();

                for _iter in 0..owner_iters {
                    let slot_idx = (next_u64(&mut rng) as usize) % n_slots;
                    let slot = &slots[slot_idx];

                    let mut lock = slot.handles.lock().unwrap();
                    let Some((ref mut stream, _)) = *lock else {
                        continue;
                    };

                    let queue_id = VarInt::new(slot.queue_id.load(Ordering::Acquire))
                        .expect("active slot must have a valid queue_id");
                    let key_id = slot.key_id.load(Ordering::Relaxed);

                    drain_stream_assert_routing(stream, queue_id, key_id);
                    drop(lock);
                }
            });
        }
    });

    // Final drain: verify routing on any messages still in populated slots.
    for slot in slots.iter() {
        let mut lock = slot.handles.lock().unwrap();
        if let Some((ref mut stream, ref mut control)) = *lock {
            let queue_id =
                VarInt::new(slot.queue_id.load(Ordering::Relaxed)).expect("valid queue_id");
            let key_id = slot.key_id.load(Ordering::Relaxed);
            drain_stream_assert_routing(stream, queue_id, key_id);
            // Drain control too (no oracle beyond "must not error")
            while control.try_recv().unwrap().is_some() {}
        }
    }
}


// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn smoke_alloc_route_oracle_single_threaded() {
    crate::testing::init_tracing();

    let mut dispatch = TestAllocator::new().dispatcher();
    let next_key = AtomicU64::new(0);
    let mut active_ids = Vec::new();
    let mut active = BTreeMap::<VarInt, ActiveQueue>::new();

    for _ in 0..256 {
        alloc_queue(&mut dispatch, &next_key, &mut active_ids, &mut active);
    }

    for i in 0..512_usize {
        let idx = i % active_ids.len();
        let queue_id = active_ids[idx];
        let queue = active.get_mut(&queue_id).unwrap();
        let seq = queue.send_seq;
        queue.send_seq += 1;

        let stream_msg = Message {
            queue_id,
            key_id: queue.key_id,
            seq,
        };
        let control_msg = Message {
            queue_id,
            key_id: queue.key_id,
            seq: seq ^ u64::MAX,
        };

        dispatch
            .send_stream(
                queue_id,
                None,
                &queue.key_id,
                intrusive_queue::Entry::new(stream_msg),
            )
            .unwrap();
        dispatch
            .send_control(
                queue_id,
                None,
                &queue.key_id,
                intrusive_queue::Entry::new(control_msg),
            )
            .unwrap();

        assert_eq!(
            queue.stream.try_recv().unwrap().unwrap().into_inner(),
            stream_msg,
            "stream message must arrive at its own queue"
        );
        assert_eq!(
            queue.control.try_recv().unwrap().unwrap().into_inner(),
            control_msg,
            "control message must arrive at its own queue"
        );
        queue.recv_seq = queue.send_seq;
    }

    while !active_ids.is_empty() {
        remove_random_queue(&mut active_ids, &mut active, &mut 1234u64);
    }
}

/// Multi-threaded oracle with moderate churn — runs as part of the normal test suite.
///
/// Each thread owns its queue handles and drives alloc/route/free completely
/// independently.  The shared dispatcher adds cross-thread sender contention.
#[test]
fn stress_alloc_route_oracle_multi_threaded() {
    run_oracle_stress(Config {
        threads: 4,
        iters_per_thread: 64,
        target_active_queues: 128,
        min_retained_queues: 96,
        alloc_burst: 8,
        route_batch: 16,
        free_batch: 8,
    });
}

/// Cross-thread routing oracle — runs as part of the normal test suite.
///
/// Sender threads dispatch to queues owned by owner/drain threads; the allocator
/// thread concurrently replaces slots.  This is the test most likely to surface
/// routing-correctness bugs under concurrency.
#[test]
fn stress_cross_thread_routing_oracle() {
    run_cross_thread_stress(
        /* n_slots       */ 64,
        /* alloc_iters   */ 512,
        /* sender_iters  */ 2048,
        /* owner_iters   */ 2048,
        /* n_senders     */ 4,
        /* n_owners      */ 2,
    );
}

// ── Ignored (long-running) variants — run with --profile release-debug ───────
//
// cargo test -p s2n-quic-dc --profile release-debug -- <test_name> --ignored --exact

/// Aggressive single-owner multi-sender stress.
///
/// `cargo test -p s2n-quic-dc --profile release-debug -- flow::queue::tests::stress_heavy_alloc_churn --ignored --exact`
#[test]
#[ignore = "long-running; run with --profile release-debug"]
fn stress_heavy_alloc_churn() {
    run_oracle_stress(Config {
        threads: 8,
        iters_per_thread: 10_000,
        target_active_queues: 8_192,
        // Very aggressive churn: nearly all queues recycled every iteration.
        // This maximises descriptor reuse under concurrent senders.
        min_retained_queues: 256,
        alloc_burst: 512,
        route_batch: 512,
        free_batch: 2_048,
    });
}

/// Hundreds-of-thousands of concurrent streams with high retention.
///
/// `cargo test -p s2n-quic-dc --profile release-debug -- flow::queue::tests::stress_huge_retained --ignored --exact`
#[test]
#[ignore = "long-running; run with --profile release-debug"]
fn stress_huge_retained() {
    run_oracle_stress(Config {
        threads: 8,
        iters_per_thread: 10_000,
        target_active_queues: 100_000,
        min_retained_queues: 80_000,
        alloc_burst: 256,
        route_batch: 512,
        free_batch: 256,
    });
}

/// Cross-thread routing with very heavy sender / owner contention and rapid slot recycling.
///
/// n_slots is intentionally tiny (16) relative to alloc_iters so each slot is replaced
/// thousands of times, maximising the window for key-validation and routing races.
///
/// `cargo test -p s2n-quic-dc --profile release-debug -- flow::queue::tests::stress_cross_thread_heavy --ignored --exact`
#[test]
#[ignore = "long-running; run with --profile release-debug"]
fn stress_cross_thread_heavy() {
    run_cross_thread_stress(
        /* n_slots       */ 16,
        /* alloc_iters   */ 500_000,
        /* sender_iters  */ 2_000_000,
        /* owner_iters   */ 1_000_000,
        /* n_senders     */ 12,
        /* n_owners      */ 4,
    );
}

