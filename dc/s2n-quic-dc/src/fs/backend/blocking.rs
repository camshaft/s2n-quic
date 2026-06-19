// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Drive a [`Receiver`] on a dedicated **blocking** OS thread.
//!
//! A storage worker thread runs a real blocking syscall (`pread`/`pwrite`) per op, so it cannot be a
//! cooperative async task. But it still wants the *same* channel and the *same* `Waker` wake-path as
//! the rest of the pipeline — so rather than hand-rolling a `Mutex + Condvar`, the worker drives a
//! normal [`Receiver<Entry<T>>`] (e.g. the [`sync`](crate::socket::channel::intrusive::sync) channel)
//! with a lightweight [`Waker`] that parks/unparks *this* thread. A producer's ordinary `waker.wake()`
//! both wakes async tasks and unparks blocked workers, with no special-casing at the send site.
//!
//! # Cooldown
//!
//! Blocking-park-per-op would add a futex round-trip to every op even under steady load. Instead the
//! worker **spins** (re-polling, yielding between attempts) up to [`SPIN_BEFORE_PARK`] times when the
//! queue is momentarily empty, and only then parks until woken. Under load it never parks (the next
//! poll finds work); when genuinely idle it parks and consumes no CPU. The park is lost-wakeup-safe:
//! the waker sets an `AtomicBool` *before* unparking, and the worker re-checks that flag in its park
//! loop, so a wake that races the park is never slept through (and a spurious unpark just re-polls).

use crate::{
    intrusive::Entry,
    socket::channel::{Budget, Receiver},
};
use core::task::{Context, Poll};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::Wake,
    thread::Thread,
};

/// Number of empty polls (each followed by a `yield_now`) a worker makes before it parks. Sized to
/// ride out a brief gap in the work stream without a futex round-trip, while still parking promptly
/// when truly idle so an idle pool burns no CPU.
const SPIN_BEFORE_PARK: u32 = 64;

/// A [`Waker`] that wakes one specific parked thread. Holds the `notified` flag the parked thread
/// re-checks, so a wake racing the park is never lost.
struct ThreadWaker {
    notified: AtomicBool,
    thread: Thread,
}

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // Set the flag BEFORE unparking: the worker's park loop swaps the flag and only sleeps when
        // it reads `false`, so a wake that lands between the worker's flag-check and its `park()` is
        // observed (either by the swap, or by `park()` returning on the unpark token).
        self.notified.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

/// Drive `rx` to completion on the current thread, invoking `on_item` for each received entry.
///
/// Returns when the channel closes (`poll_recv` yields `Ready(None)` — i.e. every sender has been
/// dropped), so a `Drop`-driven shutdown that closes the lane channel makes the worker exit.
pub(super) fn run<T, R, F>(mut rx: R, mut on_item: F)
where
    R: Receiver<Entry<T>>,
    F: FnMut(Entry<T>),
{
    let state = Arc::new(ThreadWaker {
        notified: AtomicBool::new(false),
        thread: std::thread::current(),
    });
    let waker = state.clone().into();
    let mut cx = Context::from_waker(&waker);
    // One op per poll: pop it, run its (blocking) syscall, repeat. The budget only needs to admit a
    // single entry per cycle — the cooldown below, not the budget, governs idling.
    let mut budget = Budget::new(1);
    let mut idle: u32 = 0;

    loop {
        budget.reset();
        match rx.poll_recv(&mut cx, &mut budget) {
            Poll::Ready(Some(entry)) => {
                on_item(entry);
                idle = 0;
            }
            // All senders dropped — the lane is shutting down.
            Poll::Ready(None) => return,
            Poll::Pending => {
                // `poll_recv` registered our (thread-park) waker. Spin-with-yield a while before
                // committing to a park, so a brief lull doesn't cost a futex round-trip.
                idle += 1;
                if idle < SPIN_BEFORE_PARK {
                    std::thread::yield_now();
                } else {
                    // Park until woken. The flag guards against both a wake that raced us here and a
                    // spurious unpark: only sleep while the flag reads false.
                    while !state.notified.swap(false, Ordering::Acquire) {
                        std::thread::park();
                    }
                    idle = 0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        intrusive::Entry,
        socket::channel::{intrusive::sync as sync_chan, UnboundedSender as _},
    };
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };

    /// A worker driven by `run` processes everything sent, then exits when the sender drops — and the
    /// park/wake path delivers work that arrives *after* the worker has gone idle and parked.
    #[test]
    fn run_processes_then_exits_on_close_including_post_park() {
        let (mut tx, rx) = sync_chan::new::<u64>();
        let sum = Arc::new(AtomicU64::new(0));
        let worker_sum = sum.clone();
        let handle = std::thread::spawn(move || {
            run(rx, |entry: Entry<u64>| {
                worker_sum.fetch_add(*entry, Ordering::Relaxed);
            });
        });

        // First burst.
        for i in 1..=10u64 {
            tx.send(Entry::new(i)).unwrap();
        }
        // Give the worker time to drain and park (well past the spin window).
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Work that arrives after the worker has parked must still be delivered (wake path).
        for i in 11..=20u64 {
            tx.send(Entry::new(i)).unwrap();
        }

        // Dropping the sender closes the channel; the worker drains the rest and exits.
        drop(tx);
        handle.join().unwrap();
        assert_eq!(sum.load(Ordering::Relaxed), (1..=20).sum::<u64>());
    }
}
