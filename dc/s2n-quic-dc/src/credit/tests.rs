// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{
    pool::Priority,
    slot::{DeadSlotQueue, GrantResult, Slot},
    Config, Pool,
};
use crate::socket::channel::UnboundedSender;
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

/// Waker sender that immediately wakes.
struct InlineWakeSender;

impl UnboundedSender<Waker> for InlineWakeSender {
    fn send(&mut self, waker: Waker) -> Result<(), Waker> {
        waker.wake();
        Ok(())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn fast_path_acquire() {
    let pool = Pool::new(Config {
        capacity: 100,
        min_grant: 1,
        max_single_acquire: 100,
    });

    let acquired = pool.try_acquire(10);
    assert_eq!(acquired, 10);
    assert_eq!(pool.debug_available(), 90);
}

#[test]
fn fast_path_exhaustion_returns_zero() {
    let pool = Pool::new(Config {
        capacity: 10,
        min_grant: 1,
        max_single_acquire: 100,
    });

    let acquired = pool.try_acquire(20);
    assert_eq!(acquired, 0);
}

#[test]
fn park_and_grant() {
    let pool = Pool::new(Config {
        capacity: 0,
        min_grant: 1,
        max_single_acquire: 100,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);

    let slot_ptr = alloc_test_slot();

    let result = unsafe { pool.poll_acquire(&mut cx, slot_ptr, 10, Priority::Medium) };
    assert!(matches!(result, Poll::Pending));

    let mut dead = DeadSlotQueue::new();
    pool.release(10, &mut InlineWakeSender, &mut dead);
    assert_eq!(counter.wakeups(), 1);
    assert!(dead.is_empty());

    let slot = unsafe { &*slot_ptr.as_ptr() };
    assert_eq!(slot.poll_granted(), GrantResult::Granted(10));

    unsafe { free_test_slot(slot_ptr) };
}

#[test]
fn proportional_distribution() {
    let pool = Pool::new(Config {
        capacity: 0,
        min_grant: 10,
        max_single_acquire: 1000,
    });

    let counters: Vec<_> = (0..3).map(|_| Arc::new(WakeCounter::default())).collect();
    let wakers: Vec<_> = counters.iter().map(|c| Waker::from(c.clone())).collect();
    let slots: Vec<_> = (0..3).map(|_| alloc_test_slot()).collect();

    for i in 0..3 {
        let mut cx = Context::from_waker(&wakers[i]);
        let result = unsafe { pool.poll_acquire(&mut cx, slots[i], 100, Priority::Medium) };
        assert!(matches!(result, Poll::Pending));
    }

    let mut dead = DeadSlotQueue::new();
    pool.release(60, &mut InlineWakeSender, &mut dead);

    for c in &counters {
        assert_eq!(c.wakeups(), 1);
    }

    for slot_ptr in &slots {
        let slot = unsafe { &*slot_ptr.as_ptr() };
        assert_eq!(slot.poll_granted(), GrantResult::Granted(20));
    }

    for slot_ptr in slots {
        unsafe { free_test_slot(slot_ptr) };
    }
}

#[test]
fn remainder_carries() {
    // 17 budget, min_grant=5, 3 waiters. grantable=3, share=5. Each gets 5.
    // Remainder of 2 stays in carry for the next release.
    let pool = Pool::new(Config {
        capacity: 0,
        min_grant: 5,
        max_single_acquire: 1000,
    });

    let counters: Vec<_> = (0..3).map(|_| Arc::new(WakeCounter::default())).collect();
    let wakers: Vec<_> = counters.iter().map(|c| Waker::from(c.clone())).collect();
    let slots: Vec<_> = (0..3).map(|_| alloc_test_slot()).collect();

    for i in 0..3 {
        let mut cx = Context::from_waker(&wakers[i]);
        let result = unsafe { pool.poll_acquire(&mut cx, slots[i], 100, Priority::Medium) };
        assert!(matches!(result, Poll::Pending));
    }

    let mut dead = DeadSlotQueue::new();
    pool.release(17, &mut InlineWakeSender, &mut dead);

    for slot_ptr in &slots {
        let slot = unsafe { &*slot_ptr.as_ptr() };
        assert_eq!(slot.poll_granted(), GrantResult::Granted(5));
    }

    for slot_ptr in slots {
        unsafe { free_test_slot(slot_ptr) };
    }
}

#[test]
fn grant_clamped_to_requested() {
    // A slot that only requested 10 should get at most 10, even if `share` is larger.
    let pool = Pool::new(Config {
        capacity: 0,
        min_grant: 1,
        max_single_acquire: 1000,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    let slot_ptr = alloc_test_slot();

    let result = unsafe { pool.poll_acquire(&mut cx, slot_ptr, 10, Priority::Medium) };
    assert!(matches!(result, Poll::Pending));

    // Release way more than needed
    let mut dead = DeadSlotQueue::new();
    pool.release(1000, &mut InlineWakeSender, &mut dead);

    let slot = unsafe { &*slot_ptr.as_ptr() };
    assert_eq!(slot.poll_granted(), GrantResult::Granted(10));

    // The unused 990 should be available for fast-path acquirers
    assert_eq!(pool.try_acquire(990), 990);

    unsafe { free_test_slot(slot_ptr) };
}

#[test]
fn min_grant_threshold() {
    let pool = Pool::new(Config {
        capacity: 0,
        min_grant: 64,
        max_single_acquire: 1000,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    let slot_ptr = alloc_test_slot();

    let result = unsafe { pool.poll_acquire(&mut cx, slot_ptr, 100, Priority::Medium) };
    assert!(matches!(result, Poll::Pending));

    let mut dead = DeadSlotQueue::new();

    // Release 32 bytes — less than min_grant of 64
    pool.release(32, &mut InlineWakeSender, &mut dead);
    assert_eq!(counter.wakeups(), 0);

    let slot = unsafe { &*slot_ptr.as_ptr() };
    assert!(slot.is_linked());

    // Release another 32 — total now 64, crosses threshold
    pool.release(32, &mut InlineWakeSender, &mut dead);
    assert_eq!(counter.wakeups(), 1);
    assert_eq!(slot.poll_granted(), GrantResult::Granted(64));

    unsafe { free_test_slot(slot_ptr) };
}

#[test]
fn drop_while_linked() {
    DROP_COUNT.store(0, Ordering::Relaxed);

    let pool = Pool::new(Config {
        capacity: 0,
        min_grant: 1,
        max_single_acquire: 100,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    let slot_ptr = alloc_test_slot();

    let result = unsafe { pool.poll_acquire(&mut cx, slot_ptr, 10, Priority::Medium) };
    assert!(matches!(result, Poll::Pending));

    // Simulate app dropping while linked
    let slot = unsafe { &*slot_ptr.as_ptr() };
    unsafe { slot.abandon() };

    // Pool releases — encounters dead slot, pushes to dead queue
    let mut dead = DeadSlotQueue::new();
    pool.release(10, &mut InlineWakeSender, &mut dead);
    assert_eq!(counter.wakeups(), 0);
    assert!(!dead.is_empty());

    // Dropping the dead queue frees the slot
    drop(dead);
    assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 1);
}

#[test]
fn drop_while_idle() {
    DROP_COUNT.store(0, Ordering::Relaxed);

    let slot_ptr = alloc_test_slot();
    let slot = unsafe { &*slot_ptr.as_ptr() };
    assert!(slot.is_idle());

    unsafe { free_test_slot(slot_ptr) };
    assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 0);
}

#[test]
fn spurious_wake() {
    let pool = Pool::new(Config {
        capacity: 0,
        min_grant: 100,
        max_single_acquire: 100,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    let slot_ptr = alloc_test_slot();

    let result = unsafe { pool.poll_acquire(&mut cx, slot_ptr, 50, Priority::Medium) };
    assert!(matches!(result, Poll::Pending));

    let slot = unsafe { &*slot_ptr.as_ptr() };
    assert_eq!(slot.poll_granted(), GrantResult::Pending);

    let mut dead = DeadSlotQueue::new();
    pool.release(100, &mut InlineWakeSender, &mut dead);
    // Slot requested 50 — gets exactly 50, not the full 100.
    assert_eq!(slot.poll_granted(), GrantResult::Granted(50));

    unsafe { free_test_slot(slot_ptr) };
}

#[test]
fn priority_ordering() {
    let pool = Pool::new(Config {
        capacity: 0,
        min_grant: 10,
        max_single_acquire: 100,
    });

    let low_counter = Arc::new(WakeCounter::default());
    let high_counter = Arc::new(WakeCounter::default());
    let low_waker = Waker::from(low_counter.clone());
    let high_waker = Waker::from(high_counter.clone());

    let low_slot = alloc_test_slot();
    let high_slot = alloc_test_slot();

    // Park low first, then high — high should still wake first
    let mut low_cx = Context::from_waker(&low_waker);
    let result = unsafe { pool.poll_acquire(&mut low_cx, low_slot, 10, Priority::Low) };
    assert!(matches!(result, Poll::Pending));

    let mut high_cx = Context::from_waker(&high_waker);
    let result = unsafe { pool.poll_acquire(&mut high_cx, high_slot, 10, Priority::Highest) };
    assert!(matches!(result, Poll::Pending));

    let mut dead = DeadSlotQueue::new();

    pool.release(10, &mut InlineWakeSender, &mut dead);
    assert_eq!(high_counter.wakeups(), 1);
    assert_eq!(low_counter.wakeups(), 0);

    pool.release(10, &mut InlineWakeSender, &mut dead);
    assert_eq!(low_counter.wakeups(), 1);

    unsafe {
        free_test_slot(high_slot);
        free_test_slot(low_slot);
    }
}

#[test]
fn dead_entry_skipped_in_distribution() {
    DROP_COUNT.store(0, Ordering::Relaxed);

    let pool = Pool::new(Config {
        capacity: 0,
        min_grant: 1,
        max_single_acquire: 100,
    });

    let counter1 = Arc::new(WakeCounter::default());
    let counter2 = Arc::new(WakeCounter::default());
    let waker1 = Waker::from(counter1.clone());
    let waker2 = Waker::from(counter2.clone());

    let slot1 = alloc_test_slot();
    let slot2 = alloc_test_slot();

    let mut cx1 = Context::from_waker(&waker1);
    let mut cx2 = Context::from_waker(&waker2);

    let result = unsafe { pool.poll_acquire(&mut cx1, slot1, 10, Priority::Medium) };
    assert!(matches!(result, Poll::Pending));
    let result = unsafe { pool.poll_acquire(&mut cx2, slot2, 10, Priority::Medium) };
    assert!(matches!(result, Poll::Pending));

    unsafe { (*slot1.as_ptr()).abandon() };

    let mut dead = DeadSlotQueue::new();
    pool.release(20, &mut InlineWakeSender, &mut dead);
    assert_eq!(counter1.wakeups(), 0);
    assert_eq!(counter2.wakeups(), 1);
    assert!(!dead.is_empty());

    drop(dead);
    assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 1);

    let slot2_ref = unsafe { &*slot2.as_ptr() };
    assert!(matches!(slot2_ref.poll_granted(), GrantResult::Granted(n) if n > 0));

    unsafe { free_test_slot(slot2) };
}

#[test]
fn mixed_alive_and_dead_in_distribution() {
    let pool = Pool::new(Config {
        capacity: 0,
        min_grant: 1,
        max_single_acquire: 1000,
    });

    let slots: Vec<_> = (0..5).map(|_| alloc_test_slot()).collect();
    let counters: Vec<_> = (0..5).map(|_| Arc::new(WakeCounter::default())).collect();
    let wakers: Vec<_> = counters.iter().map(|c| Waker::from(c.clone())).collect();

    let requests = [10u64, 20, 30, 40, 50];
    for i in 0..5 {
        let mut cx = Context::from_waker(&wakers[i]);
        let result = unsafe { pool.poll_acquire(&mut cx, slots[i], requests[i], Priority::Medium) };
        assert!(matches!(result, Poll::Pending));
    }

    unsafe { (*slots[2].as_ptr()).abandon() };

    DROP_COUNT.store(0, Ordering::Relaxed);
    let mut dead = DeadSlotQueue::new();
    pool.release(100, &mut InlineWakeSender, &mut dead);

    drop(dead);
    assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 1);

    for i in [0, 1, 3, 4] {
        assert_eq!(counters[i].wakeups(), 1);
        let slot = unsafe { &*slots[i].as_ptr() };
        assert!(matches!(slot.poll_granted(), GrantResult::Granted(n) if n > 0));
    }

    for i in [0, 1, 3, 4] {
        unsafe { free_test_slot(slots[i]) };
    }
}

#[test]
fn burst_cap_enforced() {
    let pool = Pool::new(Config {
        capacity: 1000,
        min_grant: 1,
        max_single_acquire: 16,
    });

    let acquired = pool.try_acquire(100);
    assert_eq!(acquired, 16);
}

#[test]
fn release_does_not_let_newcomers_steal_from_waiters() {
    // A parked waiter should be served before a concurrent fast-path acquirer
    // can snipe credits returned to the pool.
    let pool = Pool::new(Config {
        capacity: 0,
        min_grant: 1,
        max_single_acquire: 100,
    });

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    let slot_ptr = alloc_test_slot();

    // Park requesting 10
    let result = unsafe { pool.poll_acquire(&mut cx, slot_ptr, 10, Priority::Medium) };
    assert!(matches!(result, Poll::Pending));

    // Release exactly 10. The parked waiter should get all of it.
    // A subsequent try_acquire should see nothing to steal.
    let mut dead = DeadSlotQueue::new();
    pool.release(10, &mut InlineWakeSender, &mut dead);

    // After release, available should be back to 0 (parked sub of -10 + release of 10)
    // The waiter should have received its grant directly; nothing should be free.
    assert_eq!(pool.try_acquire(1), 0, "newcomer should not be able to steal");

    let slot = unsafe { &*slot_ptr.as_ptr() };
    assert_eq!(slot.poll_granted(), GrantResult::Granted(10));

    unsafe { free_test_slot(slot_ptr) };
}

#[test]
fn pool_drop_signals_closed() {
    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);

    let slot_ptr = alloc_test_slot();

    {
        let pool = Pool::new(Config {
            capacity: 0,
            min_grant: 1,
            max_single_acquire: 100,
        });

        let result = unsafe { pool.poll_acquire(&mut cx, slot_ptr, 10, Priority::Medium) };
        assert!(matches!(result, Poll::Pending));

        // Pool drops here — should write GRANT_CLOSED and wake
    }

    assert_eq!(counter.wakeups(), 1);
    let slot = unsafe { &*slot_ptr.as_ptr() };
    assert_eq!(slot.poll_granted(), GrantResult::Closed);

    unsafe { free_test_slot(slot_ptr) };
}
