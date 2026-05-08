// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{
    free_list::FreeList,
    inner::{Half, Queue},
    probes,
};
use s2n_quic_core::{ensure, varint::VarInt};
use std::{
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

/// Trait for validating keys during dispatch
pub trait Key: 'static + Send {
    /// The request type used for validation
    type Request;

    /// Validates the provided request parameters against this key
    fn validate(&self, params: &Self::Request) -> bool;
}

impl Key for crate::credentials::Credentials {
    type Request = crate::credentials::Credentials;

    #[inline]
    fn validate(&self, params: &Self::Request) -> bool {
        self == params
    }
}

pub(super) struct Descriptor<S, C, Key> {
    inner: Arc<DescriptorInner<S, C, Key>>,
}

impl<S, C, Key> Clone for Descriptor<S, C, Key> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: 'static, C: 'static, Key: 'static> Descriptor<S, C, Key> {
    #[inline]
    pub(super) fn new(id: VarInt, free_list: Arc<dyn FreeList<S, C, Key>>) -> Self {
        Self {
            inner: Arc::new(DescriptorInner::new(id, free_list)),
        }
    }

    #[inline]
    pub fn clone_for_sender(&self) -> Descriptor<S, C, Key> {
        self.inner.senders.fetch_add(1, Ordering::Relaxed);
        self.clone()
    }

    #[cfg(debug_assertions)]
    pub(super) fn as_usize(&self) -> usize {
        Arc::as_ptr(&self.inner).addr()
    }

    /// Returns the peer's queue ID, or `None` if not yet observed.
    #[inline]
    pub fn queue_id(&self) -> VarInt {
        self.inner.id
    }

    /// Returns the peer's queue ID, or `None` if not yet observed.
    #[inline]
    pub fn remote_queue_id(&self) -> Option<VarInt> {
        let v = self.inner.remote_queue_id.load(Ordering::Relaxed);
        VarInt::new(v).ok()
    }

    /// Stores the peer's queue ID with a relaxed store.
    ///
    /// Should only be called once per flow — guarded by the `HAS_OBSERVED` flag in the queue.
    ///
    #[inline]
    pub fn set_remote_queue_id(&self, id: VarInt) {
        self.inner.remote_queue_id.store(id.as_u64(), Ordering::Relaxed);
    }

    #[inline]
    pub fn with_key<R, F>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&Key) -> R,
    {
        let key = self.inner.key.lock().unwrap();
        key.as_ref().map(f)
    }

    #[inline]
    pub fn stream_queue(&self) -> &Queue<S> {
        &self.inner.stream
    }

    #[inline]
    pub fn control_queue(&self) -> &Queue<C> {
        &self.inner.control
    }

    #[inline]
    pub fn init_key(&self, key: Key) {
        let mut current = self.inner.key.lock().unwrap();
        debug_assert!(current.is_none(), "descriptor key already initialized");
        *current = Some(key);
    }

    /// If `remote_queue_id` is `Some`, the value is stored immediately and both queue
    /// halves are marked as already observed (no dispatcher-side store needed).
    #[inline]
    pub fn into_receiver_pair(self, remote_queue_id: Option<VarInt>) -> (Self, Self) {
        let has_remote_queue_id = remote_queue_id.is_some();
        if let Some(id) = remote_queue_id {
            self.inner.remote_queue_id.store(id.as_u64(), Ordering::Relaxed);
        } else {
            self.inner
                .remote_queue_id
                .store(REMOTE_QUEUE_ID_UNKNOWN, Ordering::Relaxed);
        }

        // open the queues back up for receiving
        self.inner
            .stream
            .open_receivers(&self.inner.control, has_remote_queue_id)
            .unwrap();

        probes::on_receiver_open(self.inner.id);

        let other = self.clone();

        (self, other)
    }

    #[inline]
    pub fn drop_sender(&self) {
        let desc_ref = self.inner.senders.fetch_sub(1, Ordering::Release);
        debug_assert_ne!(desc_ref, 0, "reference count underflow");

        // based on the implementation in:
        // https://github.com/rust-lang/rust/blob/28b83ee59698ae069f5355b8e03f976406f410f5/library/alloc/src/sync.rs#L2551
        if desc_ref != 1 {
            probes::on_sender_drop(self.inner.id);
            return;
        }

        core::sync::atomic::fence(Ordering::Acquire);

        // close both of the queues so the receivers are notified
        self.inner.control.close();
        self.inner.stream.close();

        probes::on_sender_close(self.inner.id);
    }

    #[inline]
    pub fn drop_receiver(&self, half: Half)
    where
        Key: 'static,
    {
        probes::on_receiver_drop(self.inner.id, half);

        ensure!(self
            .inner
            .stream
            .close_receiver(&self.inner.control, half)
            .is_continue());

        probes::on_receiver_free(self.inner.id, half);

        // Drop the key before freeing the descriptor
        let mut key = self.inner.key.lock().unwrap();
        let key = key.take();
        debug_assert!(key.is_some(), "descriptor key was not initialized");
        drop(key);

        let storage = self.inner.free_list.free(self.clone());
        drop(storage);
    }
}

/// Sentinel value indicating the remote queue ID is not yet known.
const REMOTE_QUEUE_ID_UNKNOWN: u64 = u64::MAX;

pub(super) struct DescriptorInner<S, C, Key> {
    id: VarInt,
    /// The peer's queue ID, written once by the dispatcher on first observation.
    /// Initialized to `u64::MAX` (unknown) and set via a relaxed store.
    remote_queue_id: AtomicU64,
    key: Mutex<Option<Key>>,
    stream: Queue<S>,
    control: Queue<C>,
    /// A reference back to the free list
    free_list: Arc<dyn FreeList<S, C, Key>>,
    senders: AtomicUsize,
}

impl<S, C, Key> DescriptorInner<S, C, Key> {
    pub(super) fn new(id: VarInt, free_list: Arc<dyn FreeList<S, C, Key>>) -> Self {
        let stream = Queue::new(Half::Stream);
        let control = Queue::new(Half::Control);
        Self {
            id,
            remote_queue_id: AtomicU64::new(REMOTE_QUEUE_ID_UNKNOWN),
            key: Mutex::new(None),
            stream,
            control,
            senders: AtomicUsize::new(0),
            free_list,
        }
    }
}
