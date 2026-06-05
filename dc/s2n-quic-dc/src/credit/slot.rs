// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::intrusive::{Adapter, Links};
use core::cell::UnsafeCell;
use std::{
    ptr::NonNull,
    sync::atomic::{AtomicU32, Ordering},
    task::Waker,
};

/// Result of checking a slot after being woken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantResult {
    /// Credits were granted by the pool.
    Granted(u64),
    /// The pool was dropped. No more credits will be issued.
    Closed,
    /// Spurious wake — still linked, no grant yet.
    Pending,
}

/// Refcount value when the application owns the slot exclusively.
const RC_APP: u32 = 1;
/// Refcount value when the slot is linked in the pool's wait list.
const RC_LINKED: u32 = 2;
/// Refcount value when the application abandoned the slot while linked.
const RC_DEAD: u32 = 0;

/// Sentinel value written to `granted` when the pool is closed/dropped.
/// The application checks for this to distinguish "granted 0 bytes" from "pool gone."
pub const GRANT_CLOSED: u64 = u64::MAX;

/// A credit slot embedded as the first field of a stream allocation.
///
/// The pool sees only `NonNull<Slot>`. The application casts to its full typed pointer
/// (`WriterAlloc`, `ReaderAlloc`, etc.) using the `#[repr(C)]` guarantee that `Slot`
/// shares the same address as the outer struct.
///
/// Thread safety is enforced by the refcount state machine — see module-level docs.
#[repr(C)]
pub struct Slot {
    refcount: AtomicU32,
    drop_fn: unsafe fn(NonNull<Slot>),
    links: Links,
    waker: UnsafeCell<Option<Waker>>,
    requested: UnsafeCell<u64>,
    granted: UnsafeCell<u64>,
}

unsafe impl Send for Slot {}
unsafe impl Sync for Slot {}

impl Slot {
    /// Create a new idle slot with the given drop function.
    ///
    /// The `drop_fn` is called when the pool pops a dead slot (refcount=0).
    /// It must cast the pointer to the outer type, drop_in_place, and dealloc.
    #[inline]
    pub fn new(drop_fn: unsafe fn(NonNull<Slot>)) -> Self {
        Self {
            refcount: AtomicU32::new(RC_APP),
            drop_fn,
            links: Links::new(),
            waker: UnsafeCell::new(None),
            requested: UnsafeCell::new(0),
            granted: UnsafeCell::new(0),
        }
    }

    /// Prepare the slot for parking. Writes `requested` and `waker`.
    ///
    /// # Safety
    ///
    /// Caller must hold refcount=1 (exclusive app ownership). After this call,
    /// the caller must link the slot into the pool under the tier mutex and
    /// transition to refcount=2.
    #[inline]
    pub unsafe fn prepare_park(&self, requested: u64, waker: &Waker) {
        debug_assert_eq!(self.refcount.load(Ordering::Relaxed), RC_APP);
        *self.waker.get() = Some(waker.clone());
        *self.requested.get() = requested;
        *self.granted.get() = 0;
    }

    /// Cancel a park that was prepared but not committed (CAS succeeded under lock).
    ///
    /// # Safety
    ///
    /// Must only be called after `prepare_park` and before `transition_to_linked`.
    #[inline]
    pub unsafe fn cancel_park(&self) {
        debug_assert_eq!(self.refcount.load(Ordering::Relaxed), RC_APP);
        *self.waker.get() = None;
    }

    /// Transition from app-owned to linked. Must be called under the tier mutex
    /// after linking into the list.
    #[inline]
    pub unsafe fn transition_to_linked(&self) {
        debug_assert_eq!(self.refcount.load(Ordering::Relaxed), RC_APP);
        self.refcount.store(RC_LINKED, Ordering::Release);
    }

    /// Read the granted credits after being woken.
    ///
    /// Returns `Ok(granted)` if the pool has written a grant (refcount=1).
    /// Returns `Err(Closed)` if the pool was dropped (sentinel value).
    /// Returns `Err(Pending)` if this is a spurious wake (refcount still 2).
    #[inline]
    pub fn poll_granted(&self) -> GrantResult {
        let rc = self.refcount.load(Ordering::Acquire);
        match rc {
            RC_APP => {
                let granted = unsafe { *self.granted.get() };
                if granted == GRANT_CLOSED {
                    GrantResult::Closed
                } else {
                    GrantResult::Granted(granted)
                }
            }
            RC_LINKED => GrantResult::Pending,
            _ => unreachable!("unexpected refcount {rc} in poll_granted"),
        }
    }

    /// Called by the pool under the tier mutex to grant credits and release
    /// the slot back to the application.
    ///
    /// Returns the waker to be called after releasing the mutex.
    /// Returns `None` if the slot is dead (app abandoned it concurrently).
    ///
    /// Uses CAS so the grant only succeeds if the slot is still LINKED — if
    /// the app raced and stored DEAD, this returns None and the pool treats
    /// the slot as abandoned.
    ///
    /// # Safety
    ///
    /// Must be called while holding the tier mutex, after popping from the list.
    #[inline]
    pub unsafe fn grant(&self, amount: u64) -> Option<Waker> {
        // Speculatively write the grant fields. If the CAS fails (app abandoned),
        // these writes are observable to nobody — the app already dropped its
        // reference and won't read these fields, and the pool's `DeadSlot::drop`
        // will free the allocation.
        *self.granted.get() = amount;
        let waker = (*self.waker.get()).take();

        // Try to transition LINKED → APP. If the app raced and set DEAD, the
        // CAS fails and we return None so the pool can free the allocation.
        match self.refcount.compare_exchange(
            RC_LINKED,
            RC_APP,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => waker,
            Err(rc) => {
                debug_assert_eq!(rc, RC_DEAD, "unexpected refcount {rc} in grant");
                // App abandoned. Drop the waker we took (it would never be
                // useful). The pool will free the slot.
                drop(waker);
                None
            }
        }
    }

    /// Read the requested amount. Called by the pool under the tier mutex.
    ///
    /// # Safety
    ///
    /// Must be called while holding the tier mutex, with the slot linked (rc=2 or rc=0).
    #[inline]
    pub unsafe fn requested(&self) -> u64 {
        *self.requested.get()
    }

    /// Check if the slot is dead (application abandoned it).
    #[inline]
    pub fn is_dead(&self) -> bool {
        self.refcount.load(Ordering::Relaxed) == RC_DEAD
    }

    /// Abandon the slot from the application side while linked.
    ///
    /// Returns `Ok(())` if the slot was successfully marked DEAD — the pool
    /// will free the allocation when it next pops this entry.
    ///
    /// Returns `Err(granted)` if the pool concurrently granted credits — the
    /// caller must perform an idle-state cleanup (free the allocation itself
    /// since the pool has already released ownership). The returned `granted`
    /// value indicates how much was granted (which the caller might want to
    /// release back to the pool).
    ///
    /// # Safety
    ///
    /// Must only be called from the application side while the slot is in the
    /// LINKED or APP state. The caller must NOT access any non-thread-safe
    /// fields after a successful abandon (Ok return).
    #[inline]
    pub unsafe fn abandon(&self) -> Result<(), u64> {
        // Try to transition LINKED → DEAD. If the pool already transitioned
        // to APP (granted), the CAS fails and we report the grant amount.
        match self.refcount.compare_exchange(
            RC_LINKED,
            RC_DEAD,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(rc) => {
                debug_assert_eq!(rc, RC_APP, "unexpected refcount {rc} in abandon");
                // Pool granted just before we abandoned. Read the grant; the
                // caller now owns the allocation outright.
                Err(*self.granted.get())
            }
        }
    }

    /// Call the stored drop function to free the outer allocation.
    ///
    /// # Safety
    ///
    /// Must only be called when the pool owns the slot (rc=0) and has popped
    /// it from all lists. The pointer must not be used after this call.
    #[inline]
    pub unsafe fn call_drop_fn(ptr: NonNull<Slot>) {
        let drop_fn = (*ptr.as_ptr()).drop_fn;
        drop_fn(ptr);
    }

    /// Returns whether the slot is currently idle (refcount=1, app-owned exclusively).
    #[inline]
    pub fn is_idle(&self) -> bool {
        self.refcount.load(Ordering::Relaxed) == RC_APP
    }

    /// Returns whether the slot is currently linked (refcount=2).
    #[inline]
    pub fn is_linked(&self) -> bool {
        self.refcount.load(Ordering::Relaxed) == RC_LINKED
    }
}

// ── Adapter for intrusive list ───────────────────────────────────────────────

/// An owning handle to a linked slot in the pool's wait list.
///
/// On drop (pool shutdown), writes `GRANT_CLOSED` as the sentinel, transitions
/// refcount 2→1, and wakes the task. If the slot is dead (rc=0), calls `drop_fn`.
///
/// In the normal grant path, the pool calls `take()` to suppress this drop
/// behavior before writing the real grant.
pub struct SlotPtr(NonNull<Slot>);

unsafe impl Send for SlotPtr {}
unsafe impl Sync for SlotPtr {}

impl SlotPtr {
    #[inline]
    pub fn new(ptr: NonNull<Slot>) -> Self {
        Self(ptr)
    }

    #[inline]
    pub fn as_non_null(&self) -> NonNull<Slot> {
        self.0
    }

    /// Consume the pointer without running the drop logic.
    ///
    /// Used by the pool's grant path after popping from the list — the slot
    /// is being granted normally, not shut down.
    #[inline]
    pub fn take(self) -> NonNull<Slot> {
        let ptr = self.0;
        core::mem::forget(self);
        ptr
    }
}

impl Drop for SlotPtr {
    fn drop(&mut self) {
        unsafe {
            let slot = &*self.0.as_ptr();

            // Speculatively write the closed sentinel. The CAS below decides
            // whether this write survives.
            *slot.granted.get() = GRANT_CLOSED;
            let waker = (*slot.waker.get()).take();

            // Try to transition LINKED → APP, signalling closure. If the app
            // raced and abandoned, the CAS fails (rc was DEAD) and we own
            // the allocation.
            match slot.refcount.compare_exchange(
                RC_LINKED,
                RC_APP,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if let Some(w) = waker {
                        w.wake();
                    }
                }
                Err(rc) => {
                    debug_assert_eq!(rc, RC_DEAD, "unexpected refcount {rc} in SlotPtr::drop");
                    drop(waker);
                    Slot::call_drop_fn(self.0);
                }
            }
        }
    }
}

impl From<NonNull<Slot>> for SlotPtr {
    fn from(ptr: NonNull<Slot>) -> Self {
        Self(ptr)
    }
}

/// Adapter for the intrusive list. Non-owning — lifetime is managed by refcount.
pub struct SlotAdapter;

impl Adapter for SlotAdapter {
    type Value = Slot;
    type Target = Slot;
    type Pointer = SlotPtr;

    unsafe fn links(value: *mut Self::Value) -> *mut Links {
        &raw mut (*value).links
    }

    unsafe fn target(value: *mut Self::Value) -> *mut Self::Target {
        value
    }

    fn as_ptr(ptr: &Self::Pointer) -> *const Self::Value {
        ptr.0.as_ptr()
    }

    fn into_raw(ptr: Self::Pointer) -> *mut Self::Value {
        ptr.take().as_ptr()
    }

    unsafe fn from_raw(ptr: *mut Self::Value) -> Self::Pointer {
        SlotPtr(NonNull::new_unchecked(ptr))
    }
}

// ── Dead slot queue ──────────────────────────────────────────────────────────

/// An owning handle to a dead slot. Calls `drop_fn` on drop, freeing the
/// outer allocation.
pub struct DeadSlot(NonNull<Slot>);

unsafe impl Send for DeadSlot {}
unsafe impl Sync for DeadSlot {}

impl DeadSlot {
    /// Wrap a dead slot pointer for deferred deallocation.
    ///
    /// # Safety
    ///
    /// The slot must have refcount=0 and must not be linked in any list.
    #[inline]
    pub unsafe fn new(ptr: NonNull<Slot>) -> Self {
        Self(ptr)
    }
}

impl Drop for DeadSlot {
    fn drop(&mut self) {
        unsafe { Slot::call_drop_fn(self.0) };
    }
}

/// Adapter for the dead-slot queue. Uses the same `links` field on `Slot`
/// (safe because the slot has already been popped from the tier list).
///
/// Unlike `SlotAdapter`, this adapter *owns* the slot: when the list drops,
/// each entry is reconstructed as `DeadSlot` and freed via its `Drop` impl.
pub struct DeadSlotAdapter;

impl Adapter for DeadSlotAdapter {
    type Value = Slot;
    type Target = Slot;
    type Pointer = DeadSlot;

    unsafe fn links(value: *mut Self::Value) -> *mut Links {
        &raw mut (*value).links
    }

    unsafe fn target(value: *mut Self::Value) -> *mut Self::Target {
        value
    }

    fn as_ptr(ptr: &Self::Pointer) -> *const Self::Value {
        ptr.0.as_ptr()
    }

    fn into_raw(ptr: Self::Pointer) -> *mut Self::Value {
        let raw = ptr.0.as_ptr();
        core::mem::forget(ptr);
        raw
    }

    unsafe fn from_raw(ptr: *mut Self::Value) -> Self::Pointer {
        DeadSlot(NonNull::new_unchecked(ptr))
    }
}

/// A queue of dead slots. Dropping the queue frees all entries automatically.
pub type DeadSlotQueue = crate::intrusive::List<DeadSlotAdapter>;

impl crate::socket::channel::UnboundedSender<DeadSlot> for DeadSlotQueue {
    fn send(&mut self, slot: DeadSlot) -> Result<(), DeadSlot> {
        self.push_back(slot);
        Ok(())
    }
}
