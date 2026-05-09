// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Throughput test for the sharded intrusive queue channel.
//!
//! Spins up a configurable number of sender threads that each continuously
//! submit single-entry lists (worst case for contention) and a single receiver
//! thread that drains the queue. Throughput is reported every second.
//!
//! Usage: sharded_queue_throughput [senders] [shards] [duration_secs]
//!   senders      – number of sender threads (default: 4)
//!   shards       – shard count, must be a power of two (default: next power of
//!                  two >= senders, minimum 1)
//!   duration_secs – how long to run in seconds (default: 10)

use s2n_quic_dc::{
    intrusive_queue::{Entry, Queue},
    socket::channel::{intrusive_queue::sharded, Receiver as _},
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    task::{Context, RawWaker, RawWakerVTable, Waker},
    thread,
    time::{Duration, Instant},
};

// ── Thread-unpark waker ───────────────────────────────────────────────────────

static THREAD_WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_waker, wake, wake_by_ref, drop_waker);

/// Build a `Waker` that unparks `thread` when woken.
fn thread_unpark_waker(thread: thread::Thread) -> Waker {
    let ptr = Box::into_raw(Box::new(thread)) as *const ();
    unsafe { Waker::from_raw(RawWaker::new(ptr, &THREAD_WAKER_VTABLE)) }
}

unsafe fn clone_waker(ptr: *const ()) -> RawWaker {
    let t = &*(ptr as *const thread::Thread);
    let cloned = Box::new(t.clone());
    RawWaker::new(Box::into_raw(cloned) as *const (), &THREAD_WAKER_VTABLE)
}

unsafe fn wake(ptr: *const ()) {
    let t = Box::from_raw(ptr as *mut thread::Thread);
    t.unpark();
}

unsafe fn wake_by_ref(ptr: *const ()) {
    let t = &*(ptr as *const thread::Thread);
    t.unpark();
}

unsafe fn drop_waker(ptr: *const ()) {
    drop(Box::from_raw(ptr as *mut thread::Thread));
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let n_senders: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(4);
    let n_shards_arg: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let duration_secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);

    // Shards must be a power of two. If the caller didn't provide one (or
    // provided 0), pick the next power of two >= n_senders (minimum 1).
    let n_shards = if n_shards_arg.is_power_of_two() && n_shards_arg > 0 {
        n_shards_arg
    } else {
        n_senders.next_power_of_two().max(1)
    };

    let duration = Duration::from_secs(duration_secs);

    eprintln!(
        "sharded_queue_throughput: senders={n_senders} shards={n_shards} duration={duration_secs}s"
    );

    // Create channel. The waker must be registered before senders are cloned or
    // exposed to other threads, so we register immediately using the current
    // (receiver) thread.
    let (tx, mut rx) = sharded::new::<u64>(n_shards);
    let waker = thread_unpark_waker(thread::current());
    rx.register(&waker);

    let received = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    // Spawn sender threads. Each clones `tx` (which registers its shard inside
    // `clone`) and loops, submitting one-entry lists until `stop` is set.
    let sender_handles: Vec<_> = (0..n_senders)
        .map(|_| {
            // Clone before moving into the thread so the original `tx` outlives
            // all clones and is dropped last, after all threads have stopped.
            let mut sender = tx.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let mut list = Queue::new();
                    list.push_back(Entry::new(0u64));
                    // send_batch returns Err if the receiver has been dropped.
                    if sender.send_batch(list).is_err() {
                        break;
                    }
                }
            })
        })
        .collect();

    // Drop the original sender so only the per-thread clones keep the channel
    // alive.
    drop(tx);

    // Stats thread: print throughput every second.
    {
        let received = received.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            let mut prev = 0u64;
            loop {
                thread::sleep(Duration::from_secs(1));
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let curr = received.load(Ordering::Relaxed);
                eprintln!("{} msgs/s", curr - prev);
                prev = curr;
            }
        });
    }

    // Receiver loop (main thread).
    // The channel ignores the cx waker – wakeups come through the pre-registered
    // thread-unpark waker – so we pass a dummy context.
    let cx_waker = waker.clone();
    let mut cx = Context::from_waker(&cx_waker);
    let deadline = Instant::now() + duration;

    loop {
        match rx.poll_recv(&mut cx) {
            std::task::Poll::Ready(Some(list)) => {
                received.fetch_add(list.len() as u64, Ordering::Relaxed);
            }
            std::task::Poll::Ready(None) => {
                // All senders dropped – shouldn't happen during the test run.
                break;
            }
            std::task::Poll::Pending => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                // Park until a sender wakes us (via the registered waker) or the
                // deadline timeout fires.
                thread::park_timeout(remaining);
            }
        }
    }

    // Signal senders to stop and wait for them.
    stop.store(true, Ordering::Relaxed);
    for h in sender_handles {
        let _ = h.join();
    }

    let total = received.load(Ordering::Relaxed);
    eprintln!("total received: {total} msgs in {duration_secs}s");
    eprintln!(
        "average: {} msgs/s",
        total / duration_secs.max(1)
    );
}
