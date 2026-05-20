// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{
    config::Config,
    waiter::{WaiterEntry, WaiterQueue, PRIORITY_LEVELS},
};
use std::{
    sync::{
        atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering},
        Mutex,
    },
    task::{Context, Poll},
};

pub struct Pool {
    /// Credits available to inactive streams (and fallback for active).
    /// Negative values signal that waiters are blocked.
    available: AtomicI64,

    /// Reserved credits for active streams only.
    active_reserve: AtomicI64,

    /// Monotonic epoch counter. Streams polled within 1 epoch are "active".
    epoch: AtomicU64,

    /// Priority-ordered wait queue (slow path only).
    waiters: Mutex<WaiterQueue>,

    waiter_total: AtomicUsize,

    /// Immutable configuration.
    config: Config,
}

impl Pool {
    /// Create a new pool with the given config.
    pub fn new(config: Config) -> Self {
        let config = config.normalized();
        let active_reserve = config.active_reserve_target().min(config.capacity as i64);
        let available = (config.capacity as i64).saturating_sub(active_reserve);

        Self {
            available: AtomicI64::new(available),
            active_reserve: AtomicI64::new(active_reserve),
            epoch: AtomicU64::new(0),
            waiters: Mutex::new(WaiterQueue::new()),
            waiter_total: AtomicUsize::new(0),
            config,
        }
    }

    /// Try to acquire `n` bytes. If the stream is active (last_epoch within 1 of current),
    /// tries active_reserve first. Falls back to available. If neither has enough,
    /// parks the waker in the priority-ordered wait queue and returns Pending.
    pub fn poll_acquire(
        &self,
        cx: &mut Context<'_>,
        n: u64,
        last_epoch: u64,
        priority: usize,
    ) -> Poll<u64> {
        let n = self.config.clamp_request(n);
        if n == 0 {
            return Poll::Ready(0);
        }

        if self.is_active(last_epoch) && Self::acquire_from(&self.active_reserve, n) {
            return Poll::Ready(n);
        }

        if Self::acquire_from(&self.available, n) {
            return Poll::Ready(n);
        }

        let mut waiters = self.waiters.lock().expect("waiters mutex poisoned");
        waiters.push(
            priority,
            WaiterEntry {
                waker: cx.waker().clone(),
                requested: n,
            },
        );
        self.waiter_total.fetch_add(1, Ordering::Relaxed);
        Poll::Pending
    }

    /// Non-blocking best-effort acquire. Returns 0 if pool is exhausted.
    /// Used by readers who don't want to park.
    pub fn try_acquire(&self, n: u64, last_epoch: u64) -> u64 {
        let n = self.config.clamp_request(n);
        if n == 0 {
            return 0;
        }

        if self.is_active(last_epoch) && Self::acquire_from(&self.active_reserve, n) {
            return n;
        }

        if Self::acquire_from(&self.available, n) {
            return n;
        }

        0
    }

    /// Return credits to the pool. Wakes blocked waiters in priority order.
    pub fn release(&self, n: u64) {
        if n == 0 {
            return;
        }

        let n = n.min(i64::MAX as u64);
        let prev = self.available.fetch_add(n as i64, Ordering::Release);
        if prev >= 0 && self.waiter_total.load(Ordering::Acquire) == 0 {
            return;
        }

        let mut wake_budget = n;
        let mut wake_count = 0usize;
        let mut wakers = Vec::new();

        {
            let mut waiters = self.waiters.lock().expect("waiters mutex poisoned");
            if waiters.total == 0 {
                return;
            }

            'tiers: for tier_idx in 0..PRIORITY_LEVELS {
                let tier = &mut waiters.tiers[tier_idx];

                while let Some(front) = tier.front() {
                    if wake_budget == 0 || front.requested > wake_budget {
                        break 'tiers;
                    }

                    let waiter = tier.pop_front().expect("front waiter exists");
                    wake_budget -= waiter.requested;
                    wake_count += 1;
                    wakers.push(waiter.waker);
                }
            }

            waiters.total = waiters.total.saturating_sub(wake_count);
        }

        if wake_count > 0 {
            self.waiter_total.fetch_sub(wake_count, Ordering::Relaxed);
        }

        for waker in wakers {
            waker.wake();
        }
    }

    /// Advance the epoch (called periodically by the endpoint worker).
    #[inline]
    pub fn advance_epoch(&self) {
        self.epoch.fetch_add(1, Ordering::Relaxed);
    }

    /// Current epoch value (for streams to snapshot on poll).
    #[inline]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    /// Replenish active_reserve from available up to configured fraction.
    pub fn replenish_active_reserve(&self) {
        let target = self.config.active_reserve_target();
        if target <= 0 {
            return;
        }

        loop {
            let reserve = self.active_reserve.load(Ordering::Acquire);
            if reserve >= target {
                return;
            }

            let deficit = (target - reserve) as u64;
            let available = self.available.load(Ordering::Acquire);
            if available <= 0 {
                return;
            }

            let transfer = deficit.min(available as u64);
            if transfer == 0 {
                return;
            }

            match self.available.compare_exchange_weak(
                available,
                available - transfer as i64,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.active_reserve
                        .fetch_add(transfer as i64, Ordering::AcqRel);
                    return;
                }
                Err(_) => continue,
            }
        }
    }

    #[inline]
    fn is_active(&self, last_epoch: u64) -> bool {
        self.epoch().wrapping_sub(last_epoch) <= 1
    }

    fn acquire_from(counter: &AtomicI64, n: u64) -> bool {
        let n = n as i64;

        loop {
            let current = counter.load(Ordering::Acquire);
            if current < n {
                return false;
            }

            match counter.compare_exchange_weak(
                current,
                current - n,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn debug_available(&self) -> i64 {
        self.available.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn debug_set_available(&self, value: i64) {
        self.available.store(value, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn debug_active_reserve(&self) -> i64 {
        self.active_reserve.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn debug_waiter_total(&self) -> usize {
        self.waiter_total.load(Ordering::Relaxed)
    }
}
