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
    /// Per-thread cached "nanos since [`epoch`]", refreshed once per busy-poll sweep by
    /// [`refresh_cached_now`]. `None` on threads that never call it (Tokio, the metrics reporter,
    /// client warmup), where [`Clock::coarse_now`] falls back to a real clock read.
    static CACHED_NOW: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Refresh this thread's cached timestamp with a single real clock read.
///
/// The busy-poll runner calls this once per poll sweep. A sweep polls every stage future
/// unconditionally, and each armed timer's `now()`/`poll_ready()` reads the clock — without caching
/// that is one `clock_gettime` per timer per sweep (profiled at ~17% of server CPU under load). With
/// caching the whole sweep shares one read; the resulting staleness is at most one sweep (sub-µs,
/// well within the busy-poll timer granularity, which already polls all futures every iteration).
#[inline]
pub fn refresh_cached_now() {
    let nanos = epoch().elapsed().as_nanos() as u64;
    CACHED_NOW.with(|c| c.set(Some(nanos)));
}

/// Clear this thread's cached timestamp so [`Clock::coarse_now`] falls back to a real read until the
/// next [`refresh_cached_now`]. The runner calls this at the end of every sweep, so the cache is only
/// ever live *during* a `tasks.poll()` sweep it was just refreshed for — a parked or spawn-handling
/// worker (which runs between sweeps) can never serve an arbitrarily-old coarse time.
#[inline]
pub fn clear_cached_now() {
    CACHED_NOW.with(|c| c.set(None));
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
        let nanos = self.0.elapsed().as_nanos() as u64;
        Timestamp { nanos }
    }

    fn coarse_now(&self) -> Timestamp {
        // On busy-poll threads, reuse the per-sweep cached timestamp (set by `refresh_cached_now`)
        // instead of a `clock_gettime` per timer poll. Off busy-poll threads the cache is unset, so
        // this is a fresh read. `self.0` is always the shared `epoch()`, so the cached "nanos since
        // epoch" is equivalent to a fresh read modulo the sub-sweep staleness this method opts into.
        let nanos = CACHED_NOW
            .with(|c| c.get())
            .unwrap_or_else(|| self.0.elapsed().as_nanos() as u64);
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

    fn coarse_now(&self) -> Timestamp {
        self.clock.coarse_now()
    }

    fn timer(&self) -> Self::Timer {
        self.clock.timer()
    }
}

impl precision::Timer for Timer {
    fn now(&self) -> Timestamp {
        precision::Clock::now(self)
    }

    fn coarse_now(&self) -> Timestamp {
        precision::Clock::coarse_now(self)
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
            // Expiry check is sweep-granular by design (polled once per sweep), so it opts into the
            // coarse (per-sweep cached) clock — the busy-poll hot path that made clock reads ~17% of
            // CPU. A sub-sweep-stale read only shifts a fire decision by <1µs, within polling jitter.
            if self.clock.coarse_now() >= target {
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
