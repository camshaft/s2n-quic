// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{
    config::Config,
    slot::{DeadSlot, Slot},
    tier::Tier,
};
use crate::socket::channel::UnboundedSender;
use parking_lot::Mutex;
use std::{
    ptr::NonNull,
    sync::atomic::{AtomicI64, AtomicU64, Ordering},
    task::{Context, Poll, Waker},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Priority {
    Highest = 0,
    High = 1,
    MediumHigh = 2,
    Medium = 3,
    MediumLow = 4,
    Low = 5,
    Lowest = 6,
    Background = 7,
}

impl Priority {
    pub const LEVELS: usize = 8;
}

pub struct Pool {
    available: AtomicI64,
    /// Credits that couldn't be granted in the last release (sub-min_grant or
    /// stuck behind a waiter that demanded more). Pulled into the budget at the
    /// start of each release. Never visible to the fast path.
    carry: AtomicU64,
    config: Config,
    tiers: [Mutex<Tier>; Priority::LEVELS],
}

impl Pool {
    pub fn new(config: Config) -> Self {
        let config = config.normalized();

        Self {
            available: AtomicI64::new(config.capacity as i64),
            carry: AtomicU64::new(0),
            tiers: std::array::from_fn(|_| Mutex::new(Tier::new())),
            config,
        }
    }

    /// Try to acquire `n` bytes via the fast path.
    ///
    /// Returns the granted amount, or 0 if the pool is exhausted (caller should park).
    #[inline]
    pub fn try_acquire(&self, n: u64) -> u64 {
        let n = self.config.clamp_request(n);
        if n == 0 {
            return 0;
        }

        if Self::try_subtract(&self.available, n) {
            return n;
        }

        0
    }

    /// Acquire credits, parking if necessary.
    ///
    /// Tries the fast path first. If exhausted, links the slot into the
    /// priority-ordered wait list and returns `Poll::Pending`.
    ///
    /// When the pool later grants credits, it writes directly into the slot's
    /// `granted` field and wakes the task. On subsequent poll, the caller reads
    /// `slot.poll_granted()`.
    ///
    /// # Safety
    ///
    /// The provided `slot` pointer must be valid and the slot must be in the idle
    /// state (refcount=1). The slot must remain valid until either the grant is
    /// delivered (refcount transitions back to 1) or the slot is abandoned.
    pub unsafe fn poll_acquire(
        &self,
        cx: &mut Context<'_>,
        slot: NonNull<Slot>,
        n: u64,
        priority: Priority,
    ) -> Poll<u64> {
        let n = self.config.clamp_request(n);
        if n == 0 {
            return Poll::Ready(0);
        }

        // Fast path: single fetch_sub
        if Self::try_subtract(&self.available, n) {
            return Poll::Ready(n);
        }

        // Slow path: park in the wait list
        let slot_ref = &*slot.as_ptr();

        slot_ref.prepare_park(n, cx.waker());

        let mut tier = self.tiers[priority as usize].lock();

        // Re-check after acquiring the lock (credits may have been released)
        if Self::try_subtract(&self.available, n) {
            slot_ref.cancel_park();
            return Poll::Ready(n);
        }

        tier.push(slot);
        slot_ref.transition_to_linked();

        Poll::Pending
    }

    /// Return credits to the pool and distribute to waiting streams.
    ///
    /// Walks tiers high→low priority. At each tier, accumulates that tier's
    /// `carry` into the budget and grants as many waiters as `min_grant` allows.
    /// If a tier still has unserved waiters, the remaining budget is stashed in
    /// that tier's `carry` and we return — lower-priority tiers wait for the
    /// next release cycle. If we walk all tiers without finding waiters, leftover
    /// is deposited into `available` for the fast path.
    ///
    /// Critical property: the fast path only sees `available`. `available`
    /// receives credits only after we've confirmed there are no waiters
    /// anywhere. A fast-path acquirer cannot snipe credits while waiters exist.
    ///
    /// Wakers and dead slots are deferred via `UnboundedSender`s for the caller
    /// to drain after this returns — neither is processed under any tier lock.
    pub fn release<W, D>(&self, n: u64, wakers: &mut W, dead: &mut D)
    where
        W: UnboundedSender<Waker>,
        D: UnboundedSender<DeadSlot>,
    {
        if n == 0 {
            return;
        }

        // Pull any previously-stashed carry into our budget.
        let mut budget = n.saturating_add(self.carry.swap(0, Ordering::AcqRel));

        for tier_mutex in &self.tiers {
            let mut tier = tier_mutex.lock();

            if tier.is_empty() {
                continue;
            }

            // Grant as many waiters as we can afford at min_grant each. The
            // remainder (budget % grantable_count) stays in budget and either
            // serves the next tier or carries over for the next release.
            let grantable_count = (budget / self.config.min_grant).min(tier.len() as u64);

            if grantable_count > 0 {
                let share = budget / grantable_count;

                for _ in 0..grantable_count as usize {
                    let Some(slot_handle) = tier.pop_front() else {
                        break;
                    };

                    let slot_ptr = slot_handle.take();

                    unsafe {
                        let slot = &*slot_ptr.as_ptr();
                        // Cap the grant at what the slot actually requested —
                        // a slot wanting 10 bytes shouldn't get a 64KB share.
                        let grant_amount = share.min(slot.requested());
                        if let Some(waker) = slot.grant(grant_amount) {
                            let _ = wakers.send(waker);
                            budget -= grant_amount;
                        } else {
                            // Slot is dead — its share stays in budget.
                            let _ = dead.send(DeadSlot::new(slot_ptr));
                        }
                    }
                }
            }

            // If this tier still has waiters, the remaining budget belongs to
            // them. Stash globally so the next release picks it up. We do NOT
            // continue to lower-priority tiers while higher-priority waiters
            // are unserved.
            if !tier.is_empty() {
                drop(tier);
                self.carry.fetch_add(budget, Ordering::Release);
                return;
            }

            // Tier emptied. Continue to lower-priority tiers with leftover.
        }

        // Walked all tiers, no waiters remain anywhere. Safe to deposit into
        // `available` for the fast path.
        if budget > 0 {
            self.available.fetch_add(budget as i64, Ordering::Release);
        }
    }

    /// Try to subtract `n` from the counter via a single `fetch_sub`.
    ///
    /// If the previous value was sufficient, the subtraction holds and we return true.
    /// Otherwise we went into debt — refund and return false.
    #[inline]
    fn try_subtract(counter: &AtomicI64, n: u64) -> bool {
        let n = n as i64;
        let prev = counter.fetch_sub(n, Ordering::AcqRel);
        if prev >= n {
            true
        } else {
            counter.fetch_add(n, Ordering::Release);
            false
        }
    }

    #[cfg(test)]
    pub(crate) fn debug_available(&self) -> i64 {
        self.available.load(Ordering::Relaxed)
    }
}
