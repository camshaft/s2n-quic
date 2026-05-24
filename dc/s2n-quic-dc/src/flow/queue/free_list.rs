// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{
    descriptor::Descriptor,
    handle::{Control, Stream},
    pool::Region,
    probes,
};
use s2n_quic_core::{interval_set::IntervalSet, varint::VarInt};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

/// Callback which releases a descriptor back into the free list.
///
/// Two implementations exist:
/// - `ClientFreeList`: silently recycles the descriptor
/// - `ServerFreeList`: records queue_id in pending IntervalSet, then recycles
///
/// The descriptor arrives with binding_id still set (not yet UNBOUND). The impl
/// is responsible for clearing it after reading any needed state.
pub(super) trait FreeList<S, C>: 'static + Send + Sync {
    /// Frees a descriptor back into the free list.
    ///
    /// Once the free list has been closed and all descriptors returned, the `free` function
    /// should return an object that can be dropped to release all of the memory associated
    /// with the descriptor pool. This works around any issues around the "Stacked Borrows"
    /// model by deferring freeing memory borrowed by `self`.
    fn free(&self, descriptor: Descriptor<S, C>) -> Option<Box<dyn 'static + Send>>;
}

/// Client-side free list: silently recycles descriptors without peer notification.
pub(super) struct ClientFreeList<S: 'static, C: 'static>(pub Arc<FreeVec<S, C>>);

impl<S, C> FreeList<S, C> for ClientFreeList<S, C>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
{
    #[inline]
    fn free(&self, descriptor: Descriptor<S, C>) -> Option<Box<dyn 'static + Send>> {
        self.0.recycle(descriptor)
    }
}

/// Server-side free list: records freed queue_ids for QueueFree emission, then recycles.
pub(super) struct ServerFreeList<S: 'static, C: 'static>(pub Arc<FreeVec<S, C>>);

impl<S, C> FreeList<S, C> for ServerFreeList<S, C>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
{
    #[inline]
    fn free(&self, descriptor: Descriptor<S, C>) -> Option<Box<dyn 'static + Send>> {
        let queue_id = unsafe { descriptor.queue_id() };
        self.0.recycle_with_notify(descriptor, queue_id)
    }
}

/// A free list of unfilled descriptors with O(1) indexed allocation.
///
/// Descriptors are stored in a direct-indexed Vec (by slot index) for O(1) `alloc_at`.
/// A separate VecDeque of free indices provides LIFO ordering for the unindexed `alloc`
/// path, preferring recently-freed descriptors for cache locality.
pub(super) struct FreeVec<S: 'static, C: 'static> {
    inner: Mutex<FreeInner<S, C>>,
    /// Monotonic counter for free_request_ids stamped on QueueFree batches.
    next_free_request_id: std::sync::atomic::AtomicU64,
}

impl<S: 'static, C: 'static> FreeVec<S, C> {
    #[inline]
    pub fn new(initial_cap: usize) -> (Arc<Self>, Arc<Memory<S, C>>) {
        let slots = Vec::with_capacity(initial_cap);
        let free_indices = VecDeque::with_capacity(initial_cap);
        let regions = Vec::with_capacity(1);
        let inner = FreeInner {
            slots,
            free_indices,
            regions,
            total: 0,
            free_count: 0,
            open: true,
            pending_freed: IntervalSet::new(),
            #[cfg(debug_assertions)]
            active: Default::default(),
        };
        let inner = Mutex::new(inner);
        let free = Arc::new(Self {
            inner,
            next_free_request_id: std::sync::atomic::AtomicU64::new(0),
        });
        let memory = Arc::new(Memory(free.clone()));
        (free, memory)
    }

    /// Drain pending freed queue_ids as an IntervalSet for QueueFree emission.
    ///
    /// Returns (free_request_id, queue_ids) or None if no slots have been freed.
    /// The free_request_id is a monotonic stamp for dedup on the receiving side.
    pub fn drain_pending_freed(&self) -> Option<(VarInt, IntervalSet<VarInt>)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.pending_freed.is_empty() {
            return None;
        }
        let mut pending = IntervalSet::new();
        core::mem::swap(&mut pending, &mut inner.pending_freed);
        drop(inner);

        let id = self
            .next_free_request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let request_id = VarInt::new(id).unwrap_or(VarInt::MAX);
        Some((request_id, pending))
    }

    /// Recycle a descriptor back into the pool (client path — no notification).
    fn recycle(&self, descriptor: Descriptor<S, C>) -> Option<Box<dyn 'static + Send>>
    where
        S: Send,
        C: Send,
    {
        let mut inner = self.inner.lock().unwrap();
        Self::recycle_inner(&mut inner, descriptor)
    }

    /// Recycle a descriptor and record its queue_id for pending QueueFree (server path).
    fn recycle_with_notify(
        &self,
        descriptor: Descriptor<S, C>,
        queue_id: VarInt,
    ) -> Option<Box<dyn 'static + Send>>
    where
        S: Send,
        C: Send,
    {
        let mut inner = self.inner.lock().unwrap();
        let _ = inner.pending_freed.insert_value(queue_id);
        Self::recycle_inner(&mut inner, descriptor)
    }

    fn recycle_inner(
        inner: &mut FreeInner<S, C>,
        descriptor: Descriptor<S, C>,
    ) -> Option<Box<dyn 'static + Send>>
    where
        S: Send,
        C: Send,
    {
        #[cfg(debug_assertions)]
        assert!(
            inner.active.remove(&descriptor.as_usize()),
            "{} not in {:?}",
            descriptor.as_usize(),
            inner.active
        );

        let idx = descriptor.slot_index();
        debug_assert!(inner.slots[idx].is_none());
        inner.slots[idx] = Some(descriptor);
        inner.free_indices.push_back(idx);
        inner.free_count += 1;

        if inner.open {
            return None;
        }
        inner
            .try_free()
            .map(|to_free| Box::new(to_free) as Box<dyn 'static + Send>)
    }

    #[inline]
    pub fn alloc(
        &self,
        key: crate::flow::Handle,
        remote_queue_id: Option<VarInt>,
    ) -> Result<(Control<S, C>, Stream<S, C>), crate::flow::Handle> {
        let mut inner = self.inner.lock().unwrap();

        // Skip indices that were already taken by alloc_at (lazy cleanup).
        let descriptor = loop {
            let Some(slot_index) = inner.free_indices.pop_front() else {
                return Err(key);
            };
            if let Some(desc) = inner.slots[slot_index].take() {
                inner.free_count -= 1;
                break desc;
            }
        };

        #[cfg(debug_assertions)]
        assert!(
            inner.active.insert(descriptor.as_usize()),
            "{} already in {:?}",
            descriptor.as_usize(),
            inner.active
        );

        drop(inner);

        unsafe {
            if !descriptor.init_key(key.binding_id) {
                // Stale binding_id — put the descriptor back.
                self.return_to_free_list(descriptor);
                return Err(key);
            }
            let (control, stream) = descriptor.into_receiver_pair(remote_queue_id);
            Ok((Control::new(control), Stream::new(stream)))
        }
    }

    /// Allocate a specific slot by index in O(1), removing it from the free list.
    ///
    /// Returns `Err(key)` if the slot is not in the free list (already allocated or
    /// not grown yet), or if the binding_id is stale (retransmit of a completed stream).
    ///
    /// The corresponding entry in `free_indices` is left as a stale reference; `alloc` skips
    /// stale entries lazily when it encounters them.
    #[inline]
    pub fn alloc_at(
        &self,
        slot_index: usize,
        key: crate::flow::Handle,
        remote_queue_id: Option<VarInt>,
    ) -> Result<(Control<S, C>, Stream<S, C>), crate::flow::Handle> {
        let mut inner = self.inner.lock().unwrap();

        if slot_index >= inner.slots.len() {
            return Err(key);
        }

        let Some(descriptor) = inner.slots[slot_index].take() else {
            return Err(key);
        };
        inner.free_count -= 1;

        #[cfg(debug_assertions)]
        assert!(
            inner.active.insert(descriptor.as_usize()),
            "{} already in {:?}",
            descriptor.as_usize(),
            inner.active
        );

        drop(inner);

        unsafe {
            if !descriptor.init_key(key.binding_id) {
                // Stale binding_id — put the descriptor back.
                self.return_to_free_list(descriptor);
                return Err(key);
            }
            let (control, stream) = descriptor.into_receiver_pair(remote_queue_id);
            Ok((Control::new(control), Stream::new(stream)))
        }
    }

    fn return_to_free_list(&self, descriptor: Descriptor<S, C>) {
        let mut inner = self.inner.lock().unwrap();
        #[cfg(debug_assertions)]
        {
            inner.active.remove(&descriptor.as_usize());
        }
        let idx = descriptor.slot_index();
        inner.slots[idx] = Some(descriptor);
        inner.free_indices.push_back(idx);
        inner.free_count += 1;
    }

    #[inline]
    pub fn record_region(
        &self,
        region: Region<S, C>,
        descriptors: Vec<Descriptor<S, C>>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        inner.regions.push(region);
        let prev = inner.total;
        let next = prev + descriptors.len();
        inner.total = next;

        // Grow the slots vec to cover the new indices and insert each descriptor.
        let count = descriptors.len();
        inner.slots.resize_with(next, || None);
        for descriptor in descriptors {
            let idx = descriptor.slot_index();
            debug_assert!(inner.slots[idx].is_none());
            inner.slots[idx] = Some(descriptor);
            inner.free_indices.push_back(idx);
        }
        inner.free_count += count;

        drop(inner);
        probes::on_grow(prev, next);
    }

    #[inline]
    fn try_free(&self) -> Option<FreeInner<S, C>> {
        let mut inner = self.inner.lock().unwrap();
        inner.open = false;
        inner.try_free()
    }
}

/// A memory reference to the free list
///
/// Once dropped, the pool and all associated descriptors will be
/// freed after the last handle is dropped.
pub(super) struct Memory<S: 'static, C: 'static>(Arc<FreeVec<S, C>>);

impl<S: 'static, C: 'static> Drop for Memory<S, C> {
    #[inline]
    fn drop(&mut self) {
        drop(self.0.try_free());
    }
}

struct FreeInner<S: 'static, C: 'static> {
    /// Direct-indexed storage: slot i holds `Some(descriptor)` when free, `None` when allocated.
    slots: Vec<Option<Descriptor<S, C>>>,
    /// LIFO ordering of free slot indices for `alloc` (most recently freed at back).
    /// May contain stale entries for slots already taken by `alloc_at`.
    free_indices: VecDeque<usize>,
    regions: Vec<Region<S, C>>,
    total: usize,
    /// Number of descriptors currently in the free list (slots that are Some).
    free_count: usize,
    open: bool,
    /// Server-side: queue_ids pending QueueFree notification to the peer.
    /// Stored as ranges for efficient encoding (ACK-style compression).
    pending_freed: IntervalSet<VarInt>,
    #[cfg(debug_assertions)]
    active: std::collections::BTreeSet<usize>,
}

impl<S: 'static, C: 'static> FreeInner<S, C> {
    #[inline(never)] // this is rarely called
    fn try_free(&mut self) -> Option<Self> {
        #[cfg(debug_assertions)]
        assert_eq!(self.total - self.free_count, self.active.len());

        if self.free_count < self.total {
            probes::on_draining(self.total, self.total - self.free_count);
            return None;
        }

        // move all of the allocations out of itself, since this is self-referential
        Some(core::mem::replace(
            self,
            FreeInner {
                slots: Vec::new(),
                free_indices: VecDeque::new(),
                regions: Vec::new(),
                total: 0,
                free_count: 0,
                open: false,
                pending_freed: IntervalSet::new(),
                #[cfg(debug_assertions)]
                active: Default::default(),
            },
        ))
    }
}

impl<S: 'static, C: 'static> Drop for FreeInner<S, C> {
    #[inline]
    fn drop(&mut self) {
        if self.free_count == 0 {
            return;
        }

        #[cfg(debug_assertions)]
        assert!(self.active.is_empty());

        probes::on_drained(self.total);

        for slot in self.slots.drain(..) {
            if let Some(descriptor) = slot {
                unsafe {
                    // SAFETY: the free list is closed and there are no outstanding descriptors
                    descriptor.drop_in_place();
                }
            }
        }
    }
}
