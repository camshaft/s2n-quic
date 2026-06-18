// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{endpoint::msg, intrusive};
use bytes::BytesMut;
use core::task::{Context, RawWaker, RawWakerVTable, Waker};
use s2n_quic_core::varint::VarInt;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

// ── Live-receiver registry (sim-only invariant tracking) ─────────────────────
//
// The single-slot binding gate (`validate_binding_state`) is self-consistent: it returns `Ok` only
// when the incoming binding equals the slot's stored binding AND a receiver is attached. So it can
// never, on its own, report that it dropped data for a binding whose receiver is still live — at
// the slot level that is a contradiction.
//
// The production wedge (handoff doc) is exactly that contradiction observed end-to-end: a binding
// is simultaneously an Open reader and the target of stale_binding drops. The only way that can
// arise is ABOVE the slot — a slot / queue_id reused or recycled while a `StreamReceiver` still
// holds it (the receiver reads `slot.binding_id()` live, so a rebind under it goes unnoticed).
//
// This registry makes that detectable. Each live `StreamReceiver` records the `(slot, binding)` it
// was created for. At the `push_stream` drop site, if we are about to drop stream data for a
// `(slot, incoming)` pair that still has a live receiver registered, the "acked-but-dropped a live
// binding" invariant is violated and we panic with full context. Bach is single-threaded, so the
// plain `RefCell` is race-free across the simulated endpoints sharing this thread.

use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    /// slot pointer address -> (count of live `StreamReceiver`s, the binding they were created for).
    ///
    /// Keyed by the slot's raw pointer (globally unique within the pinned page tables) so client
    /// and server queue-id spaces — which both start at 0 and share this thread — never collide.
    /// We track the binding the live receiver(s) belong to purely for diagnostics: the invariant
    /// is "a slot with a live StreamReceiver must not have its stream data dropped at dispatch",
    /// independent of which binding the dropped frame named.
    static LIVE_STREAM_RECEIVERS: RefCell<HashMap<usize, (usize, u64)>> =
        RefCell::new(HashMap::new());
}

/// Register a live `StreamReceiver` for `slot_addr` (created at `binding`). Called from
/// `StreamReceiver::new`.
pub(crate) fn register_stream_receiver(slot_addr: usize, binding: u64) {
    LIVE_STREAM_RECEIVERS.with(|r| {
        let mut map = r.borrow_mut();
        let slot = map.entry(slot_addr).or_insert((0, binding));
        slot.0 += 1;
        slot.1 = binding;
    });
}

/// Unregister a `StreamReceiver` on drop.
pub(crate) fn unregister_stream_receiver(slot_addr: usize, _binding: u64) {
    LIVE_STREAM_RECEIVERS.with(|r| {
        let mut map = r.borrow_mut();
        if let Some(slot) = map.get_mut(&slot_addr) {
            slot.0 -= 1;
            if slot.0 == 0 {
                map.remove(&slot_addr);
            }
        }
    });
}

/// If `slot_addr` currently has a live `StreamReceiver`, returns the binding it was created for.
/// Used at the dispatch drop site to assert we never drop stream data for a slot whose reader is
/// still alive.
pub(crate) fn live_receiver_binding(slot_addr: usize) -> Option<u64> {
    LIVE_STREAM_RECEIVERS.with(|r| r.borrow().get(&slot_addr).map(|(_, binding)| *binding))
}

/// Clear the registry. For tests that deliberately leak a registration (e.g. `should_panic`
/// invariant self-tests) so the process-wide thread-local doesn't pollute later tests on the
/// same thread.
pub(crate) fn clear_live_stream_receivers() {
    LIVE_STREAM_RECEIVERS.with(|r| r.borrow_mut().clear());
}

pub fn make_stream_entry() -> intrusive::Entry<msg::Stream> {
    intrusive::Entry::new(msg::Stream::Data {
        offset: VarInt::ZERO,
        peer_max_offset: VarInt::ZERO,
        fin: false,
        blocked: false,
        payload: BytesMut::from(&[42][..]),
    })
}

pub fn make_control_entry() -> intrusive::Entry<msg::Control> {
    intrusive::Entry::new(msg::Control::Frames {
        payload: BytesMut::from(&[0][..]),
    })
}

pub fn test_waker() -> (Waker, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let data = Arc::into_raw(count.clone()) as *const ();
    let raw = RawWaker::new(data, &VTABLE);
    let waker = unsafe { Waker::from_raw(raw) };
    (waker, count)
}

pub fn test_context<'a>(waker: &'a Waker) -> Context<'a> {
    Context::from_waker(waker)
}

const VTABLE: RawWakerVTable = RawWakerVTable::new(clone_fn, wake_fn, wake_by_ref_fn, drop_fn);

unsafe fn clone_fn(data: *const ()) -> RawWaker {
    Arc::increment_strong_count(data as *const AtomicUsize);
    RawWaker::new(data, &VTABLE)
}

unsafe fn wake_fn(data: *const ()) {
    let arc = Arc::from_raw(data as *const AtomicUsize);
    arc.fetch_add(1, Ordering::SeqCst);
}

unsafe fn wake_by_ref_fn(data: *const ()) {
    let arc = unsafe { &*(data as *const AtomicUsize) };
    arc.fetch_add(1, Ordering::SeqCst);
}

unsafe fn drop_fn(data: *const ()) {
    Arc::decrement_strong_count(data as *const AtomicUsize);
}
