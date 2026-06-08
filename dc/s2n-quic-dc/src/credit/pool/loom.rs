// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Loom models for [`Pool`] and [`Distributor`].
//!
//! Run via `LOOM_MAX_PREEMPTIONS=3 cargo test --profile release-debug --features loom credit::pool::loom`.

use super::*;
use crate::{
    credit::slot::{AbandonResult, GrantResult, Slot},
    intrusive::Queue,
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

/// Counts grants delivered by the distributor (and forwards each granted slot's waker).
struct CountWake(Arc<crate::sync::AtomicUsize>);
impl UnboundedSender<Queue<Waker>> for CountWake {
    fn send(&mut self, mut batch: Queue<Waker>) -> Result<(), Queue<Waker>> {
        while let Some(entry) = batch.pop_front() {
            self.0.fetch_add(1, Ordering::Release);
            entry.into_inner().wake();
        }
        Ok(())
    }
}

/// A `Send` carrier for a raw slot pointer crossing a loom thread boundary.
struct SendPtr(NonNull<Slot>);
unsafe impl Send for SendPtr {}

/// Drive the distributor to completion on its own thread, parking via `block_on` whenever
/// `poll_distribute` yields `Pending`. The thread is re-polled ONLY by a real wakeup, so a lost
/// wakeup manifests as a permanent park → loom deadlock. Completes once `granted` reaches
/// `target`. Returns the distributor's final `paid_demand` so the caller can recover outstanding
/// demand (`parked_demand − paid_demand`) for conservation assertions.
fn spawn_distributor(
    pool: Arc<Pool>,
    granted: Arc<crate::sync::AtomicUsize>,
    target: usize,
) -> loom::thread::JoinHandle<u64> {
    loom::thread::spawn(move || {
        let mut dist = Distributor::new(pool);
        let mut wakers = CountWake(granted.clone());
        loom::future::block_on(core::future::poll_fn(|cx| {
            dist.register_waker(cx);
            let mut budget = Budget::new(1 << 16);
            let _ = dist.poll_distribute(&mut budget, &mut wakers);
            if granted.load(Ordering::Acquire) >= target {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
        dist.debug_paid_demand()
    })
}

/// A parking `poll_acquire` races a distributor pass that is holding distributable credit. This
/// targets the two-load free-credit recovery in `pass`: the park does `available -= n` then
/// `parked_demand += n`, and the distributor reads `parked_demand` then `available`. If the loads
/// were in the wrong order the distributor could observe the `+n` without the `-n` and over-count
/// free credit, granting a slot it cannot afford and driving the conserved total past capacity.
/// `parked_demand` is monotonic; outstanding demand is recovered after the distributor joins as
/// `parked_demand − granted_total` (here, `granted` × the per-grant amount). We assert conservation
/// (`available + outstanding + returned + in_flight == capacity`) holds after every interleaving.
#[test]
fn park_races_distributor_no_overcommit() {
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
                let granted = Arc::new(crate::sync::AtomicUsize::new(0));
                let mut wakers = CountWake(granted);
                let mut budget = Budget::new(1 << 16);
                let dwaker = noop();
                let mut dcx = Context::from_waker(&dwaker);
                dist.register_waker(&mut dcx);
                let _ = dist.poll_distribute(&mut budget, &mut wakers);
                dist.debug_paid_demand()
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
                let granted_now =
                    match unsafe { pool.poll_acquire(&mut wcx, w, CAP as u64, Priority::Low) } {
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
        let paid = dist.join().unwrap() as i64;

        // Conservation: whatever the interleaving, the conserved total must never imply more than
        // CAP bytes in flight. The wrong load order over-counts free credit and breaks this.
        // `parked_demand` is monotonic; outstanding demand is `parked_demand − paid`.
        let available = pool.available.load(Ordering::Relaxed);
        let parked = pool.parked_demand.load(Ordering::Relaxed) as i64;
        let outstanding = parked - paid;
        let returned = pool.returned.load(Ordering::Relaxed) as i64;
        let in_flight = CAP - (available + outstanding + returned);
        assert!(
            (0..=CAP).contains(&in_flight),
            "over-commit: in_flight={in_flight} not in 0..={CAP} \
             (available={available} outstanding={outstanding} returned={returned} \
              parked={parked} paid={paid})"
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
fn release_wakes_parked_waiter() {
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

        let paid = dist.join().unwrap();

        let slot_ref = unsafe { &*slot.as_ptr() };
        assert_eq!(slot_ref.poll_granted(), GrantResult::Granted(10));
        // Conservation: the 10 is now in-flight; nothing stranded.
        // `parked_demand` is monotonic, so the cancellation check is parked == paid.
        assert_eq!(pool.available.load(Ordering::Relaxed), 0);
        assert_eq!(pool.parked_demand.load(Ordering::Relaxed), paid);
        assert_eq!(pool.returned.load(Ordering::Relaxed), 0);

        unsafe { free_slot(slot) };
    });
}

/// Two concurrent releasers each return part of what the waiter needs. Whatever order they
/// interleave with the distributor, their credit must accumulate (one pass writes the leftover
/// back to `available`, the next pass picks it up) and the waiter must be served exactly once.
#[test]
fn concurrent_releases_accumulate() {
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
        let paid = dist.join().unwrap();

        let slot_ref = unsafe { &*slot.as_ptr() };
        assert_eq!(slot_ref.poll_granted(), GrantResult::Granted(10));
        assert_eq!(pool.available.load(Ordering::Relaxed), 0);
        assert_eq!(pool.parked_demand.load(Ordering::Relaxed), paid);

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
fn newcomer_cannot_snipe() {
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
                assert!(
                    matches!(r, Poll::Pending),
                    "newcomer sniped a parked waiter"
                );
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
