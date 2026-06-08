// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A priority-aware shared byte-credit pool with a single-distributor reconciler.
//!
//! # Counter model
//!
//! Free credit is tracked with **two credit counters plus one monotonic demand counter**:
//!
//! - [`Pool::available`] (`AtomicI64`, init `capacity`) — the fast-path debit counter.
//!   [`Pool::poll_acquire`] does a lone `fetch_sub`. The distributor is the *only* actor that ever
//!   `fetch_add`s it.
//! - [`Pool::returned`] (`AtomicU64`) — the no-snipe staging buffer. [`Pool::release`] adds here; the
//!   fast path never reads it. The distributor swaps it to zero at the top of each pass. Routing
//!   returned credit here (instead of into `available`) is what prevents a brand-new acquirer from
//!   sniping credit a parked waiter has been waiting for.
//! - [`Pool::parked_demand`] (`AtomicU64`) — **monotonic** running total of every park's `requested`.
//!   `poll_acquire` adds on the park branch; nobody ever subtracts. The distributor maintains a
//!   private `paid_demand` accumulator (one `u64` field) and recovers outstanding demand as
//!   `parked_demand − paid_demand`. Making the shared atomic add-only halves its contention (one
//!   atomic op per park instead of one-per-park plus one-per-grant) and removes the producer/
//!   distributor RMW race on the same line.
//!
//! ## Invariants (verified by the loom tests)
//!
//! At every quiescent point, with `outstanding = parked_demand − paid_demand`:
//!
//! ```text
//! available + outstanding + returned + in_flight == capacity
//! ```
//!
//! and operationally `available <= 0` holds **whenever any waiter is parked** — so the fast path
//! (which needs `prev >= n > 0` to succeed) cannot acquire while waiters exist. That is the no-snipe
//! guarantee. `available` rises above zero only once the parked queue fully drains.
//!
//! # Distribution
//!
//! A single [`Distributor`] owns all distribution. It keeps a task-local mirror — one
//! [`List`](crate::intrusive::List) per priority — and refills an empty mirror by detaching the
//! shared tier under its lock (one O(1) splice). Unserved waiters stay in the mirror across passes
//! (never re-linked), so under a sustained backlog the tier mutex is taken only on refill. Grants
//! are **full** (`requested`), strict priority, FIFO within a tier, and the walk stops at the first
//! affordable-but-unaffordable live head (head-of-line blocking by design).

use super::{
    config::Config,
    slot::{DeadSlot, DeadSlotQueue, Slot, SlotAdapter, SlotPtr},
    waker::TaskWaker,
};
use crate::{
    intrusive::{Entry, List, Queue},
    socket::channel::{Budget, UnboundedSender},
    sync::{lock, Arc, AtomicI64, AtomicU64, Mutex, Ordering},
};
use core::task::{Context, Poll, Waker};
use crossbeam_utils::CachePadded;
use std::ptr::NonNull;

#[cfg(all(test, not(feature = "loom")))]
mod tests;

#[cfg(all(test, feature = "loom"))]
mod loom;

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
    /// Fast-path debit counter. `poll_acquire` subtracts; the distributor is the sole adder.
    /// Padded to keep producer fast-path RMWs off the same line as `returned`/`parked_demand`.
    available: CachePadded<AtomicI64>,
    /// No-snipe staging buffer for returned credit. The fast path never reads this.
    /// Padded — releasers and the distributor's `swap` would otherwise share a line with
    /// `available`'s hot fast-path traffic.
    returned: CachePadded<AtomicU64>,
    /// Monotonic running total of `requested` over every park. Producers `fetch_add`; the
    /// distributor only loads. Outstanding demand is recovered as `parked_demand − paid_demand`
    /// (a Distributor-private `u64`), so this atomic carries no decrements.
    parked_demand: CachePadded<AtomicU64>,
    config: Config,
    waker: TaskWaker,
    /// One wait list per priority. Held under a mutex; the distributor briefly locks each tier
    /// only to refill an empty mirror via `List::detach`. Each tier is cache-padded so that
    /// producers parking on different priorities don't share a line — without padding, a producer
    /// on tier 3 invalidates the distributor's cached read of tier 4 on every walk.
    tiers: [CachePadded<Mutex<List<SlotAdapter>>>; Priority::LEVELS],
}

impl Pool {
    pub fn new(config: Config) -> Self {
        let config = config.normalized();

        Self {
            available: CachePadded::new(AtomicI64::new(config.capacity as i64)),
            returned: CachePadded::new(AtomicU64::new(0)),
            parked_demand: CachePadded::new(AtomicU64::new(0)),
            config,
            waker: TaskWaker::new(),
            tiers: std::array::from_fn(|_| CachePadded::new(Mutex::new(List::new()))),
        }
    }

    /// Acquire `n` bytes, parking the slot if the pool cannot satisfy it immediately.
    ///
    /// The fast path is a single `fetch_sub`. If the previous value was sufficient, the subtraction
    /// holds and the acquire succeeds. Otherwise the subtraction is **left in place** — the parked
    /// slot is the record of that demand — the slot is linked into its priority tier, and
    /// `parked_demand` is bumped (monotonic, never decremented). The distributor will deliver a
    /// full grant and wake the slot once enough credit is returned.
    ///
    /// # Safety
    ///
    /// The provided `slot` pointer must be valid and the slot must be idle (refcount=1). It must
    /// remain valid until either the grant is delivered (refcount transitions back to 1) or the slot
    /// is abandoned.
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

        // Fast path: a single debit. Success if the pool had the credit.
        //
        // Acquire pairs with the distributor's writeback at end-of-pass so a successful debit
        // observes returned credit promptly. The park branch publishes via
        // `parked_demand.fetch_add(Release)` below, so no Release is needed here.
        let prev = self.available.fetch_sub(n as i64, Ordering::Acquire);
        if prev >= n as i64 {
            return Poll::Ready(n);
        }

        // Slow path: the subtraction stays in `available` (it now represents this waiter's unmet
        // demand). No refund and no distributor wake — a park adds demand, not credit.
        //
        // Ordering: the `fetch_sub` above (the `-n` to `available`) is sequenced before the
        // `fetch_add` below (the `+n` to `parked_demand`). The distributor relies on this order,
        // reading `parked_demand` before `available`, so it can never observe the `+n` without the
        // `-n` and thus never over-counts free credit. See `Distributor::pass`.
        let slot_ref = &*slot.as_ptr();
        slot_ref.prepare_park(n, cx.waker());

        let mut tier = lock(&self.tiers[priority as usize]);
        self.parked_demand.fetch_add(n, Ordering::Release);
        tier.push_back(SlotPtr::new(slot));
        slot_ref.transition_to_linked();

        Poll::Pending
    }

    /// Return `n` bytes of credit to the pool and wake the distributor.
    ///
    /// Returned credit is staged in `returned`, where the fast path cannot see it, so a concurrent
    /// acquirer cannot snipe ahead of a parked waiter. The distributor pulls `returned` at the top
    /// of its next pass.
    pub fn release(&self, n: u64) {
        if n == 0 {
            return;
        }
        self.returned.fetch_add(n, Ordering::Release);
        self.waker.wake();
    }

    // Used by the deterministic suite (compiled out under the loom feature, hence allow(dead_code)).
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn debug_available(&self) -> i64 {
        self.available.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn debug_parked_demand(&self) -> u64 {
        self.parked_demand.load(Ordering::Relaxed)
    }
}

/// The single owner of all credit distribution.
///
/// Holds the task-local mirror (one list per priority) and an [`Arc`] to the shared [`Pool`]. Run
/// it as a task with [`Distributor::distribute`]; the future registers the distributor's waker on
/// the pool once and yields cooperatively under a budget.
///
/// On drop, the mirror lists drop, which closes (writes `GRANT_CLOSED` and wakes) any waiters the
/// distributor was holding — the distributor-owned half of the shutdown path. Slots still in the
/// shared tiers are closed by [`Pool`]'s implicit drop.
pub struct Distributor {
    pool: Arc<Pool>,
    /// One mirror list per priority. Each shares its shared tier's list id (seeded by `detach`).
    local: [List<SlotAdapter>; Priority::LEVELS],
    /// Cumulative `requested` for every park we have already granted or reaped. Outstanding demand
    /// is `pool.parked_demand − paid_demand`. Lives here so the shared atomic stays add-only —
    /// producers never contend with the distributor on the same line for a decrement.
    paid_demand: u64,
}

impl Distributor {
    pub fn new(pool: Arc<Pool>) -> Self {
        Self {
            pool,
            local: std::array::from_fn(|_| List::new()),
            paid_demand: 0,
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn debug_paid_demand(&self) -> u64 {
        self.paid_demand
    }

    /// Drive the distributor as an async task.
    ///
    /// `budget` bounds the work per poll; the future yields when the budget is exhausted and
    /// re-arms via the standard `take_needs_wake` self-wake. `wakers_tx` receives one
    /// [`Queue<Waker>`] batch per poll cycle holding every slot the distributor woke during that
    /// cycle — empty cycles do not send. The future never resolves; drop the task to shut down.
    pub async fn distribute<W>(mut self, mut budget: Budget, mut wakers_tx: W)
    where
        W: UnboundedSender<Queue<Waker>>,
    {
        // Register exactly once on the first poll. Subsequent polls keep the registered waker —
        // the steady-state loop below never touches it.
        core::future::poll_fn(|cx| {
            self.pool.waker.register(cx.waker());
            Poll::Ready(())
        })
        .await;

        core::future::poll_fn(move |cx| {
            budget.reset();
            let _ = self.poll_distribute(&mut budget, &mut wakers_tx);
            // poll_distribute always returns Pending (it loops until quiescent); honor the
            // standard budget-exhausted self-wake.
            if budget.take_needs_wake() {
                cx.waker().wake_by_ref();
            }
            Poll::Pending
        })
        .await
    }

    /// Run bounded distribution passes until quiescent or the budget is exhausted, then flush any
    /// accumulated waker batch.
    ///
    /// Loops passes while each makes progress and budget remains. When a pass makes no progress,
    /// returns `Poll::Pending`; when budget runs out with work remaining, sets `needs_wake` so the
    /// outer drain re-polls. Work per poll is bounded by the budget, regardless of backlog size or
    /// `release`-during-pass churn. The caller is responsible for [`register_waker`](Self::register_waker)
    /// (lost-wakeup discipline) before the first poll.
    ///
    /// On every `Pending` exit, the accumulated waker batch is shipped to `wakers_tx` as a single
    /// send (no send when empty). Dead slots are freed inline inside `pass`, outside any tier lock.
    pub(crate) fn poll_distribute<W>(&mut self, budget: &mut Budget, wakers_tx: &mut W) -> Poll<()>
    where
        W: UnboundedSender<Queue<Waker>>,
    {
        // Wakers and dead slots accumulate during the pass and are flushed at the end of the
        // poll cycle: linking each one is two pointer writes (cheap), but the actual work — a
        // channel send for the wakers, and `drop_fn` per dead slot for the dead queue — would
        // otherwise stall the inner loop on every grant/reap. Both queues live on the poll stack,
        // so storage is reclaimed between polls.
        let mut pending_wakers = Queue::<Waker>::new();
        let mut dead = DeadSlotQueue::new();
        let result = loop {
            let progressed = self.pass(budget, &mut pending_wakers, &mut dead);
            if budget.is_exhausted() {
                budget.set_needs_wake();
                break Poll::Pending;
            }
            if !progressed {
                break Poll::Pending;
            }
        };

        if !pending_wakers.is_empty() {
            // UnboundedSender::send returning Err means the channel is closed; the returned batch
            // drops here, which is fine — a closed waker channel implies shutdown and the slots
            // will see Closed via the pool's drop path.
            let _ = wakers_tx.send(pending_wakers);
        }
        // `dead` drops here at the end of the poll cycle — its drop walks the list and runs each
        // slot's `drop_fn`, freeing the outer allocations outside the work loop.
        drop(dead);

        result
    }

    /// A single distribution pass. Returns whether it made progress (granted or reaped anything, or
    /// pulled fresh credit). Stops early if the budget is exhausted. Granted slots' wakers are
    /// pushed into `pending_wakers`; dead slots are pushed into `dead` and freed when that queue
    /// drops at the end of the poll cycle (their drop_fn must run outside any tier lock).
    fn pass(
        &mut self,
        budget: &mut Budget,
        pending_wakers: &mut Queue<Waker>,
        dead: &mut DeadSlotQueue,
    ) -> bool {
        // Recover true free credit `= capacity − in_flight = available + outstanding_demand`,
        // plus the newly-returned `pull`. `available` is held negative by every parked waiter's
        // subtraction; `outstanding_demand` cancels exactly that.
        //
        // `parked_demand` is monotonic (producers `fetch_add`, nobody subtracts). The distributor
        // tracks every grant/reap in its private `paid_demand`, so true outstanding demand is
        // `parked_demand − paid_demand`. Equivalent to the old `parked_demand` semantics; the only
        // change is that the subtraction lives in this thread's local instead of in a shared RMW.
        //
        // Load order matters: `poll_acquire`'s park does `available -= n` and THEN
        // `parked_demand += n` (two non-atomic steps). We read `parked_demand` FIRST, then
        // `available`, so a park landing between our two loads can only be observed as the `-n` in
        // `available` without the matching `+n` in `parked_demand` — i.e. `free` is *under*-counted
        // (conservative, that waiter is simply served next pass). The reverse order could
        // *over*-count by `n` and grant a waiter we can't afford (over-commit past capacity).
        // Acquire pairs with `release()`'s Release-add. The stored `0` has no acquiring reader.
        let pull = self.pool.returned.swap(0, Ordering::Acquire);
        let outstanding =
            (self.pool.parked_demand.load(Ordering::Acquire) - self.paid_demand) as i64;
        let mut free = self.pool.available.load(Ordering::Acquire) + outstanding + pull as i64;

        let mut dead_released: u64 = 0;
        let mut granted_any = false;

        'walk: for (local, tier) in self.local.iter_mut().zip(&self.pool.tiers) {
            // Refill from the shared tier when the mirror drains, capped at one extra refill per
            // tier per pass. The cap bounds tier-lock acquisitions: at most two locks per tier per
            // pass even if waiters keep arriving. Without the second refill, cached mirror entries
            // would shadow fresh arrivals in the shared tier — granting only the cached ones and
            // moving on, leaving the new ones for the next pass.
            let mut refilled = false;
            loop {
                if local.is_empty() {
                    if refilled {
                        break;
                    }
                    refilled = true;
                    let mut shared = lock(tier);
                    if shared.is_empty() {
                        break;
                    }
                    core::mem::swap(local, &mut *shared);
                }

                // Grant heads while affordable. Reap dead heads unconditionally (a dead corpse at
                // the head must not block live waiters behind it). Stop the whole walk at the first
                // live head we cannot afford — strict priority means lower tiers wait too.
                //
                // SAFETY: the slot is in our mirror, so it is linked (not idle); `requested` was set
                // at park time under the tier lock and is stable until we grant or reap it here.
                let front = local.front().unwrap();
                let req = unsafe { front.requested() };

                if front.is_dead() {
                    let ptr = local.pop_front().unwrap().take();
                    self.paid_demand += req;
                    dead_released += req;
                    free += req as i64;
                    // SAFETY: a dead slot (refcount=0) has been popped from every list and is owned
                    // by us; wrapping it in `DeadSlot` transfers ownership to the dead queue, which
                    // runs the drop_fn at end-of-poll outside the work loop.
                    dead.push_back(unsafe { DeadSlot::new(ptr) });
                    continue;
                }

                if req as i64 > free {
                    break 'walk;
                }

                if !budget.consume() {
                    break 'walk;
                }

                let ptr = local.pop_front().unwrap().take();
                // SAFETY: `ptr` was just popped from our mirror, so it is linked and exclusively
                // ours for the duration of this grant; we hold no tier lock (grant must not run
                // under one) but the slot cannot be concurrently freed — only the app's `abandon`
                // (refcount CAS) and our `grant` race, and `grant` resolves that race.
                unsafe {
                    let slot = &*ptr.as_ptr();
                    let res = slot.grant(req);

                    // Whether granted or raced, this park is now resolved — credit it locally so
                    // the next pass's `outstanding` reflects reality.
                    self.paid_demand += req;

                    match res {
                        Some(waker) => {
                            // Full grant: the waiter's park-time subtraction stays in `available`
                            // (reclassified parked → in-flight); we only drop its demand. Stash
                            // the waker locally; the batch is shipped at the end of the poll cycle.
                            free -= req as i64;
                            granted_any = true;
                            pending_wakers.push_back(Entry::new(waker));
                        }
                        None => {
                            // Raced: the app abandoned between our `is_dead` check and the CAS.
                            // `grant` returning None means refcount=0 and the slot is ours to free.
                            // Push into the dead queue; its drop at end-of-poll frees the slot.
                            dead_released += req;
                            free += req as i64;
                            dead.push_back(DeadSlot::new(ptr));
                        }
                    }
                }
            }
        }

        // Single end-of-pass write: return pulled credit plus any reclaimed dead demand. Grants do
        // not write `available`. If any live waiter remains, its `requested` exceeded `free`, which
        // forces `available <= 0` here — preserving no-snipe.
        let writeback = pull + dead_released;
        if writeback > 0 {
            self.pool
                .available
                .fetch_add(writeback as i64, Ordering::Release);
        }

        granted_any || dead_released > 0 || pull > 0
    }
}
