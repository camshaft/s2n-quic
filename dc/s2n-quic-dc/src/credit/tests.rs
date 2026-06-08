// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{
    pool::{Distributor, Priority},
    slot::{AbandonResult, DeadSlotQueue, GrantResult, Slot},
    Config, Pool,
};
use crate::socket::channel::{Budget, UnboundedSender};
use std::{
    alloc::{self, Layout},
    ptr::NonNull,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll, Wake, Waker},
};

// ── Test helpers ─────────────────────────────────────────────────────────────

/// A minimal `#[repr(C)]` allocation with Slot as the prefix.
#[repr(C)]
struct TestAlloc {
    slot: Slot,
    value: u64,
}

static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe fn drop_test_alloc(ptr: NonNull<Slot>) {
    DROP_COUNT.fetch_add(1, Ordering::Relaxed);
    let ptr = ptr.cast::<TestAlloc>();
    std::ptr::drop_in_place(ptr.as_ptr());
    alloc::dealloc(ptr.as_ptr().cast(), Layout::new::<TestAlloc>());
}

fn alloc_test_slot() -> NonNull<Slot> {
    let layout = Layout::new::<TestAlloc>();
    let ptr = unsafe { alloc::alloc(layout) as *mut TestAlloc };
    assert!(!ptr.is_null());
    unsafe {
        std::ptr::write(
            ptr,
            TestAlloc {
                slot: Slot::new(drop_test_alloc),
                value: 42,
            },
        );
        NonNull::new_unchecked(ptr as *mut Slot)
    }
}

/// Free a test slot that is in the idle state (rc=1).
unsafe fn free_test_slot(ptr: NonNull<Slot>) {
    let ptr = ptr.cast::<TestAlloc>();
    std::ptr::drop_in_place(ptr.as_ptr());
    alloc::dealloc(ptr.as_ptr().cast(), Layout::new::<TestAlloc>());
}

#[derive(Default)]
struct WakeCounter {
    wakeups: AtomicUsize,
}

impl WakeCounter {
    fn wakeups(&self) -> usize {
        self.wakeups.load(Ordering::Relaxed)
    }
}

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.wakeups.fetch_add(1, Ordering::Relaxed);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakeups.fetch_add(1, Ordering::Relaxed);
    }
}

/// Waker sender that immediately wakes (mirrors the grant path delivering a waker).
struct InlineWakeSender;

impl UnboundedSender<Waker> for InlineWakeSender {
    fn send(&mut self, waker: Waker) -> Result<(), Waker> {
        waker.wake();
        Ok(())
    }
}

/// A no-op waker used to register the distributor (its identity is stable across polls).
struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
    fn wake_by_ref(self: &Arc<Self>) {}
}

/// Test harness: a shared pool plus its single distributor.
struct Harness {
    pool: Arc<Pool>,
    dist: Distributor,
    dist_waker: Waker,
}

impl Harness {
    fn new(config: Config) -> Self {
        let pool = Arc::new(Pool::new(config));
        let dist = Distributor::new(pool.clone());
        Self {
            pool,
            dist,
            dist_waker: Waker::from(Arc::new(NoopWake)),
        }
    }

    /// Acquire `n` at `priority`, parking with the given waker if necessary.
    ///
    /// # Safety
    /// `slot` must be a valid idle slot that outlives any resulting park.
    unsafe fn poll_acquire(
        &self,
        slot: NonNull<Slot>,
        n: u64,
        priority: Priority,
        waker: &Waker,
    ) -> Poll<u64> {
        let mut cx = Context::from_waker(waker);
        self.pool.poll_acquire(&mut cx, slot, n, priority)
    }

    fn release(&self, n: u64) {
        self.pool.release(n);
    }

    /// Run the distributor to quiescence, returning the dead-slot queue it produced (dropping the
    /// queue frees those allocations).
    fn distribute(&mut self) -> DeadSlotQueue {
        let mut cx = Context::from_waker(&self.dist_waker);
        let mut budget = Budget::new(1 << 20);
        let mut wakers = InlineWakeSender;
        let mut dead = DeadSlotQueue::new();
        let _ = self
            .dist
            .poll_distribute(&mut cx, &mut budget, &mut wakers, &mut dead);
        dead
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn fast_path_acquire() {
    let pool = Pool::new(Config {
        capacity: 100,
        max_single_acquire: 100,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    let slot = alloc_test_slot();

    let result = unsafe { pool.poll_acquire(&mut cx, slot, 10, Priority::Medium) };
    assert_eq!(result, Poll::Ready(10));
    assert_eq!(pool.debug_available(), 90);

    unsafe { free_test_slot(slot) };
}

#[test]
fn fast_path_exhaustion_parks() {
    // With no fast-path success the acquirer parks (the old try_acquire-returns-0 path is gone).
    let mut h = Harness::new(Config {
        capacity: 10,
        max_single_acquire: 100,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let slot = alloc_test_slot();

    let result = unsafe { h.poll_acquire(slot, 20, Priority::Medium, &waker) };
    assert!(matches!(result, Poll::Pending));
    // The subtraction stays in place: 10 - 20 = -10, recorded as parked_demand.
    assert_eq!(h.pool.debug_available(), -10);
    assert_eq!(h.pool.debug_parked_demand(), 20);

    // Returning the missing 10 lets the distributor grant the full 20.
    h.release(10);
    let dead = h.distribute();
    assert!(dead.is_empty());
    assert_eq!(counter.wakeups(), 1);

    let slot_ref = unsafe { &*slot.as_ptr() };
    assert_eq!(slot_ref.poll_granted(), GrantResult::Granted(20));

    unsafe { free_test_slot(slot) };
}

#[test]
fn park_and_grant() {
    let mut h = Harness::new(Config {
        capacity: 0,
        max_single_acquire: 100,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let slot = alloc_test_slot();

    let result = unsafe { h.poll_acquire(slot, 10, Priority::Medium, &waker) };
    assert!(matches!(result, Poll::Pending));

    h.release(10);
    let dead = h.distribute();
    assert_eq!(counter.wakeups(), 1);
    assert!(dead.is_empty());

    let slot_ref = unsafe { &*slot.as_ptr() };
    assert_eq!(slot_ref.poll_granted(), GrantResult::Granted(10));

    unsafe { free_test_slot(slot) };
}

#[test]
fn full_grants_to_multiple_waiters() {
    // Three waiters each requesting 20; releasing 60 grants all three their full request.
    let mut h = Harness::new(Config {
        capacity: 0,
        max_single_acquire: 1000,
    });

    let counters: Vec<_> = (0..3).map(|_| Arc::new(WakeCounter::default())).collect();
    let wakers: Vec<_> = counters.iter().map(|c| Waker::from(c.clone())).collect();
    let slots: Vec<_> = (0..3).map(|_| alloc_test_slot()).collect();

    for i in 0..3 {
        let result = unsafe { h.poll_acquire(slots[i], 20, Priority::Medium, &wakers[i]) };
        assert!(matches!(result, Poll::Pending));
    }

    h.release(60);
    let _dead = h.distribute();

    for c in &counters {
        assert_eq!(c.wakeups(), 1);
    }
    for slot in &slots {
        let slot_ref = unsafe { &*slot.as_ptr() };
        assert_eq!(slot_ref.poll_granted(), GrantResult::Granted(20));
    }

    for slot in slots {
        unsafe { free_test_slot(slot) };
    }
}

#[test]
fn partial_budget_serves_priority_prefix() {
    // 50 released, three waiters each wanting 20: the first two (FIFO) get full grants, the third
    // is unaffordable (20 > remaining 10) and stays parked. No partial grant.
    let mut h = Harness::new(Config {
        capacity: 0,
        max_single_acquire: 1000,
    });

    let counters: Vec<_> = (0..3).map(|_| Arc::new(WakeCounter::default())).collect();
    let wakers: Vec<_> = counters.iter().map(|c| Waker::from(c.clone())).collect();
    let slots: Vec<_> = (0..3).map(|_| alloc_test_slot()).collect();

    for i in 0..3 {
        let result = unsafe { h.poll_acquire(slots[i], 20, Priority::Medium, &wakers[i]) };
        assert!(matches!(result, Poll::Pending));
    }

    h.release(50);
    let _dead = h.distribute();

    assert_eq!(counters[0].wakeups(), 1);
    assert_eq!(counters[1].wakeups(), 1);
    assert_eq!(counters[2].wakeups(), 0);

    let s2 = unsafe { &*slots[2].as_ptr() };
    assert_eq!(s2.poll_granted(), GrantResult::Pending);

    // 10 leftover is still owed to the parked head, so available must stay <= 0 (no-snipe).
    assert!(h.pool.debug_available() <= 0);

    // Releasing the remaining 10 serves the third on the next pass.
    h.release(10);
    let _dead = h.distribute();
    assert_eq!(counters[2].wakeups(), 1);

    for slot in slots {
        unsafe { free_test_slot(slot) };
    }
}

#[test]
fn grant_is_exactly_requested_surplus_to_fast_path() {
    // A waiter that requested 10 gets exactly 10; the surplus lands in `available` for the fast path.
    let mut h = Harness::new(Config {
        capacity: 0,
        max_single_acquire: 1000,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let slot = alloc_test_slot();

    let result = unsafe { h.poll_acquire(slot, 10, Priority::Medium, &waker) };
    assert!(matches!(result, Poll::Pending));

    h.release(1000);
    let _dead = h.distribute();

    let slot_ref = unsafe { &*slot.as_ptr() };
    assert_eq!(slot_ref.poll_granted(), GrantResult::Granted(10));

    // 990 surplus is now free for the fast path.
    assert_eq!(h.pool.debug_available(), 990);
    let counter2 = Arc::new(WakeCounter::default());
    let waker2 = Waker::from(counter2);
    let slot2 = alloc_test_slot();
    let r = unsafe { h.poll_acquire(slot2, 990, Priority::Medium, &waker2) };
    assert_eq!(r, Poll::Ready(990));

    unsafe {
        free_test_slot(slot);
        free_test_slot(slot2);
    }
}

#[test]
fn spurious_wake_then_grant() {
    let mut h = Harness::new(Config {
        capacity: 0,
        max_single_acquire: 100,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let slot = alloc_test_slot();

    let result = unsafe { h.poll_acquire(slot, 50, Priority::Medium, &waker) };
    assert!(matches!(result, Poll::Pending));

    let slot_ref = unsafe { &*slot.as_ptr() };
    assert_eq!(slot_ref.poll_granted(), GrantResult::Pending);

    h.release(100);
    let _dead = h.distribute();
    // Requested 50 → granted exactly 50; the other 50 is surplus.
    assert_eq!(slot_ref.poll_granted(), GrantResult::Granted(50));
    assert_eq!(h.pool.debug_available(), 50);

    unsafe { free_test_slot(slot) };
}

#[test]
fn priority_ordering() {
    let mut h = Harness::new(Config {
        capacity: 0,
        max_single_acquire: 100,
    });

    let low_counter = Arc::new(WakeCounter::default());
    let high_counter = Arc::new(WakeCounter::default());
    let low_waker = Waker::from(low_counter.clone());
    let high_waker = Waker::from(high_counter.clone());

    let low_slot = alloc_test_slot();
    let high_slot = alloc_test_slot();

    // Park low first, then high — high should still be served first.
    let result = unsafe { h.poll_acquire(low_slot, 10, Priority::Low, &low_waker) };
    assert!(matches!(result, Poll::Pending));
    let result = unsafe { h.poll_acquire(high_slot, 10, Priority::Highest, &high_waker) };
    assert!(matches!(result, Poll::Pending));

    // Only enough for one grant: strict priority gives it to the high tier.
    h.release(10);
    let _dead = h.distribute();
    assert_eq!(high_counter.wakeups(), 1);
    assert_eq!(low_counter.wakeups(), 0);

    h.release(10);
    let _dead = h.distribute();
    assert_eq!(low_counter.wakeups(), 1);

    unsafe {
        free_test_slot(high_slot);
        free_test_slot(low_slot);
    }
}

#[test]
fn drop_while_linked() {
    DROP_COUNT.store(0, Ordering::Relaxed);

    let mut h = Harness::new(Config {
        capacity: 0,
        max_single_acquire: 100,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let slot = alloc_test_slot();

    let result = unsafe { h.poll_acquire(slot, 10, Priority::Medium, &waker) };
    assert!(matches!(result, Poll::Pending));

    // App drops while linked.
    let slot_ref = unsafe { &*slot.as_ptr() };
    assert_eq!(unsafe { slot_ref.abandon() }, AbandonResult::Abandoned);

    // The distributor reaps the dead slot; nothing is granted.
    h.release(10);
    let dead = h.distribute();
    assert_eq!(counter.wakeups(), 0);
    assert!(!dead.is_empty());

    // Dropping the dead queue frees the slot.
    drop(dead);
    assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 1);
}

#[test]
fn drop_while_idle() {
    DROP_COUNT.store(0, Ordering::Relaxed);

    let slot = alloc_test_slot();
    let slot_ref = unsafe { &*slot.as_ptr() };
    assert!(slot_ref.is_idle());

    unsafe { free_test_slot(slot) };
    assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 0);
}

#[test]
fn dead_entry_skipped_in_distribution() {
    DROP_COUNT.store(0, Ordering::Relaxed);

    let mut h = Harness::new(Config {
        capacity: 0,
        max_single_acquire: 100,
    });

    let counter1 = Arc::new(WakeCounter::default());
    let counter2 = Arc::new(WakeCounter::default());
    let waker1 = Waker::from(counter1.clone());
    let waker2 = Waker::from(counter2.clone());

    let slot1 = alloc_test_slot();
    let slot2 = alloc_test_slot();

    let result = unsafe { h.poll_acquire(slot1, 10, Priority::Medium, &waker1) };
    assert!(matches!(result, Poll::Pending));
    let result = unsafe { h.poll_acquire(slot2, 10, Priority::Medium, &waker2) };
    assert!(matches!(result, Poll::Pending));

    assert_eq!(unsafe { (*slot1.as_ptr()).abandon() }, AbandonResult::Abandoned);

    h.release(20);
    let dead = h.distribute();
    assert_eq!(counter1.wakeups(), 0);
    assert_eq!(counter2.wakeups(), 1);
    assert!(!dead.is_empty());

    drop(dead);
    assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 1);

    let slot2_ref = unsafe { &*slot2.as_ptr() };
    assert_eq!(slot2_ref.poll_granted(), GrantResult::Granted(10));

    unsafe { free_test_slot(slot2) };
}

#[test]
fn mixed_alive_and_dead_in_distribution() {
    let mut h = Harness::new(Config {
        capacity: 0,
        max_single_acquire: 1000,
    });

    let slots: Vec<_> = (0..5).map(|_| alloc_test_slot()).collect();
    let counters: Vec<_> = (0..5).map(|_| Arc::new(WakeCounter::default())).collect();
    let wakers: Vec<_> = counters.iter().map(|c| Waker::from(c.clone())).collect();

    let requests = [10u64, 20, 30, 40, 50];
    for i in 0..5 {
        let result = unsafe { h.poll_acquire(slots[i], requests[i], Priority::Medium, &wakers[i]) };
        assert!(matches!(result, Poll::Pending));
    }

    assert_eq!(unsafe { (*slots[2].as_ptr()).abandon() }, AbandonResult::Abandoned);

    DROP_COUNT.store(0, Ordering::Relaxed);
    h.release(150);
    let dead = h.distribute();

    drop(dead);
    assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 1);

    for i in [0, 1, 3, 4] {
        assert_eq!(counters[i].wakeups(), 1);
        let slot_ref = unsafe { &*slots[i].as_ptr() };
        assert_eq!(slot_ref.poll_granted(), GrantResult::Granted(requests[i]));
    }

    for i in [0, 1, 3, 4] {
        unsafe { free_test_slot(slots[i]) };
    }
}

#[test]
fn burst_cap_enforced() {
    // A request larger than max_single_acquire is clamped, and the clamped amount is granted from
    // the fast path when capacity allows.
    let pool = Pool::new(Config {
        capacity: 1000,
        max_single_acquire: 16,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter);
    let mut cx = Context::from_waker(&waker);
    let slot = alloc_test_slot();

    let result = unsafe { pool.poll_acquire(&mut cx, slot, 100, Priority::Medium) };
    assert_eq!(result, Poll::Ready(16));

    unsafe { free_test_slot(slot) };
}

#[test]
fn newcomer_cannot_snipe_parked_waiter() {
    // A parked waiter must be served before a fresh acquirer can take returned credit.
    let mut h = Harness::new(Config {
        capacity: 0,
        max_single_acquire: 100,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let slot = alloc_test_slot();

    // W parks requesting 10 → available = -10.
    let result = unsafe { h.poll_acquire(slot, 10, Priority::Medium, &waker) };
    assert!(matches!(result, Poll::Pending));

    // Credit comes back, but it is staged in `returned`, invisible to the fast path.
    h.release(10);

    // A newcomer tries the fast path BEFORE the distributor runs: it cannot see the returned credit
    // (available is still -10), so it parks instead of sniping.
    let nc_counter = Arc::new(WakeCounter::default());
    let nc_waker = Waker::from(nc_counter.clone());
    let nc_slot = alloc_test_slot();
    let result = unsafe { h.poll_acquire(nc_slot, 1, Priority::Medium, &nc_waker) };
    assert!(matches!(result, Poll::Pending), "newcomer must not snipe");

    // The distributor serves the original waiter.
    let _dead = h.distribute();
    assert_eq!(counter.wakeups(), 1);
    let slot_ref = unsafe { &*slot.as_ptr() };
    assert_eq!(slot_ref.poll_granted(), GrantResult::Granted(10));

    // Clean up: serve the newcomer too so we can free it.
    h.release(1);
    let _dead = h.distribute();
    assert_eq!(nc_counter.wakeups(), 1);

    unsafe {
        free_test_slot(slot);
        free_test_slot(nc_slot);
    }
}

#[test]
fn pool_drop_signals_closed() {
    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());

    let slot = alloc_test_slot();

    {
        let h = Harness::new(Config {
            capacity: 0,
            max_single_acquire: 100,
        });

        let result = unsafe { h.poll_acquire(slot, 10, Priority::Medium, &waker) };
        assert!(matches!(result, Poll::Pending));

        // Harness drops here: the distributor (empty mirror) and the pool's tiers drop, closing the
        // still-linked waiter.
    }

    assert_eq!(counter.wakeups(), 1);
    let slot_ref = unsafe { &*slot.as_ptr() };
    assert_eq!(slot_ref.poll_granted(), GrantResult::Closed);

    unsafe { free_test_slot(slot) };
}

#[test]
fn abandon_then_pool_drop_frees_dead_slot() {
    // Covers the SlotPtr::drop DEAD branch: a slot abandoned (rc=DEAD) before pool shutdown must be
    // freed by the close path, not closed/woken.
    DROP_COUNT.store(0, Ordering::Relaxed);

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let slot = alloc_test_slot();

    {
        let h = Harness::new(Config {
            capacity: 0,
            max_single_acquire: 100,
        });

        let result = unsafe { h.poll_acquire(slot, 10, Priority::Medium, &waker) };
        assert!(matches!(result, Poll::Pending));

        // Abandon while linked, then drop the pool without ever distributing.
        assert_eq!(unsafe { (*slot.as_ptr()).abandon() }, AbandonResult::Abandoned);
    }

    // The DEAD slot was freed by the shutdown close path, and never woken.
    assert_eq!(counter.wakeups(), 0);
    assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 1);
}

#[test]
fn split_credit_no_longer_strands() {
    // Regression for the original split-credit deadlock: A acquires 50, B requests 60 and parks,
    // A releases 50. The old two-counter design stranded B (50 in available + 50 in carry, neither
    // alone >= 60). The single distributor reunifies and serves B.
    let mut h = Harness::new(Config {
        capacity: 100,
        max_single_acquire: 100,
    });

    let a_counter = Arc::new(WakeCounter::default());
    let a_waker = Waker::from(a_counter);
    let a_slot = alloc_test_slot();
    let r = unsafe { h.poll_acquire(a_slot, 50, Priority::Medium, &a_waker) };
    assert_eq!(r, Poll::Ready(50));

    let b_counter = Arc::new(WakeCounter::default());
    let b_waker = Waker::from(b_counter.clone());
    let b_slot = alloc_test_slot();
    let r = unsafe { h.poll_acquire(b_slot, 60, Priority::Medium, &b_waker) };
    assert!(matches!(r, Poll::Pending));

    // A's bytes complete and are returned.
    h.release(50);
    let _dead = h.distribute();

    // B is served its full 60.
    assert_eq!(b_counter.wakeups(), 1);
    let b_ref = unsafe { &*b_slot.as_ptr() };
    assert_eq!(b_ref.poll_granted(), GrantResult::Granted(60));
    // 40 surplus is free for the fast path.
    assert_eq!(h.pool.debug_available(), 40);

    unsafe {
        free_test_slot(a_slot);
        free_test_slot(b_slot);
    }
}

#[test]
fn concurrent_release_halves_serve_waiter() {
    // The old design's concurrent-release double-stash strands a waiter when two sub-threshold
    // releases race. With a single counter, two releases simply sum; the distributor serves the
    // waiter once their total covers it.
    let mut h = Harness::new(Config {
        capacity: 0,
        max_single_acquire: 100,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let slot = alloc_test_slot();

    let r = unsafe { h.poll_acquire(slot, 10, Priority::Medium, &waker) };
    assert!(matches!(r, Poll::Pending));

    // Two independent half-releases (sub-threshold individually).
    h.release(5);
    h.release(5);
    let _dead = h.distribute();

    assert_eq!(counter.wakeups(), 1);
    let slot_ref = unsafe { &*slot.as_ptr() };
    assert_eq!(slot_ref.poll_granted(), GrantResult::Granted(10));

    unsafe { free_test_slot(slot) };
}
