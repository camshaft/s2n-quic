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

/// MSB-encoded binding_id scheme:
///
/// bit 63 = UNALLOCATED (1 = slot is free, 0 = slot is in use)
/// bits 0-61 = binding_id value (VarInt range, max 2^62-1)
///
/// This unifies the old `allocated: AtomicBool` and `binding_id: AtomicU64` into a single
/// atomic word. bind, validate, and free are each a single atomic operation with no TOCTOU.
///
/// A fresh slot starts at UNBOUND (u64::MAX, all bits set including MSB → unallocated).
/// When bound: binding_id alone (MSB clear → allocated).
/// When freed: binding_id | UNALLOCATED_BIT (MSB set, preserving binding for stale detection).
const UNALLOCATED_BIT: u64 = 1 << 63;

/// Sentinel: slot has never been bound. u64::MAX has the UNALLOCATED_BIT set naturally,
/// plus all lower bits set, making it impossible to confuse with any valid binding_id.
const UNBOUND: u64 = u64::MAX;

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

    /// Returns true if this descriptor currently has active receivers (in use).
    ///
    /// MSB clear = allocated. MSB set = free (or UNBOUND). Single condition check.
    #[inline]
    pub fn is_allocated(&self) -> bool {
        self.inner().binding_id.load(Ordering::Acquire) & UNALLOCATED_BIT == 0
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

    /// Atomically bind this descriptor: stores binding_id with MSB clear (= allocated).
    ///
    /// Only succeeds if the slot is currently free (UNALLOCATED_BIT set or UNBOUND)
    /// AND the new binding_id is strictly greater than any prior binding on this slot.
    /// Returns false if the binding_id is stale (retransmit of an old stream).
    ///
    /// # Safety
    ///
    /// * The [`Descriptor`] needs to be in the free list (not currently allocated)
    #[inline]
    pub unsafe fn init_key(&self, binding_id: VarInt) -> bool {
        let inner = self.inner();
        let desired = binding_id.as_u64(); // MSB clear = allocated
        inner
            .binding_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                if current == UNBOUND {
                    return Some(desired);
                }
                // Slot must be free (UNALLOCATED_BIT set) and new binding must exceed prior
                if current & UNALLOCATED_BIT != 0 {
                    let prior_binding = current & !UNALLOCATED_BIT;
                    if binding_id.as_u64() > prior_binding {
                        return Some(desired);
                    }
                }
                None
            })
            .is_ok()
    }

    /// # Safety
    ///
    /// * The [`Descriptor`] must have been bound via `init_key` (ALLOCATED_BIT is set)
    ///
    /// If `remote_queue_id` is `Some`, the value is stored immediately and both queue
    /// halves are marked as already observed (no dispatcher-side store needed).
    #[inline]
    pub unsafe fn into_receiver_pair(self, remote_queue_id: Option<VarInt>) -> (Self, Self) {
        let inner = self.inner();
        debug_assert!(
            inner.binding_id.load(Ordering::Relaxed) & UNALLOCATED_BIT == 0,
            "into_receiver_pair called on unbound descriptor"
        );

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
            .close_receiver(&inner.control, half, || {
                inner.binding_id.fetch_or(UNALLOCATED_BIT, Ordering::Release);
            })
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


    /// Validate the binding_id against the current slot binding.
    ///
    /// Lock-free comparison using the MSB-encoded binding_id word:
    /// - Slot free (UNALLOCATED_BIT set, including UNBOUND) → StaleBinding
    /// - Equal → accept
    /// - Received < current → stale (drop)
    /// - Received > current → future (BUG: client rebound before QueueFree)
    #[inline]
    pub fn validate(&self, received: &VarInt) -> Result<(), ValidationError> {
        let inner = self.inner();
        let current = inner.binding_id.load(Ordering::Acquire);
        if current & UNALLOCATED_BIT != 0 {
            return Err(ValidationError::StaleBinding);
        }
        // MSB is clear, so current IS the binding_id directly
        match current.cmp(&received.as_u64()) {
            std::cmp::Ordering::Equal => Ok(()),
            std::cmp::Ordering::Greater => Err(ValidationError::StaleBinding),
            std::cmp::Ordering::Less => {
                crate::tracing::error!(
                    queue_id = inner.id.as_u64(),
                    current,
                    received = received.as_u64(),
                    "BUG: received binding_id greater than current — client rebound before QueueFree"
                );
                debug_assert!(
                    false,
                    "received binding_id ({}) greater than current ({}) on queue_id={}",
                    received.as_u64(),
                    current,
                    inner.id.as_u64(),
                );
                Err(ValidationError::FutureBinding)
            }
        }
    }

    /// Server-side: validate or atomically bind if the slot is free.
    ///
    /// Uses `fetch_update` to atomically check "is this slot free?" (UNALLOCATED_BIT set)
    /// and set the binding_id (MSB clear = allocated) in a single CAS. No TOCTOU possible.
    #[inline]
    pub fn validate_or_bind(
        &self,
        received: &VarInt,
    ) -> Result<ServerValidation, ValidationError> {
        let inner = self.inner();
        let desired = received.as_u64(); // MSB clear = allocated

        match inner.binding_id.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            if current & UNALLOCATED_BIT != 0 {
                // Slot is free (UNBOUND or previously freed). Check stale.
                if current != UNBOUND {
                    let prior = current & !UNALLOCATED_BIT;
                    if received.as_u64() <= prior {
                        return None; // stale retransmit
                    }
                }
                Some(desired)
            } else {
                // Slot is allocated — reject via None, validate externally
                None
            }
        }) {
            Ok(_) => Ok(ServerValidation::NewBinding),
            Err(current) => self.validate_against(current, received),
        }
    }

    #[inline]
    fn validate_against(&self, current: u64, received: &VarInt) -> Result<ServerValidation, ValidationError> {
        if current & UNALLOCATED_BIT != 0 {
            return Err(ValidationError::StaleBinding);
        }
        // MSB is clear, so current IS the binding_id directly
        match current.cmp(&received.as_u64()) {
            std::cmp::Ordering::Equal => Ok(ServerValidation::Bound),
            std::cmp::Ordering::Greater => Err(ValidationError::StaleBinding),
            std::cmp::Ordering::Less => {
                let inner = self.inner();
                crate::tracing::error!(
                    queue_id = inner.id.as_u64(),
                    current,
                    received = received.as_u64(),
                    "BUG: received binding_id greater than current — client rebound before QueueFree"
                );
                debug_assert!(
                    false,
                    "received binding_id ({}) greater than current ({}) on queue_id={}",
                    received.as_u64(),
                    current,
                    inner.id.as_u64(),
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
    /// MSB-encoded binding_id:
    ///   bit 63 = ALLOCATED (in use)
    ///   bits 0-61 = binding_id value
    ///   u64::MAX = UNBOUND (never used)
    ///
    /// After free, MSB is cleared but binding_id value preserved for stale detection.
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
