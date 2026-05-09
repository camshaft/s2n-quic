// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Send-safe sharded intrusive queue channel for normal async runtimes.
//!
//! The sender has no backpressure - it can always push entries to one of the
//! shards. The receiver drains one shard at a time, returning the entire list.
//! Receivers are expected to register their waker once before exposing senders.

use crate::intrusive_queue;
use core::{
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    task::{Poll, Waker},
};
use parking_lot::Mutex;
use std::sync::{Arc, OnceLock};

struct Shared<A: intrusive_queue::Adapter> {
    is_open: AtomicBool,
    sender_count: AtomicUsize,
    next_sender_shard: AtomicUsize,
    sender_stride: usize,
    shard_mask: usize,
    occupancy: Box<[AtomicU64]>,
    recv_waker: OnceLock<Waker>,
    shards: Box<[Mutex<intrusive_queue::List<A>>]>,
}

impl<A: intrusive_queue::Adapter> Shared<A> {
    #[inline(always)]
    fn allocate_sender_shard(&self) -> usize {
        // Sender start positions intentionally wrap around the shard mask once there are more
        // senders than shards.
        self.next_sender_shard
            .fetch_add(self.sender_stride, Ordering::Relaxed)
            & self.shard_mask
    }

    #[inline(always)]
    fn bit(shard: usize) -> (usize, u64) {
        // Map a shard index to its occupancy word and bit in the bitmap.
        let word = shard / u64::BITS as usize;
        let bit = 1 << (shard % u64::BITS as usize);
        (word, bit)
    }

    #[inline(always)]
    fn set_occupied(&self, shard: usize) {
        let (word, bit) = Self::bit(shard);
        self.occupancy[word].fetch_or(bit, Ordering::Release);
    }

    #[inline(always)]
    fn clear_occupied(&self, shard: usize) {
        let (word, bit) = Self::bit(shard);
        self.occupancy[word].fetch_and(!bit, Ordering::AcqRel);
    }

    #[inline(always)]
    fn wake_receiver(&self) {
        if let Some(waker) = self.recv_waker.get() {
            waker.wake_by_ref();
        }
    }
}

#[inline(always)]
fn sender_stride(shard_count: usize) -> usize {
    // Start near half the shard count to spread consecutive senders apart, then force the result
    // to be odd. Odd values are coprime with power-of-two shard counts, so each sender walks every
    // shard before repeating.
    ((shard_count / 2).saturating_sub(1)) | 1
}

pub fn new<T>(
    shard_count: usize,
) -> (
    Sender<intrusive_queue::EntryAdapter<T>>,
    Receiver<intrusive_queue::EntryAdapter<T>>,
) {
    new_with_adapter::<intrusive_queue::EntryAdapter<T>>(shard_count)
}

pub fn new_with_adapter<A: intrusive_queue::Adapter>(
    shard_count: usize,
) -> (Sender<A>, Receiver<A>) {
    assert!(
        shard_count.is_power_of_two(),
        "shard count must be a power of two"
    );

    let occupancy_len = shard_count.div_ceil(u64::BITS as usize);
    let occupancy = (0..occupancy_len)
        .map(|_| AtomicU64::new(0))
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let shards = (0..shard_count)
        .map(|_| Mutex::new(intrusive_queue::List::new()))
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let sender_stride = sender_stride(shard_count);
    let shared = Arc::new(Shared {
        is_open: AtomicBool::new(true),
        sender_count: AtomicUsize::new(1),
        next_sender_shard: AtomicUsize::new(0),
        sender_stride,
        shard_mask: shard_count - 1,
        occupancy,
        recv_waker: OnceLock::new(),
        shards,
    });

    let sender = Sender {
        next_shard: shared.allocate_sender_shard(),
        shared: shared.clone(),
    };
    let receiver = Receiver {
        next_shard: 0,
        shared,
    };

    (sender, receiver)
}

pub struct Sender<A: intrusive_queue::Adapter> {
    next_shard: usize,
    shared: Arc<Shared<A>>,
}

impl<A: intrusive_queue::Adapter> Clone for Sender<A> {
    fn clone(&self) -> Self {
        self.shared.sender_count.fetch_add(1, Ordering::Relaxed);
        Self {
            next_shard: self.shared.allocate_sender_shard(),
            shared: self.shared.clone(),
        }
    }
}

impl<A: intrusive_queue::Adapter> Drop for Sender<A> {
    fn drop(&mut self) {
        if self.shared.sender_count.fetch_sub(1, Ordering::Release) == 1 {
            self.shared.wake_receiver();
        }
    }
}

impl<A: intrusive_queue::Adapter> Sender<A> {
    #[inline(always)]
    fn next_shard(&mut self) -> usize {
        let shard = self.next_shard;
        self.next_shard = (shard + self.shared.sender_stride) & self.shared.shard_mask;
        shard
    }

    #[inline(always)]
    fn send_to_shard(&self, shard: usize, value: A::Pointer) -> Result<(), A::Pointer> {
        if !self.shared.is_open.load(Ordering::Acquire) {
            return Err(value);
        }

        let mut queue = self.shared.shards[shard].lock();
        let was_empty = queue.is_empty();
        queue.push_back(value);
        drop(queue);

        if was_empty {
            self.shared.set_occupied(shard);
            self.shared.wake_receiver();
        }

        Ok(())
    }

    pub fn send_batch(
        &mut self,
        mut list: intrusive_queue::List<A>,
    ) -> Result<(), intrusive_queue::List<A>> {
        if list.is_empty() {
            return Ok(());
        }

        if !self.shared.is_open.load(Ordering::Acquire) {
            return Err(list);
        }

        let shard = self.next_shard();
        let mut queue = self.shared.shards[shard].lock();
        let was_empty = queue.is_empty();
        queue.append(&mut list);
        drop(queue);

        if was_empty {
            self.shared.set_occupied(shard);
            self.shared.wake_receiver();
        }

        Ok(())
    }
}

impl<A: intrusive_queue::Adapter> super::super::UnboundedSender<A::Pointer> for Sender<A> {
    #[inline(always)]
    fn send(&mut self, value: A::Pointer) -> Result<(), A::Pointer> {
        let shard = self.next_shard();
        self.send_to_shard(shard, value)
    }
}

impl<A: intrusive_queue::Adapter> super::super::Sender<A::Pointer> for Sender<A> {
    #[inline(always)]
    fn poll_send(
        &mut self,
        _cx: &mut core::task::Context<'_>,
        slot: &mut core::mem::MaybeUninit<A::Pointer>,
    ) -> Poll<Result<(), ()>> {
        if !self.shared.is_open.load(Ordering::Acquire) {
            return Poll::Ready(Err(()));
        }

        let value = unsafe { slot.assume_init_read() };
        match <Self as super::super::UnboundedSender<A::Pointer>>::send(self, value) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(value) => {
                slot.write(value);
                Poll::Ready(Err(()))
            }
        }
    }
}

pub struct Receiver<A: intrusive_queue::Adapter> {
    next_shard: usize,
    shared: Arc<Shared<A>>,
}

impl<A: intrusive_queue::Adapter> Drop for Receiver<A> {
    fn drop(&mut self) {
        self.shared.is_open.store(false, Ordering::Release);
    }
}

impl<A: intrusive_queue::Adapter> Receiver<A> {
    /// Registers the receiver waker.
    ///
    /// This channel expects the receiver to register once before exposing senders. Subsequent
    /// registrations are ignored.
    pub fn register(&self, waker: &Waker) {
        let _ = self.shared.recv_waker.set(waker.clone());
    }

    #[inline(always)]
    fn try_recv(&mut self) -> Option<intrusive_queue::List<A>> {
        let shard_count = self.shared.shards.len();

        for offset in 0..shard_count {
            let shard = (self.next_shard + offset) & self.shared.shard_mask;
            let (word, bit) = Shared::<A>::bit(shard);

            if self.shared.occupancy[word].load(Ordering::Acquire) & bit == 0 {
                continue;
            }

            let mut queue = self.shared.shards[shard].lock();
            if queue.is_empty() {
                // Clear while holding the shard lock so a sender cannot enqueue before the bit
                // is cleared.
                self.shared.clear_occupied(shard);
                drop(queue);
                continue;
            }

            let list = core::mem::take(&mut *queue);
            // Clear while holding the shard lock so any following sender observes an empty queue
            // and sets the bit again after enqueueing.
            self.shared.clear_occupied(shard);
            drop(queue);
            self.next_shard = (shard + 1) & self.shared.shard_mask;
            return Some(list);
        }

        None
    }
}

impl<A: intrusive_queue::Adapter> super::super::Receiver<intrusive_queue::List<A>> for Receiver<A> {
    #[inline(always)]
    fn poll_recv(
        &mut self,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<Option<intrusive_queue::List<A>>> {
        if self.shared.recv_waker.get().is_none() {
            self.register(cx.waker());
        }

        if let Some(list) = self.try_recv() {
            return Poll::Ready(Some(list));
        }

        if self.shared.sender_count.load(Ordering::Acquire) == 0 {
            if let Some(list) = self.try_recv() {
                return Poll::Ready(Some(list));
            }

            return Poll::Ready(None);
        }

        Poll::Pending
    }

    #[inline(always)]
    fn on_consumed(&mut self, _bytes: u64) {}
}
