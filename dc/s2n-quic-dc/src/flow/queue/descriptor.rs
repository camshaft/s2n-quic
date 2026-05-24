// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{
    free_list::FreeList,
    inner::{Half, Queue},
    probes,
};
use s2n_quic_core::{ensure, varint::VarInt};
use std::{
    marker::PhantomData,
    ptr::NonNull,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
};

/// Sentinel: slot is unbound (no active binding).
const UNBOUND: u64 = u64::MAX;

/// Information about a freed queue slot, emitted when both receivers drop.
#[derive(Debug, Clone, Copy)]
pub struct FreedSlot {
    /// The local queue_id (slot index) that was freed.
    pub queue_id: VarInt,
    /// The binding_id that was active when the slot was freed.
    pub binding_id: VarInt,
}

/// Lock-guarded freed-slot notification channel with an atomic fast-path check.
///
/// Producers push freed slots and set the `has_items` flag. The consumer checks
/// the flag before acquiring the mutex, skipping it entirely when empty (common case).
pub struct FreeNotify {
    has_items: std::sync::atomic::AtomicBool,
    inner: std::sync::Mutex<Vec<FreedSlot>>,
}

impl FreeNotify {
    pub fn new() -> Self {
        Self {
            has_items: std::sync::atomic::AtomicBool::new(false),
            inner: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn push(&self, slot: FreedSlot) {
        let mut guard = self.inner.lock().unwrap();
        guard.push(slot);
        self.has_items
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Drain all freed slots. Returns an empty Vec without acquiring the mutex
    /// when no items have been pushed since the last drain.
    pub fn drain(&self) -> Vec<FreedSlot> {
        if !self
            .has_items
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Vec::new();
        }
        let mut guard = self.inner.lock().unwrap();
        self.has_items
            .store(false, std::sync::atomic::Ordering::Release);
        core::mem::take(&mut *guard)
    }
}

/// Indicates why a queue key validation failed
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidationError {
    /// The received binding_id is older than the current slot binding.
    /// This is a stale packet routed to a recycled slot — drop silently.
    StaleBinding,
    /// The received binding_id is newer than the current slot binding.
    /// This indicates a protocol bug: the client rebound before receiving QueueFree.
    FutureBinding,
}

impl ValidationError {
    /// Returns the reset error code to send to the peer, if any.
    pub fn as_reset_code(self) -> Option<VarInt> {
        use crate::stream::endpoint::error;
        match self {
            Self::StaleBinding => None,
            Self::FutureBinding => Some(error::BINDING_ID_MISMATCH),
        }
    }
}

/// Result of server-side validation
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerValidation {
    /// Key matched an existing binding
    Bound,
    /// Queue was unbound; key has been set (new binding created)
    NewBinding,
}

/// A pointer to a single descriptor in a group
///
/// Fundamentally, this is similar to something like `Arc<DescriptorInner>`. However,
/// unlike [`Arc`] which frees back to the global allocator, a Descriptor deallocates into
/// the backing [`FreeList`].
pub(super) struct Descriptor<S, C> {
    ptr: NonNull<DescriptorInner<S, C>>,
    phantom: PhantomData<DescriptorInner<S, C>>,
}

impl<S: 'static, C: 'static> Descriptor<S, C> {
    #[inline]
    pub(super) fn new(ptr: NonNull<DescriptorInner<S, C>>) -> Self {
        Self {
            ptr,
            phantom: PhantomData,
        }
    }

    /// # Safety
    ///
    /// The caller needs to guarantee the [`Descriptor`] is still allocated. Additionally,
    /// the [`Self::drop_sender`] method should be used when the cloned descriptor is
    /// no longer needed.
    #[inline]
    pub unsafe fn clone_for_sender(&self) -> Descriptor<S, C> {
        self.inner().senders.fetch_add(1, Ordering::Relaxed);
        Descriptor::new(self.ptr)
    }

    /// # Safety
    ///
    /// This should only be called once the caller can guarantee the descriptor is no longer
    /// used.
    #[inline]
    pub unsafe fn drop_in_place(&self) {
        core::ptr::drop_in_place(self.ptr.as_ptr());
    }

    #[cfg(debug_assertions)]
    pub(super) fn as_usize(&self) -> usize {
        self.ptr.as_ptr().addr()
    }

    /// Returns the slot index of this descriptor. This is the queue_id.
    #[inline]
    pub(super) fn slot_index(&self) -> usize {
        self.inner().id.as_u64() as usize
    }

    /// Returns true if this descriptor currently has an active binding.
    #[inline]
    pub fn is_allocated(&self) -> bool {
        self.inner().binding_id.load(Ordering::Acquire) != UNBOUND
    }

    /// Returns the queue_id for this descriptor. Queue IDs are just slot indices.
    #[inline]
    pub unsafe fn queue_id(&self) -> VarInt {
        self.inner().id
    }

    /// Returns the peer's queue ID, or `None` if not yet observed.
    #[inline]
    pub unsafe fn remote_queue_id(&self) -> Option<VarInt> {
        let v = self.inner().remote_queue_id.load(Ordering::Relaxed);
        VarInt::new(v).ok()
    }

    /// Stores the peer's queue ID with a relaxed store.
    ///
    /// Should only be called once per flow — guarded by the `HAS_OBSERVED` flag in the queue.
    #[inline]
    pub unsafe fn set_remote_queue_id(&self, id: VarInt) {
        self.inner()
            .remote_queue_id
            .store(id.as_u64(), Ordering::Relaxed);
    }

    #[inline]
    pub unsafe fn stream_queue(&self) -> &Queue<S> {
        &self.inner().stream
    }

    #[inline]
    pub unsafe fn control_queue(&self) -> &Queue<C> {
        &self.inner().control
    }

    #[inline]
    fn inner(&self) -> &DescriptorInner<S, C> {
        unsafe { self.ptr.as_ref() }
    }

    /// Set the binding_id for this descriptor.
    ///
    /// # Safety
    ///
    /// * The [`Descriptor`] needs to be marked as free of receivers
    #[inline]
    pub unsafe fn init_key(&self, binding_id: VarInt) {
        let inner = self.inner();
        debug_assert_eq!(inner.binding_id.load(Ordering::Relaxed), UNBOUND);
        inner.binding_id.store(binding_id.as_u64(), Ordering::Release);
    }

    /// # Safety
    ///
    /// * The [`Descriptor`] needs to be marked as free of receivers
    ///
    /// If `remote_queue_id` is `Some`, the value is stored immediately and both queue
    /// halves are marked as already observed (no dispatcher-side store needed).
    #[inline]
    pub unsafe fn into_receiver_pair(self, remote_queue_id: Option<VarInt>) -> (Self, Self) {
        let inner = self.inner();

        let has_remote_queue_id = remote_queue_id.is_some();
        if let Some(id) = remote_queue_id {
            inner.remote_queue_id.store(id.as_u64(), Ordering::Relaxed);
        } else {
            inner.remote_queue_id.store(UNBOUND, Ordering::Relaxed);
        }

        // open the queues back up for receiving
        inner
            .stream
            .open_receivers(&inner.control, has_remote_queue_id)
            .unwrap();

        probes::on_receiver_open(inner.id);

        let other = Self {
            ptr: self.ptr,
            phantom: PhantomData,
        };

        (self, other)
    }

    /// # Safety
    ///
    /// This method can be used to drop the Descriptor, but shouldn't be called after the last sender Descriptor
    /// is released. That implies only calling it once on a given Descriptor handle obtained from [`Self::clone_for_sender`].
    #[inline]
    pub unsafe fn drop_sender(&self) {
        let inner = self.inner();
        let desc_ref = inner.senders.fetch_sub(1, Ordering::Release);
        debug_assert_ne!(desc_ref, 0, "reference count underflow");

        // based on the implementation in:
        // https://github.com/rust-lang/rust/blob/28b83ee59698ae069f5355b8e03f976406f410f5/library/alloc/src/sync.rs#L2551
        if desc_ref != 1 {
            probes::on_sender_drop(inner.id);
            return;
        }

        core::sync::atomic::fence(Ordering::Acquire);

        // close both of the queues so the receivers are notified
        // Wakers fire inline here. This only happens when the last sender reference
        // is dropped, which occurs during path secret entry cleanup (no activity on
        // this path) — so inline wakes are acceptable.
        inner.control.close();
        inner.stream.close();

        probes::on_sender_close(inner.id);
    }

    /// # Safety
    ///
    /// This method can be used to drop the Descriptor, but shouldn't be called after the last receiver Descriptor
    /// is released. That implies only calling it once on a given Descriptor handle obtained from [`Self::into_receiver_pair`].
    #[inline]
    pub unsafe fn drop_receiver(&self, half: Half) {
        let inner = self.inner();
        probes::on_receiver_drop(inner.id, half);

        ensure!(inner
            .stream
            .close_receiver(&inner.control, half, || {})
            .is_continue());

        probes::on_receiver_free(inner.id, half);

        // The free list impl handles notification (server pushes QueueFree info,
        // client just recycles). binding_id is still set so the free list can read it.
        let storage = inner.free_list.free(Descriptor {
            ptr: self.ptr,
            phantom: PhantomData,
        });
        drop(storage);
    }

    /// Read the binding_id and queue_id, then clear binding_id to UNBOUND.
    ///
    /// Returns `Some(FreedSlot)` if the slot was bound, `None` if already unbound.
    /// Called by the free list impl to capture state for QueueFree before recycling.
    ///
    /// # Safety
    ///
    /// Must only be called once during the free path, after both receivers have closed.
    #[inline]
    pub unsafe fn take_freed_slot(&self) -> Option<FreedSlot> {
        let inner = self.inner();
        let binding_id_raw = inner.binding_id.swap(UNBOUND, Ordering::AcqRel);
        VarInt::new(binding_id_raw).ok().map(|binding_id| FreedSlot {
            queue_id: inner.id,
            binding_id,
        })
    }

    /// Clear binding_id to UNBOUND without reading it.
    ///
    /// Used by client-side free lists that don't need the freed slot info.
    ///
    /// # Safety
    ///
    /// Must only be called once during the free path, after both receivers have closed.
    #[inline]
    pub unsafe fn clear_binding(&self) {
        self.inner().binding_id.store(UNBOUND, Ordering::Release);
    }

    /// Validate the binding_id against the current slot binding.
    ///
    /// Lock-free ordered comparison:
    /// - Equal → accept
    /// - Received < current → stale (drop)
    /// - Received > current → future (BUG)
    /// - Slot unbound → StaleBinding (treated as unallocated by caller)
    #[inline]
    pub fn validate(&self, received: &VarInt) -> Result<(), ValidationError> {
        let current = self.inner().binding_id.load(Ordering::Acquire);
        if current == UNBOUND {
            return Err(ValidationError::StaleBinding);
        }
        match current.cmp(&received.as_u64()) {
            std::cmp::Ordering::Equal => Ok(()),
            std::cmp::Ordering::Greater => Err(ValidationError::StaleBinding),
            std::cmp::Ordering::Less => {
                crate::tracing::error!(
                    current,
                    received = received.as_u64(),
                    "BUG: received binding_id greater than current — client rebound before QueueFree"
                );
                debug_assert!(
                    false,
                    "received binding_id greater than current — client rebound before QueueFree"
                );
                Err(ValidationError::FutureBinding)
            }
        }
    }

    /// Server-side: validate or atomically bind if unbound.
    ///
    /// Uses CAS to ensure only one caller succeeds at binding an unbound slot.
    #[inline]
    pub fn validate_or_bind(
        &self,
        received: &VarInt,
    ) -> Result<ServerValidation, ValidationError> {
        let inner = self.inner();
        let current = inner.binding_id.load(Ordering::Acquire);

        if current == UNBOUND {
            // Attempt to atomically bind
            match inner.binding_id.compare_exchange(
                UNBOUND,
                received.as_u64(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(ServerValidation::NewBinding),
                Err(actual) => {
                    // Someone else bound it first — validate against their binding
                    return self.validate_against(actual, received);
                }
            }
        }

        self.validate_against(current, received)
    }

    #[inline]
    fn validate_against(&self, current: u64, received: &VarInt) -> Result<ServerValidation, ValidationError> {
        match current.cmp(&received.as_u64()) {
            std::cmp::Ordering::Equal => Ok(ServerValidation::Bound),
            std::cmp::Ordering::Greater => Err(ValidationError::StaleBinding),
            std::cmp::Ordering::Less => {
                crate::tracing::error!(
                    current,
                    received = received.as_u64(),
                    "BUG: received binding_id greater than current — client rebound before QueueFree"
                );
                debug_assert!(
                    false,
                    "received binding_id greater than current — client rebound before QueueFree"
                );
                Err(ValidationError::FutureBinding)
            }
        }
    }
}

unsafe impl<S: Send, C: Send> Send for Descriptor<S, C> {}
unsafe impl<S: Sync, C: Sync> Sync for Descriptor<S, C> {}

pub(super) struct DescriptorInner<S, C> {
    /// The slot index. This IS the queue_id — it never changes after initialization.
    id: VarInt,
    /// The current binding_id. `UNBOUND` (u64::MAX) means the slot is free/unbound.
    /// Set atomically during init_key, cleared during drop_receiver.
    binding_id: AtomicU64,
    /// The peer's queue ID, written once by the dispatcher on first observation.
    /// Initialized to `u64::MAX` (unknown) and set via a relaxed store.
    remote_queue_id: AtomicU64,
    stream: Queue<S>,
    control: Queue<C>,
    /// A reference back to the free list.
    /// Server impl pushes QueueFree notifications; client impl just recycles.
    free_list: Arc<dyn FreeList<S, C>>,
    senders: AtomicUsize,
}

impl<S, C> DescriptorInner<S, C> {
    pub(super) fn new(
        index: usize,
        free_list: Arc<dyn FreeList<S, C>>,
    ) -> Self {
        let stream = Queue::new(Half::Stream);
        let control = Queue::new(Half::Control);
        Self {
            id: VarInt::new(index as u64).unwrap(),
            binding_id: AtomicU64::new(UNBOUND),
            remote_queue_id: AtomicU64::new(UNBOUND),
            stream,
            control,
            senders: AtomicUsize::new(0),
            free_list,
        }
    }
}
