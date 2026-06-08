// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A priority-aware shared byte-credit pool with a single-distributor reconciler.
//!
//! # Counter model
//!
//! Free credit is tracked with **two credit counters plus one demand counter**:
//!
//! - [`Pool::available`] (`AtomicI64`, init `capacity`) — the fast-path debit counter.
//!   [`Pool::poll_acquire`] does a lone `fetch_sub`. The distributor is the *only* actor that ever
//!   `fetch_add`s it.
//! - [`Pool::returned`] (`AtomicU64`) — the no-snipe staging buffer. [`Pool::release`] adds here; the
//!   fast path never reads it. The distributor swaps it to zero at the top of each pass. Routing
//!   returned credit here (instead of into `available`) is what prevents a brand-new acquirer from
//!   sniping credit a parked waiter has been waiting for.
//! - [`Pool::parked_demand`] (`AtomicU64`) — the sum of `requested` over all currently-parked
//!   waiters. Bumped by `poll_acquire` only on the park branch; decremented by the distributor per
//!   grant / dead-slot. It is *not* a third credit pool: it lets the distributor recover true free
//!   credit from the signed `available` exactly, since `available + parked_demand` cancels every
//!   parked waiter's subtraction, yielding `capacity − in_flight`.
//!
//! ## Invariants (verified by the loom tests)
//!
//! At every quiescent point:
//!
//! ```text
//! available + parked_demand + returned + in_flight == capacity
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
    slot::{DeadSlot, Slot, SlotAdapter},
    tier::Tier,
    waker::TaskWaker,
};
use crate::{
    intrusive::List,
    socket::channel::{Budget, UnboundedSender},
    sync::{lock, Arc, AtomicI64, AtomicU64, Mutex, Ordering},
};
use core::task::{Context, Poll, Waker};
use std::ptr::NonNull;

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
    available: AtomicI64,
    /// No-snipe staging buffer for returned credit. The fast path never reads this.
    returned: AtomicU64,
    /// Sum of `requested` over all parked waiters. Lets the distributor recover true free credit.
    parked_demand: AtomicU64,
    config: Config,
    waker: TaskWaker,
    tiers: [Mutex<Tier>; Priority::LEVELS],
}

impl Pool {
    pub fn new(config: Config) -> Self {
        let config = config.normalized();

        Self {
            available: AtomicI64::new(config.capacity as i64),
            returned: AtomicU64::new(0),
            parked_demand: AtomicU64::new(0),
            config,
            waker: TaskWaker::new(),
            tiers: std::array::from_fn(|_| Mutex::new(Tier::new())),
        }
    }

    /// Acquire `n` bytes, parking the slot if the pool cannot satisfy it immediately.
    ///
    /// The fast path is a single `fetch_sub`. If the previous value was sufficient, the subtraction
    /// holds and the acquire succeeds. Otherwise the subtraction is **left in place** — the parked
    /// slot is the record of that demand — the slot is linked into its priority tier, and
    /// `parked_demand` is bumped under the tier lock. The distributor will deliver a full grant and
    /// wake the slot once enough credit is returned.
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
        let prev = self.available.fetch_sub(n as i64, Ordering::AcqRel);
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
        tier.push(slot);
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
/// Holds the task-local mirror (one list per priority) and an [`Arc`] to the shared [`Pool`]. The
/// eventual integration spawns one of these as a task; the [`Distributor::poll_distribute`] core is
/// exposed directly so tests can drive it synchronously.
///
/// On drop, the mirror lists drop, which closes (writes `GRANT_CLOSED` and wakes) any waiters the
/// distributor was holding — the distributor-owned half of the shutdown path. Slots still in the
/// shared tiers are closed by [`Pool`]'s implicit drop.
pub struct Distributor {
    pool: Arc<Pool>,
    /// One mirror list per priority. Each shares its shared tier's list id (seeded by `detach`).
    local: [List<SlotAdapter>; Priority::LEVELS],
}

impl Distributor {
    pub fn new(pool: Arc<Pool>) -> Self {
        Self {
            pool,
            local: std::array::from_fn(|_| List::new()),
        }
    }

    /// Run bounded distribution passes until quiescent or the budget is exhausted.
    ///
    /// Registers the distributor's waker first (lost-wakeup discipline), then loops passes while
    /// each makes progress and budget remains. When a pass makes no progress, returns
    /// `Poll::Pending` cleanly; when budget runs out with work remaining, sets `needs_wake` so the
    /// outer drain re-polls (yielding to other tasks). Work per poll is bounded by the budget,
    /// regardless of backlog size or `release`-during-pass churn.
    ///
    /// `wakers` receives the [`Waker`] of every granted slot and `dead` receives every abandoned
    /// slot; both are drained by the caller **after** this returns — neither is touched under a tier
    /// lock (sending may itself wake or free, which must not run under the lock).
    pub fn poll_distribute<W, D>(
        &mut self,
        cx: &mut Context<'_>,
        budget: &mut Budget,
        wakers: &mut W,
        dead: &mut D,
    ) -> Poll<()>
    where
        W: UnboundedSender<Waker>,
        D: UnboundedSender<DeadSlot>,
    {
        self.pool.waker.register(cx.waker());

        loop {
            let progressed = self.pass(budget, wakers, dead);
            if budget.is_exhausted() {
                budget.set_needs_wake();
                return Poll::Pending;
            }
            if !progressed {
                return Poll::Pending;
            }
        }
    }

    /// A single distribution pass. Returns whether it made progress (granted or reaped anything, or
    /// pulled fresh credit). Stops early if the budget is exhausted.
    fn pass<W, D>(&mut self, budget: &mut Budget, wakers: &mut W, dead: &mut D) -> bool
    where
        W: UnboundedSender<Waker>,
        D: UnboundedSender<DeadSlot>,
    {
        // Recover true free credit `= capacity − in_flight = available + parked_demand`, plus the
        // newly-returned `pull`. `available` is held negative by every parked waiter's subtraction;
        // `parked_demand` cancels exactly that.
        //
        // Load order matters: `poll_acquire`'s park does `available -= n` and THEN
        // `parked_demand += n` (two non-atomic steps). We read `parked_demand` FIRST, then
        // `available`, so a park landing between our two loads can only be observed as the `-n` in
        // `available` without the matching `+n` in `parked_demand` — i.e. `free` is *under*-counted
        // (conservative, that waiter is simply served next pass). The reverse order could
        // *over*-count by `n` and grant a waiter we can't afford (over-commit past capacity).
        let pull = self.pool.returned.swap(0, Ordering::AcqRel);
        let parked = self.pool.parked_demand.load(Ordering::Acquire) as i64;
        let mut free = self.pool.available.load(Ordering::Acquire) + parked + pull as i64;

        let mut dead_released: u64 = 0;
        let mut granted_any = false;

        'walk: for (local, tier) in self.local.iter_mut().zip(&self.pool.tiers) {
            // Refill an empty mirror from the shared tier (the only place we lock a tier). Holding
            // the guard is what authorizes the move, so `detach` is a safe operation.
            if local.is_empty() {
                let mut tier = lock(tier);
                if tier.is_empty() {
                    continue;
                }
                *local = tier.detach();
            }

            // Grant heads while affordable. Reap dead heads unconditionally (a dead corpse at the
            // head must not block live waiters behind it). Stop the whole walk at the first live
            // head we cannot afford — strict priority means lower tiers wait too.
            loop {
                let Some(front) = local.front() else {
                    break;
                };
                // SAFETY: the slot is in our mirror, so it is linked (not idle); `requested` was set
                // at park time under the tier lock and is stable until we grant or reap it here.
                let req = unsafe { front.requested() };

                if front.is_dead() {
                    let ptr = local.pop_front().unwrap().take();
                    self.pool.parked_demand.fetch_sub(req, Ordering::AcqRel);
                    dead_released += req;
                    free += req as i64;
                    // SAFETY: a dead slot (refcount=0) has been popped from every list and is owned
                    // by us; wrapping it in `DeadSlot` transfers that ownership to the dead queue,
                    // which frees the allocation on drop.
                    let _ = dead.send(unsafe { DeadSlot::new(ptr) });
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
                    match slot.grant(req) {
                        Some(waker) => {
                            // Full grant: the waiter's park-time subtraction stays in `available`
                            // (reclassified parked → in-flight); we only drop its demand.
                            self.pool.parked_demand.fetch_sub(req, Ordering::AcqRel);
                            free -= req as i64;
                            granted_any = true;
                            let _ = wakers.send(waker);
                        }
                        None => {
                            // Raced: the app abandoned between our `is_dead` check and the CAS.
                            self.pool.parked_demand.fetch_sub(req, Ordering::AcqRel);
                            dead_released += req;
                            free += req as i64;
                            // SAFETY: `grant` returning None means the slot is now refcount=0 and
                            // owned by us; hand it to the dead queue to free.
                            let _ = dead.send(DeadSlot::new(ptr));
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

#[cfg(all(feature = "loom", test))]
mod loom_tests {
    use super::*;
    use crate::{
        credit::slot::{AbandonResult, DeadSlotQueue, GrantResult, Slot},
        testing::loom,
    };
    use core::task::{Context, Poll, Waker};
    use std::{
        alloc::{self, Layout},
        ptr::NonNull,
        task::Wake,
    };

    #[repr(C)]
    struct TestAlloc {
        slot: Slot,
    }

    unsafe fn drop_test_alloc(ptr: NonNull<Slot>) {
        let ptr = ptr.cast::<TestAlloc>();
        std::ptr::drop_in_place(ptr.as_ptr());
        alloc::dealloc(ptr.as_ptr().cast(), Layout::new::<TestAlloc>());
    }

    fn alloc_slot() -> NonNull<Slot> {
        let layout = Layout::new::<TestAlloc>();
        let ptr = unsafe { alloc::alloc(layout) as *mut TestAlloc };
        unsafe {
            std::ptr::write(
                ptr,
                TestAlloc {
                    slot: Slot::new(drop_test_alloc),
                },
            );
            NonNull::new_unchecked(ptr as *mut Slot)
        }
    }

    unsafe fn free_slot(ptr: NonNull<Slot>) {
        let ptr = ptr.cast::<TestAlloc>();
        std::ptr::drop_in_place(ptr.as_ptr());
        alloc::dealloc(ptr.as_ptr().cast(), Layout::new::<TestAlloc>());
    }

    struct Noop;
    impl Wake for Noop {
        fn wake(self: std::sync::Arc<Self>) {}
        fn wake_by_ref(self: &std::sync::Arc<Self>) {}
    }

    fn noop() -> Waker {
        Waker::from(std::sync::Arc::new(Noop))
    }

    /// Counts grants delivered by the distributor (and forwards the granted slot's waker).
    struct CountWake(Arc<crate::sync::AtomicUsize>);
    impl UnboundedSender<Waker> for CountWake {
        fn send(&mut self, waker: Waker) -> Result<(), Waker> {
            self.0.fetch_add(1, Ordering::Release);
            waker.wake();
            Ok(())
        }
    }

    /// A `Send` carrier for a raw slot pointer crossing a loom thread boundary.
    struct SendPtr(NonNull<Slot>);
    unsafe impl Send for SendPtr {}

    /// Drive the distributor to completion on its own thread, parking via `block_on` whenever
    /// `poll_distribute` yields `Pending`. The thread is re-polled ONLY by a real wakeup, so a lost
    /// wakeup manifests as a permanent park → loom deadlock. Completes once `granted` reaches
    /// `target`.
    fn spawn_distributor(
        pool: Arc<Pool>,
        granted: Arc<crate::sync::AtomicUsize>,
        target: usize,
    ) -> loom::thread::JoinHandle<()> {
        loom::thread::spawn(move || {
            let mut dist = Distributor::new(pool);
            let mut dead = DeadSlotQueue::new();
            let mut wakers = CountWake(granted.clone());
            loom::future::block_on(core::future::poll_fn(move |cx| {
                let mut budget = Budget::new(1 << 16);
                let _ = dist.poll_distribute(cx, &mut budget, &mut wakers, &mut dead);
                if granted.load(Ordering::Acquire) >= target {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }));
        })
    }

    /// A parking `poll_acquire` races a distributor pass that is holding distributable credit. This
    /// targets the two-load free-credit recovery in `pass`: the park does `available -= n` then
    /// `parked_demand += n`, and the distributor reads `parked_demand` then `available`. If the loads
    /// were in the wrong order the distributor could observe the `+n` without the `-n` and over-count
    /// free credit, granting a slot it cannot afford and driving the conserved total past capacity.
    /// We assert conservation (`available + parked_demand + returned + in_flight == capacity`) holds
    /// after every interleaving.
    #[test]
    fn loom_park_races_distributor_no_overcommit() {
        loom::model(|| {
            // Total credit that will ever exist in this pool (capacity 0 + one release of CAP).
            const CAP: i64 = 10;
            let pool = Arc::new(Pool::new(Config {
                capacity: 0,
                max_single_acquire: 100,
            }));

            // Park a waiter wanting CAP → available = -CAP, parked_demand = CAP.
            let w0 = alloc_slot();
            let w0w = noop();
            let mut w0cx = Context::from_waker(&w0w);
            let r = unsafe { pool.poll_acquire(&mut w0cx, w0, CAP as u64, Priority::Medium) };
            assert!(matches!(r, Poll::Pending));

            // A second waiter parks concurrently with a distributor pass + the release of CAP. The
            // pass must never over-grant. The distributor here runs a SINGLE bounded pass (no
            // target-blocking) — we are checking the conservation arithmetic of one pass racing a
            // park, not wakeup liveness (covered by other models), so it must not park/deadlock.
            let dist = {
                let pool = pool.clone();
                loom::thread::spawn(move || {
                    let mut dist = Distributor::new(pool);
                    let mut dead = DeadSlotQueue::new();
                    let granted = Arc::new(crate::sync::AtomicUsize::new(0));
                    let mut wakers = CountWake(granted);
                    let mut budget = Budget::new(1 << 16);
                    let dwaker = noop();
                    let mut dcx = Context::from_waker(&dwaker);
                    let _ = dist.poll_distribute(&mut dcx, &mut budget, &mut wakers, &mut dead);
                })
            };

            let releaser = {
                let pool = pool.clone();
                loom::thread::spawn(move || pool.release(CAP as u64))
            };
            let parker = {
                let pool = pool.clone();
                loom::thread::spawn(move || {
                    let w = alloc_slot();
                    let ww = noop();
                    let mut wcx = Context::from_waker(&ww);
                    // May succeed (if capacity frees up as w0 is served) or park — either is correct.
                    // What must NOT happen is an over-count letting more total credit out than CAP.
                    let granted_now = match unsafe {
                        pool.poll_acquire(&mut wcx, w, CAP as u64, Priority::Low)
                    } {
                        Poll::Ready(n) => {
                            // took it via the fast path; release it straight back so teardown is clean
                            pool.release(n);
                            true
                        }
                        Poll::Pending => {
                            let _ = unsafe { (*w.as_ptr()).abandon() };
                            false
                        }
                    };
                    (SendPtr(w), granted_now)
                })
            };

            releaser.join().unwrap();
            let (w_ptr, granted_now) = parker.join().unwrap();
            dist.join().unwrap();

            // Conservation: whatever the interleaving, the conserved total must never imply more than
            // CAP bytes in flight. The wrong load order over-counts free credit and breaks this.
            let available = pool.available.load(Ordering::Relaxed);
            let parked = pool.parked_demand.load(Ordering::Relaxed) as i64;
            let returned = pool.returned.load(Ordering::Relaxed) as i64;
            let in_flight = CAP - (available + parked + returned);
            assert!(
                (0..=CAP).contains(&in_flight),
                "over-commit: in_flight={in_flight} not in 0..={CAP} \
                 (available={available} parked={parked} returned={returned})"
            );

            drop(pool);
            // The parker's slot is idle (fast-path granted) or abandoned-then-freed-on-pool-drop.
            if granted_now {
                unsafe { free_slot(w_ptr.0) };
            }
            unsafe { free_slot(w0) };
        });
    }

    /// A single release must wake a parked distributor. If the wakeup were lost the distributor would
    /// park forever and loom would report a deadlock; conservation must also hold at the end.
    #[test]
    fn loom_release_wakes_parked_waiter() {
        loom::model(|| {
            let pool = Arc::new(Pool::new(Config {
                capacity: 0,
                max_single_acquire: 100,
            }));

            let slot = alloc_slot();
            let waker = noop();
            let mut cx = Context::from_waker(&waker);
            let r = unsafe { pool.poll_acquire(&mut cx, slot, 10, Priority::Medium) };
            assert!(matches!(r, Poll::Pending));
            assert_eq!(pool.available.load(Ordering::Relaxed), -10);

            let granted = Arc::new(crate::sync::AtomicUsize::new(0));
            let dist = spawn_distributor(pool.clone(), granted.clone(), 1);

            // Releaser races the distributor's first poll/park.
            pool.release(10);

            dist.join().unwrap();

            let slot_ref = unsafe { &*slot.as_ptr() };
            assert_eq!(slot_ref.poll_granted(), GrantResult::Granted(10));
            // Conservation: the 10 is now in-flight; nothing stranded.
            assert_eq!(pool.available.load(Ordering::Relaxed), 0);
            assert_eq!(pool.parked_demand.load(Ordering::Relaxed), 0);
            assert_eq!(pool.returned.load(Ordering::Relaxed), 0);

            unsafe { free_slot(slot) };
        });
    }

    /// Two concurrent releasers each return part of what the waiter needs. Whatever order they
    /// interleave with the distributor, their credit must accumulate (one pass writes the leftover
    /// back to `available`, the next pass picks it up) and the waiter must be served exactly once.
    #[test]
    fn loom_concurrent_releases_accumulate() {
        loom::model(|| {
            let pool = Arc::new(Pool::new(Config {
                capacity: 0,
                max_single_acquire: 100,
            }));

            let slot = alloc_slot();
            let waker = noop();
            let mut cx = Context::from_waker(&waker);
            let r = unsafe { pool.poll_acquire(&mut cx, slot, 10, Priority::Medium) };
            assert!(matches!(r, Poll::Pending));

            let granted = Arc::new(crate::sync::AtomicUsize::new(0));
            let dist = spawn_distributor(pool.clone(), granted.clone(), 1);

            let r1 = {
                let pool = pool.clone();
                loom::thread::spawn(move || pool.release(5))
            };
            let r2 = {
                let pool = pool.clone();
                loom::thread::spawn(move || pool.release(5))
            };

            r1.join().unwrap();
            r2.join().unwrap();
            dist.join().unwrap();

            let slot_ref = unsafe { &*slot.as_ptr() };
            assert_eq!(slot_ref.poll_granted(), GrantResult::Granted(10));
            assert_eq!(pool.available.load(Ordering::Relaxed), 0);
            assert_eq!(pool.parked_demand.load(Ordering::Relaxed), 0);

            unsafe { free_slot(slot) };
        });
    }

    /// No-snipe: while a waiter is parked, a newcomer's fast-path `poll_acquire` racing a `release`
    /// must never succeed — released credit goes to `returned` (invisible to the fast path) and the
    /// fast path debits an `available` already driven negative by the parked waiter's demand. We
    /// assert directly on the newcomer's poll result across every interleaving; no distributor is
    /// involved (the property is purely about `release` vs the fast path), so there is no wakeup
    /// dependency to deadlock on.
    #[test]
    fn loom_newcomer_cannot_snipe() {
        loom::model(|| {
            let pool = Arc::new(Pool::new(Config {
                capacity: 0,
                max_single_acquire: 100,
            }));

            // Pre-park a waiter requesting 10 → available = -10.
            let w = alloc_slot();
            let wwaker = noop();
            let mut wcx = Context::from_waker(&wwaker);
            let r = unsafe { pool.poll_acquire(&mut wcx, w, 10, Priority::Highest) };
            assert!(matches!(r, Poll::Pending));

            // A release of 10 races the newcomer's fast-path acquire. Because the release only ever
            // touches `returned`, `available` stays <= 0 until the distributor (not present here)
            // reconciles, so the newcomer can never see positive credit to take.
            let releaser = {
                let pool = pool.clone();
                loom::thread::spawn(move || pool.release(10))
            };
            let newcomer = {
                let pool = pool.clone();
                loom::thread::spawn(move || {
                    let n = alloc_slot();
                    let nwaker = noop();
                    let mut ncx = Context::from_waker(&nwaker);
                    let r = unsafe { pool.poll_acquire(&mut ncx, n, 5, Priority::Medium) };
                    assert!(matches!(r, Poll::Pending), "newcomer sniped a parked waiter");
                    // The newcomer parked; mark it dead so the tier-list drop frees it (no
                    // distributor runs in this model).
                    let dead = matches!(unsafe { (*n.as_ptr()).abandon() }, AbandonResult::Abandoned);
                    assert!(dead);
                    SendPtr(n)
                })
            };

            releaser.join().unwrap();
            let n_ptr = newcomer.join().unwrap().0;

            // Drain the tiers so both parked slots are released exactly once (pool drop would also do
            // this, but we free `w` explicitly below and the newcomer was abandoned → freed on drop).
            let w_ref = unsafe { &*w.as_ptr() };
            // The waiter was never granted (no distributor ran); it is still linked.
            assert!(w_ref.is_linked());

            // Free the still-linked waiter directly (it never transitioned out of the tier). The
            // abandoned newcomer is freed when the pool's tiers drop.
            drop(pool);
            let _ = n_ptr;
            unsafe { free_slot(w) };
        });
    }
}
