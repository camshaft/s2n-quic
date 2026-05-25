// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Server-side queue dispatch.
//!
//! The server does not allocate local queue slots; the client chooses the
//! slot indices and sends them in the `dest_queue_id` field.  The server's
//! job is:
//!
//! 1. **Bind-and-send** (`bind_and_send_stream`): on the first packet for a
//!    stream, CAS the slot from an unbound state into the per-stream
//!    `binding_id`.  If the CAS succeeds, open the receiver halves and return
//!    the new `StreamReceiver` / `ControlReceiver` for the handshake path.
//!
//! 2. **Dispatch** (`send_stream`, `send_control`): on subsequent packets,
//!    validate `binding_id` and push the entry.
//!
//! ## Slot lifecycle (server side)
//!
//! ```text
//! client creates stream    →  slot allocated, binding_id = session_binding_id
//! first server packet      →  bind_and_send_stream: CAS binding_id → opened
//! data packets             →  send_stream / send_control
//! stream complete          →  ControlReceiver / StreamReceiver dropped
//!                          →  freed_sender.record(queue_id) → QueueFree to client
//! ```

use super::{
    freed::FreedSender,
    half::AutoWake,
    handle::{ControlReceiver, OnFree, StreamReceiver},
    page_table::PageTable,
    slot::{Slot, UNALLOCATED_BIT},
    Error,
};
use crate::{endpoint::msg, intrusive};
use s2n_quic_core::varint::VarInt;
use std::ptr::NonNull;

// ── BindResult ────────────────────────────────────────────────────────────────

/// Outcome of `ServerDispatch::bind_and_send_stream`.
pub enum BindResult {
    /// The slot already had a matching binding — packet pushed.
    Bound(AutoWake),
    /// A new binding was created.  The caller must hand the receivers to the
    /// stream handshake task.
    NewBinding {
        waker: AutoWake,
        stream: StreamReceiver,
        control: ControlReceiver,
    },
}

// ── ServerDispatch ────────────────────────────────────────────────────────────

pub struct ServerDispatch {
    page_table: PageTable,
    freed: FreedSender,
}

impl ServerDispatch {
    pub fn new(freed: FreedSender) -> Self {
        Self {
            page_table: PageTable::new(),
            freed,
        }
    }

    /// Attempt to bind a slot and push the first stream entry.
    ///
    /// `queue_id` — the slot index chosen by the client.
    /// `binding_id` — the per-stream binding credential (client-chosen).
    ///
    /// If the slot at `queue_id` is currently unallocated the binding will be
    /// established; if it is already bound to `binding_id` the entry is simply
    /// pushed.  Any other state is rejected.
    pub fn bind_and_send_stream(
        &mut self,
        queue_id: VarInt,
        binding_id: VarInt,
        entry: intrusive::Entry<msg::Stream>,
    ) -> Result<BindResult, Error<intrusive::Entry<msg::Stream>>> {
        let index = queue_id.as_u64() as usize;

        // Grow the page table on demand — the client controls queue_id space.
        if index >= self.page_table.total_slots() {
            self.page_table.grow_to_fit(index);
        }

        let mut view = self.page_table.sender_view();
        let Some(slot) = view.get(index) else {
            return Err(Error::Unallocated(entry));
        };

        // Probe the current binding_id.
        let raw = slot.binding_id.load(core::sync::atomic::Ordering::Acquire);
        let unallocated = raw & UNALLOCATED_BIT != 0;

        if unallocated {
            // Attempt to claim this slot with a new binding.
            return self.try_new_binding(slot, queue_id, binding_id, entry);
        }

        // Slot is already allocated — validate binding.
        match VarInt::new(raw).ok() {
            Some(b) if b == binding_id => {
                // Existing binding matches: hot path.
                let waker = slot.stream.push(entry).map_err(|err| match err {
                    super::half::Error::HalfClosed(e) => Error::HalfClosed(e),
                    super::half::Error::SenderClosed => Error::SenderClosed,
                    super::half::Error::Unallocated(e) => Error::Unallocated(e),
                })?;
                Ok(BindResult::Bound(waker))
            }
            _ => Err(Error::BindingMismatch(entry)),
        }
    }

    /// Push to an already-bound stream slot.
    #[inline]
    pub fn send_stream(
        &mut self,
        queue_id: VarInt,
        binding_id: VarInt,
        entry: intrusive::Entry<msg::Stream>,
    ) -> Result<AutoWake, Error<intrusive::Entry<msg::Stream>>> {
        let index = queue_id.as_u64() as usize;
        let mut view = self.page_table.sender_view();

        let Some(slot) = view.get(index) else {
            return Err(Error::Unallocated(entry));
        };

        validate_binding(slot, binding_id, entry, |s, e| {
            s.stream.push(e).map_err(super::half::Error::into)
        })
    }

    /// Push to an already-bound control slot.
    #[inline]
    pub fn send_control(
        &mut self,
        queue_id: VarInt,
        binding_id: VarInt,
        entry: intrusive::Entry<msg::Control>,
    ) -> Result<AutoWake, Error<intrusive::Entry<msg::Control>>> {
        let index = queue_id.as_u64() as usize;
        let mut view = self.page_table.sender_view();

        let Some(slot) = view.get(index) else {
            return Err(Error::Unallocated(entry));
        };

        validate_binding(slot, binding_id, entry, |s, e| {
            s.control.push(e).map_err(super::half::Error::into)
        })
    }

    /// Broadcast-close all slots — called when the path secret entry is evicted.
    pub fn close(&mut self) {
        self.page_table.sender_view().for_each_slot(|slot| {
            let _wakers = slot.broadcast_close();
            // AutoWake drops here, waking stored wakers.
        });
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn try_new_binding(
        &self,
        slot: &Slot,
        queue_id: VarInt,
        binding_id: VarInt,
        entry: intrusive::Entry<msg::Stream>,
    ) -> Result<BindResult, Error<intrusive::Entry<msg::Stream>>> {
        // CAS unallocated → binding_id.
        if !slot.try_allocate(binding_id) {
            // Lost the race — another thread beat us.  Retry at the caller by
            // returning BindingMismatch so the dispatch layer re-dispatches.
            return Err(Error::BindingMismatch(entry));
        }

        // We won the CAS.  Open both receiver halves.
        if slot.open_receivers().is_err() {
            // Sender was closed while we were binding.
            slot.mark_unallocated();
            return Err(Error::SenderClosed);
        }

        let slot_ptr =
            unsafe { NonNull::new_unchecked(slot as *const Slot as *mut Slot) };
        let state = self.page_table.state.clone();

        let waker = slot.stream.push(entry).map_err(|err| {
            // Roll back — drop the just-opened receivers by closing them.
            let _ = super::half::close_receiver(&slot.stream, &slot.control, true, || {
                slot.mark_unallocated();
            });
            match err {
                super::half::Error::HalfClosed(e) => Error::HalfClosed(e),
                super::half::Error::SenderClosed => Error::SenderClosed,
                super::half::Error::Unallocated(e) => Error::Unallocated(e),
            }
        })?;

        let stream = StreamReceiver::new(
            slot_ptr,
            queue_id,
            OnFree::Server(self.freed.clone()),
            state.clone(),
        );
        let control = ControlReceiver::new(
            slot_ptr,
            queue_id,
            OnFree::Server(self.freed.clone()),
            state,
        );

        Ok(BindResult::NewBinding {
            waker,
            stream,
            control,
        })
    }
}

// ── Binding validation helper ─────────────────────────────────────────────────

#[inline]
fn validate_binding<T, F>(
    slot: &Slot,
    binding_id: VarInt,
    entry: T,
    push: F,
) -> Result<AutoWake, Error<T>>
where
    F: FnOnce(&Slot, T) -> Result<AutoWake, Error<T>>,
{
    match slot.binding_id() {
        Some(b) if b == binding_id => push(slot, entry),
        None => Err(Error::Unallocated(entry)),
        _ => Err(Error::BindingMismatch(entry)),
    }
}
