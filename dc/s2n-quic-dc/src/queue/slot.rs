// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A single queue slot: two message halves plus an atomic binding identifier.
//!
//! The top bit (bit 63) of `binding_id` is the "unallocated" sentinel.  A slot
//! with that bit set is free for the allocator to claim.  All valid `VarInt`
//! binding IDs have the top two bits clear (QUIC VarInt encoding), so there is
//! no overlap.

use super::half::{self, Half};
use crate::endpoint::msg;
use core::sync::atomic::{AtomicU64, Ordering};
use s2n_quic_core::varint::VarInt;

/// The MSB of the u64 binding_id field is set when the slot is free.
pub(crate) const UNALLOCATED_BIT: u64 = 1 << 63;

/// Initial state: unallocated, no binding.
const UNALLOCATED: u64 = UNALLOCATED_BIT;

pub(crate) struct Slot {
    /// Packed field: MSB = unallocated flag, bits 0-62 = binding_id.
    pub(crate) binding_id: AtomicU64,
    pub(crate) stream: Half<msg::Stream>,
    pub(crate) control: Half<msg::Control>,
}

impl Slot {
    /// Create a new, unallocated slot.
    pub(crate) fn new() -> Self {
        Self {
            binding_id: AtomicU64::new(UNALLOCATED),
            stream: Half::new(),
            control: Half::new(),
        }
    }

    /// Returns `true` if this slot is currently allocated (bound or bindable).
    #[inline]
    pub(crate) fn is_allocated(&self) -> bool {
        self.binding_id.load(Ordering::Acquire) & UNALLOCATED_BIT == 0
    }

    /// Attempt to allocate this slot by CAS-ing from UNALLOCATED to
    /// `binding_id`.
    ///
    /// Returns `true` on success.
    #[inline]
    pub(crate) fn try_allocate(&self, binding_id: VarInt) -> bool {
        self.binding_id
            .compare_exchange(
                UNALLOCATED,
                binding_id.as_u64(),
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Attempt to rebind this slot from `expected_binding_id` to
    /// `new_binding_id` (server bind-and-send path).
    ///
    /// Returns `true` on success.
    #[inline]
    #[allow(dead_code)]
    pub(crate) fn try_rebind(&self, expected: VarInt, new: VarInt) -> bool {
        self.binding_id
            .compare_exchange(
                expected.as_u64(),
                new.as_u64(),
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
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

    /// Open both receiver halves atomically.
    #[inline]
    pub(crate) fn open_receivers(&self) -> Result<(), half::Closed> {
        half::open_receivers(&self.stream, &self.control)
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
}

impl core::fmt::Debug for Slot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Slot")
            .field("binding_id", &self.binding_id())
            .field("stream", &self.stream)
            .field("control", &self.control)
            .finish()
    }
}
