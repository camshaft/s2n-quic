// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{Config, Handle, Pool};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll, Wake, Waker},
};

#[derive(Default)]
struct WakeCounter {
    wakeups: AtomicUsize,
}

impl WakeCounter {
    #[inline]
    fn wakeups(&self) -> usize {
        self.wakeups.load(Ordering::Relaxed)
    }
}

impl Wake for WakeCounter {
    #[inline]
    fn wake(self: Arc<Self>) {
        self.wakeups.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    fn wake_by_ref(self: &Arc<Self>) {
        self.wakeups.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline]
fn inactive_epoch_for(pool: &Pool) -> u64 {
    pool.epoch().wrapping_sub(2)
}

#[test]
fn fast_path_single_acquirer() {
    let pool = Pool::new(Config {
        capacity: 100,
        active_reserve_fraction: 0.0,
        max_single_acquire: 100,
    });

    let acquired = pool.try_acquire(16, inactive_epoch_for(&pool));
    assert_eq!(acquired, 16);
    assert_eq!(pool.debug_available(), 84);
}

#[test]
fn fast_path_concurrent() {
    let pool = Arc::new(Pool::new(Config {
        capacity: 128,
        active_reserve_fraction: 0.0,
        max_single_acquire: 1,
    }));

    let mut threads = Vec::new();
    for _ in 0..128 {
        let pool = pool.clone();
        threads.push(std::thread::spawn(move || {
            pool.try_acquire(1, inactive_epoch_for(&pool))
        }));
    }

    let mut total = 0;
    for thread in threads {
        total += thread.join().expect("thread should join");
    }

    assert_eq!(total, 128);
    assert_eq!(pool.debug_available(), 0);
}

#[test]
fn exhaustion_returns_pending() {
    let pool = Pool::new(Config {
        capacity: 0,
        active_reserve_fraction: 0.0,
        max_single_acquire: 64,
    });
    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter);
    let mut cx = Context::from_waker(&waker);

    let res = pool.poll_acquire(&mut cx, 1, inactive_epoch_for(&pool), 2);
    assert!(matches!(res, Poll::Pending));
    assert_eq!(pool.debug_waiter_total(), 1);
}

#[test]
fn release_wakes_waiter() {
    let pool = Pool::new(Config {
        capacity: 0,
        active_reserve_fraction: 0.0,
        max_single_acquire: 64,
    });
    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(
        pool.poll_acquire(&mut cx, 5, inactive_epoch_for(&pool), 2),
        Poll::Pending
    ));
    pool.release(5);
    assert_eq!(counter.wakeups(), 1);

    let res = pool.poll_acquire(&mut cx, 5, inactive_epoch_for(&pool), 2);
    assert_eq!(res, Poll::Ready(5));
}

#[test]
fn priority_ordering() {
    let pool = Pool::new(Config {
        capacity: 0,
        active_reserve_fraction: 0.0,
        max_single_acquire: 64,
    });

    let low_counter = Arc::new(WakeCounter::default());
    let high_counter = Arc::new(WakeCounter::default());
    let low_waker = Waker::from(low_counter.clone());
    let high_waker = Waker::from(high_counter.clone());
    let mut low_cx = Context::from_waker(&low_waker);
    let mut high_cx = Context::from_waker(&high_waker);

    assert!(matches!(
        pool.poll_acquire(&mut low_cx, 5, inactive_epoch_for(&pool), 4),
        Poll::Pending
    ));
    assert!(matches!(
        pool.poll_acquire(&mut high_cx, 5, inactive_epoch_for(&pool), 0),
        Poll::Pending
    ));

    pool.release(5);
    assert_eq!(high_counter.wakeups(), 1);
    assert_eq!(low_counter.wakeups(), 0);
}

#[test]
fn same_tier_fifo() {
    let pool = Pool::new(Config {
        capacity: 0,
        active_reserve_fraction: 0.0,
        max_single_acquire: 64,
    });

    let first_counter = Arc::new(WakeCounter::default());
    let second_counter = Arc::new(WakeCounter::default());
    let first_waker = Waker::from(first_counter.clone());
    let second_waker = Waker::from(second_counter.clone());
    let mut first_cx = Context::from_waker(&first_waker);
    let mut second_cx = Context::from_waker(&second_waker);

    assert!(matches!(
        pool.poll_acquire(&mut first_cx, 5, inactive_epoch_for(&pool), 2),
        Poll::Pending
    ));
    assert!(matches!(
        pool.poll_acquire(&mut second_cx, 5, inactive_epoch_for(&pool), 2),
        Poll::Pending
    ));

    pool.release(5);
    assert_eq!(first_counter.wakeups(), 1);
    assert_eq!(second_counter.wakeups(), 0);

    pool.release(5);
    assert_eq!(second_counter.wakeups(), 1);
}

#[test]
fn active_reserve_preferred() {
    let pool = Pool::new(Config {
        capacity: 100,
        active_reserve_fraction: 0.5,
        max_single_acquire: 100,
    });
    let active_epoch = pool.epoch();

    let acquired = pool.try_acquire(10, active_epoch);
    assert_eq!(acquired, 10);
    assert_eq!(pool.debug_active_reserve(), 40);
    assert_eq!(pool.debug_available(), 50);
}

#[test]
fn inactive_skips_reserve() {
    let pool = Pool::new(Config {
        capacity: 100,
        active_reserve_fraction: 0.5,
        max_single_acquire: 100,
    });

    let acquired = pool.try_acquire(10, inactive_epoch_for(&pool));
    assert_eq!(acquired, 10);
    assert_eq!(pool.debug_active_reserve(), 50);
    assert_eq!(pool.debug_available(), 40);
}

#[test]
fn drop_returns_credits() {
    let pool = Arc::new(Pool::new(Config {
        capacity: 32,
        active_reserve_fraction: 0.0,
        max_single_acquire: 32,
    }));

    {
        let mut handle = Handle::new(pool.clone());
        assert_eq!(handle.try_acquire(16, inactive_epoch_for(&pool)), 16);
    }

    let mut handle = Handle::new(pool);
    assert_eq!(handle.try_acquire(32, 0), 32);
}

#[test]
fn burst_cap_enforced() {
    let pool = Pool::new(Config {
        capacity: 100,
        active_reserve_fraction: 0.0,
        max_single_acquire: 8,
    });

    let acquired = pool.try_acquire(20, inactive_epoch_for(&pool));
    assert_eq!(acquired, 8);
}

#[test]
fn replenish_active_reserve() {
    let pool = Pool::new(Config {
        capacity: 100,
        active_reserve_fraction: 0.25,
        max_single_acquire: 100,
    });

    assert_eq!(pool.try_acquire(20, pool.epoch()), 20);
    assert_eq!(pool.debug_active_reserve(), 5);
    assert_eq!(pool.debug_available(), 75);

    pool.replenish_active_reserve();
    assert_eq!(pool.debug_active_reserve(), 25);
    assert_eq!(pool.debug_available(), 55);
}

#[test]
fn negative_available_signals_waiters() {
    let pool = Pool::new(Config {
        capacity: 0,
        active_reserve_fraction: 0.0,
        max_single_acquire: 64,
    });
    pool.debug_set_available(-10);

    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);

    assert!(matches!(
        pool.poll_acquire(&mut cx, 5, inactive_epoch_for(&pool), 1),
        Poll::Pending
    ));

    pool.release(15);
    assert_eq!(counter.wakeups(), 1);
    assert_eq!(pool.poll_acquire(&mut cx, 5, inactive_epoch_for(&pool), 1), Poll::Ready(5));
}
