// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Send-safe sharded intrusive queue channel for normal async runtimes.
//!
//! The sender has no backpressure - it can always push lists to one of the shards. The receiver
//! drains one shard at a time, returning the entire list. Receivers are expected to register their
//! waker immediately after channel creation and before cloning or exposing senders.

use crate::intrusive_queue;
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    task::{Poll, Waker},
};
use parking_lot::Mutex;
use std::sync::Arc;

const MAX_SHARDS_PER_POLL: usize = 16;

struct Shard<A: intrusive_queue::Adapter> {
    is_open: bool,
    queue: intrusive_queue::List<A>,
}

struct Shared<A: intrusive_queue::Adapter> {
    sender_count: AtomicUsize,
    next_sender_shard: AtomicUsize,
    sender_stride: usize,
    shard_mask: usize,
    occupancy: Box<[AtomicU64]>,
    // Initialized to a noop waker and updated by the receiver before senders are exposed.
    recv_waker: UnsafeCell<Waker>,
    shards: Box<[Mutex<Shard<A>>]>,
}

// SAFETY: `recv_waker` is initialized to a noop waker and only mutated by the receiver before
// senders are exposed. Senders only read it to wake the receiver. This makes shared references safe.
// The type is also safe to send between threads because all fields are `Send` under `A::Pointer:
// Send`; the waker cell is moved with `Shared`, and callers must complete waker mutation before
// exposing senders that can concurrently read it.
unsafe impl<A: intrusive_queue::Adapter> Sync for Shared<A> where A::Pointer: Send {}
unsafe impl<A: intrusive_queue::Adapter> Send for Shared<A> where A::Pointer: Send {}

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
    fn occupancy_word_and_bit(shard: usize) -> (usize, u64) {
        // Map a shard index to its occupancy word and bit in the bitmap.
        let word = shard / u64::BITS as usize;
        let bit = 1 << (shard % u64::BITS as usize);
        (word, bit)
    }

    #[inline(always)]
    fn set_occupied(&self, shard: usize) {
        let (word, bit) = Self::occupancy_word_and_bit(shard);
        self.occupancy[word].fetch_or(bit, Ordering::Release);
    }

    #[inline(always)]
    fn wake_receiver(&self) {
        // SAFETY: The receiver initializes the waker before senders are exposed. Senders only read
        // the waker after that point.
        unsafe { (&*self.recv_waker.get()).wake_by_ref() };
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

/// Creates a sharded intrusive queue channel.
///
/// Call [`Receiver::register`] immediately after creation and before cloning or exposing the
/// returned sender to another thread.
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
    let local_occupancy = vec![0; occupancy_len].into_boxed_slice();
    let shards = (0..shard_count)
        .map(|_| {
            Mutex::new(Shard {
                is_open: true,
                queue: intrusive_queue::List::new(),
            })
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let sender_stride = sender_stride(shard_count);
    let shared = Arc::new(Shared {
        sender_count: AtomicUsize::new(1),
        next_sender_shard: AtomicUsize::new(0),
        sender_stride,
        shard_mask: shard_count - 1,
        occupancy,
        recv_waker: UnsafeCell::new(s2n_quic_core::task::waker::noop()),
        shards,
    });

    let sender = Sender {
        next_shard: shared.allocate_sender_shard(),
        shared: shared.clone(),
    };
    let receiver = Receiver {
        next_shard: 0,
        local_occupancy,
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
        // The creation-time stride spreads senders out; each sender then walks adjacent shards to
        // avoid repeatedly colliding with other senders using the same stride.
        self.next_shard = (shard + 1) & self.shared.shard_mask;
        shard
    }

    pub fn send_batch(
        &mut self,
        mut list: intrusive_queue::List<A>,
    ) -> Result<(), intrusive_queue::List<A>> {
        if list.is_empty() {
            return Ok(());
        }

        let shard = self.next_shard();
        let mut queue = self.shared.shards[shard].lock();

        if !queue.is_open {
            return Err(list);
        }

        let was_empty = queue.queue.is_empty();
        queue.queue.append(&mut list);
        drop(queue);

        if was_empty {
            self.shared.set_occupied(shard);
            self.shared.wake_receiver();
        }

        Ok(())
    }
}

impl<A: intrusive_queue::Adapter> super::super::UnboundedSender<intrusive_queue::List<A>>
    for Sender<A>
{
    #[inline(always)]
    fn send(&mut self, list: intrusive_queue::List<A>) -> Result<(), intrusive_queue::List<A>> {
        self.send_batch(list)
    }
}

impl<A: intrusive_queue::Adapter> super::super::Sender<intrusive_queue::List<A>> for Sender<A> {
    #[inline(always)]
    fn poll_send(
        &mut self,
        _cx: &mut core::task::Context<'_>,
        slot: &mut core::mem::MaybeUninit<intrusive_queue::List<A>>,
    ) -> Poll<Result<(), ()>> {
        // SAFETY: the Sender trait requires callers to provide an initialized slot.
        let list = unsafe { slot.assume_init_read() };
        match self.send_batch(list) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(list) => {
                slot.write(list);
                Poll::Ready(Err(()))
            }
        }
    }
}

pub struct Receiver<A: intrusive_queue::Adapter> {
    next_shard: usize,
    local_occupancy: Box<[u64]>,
    shared: Arc<Shared<A>>,
}

impl<A: intrusive_queue::Adapter> Drop for Receiver<A> {
    fn drop(&mut self) {
        for shard in self.shared.shards.iter() {
            shard.lock().is_open = false;
        }
    }
}

impl<A: intrusive_queue::Adapter> Receiver<A> {
    /// Registers the receiver waker.
    ///
    /// This channel expects the receiver to register immediately after channel creation, before any
    /// sender is cloned or exposed to another thread.
    pub fn register(&self, waker: &Waker) {
        // SAFETY: callers must complete registration before cloning or exposing senders. After
        // that point, senders may concurrently read the waker.
        unsafe { *self.shared.recv_waker.get() = waker.clone() };
    }

    #[inline(always)]
    fn ensure_local_occupancy(&mut self) -> bool {
        if self.local_occupancy.iter().any(|word| *word != 0) {
            return true;
        }

        for (local, shared) in self
            .local_occupancy
            .iter_mut()
            .zip(self.shared.occupancy.iter())
        {
            *local |= shared.swap(0, Ordering::AcqRel);
        }

        self.local_occupancy.iter().any(|word| *word != 0)
    }

    #[inline(always)]
    fn try_recv(&mut self) -> TryRecv<A> {
        if !self.ensure_local_occupancy() {
            return TryRecv::Empty;
        }

        let shard_count = self.shared.shards.len();
        let iterations = shard_count.min(MAX_SHARDS_PER_POLL);

        for _ in 0..iterations {
            let shard = self.next_shard;
            self.next_shard = (shard + 1) & self.shared.shard_mask;
            let (word, bit) = Shared::<A>::occupancy_word_and_bit(shard);

            if self.local_occupancy[word] & bit == 0 {
                continue;
            }
            self.local_occupancy[word] &= !bit;

            let mut queue = self.shared.shards[shard].lock();
            if queue.queue.is_empty() {
                continue;
            }

            let list = core::mem::take(&mut queue.queue);
            return TryRecv::Ready(list);
        }

        if self.ensure_local_occupancy() {
            TryRecv::Yield
        } else {
            TryRecv::Empty
        }
    }
}

impl<A: intrusive_queue::Adapter> super::super::Receiver<intrusive_queue::List<A>> for Receiver<A> {
    #[inline(always)]
    fn poll_recv(
        &mut self,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<Option<intrusive_queue::List<A>>> {
        match self.try_recv() {
            TryRecv::Ready(list) => return Poll::Ready(Some(list)),
            TryRecv::Yield => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            TryRecv::Empty => {}
        }

        if self.shared.sender_count.load(Ordering::Acquire) == 0 {
            if let TryRecv::Ready(list) = self.try_recv() {
                return Poll::Ready(Some(list));
            }

            return Poll::Ready(None);
        }

        Poll::Pending
    }

    #[inline(always)]
    fn on_consumed(&mut self, _bytes: u64) {}
}

enum TryRecv<A: intrusive_queue::Adapter> {
    Ready(intrusive_queue::List<A>),
    Empty,
    Yield,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        intrusive_queue::{Entry, Queue},
        socket::channel::{Receiver as _, UnboundedSender as _},
    };
    use core::task::Poll;

    fn noop_cx() -> core::task::Context<'static> {
        let waker = s2n_quic_core::task::waker::noop();
        let waker_ref = Box::leak(Box::new(waker));
        core::task::Context::from_waker(waker_ref)
    }

    fn list(values: impl IntoIterator<Item = u32>) -> Queue<u32> {
        let mut list = Queue::new();
        for value in values {
            list.push_back(Entry::new(value));
        }
        list
    }

    fn values(list: &Queue<u32>) -> Vec<u32> {
        list.iter().copied().collect()
    }

    #[test]
    #[should_panic(expected = "shard count must be a power of two")]
    fn rejects_non_power_of_two_shards() {
        let _ = new::<u32>(3);
    }

    #[test]
    fn drains_entire_shard() {
        let (mut tx, mut rx) = new::<u32>(1);
        let mut cx = noop_cx();

        assert!(matches!(rx.poll_recv(&mut cx), Poll::Pending));

        tx.send(list([1, 2, 3])).unwrap();

        let Poll::Ready(Some(list)) = rx.poll_recv(&mut cx) else {
            panic!("expected drained list");
        };
        assert_eq!(values(&list), vec![1, 2, 3]);

        assert!(matches!(rx.poll_recv(&mut cx), Poll::Pending));
    }

    #[test]
    fn sender_creation_selects_initial_shard() {
        let (mut tx0, mut rx) = new::<u32>(4);
        let mut tx1 = tx0.clone();
        let mut tx2 = tx0.clone();
        let mut tx3 = tx0.clone();
        let mut cx = noop_cx();

        assert!(matches!(rx.poll_recv(&mut cx), Poll::Pending));

        tx3.send(list([3])).unwrap();
        tx2.send(list([2])).unwrap();
        tx1.send(list([1])).unwrap();
        tx0.send(list([0])).unwrap();

        let mut received = vec![];
        for _ in 0..4 {
            let Poll::Ready(Some(list)) = rx.poll_recv(&mut cx) else {
                panic!("expected drained list");
            };
            assert_eq!(list.len(), 1);
            received.push(*list.front().unwrap());
        }

        assert_eq!(received, vec![0, 1, 2, 3]);
    }

    #[test]
    fn sender_round_robins_locally_by_one() {
        let (mut tx, mut rx) = new::<u32>(4);
        let mut cx = noop_cx();

        for value in 0..4 {
            tx.send(list([value])).unwrap();
        }

        for expected in 0..4 {
            let Poll::Ready(Some(list)) = rx.poll_recv(&mut cx) else {
                panic!("expected drained list");
            };
            assert_eq!(values(&list), vec![expected]);
        }
    }

    #[test]
    fn sender_drop_closes_receiver() {
        let (tx, mut rx) = new::<u32>(2);
        let mut cx = noop_cx();

        assert!(matches!(rx.poll_recv(&mut cx), Poll::Pending));
        drop(tx);
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(None)));
    }

    #[test]
    fn receiver_yields_after_bounded_scan() {
        let (_tx, mut rx) = new::<u32>(MAX_SHARDS_PER_POLL * 2);
        let mut cx = noop_cx();

        // Simulate a stale receiver-local occupancy snapshot for more shards than one poll is
        // allowed to scan, without having to enqueue into every shard.
        rx.local_occupancy[0] = !0;

        assert!(matches!(rx.poll_recv(&mut cx), Poll::Pending));
        assert_ne!(rx.local_occupancy[0], 0);
    }

    #[test]
    fn receiver_drop_closes_sender() {
        let (mut tx, rx) = new::<u32>(2);
        drop(rx);

        assert_eq!(tx.send(list([1])).unwrap_err().len(), 1);
    }

    #[test]
    fn loom_concurrent_send_recv() {
        loom::model(|| {
            let (mut tx0, mut rx) = new::<u32>(2);
            let waker = s2n_quic_core::task::waker::noop();
            // Register before cloning/exposing senders so sender wakeups only read the waker.
            rx.register(&waker);
            let mut tx1 = tx0.clone();

            let a = loom::thread::spawn(move || tx0.send(list([1])).unwrap());
            let b = loom::thread::spawn(move || tx1.send(list([2])).unwrap());

            a.join().unwrap();
            b.join().unwrap();

            let mut cx = noop_cx();
            let mut received = vec![];
            while let Poll::Ready(Some(list)) = rx.poll_recv(&mut cx) {
                received.extend(values(&list));
            }
            received.sort_unstable();
            assert_eq!(received, vec![1, 2]);
        });
    }
}
