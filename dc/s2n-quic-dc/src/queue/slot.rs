// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A single queue slot: two message halves plus an atomic binding identifier.
//!
//! The top bit (bit 63) of `binding_id` is the "unallocated" sentinel.  A slot
//! with that bit set is free for the allocator to claim.  All valid `VarInt`
//! binding IDs have the top two bits clear (QUIC VarInt encoding), so there is
//! no overlap.

use super::half::{self, Flags, Half, HalfInner};
use crate::{endpoint::msg, intrusive};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};
use s2n_quic_core::varint::VarInt;

/// The MSB of the u64 binding_id field is set when the slot is free.
pub(crate) const UNALLOCATED_BIT: u64 = 1 << 63;

/// Initial state: unallocated, no binding.
const UNALLOCATED: u64 = UNALLOCATED_BIT;

pub(crate) struct Slot {
    /// Packed field: MSB = unallocated flag, bits 0-62 = binding_id.
    ///
    /// Mutations to this field are protected by holding both half locks
    /// simultaneously (lock order: stream → control).  Reads on the fast path
    /// use an Acquire load which sees all preceding stores.
    pub(crate) binding_id: AtomicU64,
    /// The slot's position in the page table, fixed at allocation time.
    queue_id: AtomicU64,
    pub(crate) stream: Half<msg::Stream>,
    pub(crate) control: Half<msg::Control>,
}

/// Result of `Slot::bind_and_push_stream`.
pub(crate) enum BindState {
    /// An existing, matching binding was found; the entry was pushed.
    AlreadyBound(half::AutoWake),
    /// A fresh binding was created; the entry was pushed.
    /// The caller must construct `StreamReceiver` / `ControlReceiver` and hand
    /// them to the stream handshake task.
    NewBinding(half::AutoWake),
}

impl Slot {
    /// Create a new, unallocated slot with its page-table index baked in.
    pub(crate) fn with_queue_id(queue_id: VarInt) -> Self {
        Self {
            binding_id: AtomicU64::new(UNALLOCATED),
            queue_id: AtomicU64::new(queue_id.as_u64()),
            stream: Half::new(),
            control: Half::new(),
        }
    }

    /// Returns this slot's position in the page table (set at creation time).
    #[inline]
    pub(crate) fn queue_id(&self) -> VarInt {
        // SAFETY: set once at slot creation and never mutated.
        let raw = self.queue_id.load(Ordering::Relaxed);
        VarInt::new(raw).unwrap_or(VarInt::MAX)
    }

    /// Returns a stable raw pointer to this slot.
    ///
    /// SAFETY: the pointer is valid as long as the `Arc<State>` that owns
    /// the pinned page is kept alive.
    #[inline]
    pub(crate) fn as_ptr(&self) -> NonNull<Slot> {
        unsafe { NonNull::new_unchecked(self as *const Slot as *mut Slot) }
    }

    /// Returns `true` if this slot is currently allocated (bound or bindable).
    #[inline]
    pub(crate) fn is_allocated(&self) -> bool {
        self.binding_id.load(Ordering::Acquire) & UNALLOCATED_BIT == 0
    }

    /// Load the current binding_id, or `None` if unallocated.
    #[inline]
    pub(crate) fn binding_id(&self) -> Option<VarInt> {
        let raw = self.binding_id.load(Ordering::Acquire);
        if raw & UNALLOCATED_BIT != 0 {
            return None;
        }
        VarInt::new(raw).ok()
    }

    /// Mark the slot as unallocated (called after both receivers are closed).
    #[inline]
    pub(crate) fn mark_unallocated(&self) {
        self.binding_id.store(UNALLOCATED, Ordering::Release);
    }

    /// Set `binding_id` and open both receiver halves in one critical section.
    ///
    /// Acquires both half locks (stream → control) so that the binding store
    /// and the `HAS_RECEIVER` flag updates are never visible in a partial state.
    /// Returns `Err(Closed)` if the sender side has already been closed.
    #[inline]
    pub(crate) fn allocate_and_open(&self, binding_id: VarInt) -> Result<(), half::Closed> {
        let mut s = self.stream.inner.lock();
        let mut c = self.control.inner.lock();
        Self::allocate_and_open_locked(&mut s, &mut c, &self.binding_id, binding_id)
    }

    /// Bind the slot (if unallocated) and push the first stream entry atomically.
    ///
    /// All state transitions happen inside the combined stream+control lock so
    /// there is no window where `binding_id` is set but `HAS_RECEIVER` is not,
    /// and no window where two concurrent packets can both "win" a new binding.
    ///
    /// Returns:
    /// - `Ok(BindState::NewBinding(waker))` — slot was unallocated; caller
    ///   must create `StreamReceiver` / `ControlReceiver` and route them.
    /// - `Ok(BindState::AlreadyBound(waker))` — existing matching binding;
    ///   entry pushed normally.
    /// - `Err(_)` — stale binding, sender closed, or half closed.
    pub(crate) fn bind_and_push_stream(
        &self,
        binding_id: VarInt,
        entry: intrusive::Entry<msg::Stream>,
    ) -> Result<BindState, super::Error<intrusive::Entry<msg::Stream>>> {
        let mut s = self.stream.inner.lock();
        let mut c = self.control.inner.lock();

        if !s.flags.contains(Flags::HAS_SENDER) {
            return Err(super::Error::SenderClosed);
        }

        // Binding check inside the lock — no race possible.
        let raw = self.binding_id.load(Ordering::Relaxed);

        if raw & UNALLOCATED_BIT != 0 {
            // Unallocated: bind and open receivers (simple store, no CAS needed).
            Self::allocate_and_open_locked(&mut s, &mut c, &self.binding_id, binding_id)
                .map_err(|_| super::Error::SenderClosed)?;

            s.queue.push_back(entry);
            let waker = s.take_waker();
            return Ok(BindState::NewBinding(waker));
        }

        // Already bound: validate.
        match VarInt::new(raw).ok() {
            Some(b) if b == binding_id => {}
            _ => return Err(super::Error::BindingMismatch(entry)),
        }

        if !s.flags.contains(Flags::HAS_RECEIVER) {
            return Err(super::Error::HalfClosed(entry));
        }

        s.queue.push_back(entry);
        let waker = s.take_waker();
        Ok(BindState::AlreadyBound(waker))
    }

    /// Broadcast-close both halves: clears HAS_SENDER, wakes receivers.
    ///
    /// This is the safe path for eviction — it does NOT push any data, so
    /// freshly-bound streams are never poisoned by a stale Reset.
    pub(crate) fn broadcast_close(&self) -> (half::AutoWake, half::AutoWake) {
        // Check allocated before taking locks (cheap fast path).
        if !self.is_allocated() {
            return (Default::default(), Default::default());
        }
        let stream_wake = self.stream.broadcast_close();
        let control_wake = self.control.broadcast_close();
        (stream_wake, control_wake)
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// Store `binding_id` and set `HAS_RECEIVER` on both halves while both
    /// half locks are already held.  Returns `Err(Closed)` if either sender
    /// is gone.
    fn allocate_and_open_locked(
        s: &mut HalfInner<msg::Stream>,
        c: &mut HalfInner<msg::Control>,
        binding_id_cell: &AtomicU64,
        binding_id: VarInt,
    ) -> Result<(), half::Closed> {
        if !s.flags.contains(Flags::HAS_SENDER) || !c.flags.contains(Flags::HAS_SENDER) {
            return Err(half::Closed);
        }
        debug_assert!(
            !s.flags.contains(Flags::HAS_RECEIVER) && !c.flags.contains(Flags::HAS_RECEIVER),
            "receivers already open"
        );
        // Safe to use Relaxed here: the mutex release acts as the memory fence.
        binding_id_cell.store(binding_id.as_u64(), Ordering::Relaxed);
        s.flags.insert(Flags::HAS_RECEIVER);
        c.flags.insert(Flags::HAS_RECEIVER);
        Ok(())
    }
}

impl core::fmt::Debug for Slot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Slot")
            .field("queue_id", &self.queue_id())
            .field("binding_id", &self.binding_id())
            .field("stream", &self.stream)
            .field("control", &self.control)
            .finish()
    }
}
