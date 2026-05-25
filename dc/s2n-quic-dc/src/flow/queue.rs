// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Flow-based queue allocation and dispatching.
//!
//! This provides queue infrastructure similar to `stream::recv::dispatch` but
//! is generic over the stream and control data types. The key type is always
//! `flow::Handle` (binding_id wrapper).
use crate::{counter, flow::Handle, intrusive, tracing::*};
use s2n_quic_core::varint::VarInt;

mod descriptor;
mod free_list;
mod handle;
mod inner;
mod pool;
mod probes;
mod sender;

pub use descriptor::{ServerValidation, ValidationError};
pub use inner::AutoWake;

/// Size of the first allocated page of queue slots.
///
/// Subsequent pages double in size so the pool converges quickly to the right
/// capacity, similar to how `std::vec::Vec` grows.
///
/// In test builds a smaller value is used to exercise growth branches without
/// the overhead of allocating 65 535 slots per test endpoint.
const INITIAL_PAGE_SIZE: usize = if cfg!(test) { 8 } else { u16::MAX as _ };

pub type Error<T> = inner::Error<T>;
pub type Control<S, C> = handle::Control<S, C>;
pub type Stream<S, C> = handle::Stream<S, C>;

/// Queue allocator for flow-based routing
///
/// Generic over stream data type `S` and control data type `C`.
pub struct Allocator<S, C>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
{
    pool: pool::Pool<S, C, INITIAL_PAGE_SIZE>,
}

impl<S, C> core::fmt::Debug for Allocator<S, C>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Allocator").finish_non_exhaustive()
    }
}

impl<S, C> Clone for Allocator<S, C>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
{
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
        }
    }
}

impl<S, C> Default for Allocator<S, C>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<S, C> Allocator<S, C>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
{
    pub fn new() -> Self {
        Self {
            pool: pool::Pool::new(None),
        }
    }

    #[inline]
    pub fn new_with_registry(registry: &counter::Registry) -> Self {
        let epoch_summary = registry
            .register_summary("flow.queue.epoch", counter::Unit::Count)
            .with_description(
                "Upper bound of allocated queue slots (epoch) after each allocator growth",
            );
        Self {
            pool: pool::Pool::new(Some(epoch_summary)),
        }
    }

    #[inline]
    pub fn dispatcher(&self) -> Dispatch<S, C> {
        Dispatch {
            senders: self.pool.senders(),
            is_open: true,
            pool: self.pool.clone(),
        }
    }

    #[inline]
    pub fn alloc(
        &self,
        key: Handle,
        remote_queue_id: Option<VarInt>,
    ) -> Result<(Control<S, C>, Stream<S, C>), Handle> {
        self.pool.alloc(key, remote_queue_id)
    }

    #[inline]
    pub fn alloc_or_grow(
        &mut self,
        key: Handle,
        remote_queue_id: Option<VarInt>,
    ) -> (Control<S, C>, Stream<S, C>) {
        self.pool.alloc_or_grow(key, remote_queue_id)
    }
}

/// Dispatcher which routes data to the specified queue
pub struct Dispatch<S, C>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
{
    senders: sender::Senders<S, C, INITIAL_PAGE_SIZE>,
    is_open: bool,
    pool: pool::Pool<S, C, INITIAL_PAGE_SIZE>,
}

impl<S, C> Clone for Dispatch<S, C>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
{
    fn clone(&self) -> Self {
        Self {
            senders: self.senders.clone(),
            is_open: self.is_open,
            pool: self.pool.clone(),
        }
    }
}

impl<S, C> Dispatch<S, C>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
{
    #[inline]
    pub fn alloc(
        &self,
        key: Handle,
        remote_queue_id: Option<VarInt>,
    ) -> Result<(Control<S, C>, Stream<S, C>), Handle> {
        self.pool.alloc(key, remote_queue_id)
    }

    #[inline]
    pub fn alloc_or_grow(
        &mut self,
        key: Handle,
        remote_queue_id: Option<VarInt>,
    ) -> (Control<S, C>, Stream<S, C>) {
        self.pool.alloc_or_grow(key, remote_queue_id)
    }

    /// Allocate a specific slot by index, growing pages as needed.
    ///
    /// Used by the server to allocate at the exact `dest_queue_id` the client specified.
    /// Returns `None` if the slot is already allocated (retransmit/duplicate race).
    #[inline]
    pub fn alloc_at_or_grow(
        &mut self,
        slot_index: usize,
        key: Handle,
        remote_queue_id: Option<VarInt>,
    ) -> Option<(Control<S, C>, Stream<S, C>)> {
        self.pool.alloc_at_or_grow(slot_index, key, remote_queue_id)
    }

    /// Drain freed queue_ids as an IntervalSet for range-compressed QueueFree emission.
    ///
    /// Returns `(free_request_id, queue_ids)` or None if no slots have been freed.
    /// The free_request_id is stamped on the QueueFree frame for receiver-side dedup.
    pub fn drain_freed(&self) -> Option<(VarInt, s2n_quic_core::interval_set::IntervalSet<VarInt>)> {
        self.pool.drain_pending_freed()
    }

    #[inline]
    pub fn send_control(
        &mut self,
        local_queue_id: VarInt,
        remote_queue_id: Option<VarInt>,
        params: &VarInt,
        data: intrusive::Entry<C>,
    ) -> Result<AutoWake, Error<intrusive::Entry<C>>> {
        let res = self.senders.lookup(local_queue_id, data, |sender, data| {
            sender.send_control(data, remote_queue_id, params)
        });

        match res {
            Ok(waker) => {
                trace!(%local_queue_id, "send_control");
                Ok(waker)
            }
            Err(Error::PermanentlyClosed) => {
                self.is_open = false;
                Err(inner::Error::PermanentlyClosed)
            }
            Err(Error::HalfClosed(data)) => {
                debug!(%local_queue_id, "control receiver closed");
                Err(inner::Error::HalfClosed(data))
            }
            Err(Error::ValidationFailed(data, reason)) => {
                debug!(%local_queue_id, ?reason, "control queue validation failed");
                Err(inner::Error::ValidationFailed(data, reason))
            }
            Err(Error::Unallocated(data)) | Err(Error::NeedsGrow(data)) => {
                debug!("unroutable control data");
                Err(inner::Error::Unallocated(data))
            }
        }
    }

    #[inline]
    pub fn send_stream(
        &mut self,
        local_queue_id: VarInt,
        remote_queue_id: Option<VarInt>,
        params: &VarInt,
        data: intrusive::Entry<S>,
    ) -> Result<AutoWake, Error<intrusive::Entry<S>>> {
        let res = self.senders.lookup(local_queue_id, data, |sender, data| {
            sender.send_stream(data, remote_queue_id, params)
        });

        match res {
            Ok(waker) => {
                trace!(%local_queue_id, "send_stream");
                Ok(waker)
            }
            Err(Error::PermanentlyClosed) => {
                self.is_open = false;
                Err(inner::Error::PermanentlyClosed)
            }
            Err(Error::HalfClosed(data)) => {
                debug!(%local_queue_id, "stream receiver closed");
                Err(inner::Error::HalfClosed(data))
            }
            Err(Error::ValidationFailed(data, reason)) => {
                debug!(%local_queue_id, ?reason, "stream queue validation failed");
                Err(inner::Error::ValidationFailed(data, reason))
            }
            Err(Error::Unallocated(data)) | Err(Error::NeedsGrow(data)) => {
                debug!("unroutable stream data");
                Err(inner::Error::Unallocated(data))
            }
        }
    }

    #[inline]
    pub fn send_both(
        &mut self,
        local_queue_id: VarInt,
        remote_queue_id: Option<VarInt>,
        params: &VarInt,
        stream_data: intrusive::Entry<S>,
        control_data: intrusive::Entry<C>,
    ) -> (AutoWake, AutoWake) {
        let res = self.senders.lookup(local_queue_id, (), |sender, ()| {
            let send = sender
                .send_stream(stream_data, remote_queue_id, params)
                .unwrap_or_default();
            let recv = sender
                .send_control(control_data, remote_queue_id, params)
                .unwrap_or_default();

            Ok((send, recv))
        });

        trace!(%local_queue_id, "send_both");

        res.unwrap_or_default()
    }

    /// Broadcast to all allocated queues without validation.
    ///
    /// Used for peer-dead reset fanout where every queue on this dispatcher
    /// needs to receive the message regardless of binding_id.
    ///
    /// # Performance
    ///
    /// This is a global scan — O(total_queues). Use sparingly for rare control-plane events.
    pub fn broadcast_both(
        &mut self,
        mut stream_data: impl FnMut() -> intrusive::Entry<S>,
        mut control_data: impl FnMut() -> intrusive::Entry<C>,
        mut on_waker: impl FnMut(AutoWake, AutoWake),
    ) {
        self.senders.for_each_sender(|sender| {
            let stream_waker = sender
                .push_stream_unchecked(stream_data())
                .unwrap_or_default();
            let control_waker = sender
                .push_control_unchecked(control_data())
                .unwrap_or_default();
            on_waker(stream_waker, control_waker);
        });
    }

}
