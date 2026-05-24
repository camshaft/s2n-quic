// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Flow-based queue allocation and dispatching.
//!
//! This provides queue infrastructure similar to `stream::recv::dispatch` but
//! is generic over the stream and control data types.
use crate::{counter, credentials::Credentials, intrusive, tracing::*};
use s2n_quic_core::varint::VarInt;
use std::sync::Arc;

mod descriptor;
mod free_list;
mod handle;
mod inner;
mod pool;
mod probes;
mod queue_id;
mod sender;

// Re-export the Key trait
pub use descriptor::{FreeNotify, FreedSlot, Key, ServerValidation, ValidationError, WakerSink};
pub use inner::AutoWake;
pub const MAX_SLOTS: usize = queue_id::MAX_SLOTS;

/// Extract the slot index from an encoded queue_id.
#[inline]
pub fn slot_index(queue_id: VarInt) -> usize {
    queue_id::index(queue_id)
}

/// Size of the first allocated page of queue slots.
///
/// Subsequent pages double in size so the pool converges quickly to the right
/// capacity, similar to how `std::vec::Vec` grows.
///
/// In test builds a smaller value is used to exercise growth branches without
/// the overhead of allocating 65 535 slots per test endpoint.
const INITIAL_PAGE_SIZE: usize = if cfg!(test) { 8 } else { u16::MAX as _ };

pub type Error<T> = inner::Error<T>;
pub type Control<S, C, K> = handle::Control<S, C, K>;
pub type Stream<S, C, K> = handle::Stream<S, C, K>;

/// Queue allocator for flow-based routing
///
/// Generic over stream data type `S` and control data type `C`.
pub struct Allocator<S, C, K = Credentials>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
    K: 'static + Send + Sync,
{
    pool: pool::Pool<S, C, K, INITIAL_PAGE_SIZE>,
}

impl<S, C, K> core::fmt::Debug for Allocator<S, C, K>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
    K: 'static + Send + Sync,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Allocator").finish_non_exhaustive()
    }
}

impl<S, C, K> Clone for Allocator<S, C, K>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
    K: 'static + Send + Sync,
{
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
        }
    }
}

impl<S, C, K> Default for Allocator<S, C, K>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
    K: 'static + Send + Sync,
 {
    fn default() -> Self {
        Self::new()
    }
}

impl<S, C, K> Allocator<S, C, K>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
    K: 'static + Send + Sync,
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
    pub fn new_with_waker_sink(
        registry: &counter::Registry,
        waker_sink: Arc<dyn WakerSink>,
    ) -> Self {
        let epoch_summary = registry
            .register_summary("flow.queue.epoch", counter::Unit::Count)
            .with_description(
                "Upper bound of allocated queue slots (epoch) after each allocator growth",
            );
        Self {
            pool: pool::Pool::new_with_waker_sink(Some(epoch_summary), waker_sink),
        }
    }

    #[inline]
    pub fn dispatcher(&self) -> Dispatch<S, C, K> {
        let free_notify = self
            .pool
            .free_notify
            .clone()
            .unwrap_or_else(|| Arc::new(descriptor::FreeNotify::new()));
        Dispatch {
            senders: self.pool.senders(),
            is_open: true,
            pool: self.pool.clone(),
            free_notify,
        }
    }

    #[inline]
    pub fn alloc(
        &self,
        key: K,
        remote_queue_id: Option<VarInt>,
    ) -> Result<(Control<S, C, K>, Stream<S, C, K>), K> {
        self.pool.alloc(key, remote_queue_id, None)
    }

    #[inline]
    pub fn alloc_or_grow(
        &mut self,
        key: K,
        remote_queue_id: Option<VarInt>,
    ) -> (Control<S, C, K>, Stream<S, C, K>) {
        self.pool.alloc_or_grow(key, remote_queue_id, None)
    }
}

/// Dispatcher which routes data to the specified queue
pub struct Dispatch<S, C, K = Credentials>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
    K: 'static + Send + Sync,
{
    senders: sender::Senders<S, C, K, INITIAL_PAGE_SIZE>,
    is_open: bool,
    pool: pool::Pool<S, C, K, INITIAL_PAGE_SIZE>,
    /// Shared sink for freed-slot notifications. Populated by descriptor drop_receiver
    /// when both queue halves close. Drained by the dispatch layer to emit QueueFree.
    free_notify: Arc<descriptor::FreeNotify>,
}

impl<S, C, K> Clone for Dispatch<S, C, K>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
    K: 'static + Send + Sync + Key,
{
    fn clone(&self) -> Self {
        Self {
            senders: self.senders.clone(),
            is_open: self.is_open,
            pool: self.pool.clone(),
            free_notify: self.free_notify.clone(),
        }
    }
}

impl<S, C, K> Dispatch<S, C, K>
where
    S: 'static + Send + Sync,
    C: 'static + Send + Sync,
    K: 'static + Send + Sync + Key,
{
    #[inline]
    pub fn alloc(
        &self,
        key: K,
        remote_queue_id: Option<VarInt>,
    ) -> Result<(Control<S, C, K>, Stream<S, C, K>), K> {
        let binding_id = key.binding_id();
        self.pool.alloc(key, remote_queue_id, binding_id)
    }

    #[inline]
    pub fn alloc_or_grow(
        &mut self,
        key: K,
        remote_queue_id: Option<VarInt>,
    ) -> (Control<S, C, K>, Stream<S, C, K>) {
        let binding_id = key.binding_id();
        self.pool.alloc_or_grow(key, remote_queue_id, binding_id)
    }

    /// Allocate a specific slot by index, growing pages as needed.
    ///
    /// Used by the server to allocate at the exact `dest_queue_id` the client specified.
    #[inline]
    pub fn alloc_at_or_grow(
        &mut self,
        slot_index: usize,
        key: K,
        remote_queue_id: Option<VarInt>,
    ) -> (Control<S, C, K>, Stream<S, C, K>) {
        let binding_id = key.binding_id();
        self.pool.alloc_at_or_grow(slot_index, key, remote_queue_id, binding_id)
    }

    /// Drain freed slots that have been released since the last call.
    ///
    /// Returns freed slot entries representing queue slots whose both stream and
    /// control receivers have dropped. Used by the server to emit QueueFree frames.
    /// Skips the mutex entirely when no items have been pushed (atomic fast-path).
    pub fn drain_freed(&mut self) -> Vec<FreedSlot> {
        self.free_notify.drain()
    }

    #[inline]
    pub fn send_control(
        &mut self,
        local_queue_id: VarInt,
        remote_queue_id: Option<VarInt>,
        params: &K::Request,
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
            Err(Error::Unallocated(data)) => {
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
        params: &K::Request,
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
            Err(Error::Unallocated(data)) => {
                debug!("unroutable stream data");
                Err(inner::Error::Unallocated(data))
            }
        }
    }

    /// Server-side stream send: validates binding_id and atomically creates binding if unbound.
    ///
    /// Returns `Ok((waker, ServerValidation))` where `ServerValidation::NewBinding` indicates
    /// the caller must complete stream creation (register with acceptor, etc).
    #[inline]
    pub fn send_stream_server(
        &mut self,
        local_queue_id: VarInt,
        remote_queue_id: Option<VarInt>,
        params: &K::Request,
        data: intrusive::Entry<S>,
        new_key: impl FnOnce() -> K,
    ) -> Result<(AutoWake, descriptor::ServerValidation), Error<intrusive::Entry<S>>> {
        let res = self
            .senders
            .lookup(local_queue_id, data, |sender, data| {
                sender.send_stream_server(data, remote_queue_id, params, new_key)
            });

        match res {
            Ok((waker, validation)) => {
                trace!(%local_queue_id, ?validation, "send_stream_server");
                Ok((waker, validation))
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
            Err(Error::Unallocated(data)) => {
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
        params: &K::Request,
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

    #[inline]
    /// Sends to both stream+control halves for every queue matching `params`.
    ///
    /// # Performance
    ///
    /// This is a global scan. Internally it walks the entire sender table and runs
    /// per-queue validation, so the cost is O(total_queues) and can be very high at
    /// large concurrency. Use sparingly for rare control-plane events only.
    /// Broadcast to all allocated queues without validation.
    ///
    /// Used for peer-dead reset fanout where every queue on this dispatcher
    /// needs to receive the message regardless of binding_id.
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

    /// Validates the queue's key against the provided parameters by checking the stream queue.
    #[inline]
    pub fn validate_stream(
        &mut self,
        local_queue_id: VarInt,
        params: &K::Request,
    ) -> Result<(), ValidateError> {
        match self.senders.lookup(local_queue_id, (), |sender, ()| {
            Ok(sender.validate_stream(params))
        }) {
            Ok(result) => result,
            Err(_) => Err(ValidateError::Unallocated),
        }
    }

    /// Validates the queue's key against the provided parameters by checking the control queue.
    #[inline]
    pub fn validate_control(
        &mut self,
        queue_id: VarInt,
        params: &K::Request,
    ) -> Result<(), ValidateError> {
        match self.senders.lookup(queue_id, (), |sender, ()| {
            Ok(sender.validate_control(params))
        }) {
            Ok(result) => result,
            Err(_) => Err(ValidateError::Unallocated),
        }
    }
}

/// Error returned by validate_stream/validate_control
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidateError {
    /// Queue not found or deallocated
    Unallocated,
    /// Key validation failed
    Validation(ValidationError),
}
