// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The credit-acquire allocation: a reusable, slot-bearing submitter context.
//!
//! [`SubmitterAlloc`] owns one heap [`Slot`] (at offset 0 of a `#[repr(C)]` allocation) plus the
//! per-acquire bookkeeping. It mirrors `stream::writer::WriterAlloc`'s slot lifecycle, with one
//! deliberate difference: it is **reusable across many acquires**. A `submit` allocates one and drops
//! it; a `MaterializeStream` allocates one and reuses it for every block it sprays, so a stream of N
//! block reads costs **one** slot allocation rather than N. After each grant the slot returns to its
//! idle (`RC_APP`) state, ready to be parked again by the next [`acquire`](SubmitterAlloc::acquire).
//!
//! The credit conservation contract: every `acquire` that succeeds leaves at least `cost` credits in
//! the alloc; the caller moves them onto the op with [`take_all`](SubmitterAlloc::take_all) (so the
//! completion dispatcher releases them exactly once), and any credits still held when the alloc drops
//! — a grant delivered while parked, then abandoned — are released back to their pool by the
//! abandon/`drop_fn` dance.

use crate::{
    credit::{AbandonResult, GrantResult, Pool, Slot},
    sched::TierPriority,
    sync::Arc,
};
use core::{
    future::poll_fn,
    task::{Context, Poll},
};
use std::{
    alloc::{self, Layout},
    ptr::NonNull,
};

/// Per-acquire state: credits granted but not yet moved onto an op, and the pool they belong to (so
/// the drop path releases them to the right pool). `pool` is `None` before the first acquire.
struct AllocState {
    pending_credits: u64,
    pool: Option<Arc<Pool>>,
    /// `(device_index, kind)` for the [flight recorder](crate::fs::trace), so a credit park/grant/
    /// partial transition can be attributed to its device and op-kind. The acquire primitive itself is
    /// device-agnostic, so the submitter sets this before driving the acquire. cfg-gated to the trace
    /// builds (zero cost otherwise).
    #[cfg(any(test, feature = "testing", feature = "io-dbg"))]
    trace_ctx: Option<(u32, crate::fs::op::IoKind)>,
}

/// `#[repr(C)]` with `Slot` at offset 0 — the credit pool casts `NonNull<Slot>` back to
/// `NonNull<AllocInner>` via the registered `drop_fn`, which is sound only at offset 0.
#[repr(C)]
struct AllocInner {
    slot: Slot,
    state: AllocState,
}

crate::assert_slot_at_offset_zero!(AllocInner);

/// An owning, reusable credit-acquire context. Drop is staged through the slot's abandon/grant state
/// machine, mirroring `stream::writer::WriterAllocPtr`.
pub(crate) struct SubmitterAlloc(NonNull<AllocInner>);

// SAFETY: the alloc is owned exclusively; the pool only ever touches the `Slot` under its own state
// machine. Recomputing the slot pointer from `self.0` inside each poll (rather than hoisting a bare
// `NonNull` across an await) keeps the `acquire` future `Send`.
unsafe impl Send for SubmitterAlloc {}

impl SubmitterAlloc {
    /// Allocate a fresh, idle submitter context. Cheap enough for the one-shot `submit` path, but
    /// intended to be **reused** across acquires by long-lived submitters (e.g. a materialize stream).
    pub(crate) fn new() -> Self {
        let layout = Layout::new::<AllocInner>();
        let raw = unsafe { alloc::alloc(layout) } as *mut AllocInner;
        let ptr = NonNull::new(raw).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        unsafe {
            std::ptr::write(
                ptr.as_ptr(),
                AllocInner {
                    slot: Slot::new(drop_alloc_inner),
                    state: AllocState {
                        pending_credits: 0,
                        pool: None,
                        #[cfg(any(test, feature = "testing", feature = "io-dbg"))]
                        trace_ctx: None,
                    },
                },
            );
        }
        Self(ptr)
    }

    #[inline]
    fn slot_ptr(&self) -> NonNull<Slot> {
        self.0.cast()
    }

    /// Record a credit-acquire transition for the [flight recorder](crate::fs::trace), using the
    /// `(device_index, kind)` context the submitter set via [`set_trace_ctx`](Self::set_trace_ctx).
    /// A no-op (and fully stripped) outside the trace builds.
    #[inline]
    fn trace_credit(&mut self, lifecycle: crate::fs::trace::IoLifecycle, cost: u64) {
        #[cfg(any(test, feature = "testing", feature = "io-dbg"))]
        if let Some((device_index, kind)) = self.state().trace_ctx {
            crate::fs::trace::credit(lifecycle, device_index, kind, cost);
        }
        #[cfg(not(any(test, feature = "testing", feature = "io-dbg")))]
        {
            let _ = (lifecycle, cost);
        }
    }

    #[inline]
    fn state(&mut self) -> &mut AllocState {
        unsafe { &mut (*self.0.as_ptr()).state }
    }

    /// Move all currently-held credits out of the alloc (resetting it to zero), to be recorded on an
    /// op as its `flow_credits`. After this the alloc holds nothing, so a subsequent drop releases
    /// nothing and the slot is clean for its next [`acquire`](Self::acquire).
    #[inline]
    pub(crate) fn take_all(&mut self) -> u64 {
        core::mem::take(&mut self.state().pending_credits)
    }

    /// Attribute subsequent credit-acquire transitions to `(device_index, kind)` for the
    /// [flight recorder](crate::fs::trace). A no-op outside the trace builds. Callers set this before
    /// driving [`acquire`](Self::acquire)/[`poll_acquire`](Self::poll_acquire).
    #[inline]
    pub(crate) fn set_trace_ctx(&mut self, device_index: usize, kind: crate::fs::op::IoKind) {
        #[cfg(any(test, feature = "testing", feature = "io-dbg"))]
        {
            self.state().trace_ctx = Some((device_index as u32, kind));
        }
        #[cfg(not(any(test, feature = "testing", feature = "io-dbg")))]
        {
            let _ = (device_index, kind);
        }
    }

    /// Park (cooperatively, on a waker — never a thread) until at least `cost` credits are held for
    /// `pool`, then return. This is the backpressure that prevents the blocking-pool deadlock. Thin
    /// async wrapper over [`poll_acquire`](Self::poll_acquire).
    pub(crate) async fn acquire(
        &mut self,
        pool: &Arc<Pool>,
        cost: u64,
        priority: TierPriority,
    ) -> std::io::Result<()> {
        poll_fn(|cx: &mut Context<'_>| self.poll_acquire(cx, pool, cost, priority)).await
    }

    /// Poll-based acquire: returns `Ready(Ok(()))` once at least `cost` credits are held for `pool`,
    /// `Ready(Err)` if the pool closed, else `Pending` (the slot is parked and `cx`'s waker registered).
    ///
    /// Exposed (rather than only the async `acquire`) so a submitter that drives **several** acquires
    /// concurrently from one task — e.g. [`MaterializeStream`](crate::fs::materialize) acquiring one
    /// block per device in parallel to avoid cross-device head-of-line blocking — can poll each in its
    /// own slot under a single task waker.
    ///
    /// The slot is reused: with the device's atomic-grant config (`min_grant_slice` floored to
    /// `max_single_acquire`) the distributor delivers the full `cost` in one wake, so this parks at
    /// most once and the slot returns to idle on grant. Holding credits for a *different* pool than
    /// `pool` (only possible if a prior acquire was abandoned mid-grant) releases them back first so
    /// they are never stranded or misattributed.
    pub(crate) fn poll_acquire(
        &mut self,
        cx: &mut Context<'_>,
        pool: &Arc<Pool>,
        cost: u64,
        priority: TierPriority,
    ) -> Poll<std::io::Result<()>> {
        // If we hold credits for a different pool, return them before switching pools so the two pools
        // stay independently conserved. (Only the IDLE slot may switch pools; a parked slot is still
        // physically LINKED in its current pool's wait list, so re-binding `state.pool` while linked
        // would misattribute that pool's eventual grant to the new pool. No caller does this — a
        // `MaterializeStream` holds one slot per device and a one-shot `submit` a fresh slot, so the
        // pool is stable per slot — but the invariant is load-bearing, hence the tripwire.)
        let switch = match &self.state().pool {
            Some(p) => !Arc::ptr_eq(p, pool),
            None => false,
        };
        if switch {
            debug_assert!(
                !unsafe { self.slot_ptr().as_ref() }.is_linked(),
                "SubmitterAlloc pool switch while the slot is parked (LINKED) on the old pool would \
                 misattribute that pool's grant; a slot's pool must be stable while it can be parked"
            );
            let leftover = core::mem::take(&mut self.state().pending_credits);
            if let Some(old) = self.state().pool.take() {
                if leftover > 0 {
                    old.release(leftover);
                }
            }
        }
        self.state().pool = Some(pool.clone());

        // Recompute the slot pointer each poll from `self.0` (a `Send` owning pointer) rather than
        // hoisting a bare `!Send` `NonNull` — keeps the enclosing future `Send`.
        let slot_ptr = self.slot_ptr();
        // Drain any grant delivered while parked.
        let slot_ref = unsafe { slot_ptr.as_ref() };
        match slot_ref.poll_granted() {
            GrantResult::Pending => return Poll::Pending,
            GrantResult::Closed => {
                // The pool was dropped (scheduler teardown). Any residual `pending_credits` from a
                // prior partial grant can no longer be returned to it — drop them so a subsequent
                // `SubmitterAlloc::drop` (which takes the `Closed` abandon arm and touches no pool)
                // has nothing dangling. There is no live pool to leak against; this is hygiene.
                self.state().pending_credits = 0;
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "io scheduler credit pool closed",
                )));
            }
            GrantResult::Granted(n) => {
                let s = self.state();
                s.pending_credits = s.pending_credits.saturating_add(n);
            }
        }

        if self.state().pending_credits >= cost {
            // Already held enough (e.g. drained from a prior partial grant): a full grant.
            self.trace_credit(crate::fs::trace::IoLifecycle::CreditGrant, cost);
            return Poll::Ready(Ok(()));
        }

        let need = cost - self.state().pending_credits;
        // SAFETY: `slot_ptr` is this future's stable, idle slot.
        match unsafe { pool.poll_acquire(cx, slot_ptr, need, priority) } {
            Poll::Ready(n) => {
                let s = self.state();
                s.pending_credits = s.pending_credits.saturating_add(n);
                if self.state().pending_credits >= cost {
                    self.trace_credit(crate::fs::trace::IoLifecycle::CreditGrant, cost);
                    Poll::Ready(Ok(()))
                } else {
                    // A partial grant (the fast path capped at `max_single_acquire`): self-wake to
                    // re-acquire the remainder. Bounded by `ceil(cost / max_single_acquire)` since
                    // `cost <= capacity` is enforced before acquire.
                    self.trace_credit(crate::fs::trace::IoLifecycle::CreditPartial, cost);
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            // The acquire request parked on a waker (device at capacity): the op is now waiting.
            Poll::Pending => {
                self.trace_credit(crate::fs::trace::IoLifecycle::CreditPark, cost);
                Poll::Pending
            }
        }
    }
}

impl Drop for SubmitterAlloc {
    fn drop(&mut self) {
        // Mirror WriterAllocPtr::drop: `abandon`'s CAS is the single source of truth for ownership.
        let slot = unsafe { &(*self.0.as_ptr()).slot };
        match unsafe { slot.abandon() } {
            AbandonResult::Abandoned => {
                // Slot was LINKED, now DEAD: the pool's pop walk calls `drop_alloc_inner`.
                return;
            }
            AbandonResult::Granted(n) => {
                let state = unsafe { &(*self.0.as_ptr()).state };
                let to_release = n.saturating_add(state.pending_credits);
                if to_release > 0 {
                    if let Some(pool) = &state.pool {
                        pool.release(to_release);
                    }
                }
            }
            AbandonResult::Closed => {
                // Pool gone; do not touch it.
            }
        }
        unsafe {
            std::ptr::drop_in_place(&raw mut (*self.0.as_ptr()).state);
            alloc::dealloc(self.0.as_ptr().cast(), Layout::new::<AllocInner>());
        }
    }
}

/// `drop_fn` the pool invokes when it pops a dead slot (the alloc was dropped while parked).
unsafe fn drop_alloc_inner(ptr: NonNull<Slot>) {
    let ptr = ptr.cast::<AllocInner>();
    let state = &(*ptr.as_ptr()).state;
    if state.pending_credits > 0 {
        if let Some(pool) = &state.pool {
            pool.release(state.pending_credits);
        }
    }
    std::ptr::drop_in_place(&raw mut (*ptr.as_ptr()).state);
    alloc::dealloc(ptr.as_ptr().cast(), Layout::new::<AllocInner>());
}
