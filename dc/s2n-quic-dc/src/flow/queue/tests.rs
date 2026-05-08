// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{Allocator, Key};
use crate::intrusive_queue;
use s2n_quic_core::varint::VarInt;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Barrier,
    },
    thread,
};

#[derive(Clone, Copy, Debug)]
struct TestKey(u64);

impl Key for TestKey {
    type Request = u64;

    #[inline]
    fn validate(&self, params: &Self::Request) -> bool {
        self.0 == *params
    }
}

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

struct ActiveQueue {
    key_id: u64,
    seq: u64,
    control: TestControl,
    stream: TestStream,
}

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

#[inline]
fn next_u64(state: &mut u64) -> u64 {
    // xorshift64*: fast deterministic pseudo-random generator for reproducible stress scheduling.
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

#[inline]
fn remove_random_queue(
    active_ids: &mut Vec<VarInt>,
    active: &mut BTreeMap<VarInt, ActiveQueue>,
    rng: &mut u64,
) {
    if active_ids.is_empty() {
        return;
    }

    let idx = (next_u64(rng) as usize) % active_ids.len();
    let queue_id = active_ids.swap_remove(idx);
    let Some(queue) = active.remove(&queue_id) else {
        panic!("missing active queue {queue_id}");
    };

    if queue.seq % 2 == 0 {
        drop(queue.stream);
        drop(queue.control);
    } else {
        drop(queue.control);
        drop(queue.stream);
    }
}

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
    assert_eq!(queue_id, control.queue_id());
    assert_eq!(stream.remote_queue_id(), None);
    assert_eq!(control.remote_queue_id(), None);

    let inserted = active.insert(
        queue_id,
        ActiveQueue {
            key_id,
            seq: 0,
            control,
            stream,
        },
    );
    assert!(inserted.is_none(), "queue IDs must be unique");
    active_ids.push(queue_id);

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
        .expect("stream dispatch should route to freshly allocated queue");
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
        .expect("control dispatch should route to freshly allocated queue");

    let queue = active.get_mut(&queue_id).unwrap();
    assert_eq!(queue.stream.remote_queue_id(), Some(remote_queue_id));
    assert_eq!(queue.control.remote_queue_id(), Some(remote_queue_id));

    let msg = queue.stream.try_recv().unwrap().unwrap().into_inner();
    assert_eq!(
        msg,
        Message {
            queue_id,
            key_id,
            seq: u64::MAX - 1,
        }
    );
    let msg = queue.control.try_recv().unwrap().unwrap().into_inner();
    assert_eq!(
        msg,
        Message {
            queue_id,
            key_id,
            seq: u64::MAX,
        }
    );
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
                let mut rng = ((worker as u64) + 1) * 0x9E37_79B9_7F4A_7C15;
                let mut active_ids = Vec::with_capacity(config.target_active_queues);
                let mut active = BTreeMap::<VarInt, ActiveQueue>::new();

                start.wait();

                for _ in 0..config.target_active_queues {
                    alloc_queue(
                        &mut dispatch,
                        &next_key,
                        &mut active_ids,
                        &mut active,
                    );
                }

                for _iter in 0..config.iters_per_thread {
                    for _ in 0..config.alloc_burst {
                        alloc_queue(
                            &mut dispatch,
                            &next_key,
                            &mut active_ids,
                            &mut active,
                        );
                    }

                    for _ in 0..config.route_batch {
                        let idx = (next_u64(&mut rng) as usize) % active_ids.len();
                        let queue_id = active_ids[idx];

                        let queue = active.get_mut(&queue_id).expect("queue should be active");
                        assert_eq!(queue.stream.queue_id(), queue_id);
                        assert_eq!(queue.control.queue_id(), queue_id);

                        let seq = queue.seq;
                        queue.seq += 1;

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
                            .expect("stream send must route to selected active queue");
                        dispatch
                            .send_control(
                                queue_id,
                                None,
                                &queue.key_id,
                                intrusive_queue::Entry::new(control_msg),
                            )
                            .expect("control send must route to selected active queue");

                        assert_eq!(
                            queue.stream.try_recv().unwrap().unwrap().into_inner(),
                            stream_msg,
                            "stream entry must be delivered to matching queue_id"
                        );
                        assert_eq!(
                            queue.control.try_recv().unwrap().unwrap().into_inner(),
                            control_msg,
                            "control entry must be delivered to matching queue_id"
                        );
                        assert!(
                            queue.stream.try_recv().unwrap().is_none(),
                            "stream queue should only receive routed entry"
                        );
                        assert!(
                            queue.control.try_recv().unwrap().is_none(),
                            "control queue should only receive routed entry"
                        );
                    }

                    if active_ids.len() > config.min_retained_queues {
                        let removable = active_ids.len() - config.min_retained_queues;
                        let to_free = config.free_batch.min(removable);
                        for _ in 0..to_free {
                            remove_random_queue(&mut active_ids, &mut active, &mut rng);
                        }
                    }

                    while active_ids.len() < config.target_active_queues {
                        alloc_queue(
                            &mut dispatch,
                            &next_key,
                            &mut active_ids,
                            &mut active,
                        );
                    }
                }

                while !active_ids.is_empty() {
                    remove_random_queue(&mut active_ids, &mut active, &mut rng);
                }
            }));
        }

        for worker in workers {
            worker.join().unwrap();
        }
    });
}

#[test]
// Run with `cargo test -p s2n-quic-dc -- flow::queue::tests::stress_alloc_grow_and_route_multi_threaded --ignored`
#[ignore = "contention stress; long-running oracle routing verification"]
fn stress_alloc_grow_and_route_multi_threaded() {
    run_oracle_stress(Config {
        threads: 4,
        iters_per_thread: 32,
        target_active_queues: 64,
        min_retained_queues: 48,
        alloc_burst: 2,
        route_batch: 4,
        free_batch: 2,
    });
}

#[test]
// Run with `cargo test -p s2n-quic-dc -- flow::queue::tests::stress_alloc_grow_and_route_multi_threaded_contention --ignored`
#[ignore = "expensive stress test with high contention and retained queue sets"]
fn stress_alloc_grow_and_route_multi_threaded_contention() {
    run_oracle_stress(Config {
        threads: 8,
        iters_per_thread: 600,
        target_active_queues: 2_048,
        min_retained_queues: 1_536,
        alloc_burst: 16,
        route_batch: 32,
        free_batch: 16,
    });
}

#[test]
// Run with `cargo test -p s2n-quic-dc -- flow::queue::tests::stress_alloc_grow_and_route_multi_threaded_huge --ignored`
#[ignore = "expensive stress test for very large retained queue sets"]
fn stress_alloc_grow_and_route_multi_threaded_huge() {
    run_oracle_stress(Config {
        threads: 8,
        iters_per_thread: 8_000,
        target_active_queues: 25_000,
        min_retained_queues: 20_000,
        alloc_burst: 16,
        route_batch: 32,
        free_batch: 16,
    });
}

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

    for _ in 0..512 {
        let idx = active_ids.len() / 2;
        let queue_id = active_ids[idx];
        let queue = active.get_mut(&queue_id).unwrap();
        let seq = queue.seq;
        queue.seq += 1;

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

        assert_eq!(queue.stream.try_recv().unwrap().unwrap().into_inner(), stream_msg);
        assert_eq!(
            queue.control.try_recv().unwrap().unwrap().into_inner(),
            control_msg
        );
    }

    while !active_ids.is_empty() {
        let queue_id = active_ids.pop().unwrap();
        let queue = active.remove(&queue_id).unwrap();
        drop(queue.control);
        drop(queue.stream);
    }
}
