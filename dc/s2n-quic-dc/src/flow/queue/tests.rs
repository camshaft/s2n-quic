// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{Allocator, Key};
use crate::intrusive_queue;
use s2n_quic_core::varint::VarInt;
use std::{
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

#[test]
fn stress_alloc_grow_and_route_multi_threaded() {
    crate::testing::init_tracing();

    const THREADS: usize = 8;
    const ITERS_PER_THREAD: usize = 1_500;
    const WORKER_ID_SHIFT: usize = 24;
    const YIELD_INTERVAL: usize = 64;

    let allocator = Allocator::<usize, usize, TestKey>::new();
    let dispatcher = allocator.dispatcher();
    let start = Arc::new(Barrier::new(THREADS));
    let next_key = Arc::new(AtomicU64::new(0));

    thread::scope(|scope| {
        let mut workers = Vec::with_capacity(THREADS);

        for worker_id in 0..THREADS {
            let mut dispatch = dispatcher.clone();
            let start = start.clone();
            let next_key = next_key.clone();

            workers.push(scope.spawn(move || {
                start.wait();

                for iter in 0..ITERS_PER_THREAD {
                    let key_id = next_key.fetch_add(1, Ordering::Relaxed);
                    let key = TestKey(key_id);
                    let (control, stream) = dispatch.alloc_or_grow(key, None);
                    let queue_id = stream.queue_id();
                    assert_eq!(queue_id, control.queue_id());
                    assert_eq!(stream.remote_queue_id(), None);
                    assert_eq!(control.remote_queue_id(), None);

                    let stream_value = (worker_id << WORKER_ID_SHIFT) | iter;
                    let control_value = stream_value ^ usize::MAX;
                    let remote_queue_id = VarInt::new((iter as u64) + 1).unwrap();

                    dispatch
                        .send_stream(
                            queue_id,
                            Some(remote_queue_id),
                            &key_id,
                            intrusive_queue::Entry::new(stream_value),
                        )
                        .expect("stream dispatch should route to allocated queue");
                    dispatch
                        .send_control(
                            queue_id,
                            Some(remote_queue_id),
                            &key_id,
                            intrusive_queue::Entry::new(control_value),
                        )
                        .expect("control dispatch should route to allocated queue");

                    assert_eq!(stream.remote_queue_id(), Some(remote_queue_id));
                    assert_eq!(control.remote_queue_id(), Some(remote_queue_id));
                    assert_eq!(
                        stream.try_recv().unwrap().unwrap().into_inner(),
                        stream_value
                    );
                    assert_eq!(
                        control.try_recv().unwrap().unwrap().into_inner(),
                        control_value
                    );
                    assert!(stream.try_recv().unwrap().is_none());
                    assert!(control.try_recv().unwrap().is_none());

                    if iter % 2 == 0 {
                        drop(stream);
                        drop(control);
                    } else {
                        drop(control);
                        drop(stream);
                    }

                    if iter % YIELD_INTERVAL == 0 {
                        thread::yield_now();
                    }
                }
            }));
        }

        for worker in workers {
            worker.join().unwrap();
        }
    });
}
