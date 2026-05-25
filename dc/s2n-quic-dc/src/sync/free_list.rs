// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Client-side peer free list for tracking available server queue IDs.
//!
//! Tracks which server_queue_ids are available for allocation. Deduplicates
//! QueueFree messages using a monotonic free_request_id tracked in an IntervalSet.
//!
//! Allocation model:
//! - A high-water mark counter provides lock-free fresh ID allocation
//! - Once exhausted, consumers wait for recycled IDs pushed via QueueFree
//! - A HierarchicalBitSet provides O(4) pop_first for recycled IDs
//!
//! Dedup model:
//! - Each QueueFree message from the server carries a monotonic free_request_id
//! - The client tracks seen request IDs in an IntervalSet
//! - Duplicate/replayed QueueFree messages are rejected without per-slot state

use super::bitset::HierarchicalBitSet;
use s2n_quic_core::{interval_set::IntervalSet, varint::VarInt};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll, Waker},
};

const MAX_WAITERS: usize = 4096;

#[derive(Debug)]
pub struct FreeList {
    high_water_mark: AtomicU64,
    max_queues: AtomicU64,
    inner: Mutex<Inner>,
}

struct Inner {
    /// Available server_queue_ids for O(4) pop_first.
    freed: HierarchicalBitSet,
    /// Tracks which free_request_ids have been processed for dedup.
    seen_requests: IntervalSet<VarInt>,
    waiters: VecDeque<Waker>,
    closed: bool,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("freed_len", &self.freed.len())
            .field("seen_requests_count", &self.seen_requests.count())
            .field("waiters_len", &self.waiters.len())
            .field("closed", &self.closed)
            .finish()
    }
}

impl FreeList {
    pub fn new(initial_max_queues: VarInt) -> Arc<Self> {
        let max = initial_max_queues.as_u64();
        let capacity = max.min(HierarchicalBitSet::MAX_CAPACITY as u64) as u32;
        Arc::new(Self {
            high_water_mark: AtomicU64::new(0),
            max_queues: AtomicU64::new(max),
            inner: Mutex::new(Inner {
                freed: HierarchicalBitSet::new(capacity.max(1)),
                seen_requests: IntervalSet::new(),
                waiters: VecDeque::new(),
                closed: false,
            }),
        })
    }

    #[inline]
    fn try_alloc_fresh(&self) -> Option<VarInt> {
        loop {
            let current = self.high_water_mark.load(Ordering::Relaxed);
            let max = self.max_queues.load(Ordering::Relaxed);
            if current >= max {
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

    pub fn try_alloc(&self) -> Option<VarInt> {
        if let Some(id) = self.try_alloc_fresh() {
            return Some(id);
        }

        let mut inner = self.inner.lock().unwrap();
        let index = inner.freed.pop_first()?;
        VarInt::new(index as u64).ok()
    }

    pub fn poll_alloc(&self, cx: &mut Context) -> Poll<Option<VarInt>> {
        if let Some(id) = self.try_alloc_fresh() {
            return Poll::Ready(Some(id));
        }

        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Poll::Ready(None);
        }
        if let Some(index) = inner.freed.pop_first() {
            return Poll::Ready(VarInt::new(index as u64).ok());
        }

        // Dedup: avoid accumulating identical wakers from repeated polls of the same future
        let should_push = inner
            .waiters
            .back()
            .map_or(true, |w| !w.will_wake(cx.waker()));
        let evicted = if should_push {
            if inner.waiters.len() >= MAX_WAITERS {
                inner.waiters.pop_front()
            } else {
                None
            }
        } else {
            None
        };
        if should_push {
            inner.waiters.push_back(cx.waker().clone());
        }
        drop(inner);
        if let Some(waker) = evicted {
            waker.wake();
        }
        Poll::Pending
    }

    /// Process a QueueFree message from the server.
    ///
    /// Returns true if the message was accepted (new free_request_id), false if
    /// it was a duplicate/replay.
    ///
    /// Wakers are shipped to `waker_sink` rather than woken inline, because this
    /// is called from the dispatcher thread where syscalls are unacceptable.
    pub fn free(
        &self,
        free_request_id: VarInt,
        queue_ids: &IntervalSet<VarInt>,
        waker_sink: &mut impl FnMut(Waker),
    ) -> bool {
        let mut inner = self.inner.lock().unwrap();

        // Dedup: reject if we've already seen this free_request_id
        if inner.seen_requests.contains(&free_request_id) {
            return false;
        }
        let _ = inner.seen_requests.insert_value(free_request_id);

        // Insert ALL freed queue_ids into the available set first, then ship wakers.
        for range in queue_ids.inclusive_ranges() {
            let start = range.start().as_u64() as u32;
            let end = range.end().as_u64() as u32;
            for id in start..=end {
                let needed = id + 1;
                if needed > inner.freed.capacity() {
                    inner.freed.grow(needed);
                }
                inner.freed.insert(id);
            }
        }

        // Ship wakers to another thread — never wake inline on the dispatch path
        for waker in inner.waiters.drain(..) {
            waker_sink(waker);
        }
        true
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

impl FreeList {
    #[cfg(test)]
    fn free_for_test(&self, free_request_id: VarInt, queue_ids: &IntervalSet<VarInt>) -> bool {
        self.free(free_request_id, queue_ids, &mut |w| w.wake())
    }
}

#[cfg(test)]
mod tests {
    use super::FreeList;
    use s2n_quic_core::{interval_set::IntervalSet, varint::VarInt};

    fn interval_set_single(id: VarInt) -> IntervalSet<VarInt> {
        let mut set = IntervalSet::new();
        let _ = set.insert_value(id);
        set
    }

    #[test]
    fn fresh_allocation_up_to_max() {
        let list = FreeList::new(VarInt::from_u8(3));
        assert_eq!(list.try_alloc(), Some(VarInt::from_u8(0)));
        assert_eq!(list.try_alloc(), Some(VarInt::from_u8(1)));
        assert_eq!(list.try_alloc(), Some(VarInt::from_u8(2)));
        assert_eq!(list.try_alloc(), None);
    }

    #[test]
    fn free_and_recycle() {
        let list = FreeList::new(VarInt::from_u8(2));
        let id0 = list.try_alloc().unwrap();
        let id1 = list.try_alloc().unwrap();
        assert_eq!(list.try_alloc(), None);

        // Free id1 with request_id=1
        let ids = interval_set_single(id1);
        assert!(list.free_for_test(VarInt::from_u8(1), &ids));
        assert_eq!(list.try_alloc(), Some(id1));
        assert_eq!(list.try_alloc(), None);

        // Free both with request_id=2
        let mut both = IntervalSet::new();
        let _ = both.insert_value(id0);
        let _ = both.insert_value(id1);
        assert!(list.free_for_test(VarInt::from_u8(2), &both));
        // pop_first returns lowest index first
        assert_eq!(list.try_alloc(), Some(id0));
        assert_eq!(list.try_alloc(), Some(id1));
    }

    #[test]
    fn duplicate_request_ids_rejected() {
        let list = FreeList::new(VarInt::from_u8(0));
        let ids = interval_set_single(VarInt::from_u8(7));

        // First free succeeds
        assert!(list.free_for_test(VarInt::from_u8(1), &ids));
        assert_eq!(list.try_alloc(), Some(VarInt::from_u8(7)));

        // Same request_id rejected (replay)
        assert!(!list.free_for_test(VarInt::from_u8(1), &ids));
        assert_eq!(list.try_alloc(), None);

        // New request_id succeeds
        assert!(list.free_for_test(VarInt::from_u8(2), &ids));
        assert_eq!(list.try_alloc(), Some(VarInt::from_u8(7)));
    }
}
