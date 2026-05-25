// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Client-side queue allocation and dispatch.
//!
//! ## Roles
//!
//! - `ClientAllocator` — stream creation path.  Allocates a local page-table
//!   slot and a `dest_queue_id` from the peer's `FreeList`, then opens the
//!   receiver handles.
//!
//! - `ClientDispatch` — inbound packet dispatch path.  Routes an incoming
//!   `msg::Stream` / `msg::Control` entry to the correct slot by `queue_id`
//!   after validating `binding_id`.  No allocation logic here.
//!
//! - `ClientFreeList` — local slot recycling.  Uses a `HierarchicalBitSet`
//!   for O(4) pop and a high-water mark for fresh-slot bump allocation.

use super::{
    half::AutoWake,
    handle::{AllocResult, ControlReceiver, OnFree, StreamReceiver},
    page_table::PageTable,
    slot::UNALLOCATED_BIT,
    Error,
};
use crate::{endpoint::msg, intrusive, sync};
use s2n_quic_core::varint::VarInt;
use std::{
    ptr::NonNull,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

// ── ClientFreeList ────────────────────────────────────────────────────────────

/// Local slot recycling for the client page table.
///
/// Freed slot indices are stored in a `HierarchicalBitSet` for O(4) pop.
/// Fresh slots beyond the current high-water mark are allocated by bumping.
pub struct ClientFreeList {
    freed: sync::bitset::HierarchicalBitSet,
    high_water_mark: usize,
    closed: bool,
    #[cfg(debug_assertions)]
    active: std::collections::BTreeSet<usize>,
}

impl ClientFreeList {
    fn new() -> Self {
        Self {
            freed: sync::bitset::HierarchicalBitSet::new(
                sync::bitset::HierarchicalBitSet::MAX_CAPACITY,
            ),
            high_water_mark: 0,
            closed: false,
            #[cfg(debug_assertions)]
            active: Default::default(),
        }
    }

    /// Pop the next available local slot index, or `None` if closed.
    ///
    /// Prefers recycled indices (pop from bitset) over fresh ones (bump).
    pub(crate) fn pop(&mut self) -> Option<usize> {
        if self.closed {
            return None;
        }
        if let Some(idx) = self.freed.pop_first() {
            #[cfg(debug_assertions)]
            {
                debug_assert!(self.active.insert(idx as usize), "double-alloc of {idx}");
            }
            return Some(idx as usize);
        }
        let idx = self.high_water_mark;
        self.high_water_mark += 1;
        #[cfg(debug_assertions)]
        {
            debug_assert!(self.active.insert(idx), "double-alloc of {idx}");
        }
        Some(idx)
    }

    /// Return a freed slot index back to the recycling set.
    pub(crate) fn push_freed(&mut self, index: usize) {
        #[cfg(debug_assertions)]
        {
            debug_assert!(self.active.remove(&index), "double-free of {index}");
        }
        let idx = index as u32;
        if idx < self.freed.capacity() {
            self.freed.insert(idx);
        } else if idx < sync::bitset::HierarchicalBitSet::MAX_CAPACITY {
            self.freed.grow(idx + 1);
            self.freed.insert(idx);
        }
        // If index is absurdly large (beyond MAX_CAPACITY), silently drop it —
        // the slot would just become permanently unavailable, which is safer
        // than panicking.
    }

    pub(crate) fn close(&mut self) {
        self.closed = true;
    }
}

// ── ClientAllocator ───────────────────────────────────────────────────────────

/// Allocates local queue slots and peer `dest_queue_id`s for client streams.
pub struct ClientAllocator {
    page_table: PageTable,
    local_free: Arc<Mutex<ClientFreeList>>,
    peer_free: Arc<sync::free_list::FreeList>,
}

impl ClientAllocator {
    pub fn new(peer_free: Arc<sync::free_list::FreeList>) -> Self {
        Self {
            page_table: PageTable::new(),
            local_free: Arc::new(Mutex::new(ClientFreeList::new())),
            peer_free,
        }
    }

    /// Non-blocking alloc.  Returns `None` if peer has no free queue IDs.
    pub fn try_alloc(&self, binding_id: VarInt) -> Option<AllocResult> {
        let dest_queue_id = self.peer_free.try_alloc()?;
        let result = self.alloc_local(binding_id, dest_queue_id);
        Some(result)
    }

    /// Async alloc.  Suspends if the peer free list is exhausted.
    pub fn poll_alloc(
        &self,
        binding_id: VarInt,
        cx: &mut Context,
    ) -> Poll<Option<AllocResult>> {
        match self.peer_free.poll_alloc(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(dest_queue_id)) => {
                Poll::Ready(Some(self.alloc_local(binding_id, dest_queue_id)))
            }
        }
    }

    pub fn close(&self) {
        self.local_free.lock().unwrap().close();
    }

    fn alloc_local(&self, binding_id: VarInt, dest_queue_id: VarInt) -> AllocResult {
        let index = {
            let mut free = self.local_free.lock().unwrap();
            free.pop().expect("ClientFreeList closed during alloc")
        };

        // Grow the page table if this index falls outside current capacity.
        if index >= self.page_table.total_slots() {
            self.page_table.grow_to_fit(index);
        }

        let mut view = self.page_table.sender_view();
        let slot_ref = view.get(index).expect("slot index out of range after grow");

        // CAS the slot from UNALLOCATED to `binding_id`.
        assert!(
            slot_ref.try_allocate(binding_id),
            "slot {index} was not unallocated"
        );

        // Open both receiver halves atomically.
        slot_ref
            .open_receivers()
            .expect("slot halves closed immediately after allocation");

        let slot_ptr = unsafe { NonNull::new_unchecked(slot_ref as *const _ as *mut _) };
        let local_queue_id = VarInt::new(index as u64).expect("slot index exceeds VarInt range");
        let state = self.page_table.state.clone();

        AllocResult {
            stream: StreamReceiver::new(
                slot_ptr,
                local_queue_id,
                OnFree::Client(self.local_free.clone()),
                state.clone(),
            ),
            control: ControlReceiver::new(
                slot_ptr,
                local_queue_id,
                OnFree::Client(self.local_free.clone()),
                state,
            ),
            local_queue_id,
            dest_queue_id,
        }
    }

    /// Build a `ClientDispatch` backed by the same page table.
    pub fn dispatcher(&self) -> ClientDispatch {
        ClientDispatch {
            view: self.page_table.sender_view(),
        }
    }
}

// ── ClientDispatch ────────────────────────────────────────────────────────────

/// Routes inbound packets to allocated client slots.
///
/// `queue_id` is the local slot index (what the peer sent as `dest_queue_id`).
/// `binding_id` is validated against the slot's stored value before pushing.
pub struct ClientDispatch {
    view: super::page_table::SenderView,
}

impl ClientDispatch {
    #[inline]
    pub fn send_stream(
        &mut self,
        queue_id: VarInt,
        binding_id: VarInt,
        entry: intrusive::Entry<msg::Stream>,
    ) -> Result<AutoWake, Error<intrusive::Entry<msg::Stream>>> {
        dispatch_to_slot(&mut self.view, queue_id, binding_id, entry, |slot, e| {
            slot.stream.push(e).map_err(|err| match err {
                super::half::Error::HalfClosed(e) => Error::HalfClosed(e),
                super::half::Error::SenderClosed => Error::SenderClosed,
                super::half::Error::Unallocated(e) => Error::Unallocated(e),
            })
        })
    }

    #[inline]
    pub fn send_control(
        &mut self,
        queue_id: VarInt,
        binding_id: VarInt,
        entry: intrusive::Entry<msg::Control>,
    ) -> Result<AutoWake, Error<intrusive::Entry<msg::Control>>> {
        dispatch_to_slot(&mut self.view, queue_id, binding_id, entry, |slot, e| {
            slot.control.push(e).map_err(|err| match err {
                super::half::Error::HalfClosed(e) => Error::HalfClosed(e),
                super::half::Error::SenderClosed => Error::SenderClosed,
                super::half::Error::Unallocated(e) => Error::Unallocated(e),
            })
        })
    }

    /// Broadcast-close all currently allocated slots.
    ///
    /// Called when the path secret entry is evicted.  Wakes all pending
    /// receivers so they observe the closed state.
    pub fn close(&mut self) {
        self.view.for_each_slot(|slot| {
            let (_sw, _cw) = slot.broadcast_close();
            // AutoWake drops here, waking the stored wakers.
        });
    }
}

// ── Shared dispatch helper ────────────────────────────────────────────────────

#[inline]
fn dispatch_to_slot<T, F>(
    view: &mut super::page_table::SenderView,
    queue_id: VarInt,
    binding_id: VarInt,
    entry: T,
    push: F,
) -> Result<AutoWake, Error<T>>
where
    F: FnOnce(&super::slot::Slot, T) -> Result<AutoWake, Error<T>>,
{
    let index = queue_id.as_u64() as usize;

    let Some(slot) = view.get(index) else {
        return Err(Error::Unallocated(entry));
    };

    // Validate binding_id.
    match slot.binding_id() {
        Some(b) if b == binding_id => {}
        _ => return Err(Error::BindingMismatch(entry)),
    }

    push(slot, entry)
}
