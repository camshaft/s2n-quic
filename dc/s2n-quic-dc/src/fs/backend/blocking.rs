// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A runtime for **blocking** futures — and the [`block_on`] driver behind it.
//!
//! # Why this is not the [`Spawner`](crate::runtime::Spawner) trait
//!
//! The scheduler has two kinds of work with fundamentally different shapes, and conflating them
//! into one trait would force a lie about bounds in one direction or the other:
//!
//! * **Cooperative `!Send` futures** — the per-device credit distributors, the waker-drain, the
//!   submission dispatch task. They time-share a single scheduler worker, interleaving at every
//!   `await`, and are `!Send` (they touch `Rc`-based channels). This is exactly the
//!   [`Spawner`](crate::runtime::Spawner) trait, which already has busy-poll / tokio / bach impls.
//! * **Blocking execution** — a real `pread`/`pwrite`, or io_uring's `submit_and_wait`. A blocking
//!   future *owns its thread* for its whole lifetime; it cannot time-share a worker. Its bound is
//!   `Send + 'static`, one thread per spawn — the opposite of `Spawner`'s `!Send` worker-affinity
//!   contract.
//!
//! So blocking work gets its own seam: a **concrete** [`Runtime`] (not a trait — the only thing
//! that varies, real-IO vs. simulated, is already expressed by *which* [`Backend`] you pick; the
//! bach backend spawns cooperative timer tasks and never holds a blocking runtime). It spawns
//! *futures* ([`Runtime::spawn`], the directive's futures-first path) by driving them to completion
//! with [`block_on`] on a fresh dedicated thread, and offers [`Runtime::spawn_blocking`] for work
//! that genuinely cannot be a cooperative future (the io_uring ring loop blocks in the kernel via
//! `submit_and_wait`, not on a waker, so it can never return `Pending` to a thread-park executor).
//!
//! # `block_on` and the thread-park waker
//!
//! [`block_on`] drives an `impl Future<Output = ()>` to completion on the current thread with a
//! lightweight [`Waker`] that parks/unparks *this* thread. A storage lane future is the ordinary
//! `Map(rx, execute+complete).drain_budgeted()` combinator: when its channel is momentarily empty
//! the combinator returns `Pending` having registered our thread-park waker, and a producer's
//! ordinary `waker.wake()` unparks the thread — no special-casing at the send site. When the lane
//! channel closes (every sender dropped at teardown) the combinator resolves `Ready(())` and
//! `block_on` returns, so the thread exits.
//!
//! ## Cooldown
//!
//! Parking on every empty poll would add a futex round-trip per op even under steady load. Instead
//! the driver **spins** (re-polling, yielding between attempts) up to [`SPIN_BEFORE_PARK`] times
//! when a poll returns `Pending`, and only then parks until woken. Under load it never parks (the
//! next poll finds work); when genuinely idle it parks and consumes no CPU. The park is
//! lost-wakeup-safe: the waker sets an `AtomicBool` *before* unparking, and the driver re-checks
//! that flag in its park loop, so a wake that races the park is never slept through (and a spurious
//! unpark just re-polls).

use core::{
    future::Future,
    pin::pin,
    task::{Context, Poll},
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::Wake,
    thread::{JoinHandle, Thread},
};

/// Number of `Pending` polls (each followed by a `yield_now`) the driver makes before it parks.
/// Sized to ride out a brief gap in the work stream without a futex round-trip, while still parking
/// promptly when truly idle so an idle thread burns no CPU.
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
        // Set the flag BEFORE unparking: the driver's park loop swaps the flag and only sleeps when
        // it reads `false`, so a wake that lands between the driver's flag-check and its `park()` is
        // observed (either by the swap, or by `park()` returning on the unpark token).
        self.notified.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

/// Drive `future` to completion on the current thread, parking the thread between polls (with a
/// spin cooldown) using a [`Waker`] that unparks it. Returns when the future resolves.
///
/// This is the blocking-thread analog of an async runtime's `block_on`: it makes no assumption
/// about *what* the future does — a storage lane future runs a blocking `pread`/`pwrite` inline in
/// its poll (the thread is dedicated, so blocking it is fine), then yields `Pending` to await more
/// work; a control future might just await a channel. Either way the thread-park waker is the wake
/// path, shared with the rest of the pipeline.
pub fn block_on<F>(future: F)
where
    F: Future<Output = ()>,
{
    let state = Arc::new(ThreadWaker {
        notified: AtomicBool::new(false),
        thread: std::thread::current(),
    });
    let waker = state.clone().into();
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    let mut idle: u32 = 0;

    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(()) => return,
            Poll::Pending => {
                // The future registered our (thread-park) waker. Spin-with-yield a while before
                // committing to a park, so a brief lull doesn't cost a futex round-trip.
                idle += 1;
                if idle < SPIN_BEFORE_PARK {
                    std::thread::yield_now();
                } else {
                    // Park until woken. The flag guards against both a wake that raced us here and
                    // a spurious unpark: only sleep while the flag reads false.
                    while !state.notified.swap(false, Ordering::Acquire) {
                        std::thread::park();
                    }
                    idle = 0;
                }
            }
        }
    }
}

/// A runtime for blocking work: every spawn moves the work onto a fresh, dedicated OS thread.
///
/// Concrete by design (see the module docs) — the backend holds one and spawns its lane threads
/// through it rather than calling [`std::thread::spawn`] directly. Cheap to construct and clone; it
/// owns no threads itself. Each spawn hands back a [`JoinOnDrop`] guard whose lifetime the caller
/// ties to the work's natural owner (for a storage lane, the lane submit handle), so the thread is
/// joined when that owner drops — *not* when the runtime drops. This keeps teardown ordering local
/// to the lanes and avoids a runtime-outlives-lanes join deadlock.
#[derive(Clone)]
pub struct Runtime {
    /// Thread-name prefix for diagnostics (e.g. `s2n-dc-fs-io`). The spawn site appends an index.
    name_prefix: Arc<str>,
}

impl Runtime {
    /// A runtime whose threads are named `{prefix}-{n}`.
    pub fn new(name_prefix: impl Into<Arc<str>>) -> Self {
        Self {
            name_prefix: name_prefix.into(),
        }
    }

    /// Spawn `future` onto a fresh dedicated thread, driving it with [`block_on`]. The directive's
    /// futures-first path: a lane is an ordinary combinator future, and the runtime owns the thread
    /// it blocks on. Returns a guard that joins the thread on drop (the future ends when its input
    /// channel closes, so the join is prompt once the lane senders are gone).
    pub fn spawn<F>(&self, idx: usize, future: F) -> JoinOnDrop
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.spawn_blocking(idx, move || block_on(future))
    }

    /// Spawn a plain blocking closure onto a fresh dedicated thread. The escape hatch for work that
    /// cannot be a cooperative future — the io_uring ring loop blocks in the kernel via
    /// `submit_and_wait` (woken by a real CQE or a self-armed eventfd), so it can never yield
    /// `Pending` to [`block_on`]'s thread-park executor and must drive its own loop. Returns a
    /// join-on-drop guard, same as [`spawn`](Self::spawn).
    pub fn spawn_blocking<C>(&self, idx: usize, work: C) -> JoinOnDrop
    where
        C: FnOnce() + Send + 'static,
    {
        let handle = std::thread::Builder::new()
            .name(format!("{}-{idx}", self.name_prefix))
            .spawn(work)
            .expect("failed to spawn blocking runtime thread");
        JoinOnDrop(Some(handle))
    }
}

/// A guard owning one blocking-runtime thread; joins it on drop. The thread's driving future/closure
/// must terminate when its input channel closes, so dropping every sender first makes the join
/// prompt. Bundle these into an `Arc<[JoinOnDrop]>` shared by a backend's lane handles so the
/// threads are joined exactly once, when the last lane handle drops.
pub struct JoinOnDrop(Option<JoinHandle<()>>);

impl Drop for JoinOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        intrusive::Entry,
        socket::channel::{
            intrusive::sync as sync_chan, Map, ReceiverExt as _, UnboundedSender as _,
        },
    };
    use std::sync::atomic::AtomicU64;

    /// A lane future driven by `block_on` processes everything sent, then exits when the sender
    /// drops — and the park/wake path delivers work that arrives *after* the driver has gone idle
    /// and parked. This is the exact shape a storage lane uses: `Map(rx, on_item).drain_budgeted()`.
    #[test]
    fn block_on_drives_lane_future_including_post_park() {
        let (mut tx, rx) = sync_chan::new::<u64>();
        let sum = Arc::new(AtomicU64::new(0));
        let worker_sum = sum.clone();
        let rt = Runtime::new("test-block-on");
        let guard = rt.spawn(
            0,
            Map::new(rx, move |entry: Entry<u64>| {
                worker_sum.fetch_add(*entry, Ordering::Relaxed);
            })
            .drain_budgeted(Some(64)),
        );

        // First burst.
        for i in 1..=10u64 {
            tx.send(Entry::new(i)).unwrap();
        }
        // Give the driver time to drain and park (well past the spin window).
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Work that arrives after the driver has parked must still be delivered (wake path).
        for i in 11..=20u64 {
            tx.send(Entry::new(i)).unwrap();
        }

        // Dropping the sender closes the channel; the driver drains the rest, the future resolves,
        // `block_on` returns, the thread exits, and the guard joins it.
        drop(tx);
        drop(guard);
        assert_eq!(sum.load(Ordering::Relaxed), (1..=20).sum::<u64>());
    }

    /// `spawn_blocking` runs a plain closure on a dedicated thread and the guard joins it on drop.
    #[test]
    fn spawn_blocking_runs_closure_and_joins() {
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        let rt = Runtime::new("test-spawn-blocking");
        let guard = rt.spawn_blocking(0, move || {
            flag.store(true, Ordering::Release);
        });
        drop(guard); // joins the thread
        assert!(ran.load(Ordering::Acquire));
    }
}
