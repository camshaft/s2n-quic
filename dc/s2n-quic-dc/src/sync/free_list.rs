// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! MPMC free list channel for queue ID allocation.
//!
//! Multiple producer (dispatch threads push freed IDs via QueueFree) and
//! multiple consumer (client tasks allocate dest_queue_ids for new streams).
//!
//! The allocation model:
//! - A high-water mark counter provides lock-free fresh ID allocation up to `initial_max_queues`
//! - Once exhausted, consumers wait for recycled IDs pushed by producers
//! - Producers push freed IDs which wake one waiting consumer

use s2n_quic_core::varint::VarInt;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll, Waker},
};

#[derive(Debug)]
pub struct FreeList {
    high_water_mark: AtomicU64,
    max_queues: u64,
    inner: Mutex<Inner>,
}

/// Maximum number of pending waiters to prevent unbounded growth under sustained exhaustion.
const MAX_WAITERS: usize = 4096;

struct Inner {
    freed: VecDeque<VarInt>,
    waiters: VecDeque<Waker>,
    closed: bool,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("freed_len", &self.freed.len())
            .field("waiters_len", &self.waiters.len())
            .field("closed", &self.closed)
            .finish()
    }
}

impl FreeList {
    pub fn new(initial_max_queues: VarInt) -> Arc<Self> {
        Arc::new(Self {
            high_water_mark: AtomicU64::new(0),
            max_queues: initial_max_queues.as_u64(),
            inner: Mutex::new(Inner {
                freed: VecDeque::new(),
                waiters: VecDeque::new(),
                closed: false,
            }),
        })
    }

    /// Try to allocate a fresh ID from the high-water mark using a CAS loop.
    ///
    /// Returns `Some(id)` on success, `None` if the mark has reached max_queues.
    #[inline]
    fn try_alloc_fresh(&self) -> Option<VarInt> {
        loop {
            let current = self.high_water_mark.load(Ordering::Relaxed);
            if current >= self.max_queues {
                return None;
            }
            match self.high_water_mark.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return VarInt::new(current).ok(),
                Err(_) => continue,
            }
        }
    }

    /// Try to allocate a queue ID without blocking.
    ///
    /// Returns `Some(id)` if a fresh or recycled ID is available, `None` if exhausted.
    pub fn try_alloc(&self) -> Option<VarInt> {
        if let Some(id) = self.try_alloc_fresh() {
            return Some(id);
        }

        let mut inner = self.inner.lock().unwrap();
        inner.freed.pop_front()
    }

    /// Poll for a queue ID allocation (async path).
    ///
    /// Fast path: try high-water mark increment (lock-free CAS).
    /// Slow path: try freed list, or register waker and return Pending.
    pub fn poll_alloc(&self, cx: &mut Context) -> Poll<Option<VarInt>> {
        if let Some(id) = self.try_alloc_fresh() {
            return Poll::Ready(Some(id));
        }

        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Poll::Ready(None);
        }
        if let Some(id) = inner.freed.pop_front() {
            return Poll::Ready(Some(id));
        }

        // Cap waiter queue to prevent unbounded growth under sustained exhaustion.
        if inner.waiters.len() >= MAX_WAITERS {
            inner.waiters.pop_front();
        }
        inner.waiters.push_back(cx.waker().clone());
        Poll::Pending
    }

    /// Push a freed queue ID back into the pool.
    ///
    /// Wakes one waiting consumer if any are registered.
    pub fn free(&self, id: VarInt) {
        let mut inner = self.inner.lock().unwrap();
        inner.freed.push_back(id);
        if let Some(waker) = inner.waiters.pop_front() {
            drop(inner);
            waker.wake();
        }
    }

    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        let waiters: Vec<_> = inner.waiters.drain(..).collect();
        drop(inner);
        for waker in waiters {
            waker.wake();
        }
    }
}
