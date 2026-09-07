// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::time::precision::{self, Clock as _, Timestamp};
use core::{cell::Cell, task::Poll};
use std::{future::poll_fn, sync::OnceLock, time::Instant};

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

thread_local! {
    /// Per-thread busy-poll clock cache: nanos-since-`epoch` captured once per task-list sweep by
    /// the busy-poll `Runner` (see [`refresh`]). `0` = no cache on this thread, so [`Clock::now`]
    /// falls back to a real read. Because busy poll polls every future every sweep with a noop
    /// waker, all the timer `now()`/`poll_ready` reads within a sweep can share ONE `clock_gettime`
    /// instead of issuing one each — perf showed ~18% of busy-poll CPU was in `clock_gettime`.
    static CACHED_NANOS: Cell<u64> = const { Cell::new(0) };
}

/// Capture the current time into this thread's busy-poll clock cache with a single real clock read.
///
/// Called once per task-list sweep by the busy-poll [`Runner`](crate::busy_poll::Runner) when clock
/// caching is enabled. When the `Runner` never calls this (caching disabled, or a non-busy-poll
/// thread), [`Clock::now`] transparently falls back to a real read, so behavior is unchanged.
#[inline]
pub(crate) fn refresh() {
    let nanos = epoch().elapsed().as_nanos() as u64;
    CACHED_NANOS.set(nanos);
}

/// Whether the busy-poll clock cache is active (default on). `DCQUIC_CLOCK_CACHE=0` makes the
/// `Runner` skip [`refresh`], so every `now()` does a real clock read (the legacy path) — for A/B
/// without a rebuild. Read once and cached.
pub(crate) fn clock_cache_enabled() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static CACHED: AtomicU8 = AtomicU8::new(2); // 2 = uninit, 0 = off, 1 = on
    match CACHED.load(Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => {
            let on = std::env::var("DCQUIC_CLOCK_CACHE")
                .map(|v| v != "0")
                .unwrap_or(true);
            CACHED.store(on as u8, Ordering::Relaxed);
            on
        }
    }
}

/// A polling-based clock and timer backed by `std::time::Instant`.
///
/// Unlike tokio/bach timers, busy-poll timers never register wakers — all futures
/// are polled unconditionally every iteration, so the timer just checks whether
/// wall-clock time has passed the target on each poll.
#[derive(Clone, Copy, Debug)]
pub struct Clock(Instant);

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    pub fn new() -> Self {
        Self(epoch())
    }
}

impl precision::Clock for Clock {
    type Timer = Timer;

    fn now(&self) -> Timestamp {
        // Prefer this thread's busy-poll sweep cache; `0` means uncached (caching disabled, or a
        // non-busy-poll thread) so fall back to a real read — identical to the legacy behavior.
        let cached = CACHED_NANOS.with(Cell::get);
        let nanos = if cached != 0 {
            cached
        } else {
            self.0.elapsed().as_nanos() as u64
        };
        Timestamp { nanos }
    }

    fn timer(&self) -> Self::Timer {
        Timer {
            clock: *self,
            target: None,
            armed: false,
        }
    }
}

impl s2n_quic_core::time::Clock for Clock {
    #[inline]
    fn get_time(&self) -> s2n_quic_core::time::Timestamp {
        precision::Clock::now(self).into()
    }
}

#[derive(Clone, Debug)]
pub struct Timer {
    clock: Clock,
    target: Option<Timestamp>,
    armed: bool,
}

impl precision::Clock for Timer {
    type Timer = Self;

    fn now(&self) -> Timestamp {
        self.clock.now()
    }

    fn timer(&self) -> Self::Timer {
        self.clock.timer()
    }
}

impl precision::Timer for Timer {
    fn now(&self) -> Timestamp {
        precision::Clock::now(self)
    }

    async fn sleep_until(&mut self, target: Timestamp) {
        self.update(target);
        poll_fn(|cx| self.poll_ready(cx)).await
    }

    fn poll_ready(&mut self, _cx: &mut core::task::Context) -> Poll<()> {
        if !self.armed {
            return Poll::Ready(());
        }

        if let Some(target) = self.target {
            if self.clock.now() >= target {
                self.cancel();
                Poll::Ready(())
            } else {
                // We don't use the waker in busy poll since all futures are polled all the time
                Poll::Pending
            }
        } else {
            Poll::Ready(())
        }
    }

    fn update(&mut self, target: Timestamp) {
        self.target = Some(target);
        self.armed = true;
    }

    fn cancel(&mut self) {
        self.armed = false;
        self.target = None;
    }

    fn is_armed(&self) -> bool {
        self.armed
    }
}

impl s2n_quic_core::time::Clock for Timer {
    #[inline]
    fn get_time(&self) -> s2n_quic_core::time::Timestamp {
        precision::Clock::now(self).into()
    }
}
