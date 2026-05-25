// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
use super::{
    descriptor::{Descriptor, DescriptorInner},
    free_list::{self, ClientFreeList, FreeVec, ServerFreeList},
    handle::{Control, Sender, Stream},
    sender::{self, Senders},
};
use crate::{counter, tracing::*};
use s2n_quic_core::varint::VarInt;
use std::{alloc::Layout, marker::PhantomData, ptr::NonNull, sync::Arc};

pub struct Pool<
    S: 'static + Send,
    C: 'static + Send,
    const INITIAL_PAGE_SIZE: usize,
> {
    pub(super) senders: Arc<sender::State<S, C>>,
    free: Arc<FreeVec<S, C>>,
    /// Holds the backing memory allocated as long as there's at least one reference
    memory_handle: Arc<free_list::Memory<S, C>>,
    epoch_summary: Option<counter::Summary>,
    epoch: usize,
    /// Maximum number of queue slots (from initial_max_queues). Prevents DoS via large queue_ids.
    max_slots: usize,
    server: bool,
}

impl<S: 'static + Send, C: 'static + Send, const INITIAL_PAGE_SIZE: usize>
    Clone for Pool<S, C, INITIAL_PAGE_SIZE>
{
    #[inline]
    fn clone(&self) -> Self {
        Self {
            free: self.free.clone(),
            memory_handle: self.memory_handle.clone(),
            senders: self.senders.clone(),
            epoch_summary: self.epoch_summary.clone(),
            epoch: self.epoch,
            max_slots: self.max_slots,
            server: self.server,
        }
    }
}

impl<S, C, const INITIAL_PAGE_SIZE: usize> Pool<S, C, INITIAL_PAGE_SIZE>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
{
    /// Create a server-side pool that records freed queue_ids for QueueFree emission.
    #[inline]
    pub fn new(epoch_summary: Option<counter::Summary>) -> Self {
        Self::with_options(epoch_summary, true)
    }

    /// Create a client-side pool that silently recycles descriptors.
    #[inline]
    pub fn new_client(epoch_summary: Option<counter::Summary>) -> Self {
        Self::with_options(epoch_summary, false)
    }

    #[inline]
    fn with_options(epoch_summary: Option<counter::Summary>, server: bool) -> Self {
        let epoch = 0;
        let senders = sender::State::new(epoch);
        let (free, memory_handle) = FreeVec::new(INITIAL_PAGE_SIZE);
        // Client pools grow without bound (client controls its own indices).
        // Server pools should be configured via set_max_slots from initial_max_queues.
        let max_slots = if server {
            VarInt::from_u32(1 << 20).as_u64() as usize
        } else {
            usize::MAX
        };
        let mut pool = Pool {
            free,
            memory_handle,
            senders,
            epoch_summary,
            epoch,
            max_slots,
            server,
        };
        pool.grow().expect("initial grow must succeed");
        pool
    }

    pub fn set_max_slots(&mut self, max: usize) {
        self.max_slots = max;
    }

    #[inline]
    pub fn senders(&self) -> Senders<S, C, INITIAL_PAGE_SIZE> {
        Senders {
            state: self.senders.clone(),
            // make sure the memory lives as long as this sender is alive
            memory_handle: self.memory_handle.clone(),
            local: Default::default(),
        }
    }

    pub fn drain_pending_freed(&self) -> Option<(VarInt, s2n_quic_core::interval_set::IntervalSet<VarInt>)> {
        self.free.drain_pending_freed()
    }

    #[inline]
    pub fn alloc(
        &self,
        key: crate::flow::Handle,
        remote_queue_id: Option<VarInt>,
    ) -> Result<(Control<S, C>, Stream<S, C>), crate::flow::Handle> {
        self.free.alloc(key, remote_queue_id)
    }

    /// Client-side allocation: always succeeds (grows without bound).
    ///
    /// The client controls its own local queue IDs — no attacker can force a specific
    /// index, so there's no DoS vector. The pool grows as needed.
    #[inline]
    pub fn alloc_or_grow(
        &mut self,
        mut key: crate::flow::Handle,
        remote_queue_id: Option<VarInt>,
    ) -> (Control<S, C>, Stream<S, C>) {
        loop {
            match self.alloc(key, remote_queue_id) {
                Ok(descriptor) => return descriptor,
                Err(k) => {
                    key = k;
                    // Client pool growth is uncapped — this is safe because
                    // the client controls allocation sequentially.
                    let _ = self.grow();
                }
            }
        }
    }

    /// Allocate a specific slot by index, growing pages as needed.
    ///
    /// Used by the server to allocate at the exact `dest_queue_id` the client specified.
    /// Returns `None` if the slot is already allocated (retransmit race — caller should
    /// retry via `send_stream` which will validate the binding_id).
    /// Returns `Err(GrowError)` if the queue_id exceeds max_slots (DoS prevention).
    #[inline]
    pub fn alloc_at_or_grow(
        &mut self,
        slot_index: usize,
        mut key: crate::flow::Handle,
        remote_queue_id: Option<VarInt>,
    ) -> Result<Option<(Control<S, C>, Stream<S, C>)>, GrowError> {
        loop {
            match self.free.alloc_at(slot_index, key, remote_queue_id) {
                Ok(descriptor) => return Ok(Some(descriptor)),
                Err(k) => {
                    key = k;
                    if self.epoch > slot_index {
                        return Ok(None);
                    }
                    self.grow()?;
                }
            }
        }
    }

    #[inline(never)] // this should happen rarely
    fn grow(&mut self) -> Result<(), GrowError> {
        let remaining = self.max_slots.saturating_sub(self.epoch);
        let page_size = (self.epoch + INITIAL_PAGE_SIZE).min(remaining);
        if page_size == 0 {
            return Err(GrowError);
        }

        let (region, layout) = Region::alloc(page_size);

        let ptr = region.ptr;

        let mut pending_desc = vec![];
        let mut pending_senders = vec![];

        // Hoist the free list Arc outside the loop — all descriptors in a pool share
        // the same impl. Creating it once avoids page_size unnecessary Arc allocations.
        let free_list: Arc<dyn super::free_list::FreeList<S, C>> = if self.server {
            Arc::new(ServerFreeList(self.free.clone()))
        } else {
            Arc::new(ClientFreeList(self.free.clone()))
        };

        for idx in 0..page_size {
            let offset = layout.size() * idx;

            unsafe {
                let descriptor = ptr
                    .as_ptr()
                    .add(offset)
                    .cast::<DescriptorInner<S, C>>();

                let free_list = free_list.clone();

                // initialize the descriptor with the channels
                descriptor.write(DescriptorInner::new(
                    self.epoch + idx,
                    free_list,
                ));

                let descriptor = NonNull::new_unchecked(descriptor);
                let descriptor = Descriptor::new(descriptor);
                let sender = Sender::new(descriptor.clone_for_sender());

                // push the descriptor into the free list
                pending_desc.push(descriptor);

                // push the senders into the sender page
                pending_senders.push(sender);
            }
        }

        let pending_senders: Arc<[_]> = pending_senders.into();

        let mut senders = self.senders.pages.write().unwrap();

        // check if another pool instance already updated the senders list
        if senders.epoch != self.epoch {
            // update our local copy
            self.epoch = senders.epoch;

            // free what we just allocated, since we raced with the other pool instance
            for desc in pending_desc {
                unsafe {
                    desc.drop_in_place();
                }
            }

            drop(senders);
            debug!("grow raced — another instance grew first");

            // return back to the alloc method, which may have a free descriptor now
            return Ok(());
        }

        // update the epoch with the latest value
        let target_epoch = self.epoch + page_size;
        senders.epoch = target_epoch;
        self.epoch = target_epoch;
        if let Some(summary) = &self.epoch_summary {
            summary.record_value(target_epoch as u64);
        }

        // update the sender list with the newly allocated channels
        senders.pages.push(pending_senders);
        let epoch = senders.epoch;
        // we don't need to synchronize with the senders any more so drop the local
        drop(senders);

        debug!(epoch = epoch, "grow");

        // push all of the descriptors into the free list
        self.free.record_region(region, pending_desc);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GrowError;

pub(super) struct Region<S: 'static, C: 'static> {
    ptr: NonNull<u8>,
    layout: Layout,
    phantom: PhantomData<(S, C)>,
}

unsafe impl<S: Send, C: Send> Send for Region<S, C> {}
unsafe impl<S: Sync, C: Sync> Sync for Region<S, C> {}

impl<S: 'static, C: 'static> Region<S, C> {
    #[inline]
    fn alloc(page_size: usize) -> (Self, Layout) {
        debug_assert!(page_size > 0, "need at least 1 entry in page");

        // first create the descriptor layout
        let descriptor = Layout::new::<DescriptorInner<S, C>>().pad_to_align();

        let descriptors = {
            // TODO use `descriptor.repeat(page_size)` once stable
            // https://doc.rust-lang.org/stable/core/alloc/struct.Layout.html#method.repeat
            Layout::from_size_align(
                descriptor.size().checked_mul(page_size).unwrap(),
                descriptor.align(),
            )
            .unwrap()
        };

        let ptr = unsafe {
            // SAFETY: the layout is non-zero size
            debug_assert_ne!(descriptors.size(), 0);
            // ensure that the allocation is zeroed out so we don't have to worry about MaybeUninit
            std::alloc::alloc_zeroed(descriptors)
        };
        let ptr = NonNull::new(ptr).unwrap_or_else(|| std::alloc::handle_alloc_error(descriptors));

        let region = Self {
            ptr,
            layout: descriptors,
            phantom: PhantomData,
        };

        (region, descriptor)
    }
}

impl<S, C> Drop for Region<S, C> {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            std::alloc::dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}
