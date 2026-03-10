// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Single-entry channels for connecting wheel ticker tasks to a socket sender.

use core::task::Poll;
use std::marker::PhantomData;

/// A channel sender.
pub trait Sender<T> {
    /// Sends the value on the channel. Returns `Err` if the channel is closed.
    async fn send(&mut self, value: T) -> Result<(), T>;
}

/// An async-capable channel receiver.
pub trait Receiver<T> {
    /// Poll for the next value. Registers the waker if nothing is available.
    ///
    /// Returns `Ready(Some(value))` when a value is available,
    /// `Pending` when empty but not closed,
    /// `Ready(None)` when the channel is closed.
    fn poll_recv(&mut self, cx: &mut core::task::Context<'_>) -> Poll<Option<T>>;

    /// Receives the next value. Returns `None` when the channel is closed.
    fn recv(&mut self) -> impl core::future::Future<Output = Option<T>> + '_ {
        core::future::poll_fn(|cx| self.poll_recv(cx))
    }
}

// ── Busy-poll cell channel ─────────────────────────────────────────────────

/// A non-Send SPSC channel with capacity 1, backed by a plain `UnsafeCell`.
///
/// This implementation assumes futures are busy polled. As such, wakers are not
/// used at all.
pub mod cell {
    use std::{cell::UnsafeCell, future::poll_fn, rc::Rc, task::Poll};

    struct Shared<T> {
        value: UnsafeCell<Option<T>>,
    }

    impl<T> Shared<T> {
        #[inline(always)]
        fn poll(self: &Rc<Self>, f: impl FnOnce(&Option<T>) -> bool) -> Poll<Result<(), ()>> {
            if Rc::strong_count(self) != 2 {
                return Poll::Ready(Err(()));
            }

            unsafe {
                // SAFETY: the Cell is non-Send
                if !f(&*self.value.get()) {
                    return Poll::Pending;
                }

                Poll::Ready(Ok(()))
            }
        }
    }

    pub fn new<T>() -> (Sender<T>, Receiver<T>) {
        let shared = Rc::new(Shared {
            value: UnsafeCell::new(None),
        });
        (
            Sender {
                shared: shared.clone(),
            },
            Receiver { shared },
        )
    }

    pub struct Sender<T> {
        shared: Rc<Shared<T>>,
    }

    impl<T> super::Sender<T> for Sender<T> {
        #[inline(always)]
        async fn send(&mut self, value: T) -> Result<(), T> {
            let res = poll_fn(|_cx| self.shared.poll(|v| v.is_none())).await;

            match res {
                Ok(()) => {
                    unsafe {
                        // SAFETY: the Cell is non-Send and we just checked that it was empty
                        let cell = &mut *self.shared.value.get();
                        *cell = Some(value);
                    }
                    Ok(())
                }
                Err(()) => Err(value),
            }
        }
    }

    pub struct Receiver<T> {
        shared: Rc<Shared<T>>,
    }

    impl<T> super::Receiver<T> for Receiver<T> {
        #[inline(always)]
        fn poll_recv(&mut self, _cx: &mut core::task::Context<'_>) -> core::task::Poll<Option<T>> {
            match self.shared.poll(|v| v.is_some()) {
                Poll::Ready(Ok(())) => {
                    unsafe {
                        // SAFETY: the Cell is non-Send and we just checked that it was non-empty
                        Poll::Ready(core::mem::take(&mut *self.shared.value.get()))
                    }
                }
                Poll::Ready(Err(())) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            }
        }
    }
}

// ── Waker-based slot channel ───────────────────────────────────────────────

/// A `Send`-friendly SPSC channel with capacity 1, backed by `Mutex`+`Waker`.
///
/// For use with normal async runtimes (tokio, bach, etc.) where the sender
/// and receiver may live on different threads/tasks.
///
/// Currently only used in tests
#[cfg(test)]
pub mod slot {
    use core::{mem::ManuallyDrop, task::Poll};
    use parking_lot::Mutex;
    use std::sync::Arc;

    struct Inner<T> {
        value: Option<T>,
        recv_waker: Option<core::task::Waker>,
        send_waker: Option<core::task::Waker>,
    }

    struct Shared<T> {
        inner: Mutex<Inner<T>>,
    }

    impl<T> Shared<T> {
        /// Returns `true` if both sender and receiver are still alive.
        #[inline]
        fn is_alive(self: &Arc<Self>) -> bool {
            Arc::strong_count(self) == 2
        }
    }

    pub fn new<T>() -> (Sender<T>, Receiver<T>) {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                value: None,
                recv_waker: None,
                send_waker: None,
            }),
        });
        (
            Sender {
                shared: ManuallyDrop::new(shared.clone()),
            },
            Receiver {
                shared: ManuallyDrop::new(shared),
            },
        )
    }

    pub struct Sender<T> {
        shared: ManuallyDrop<Arc<Shared<T>>>,
    }

    impl<T> Drop for Sender<T> {
        fn drop(&mut self) {
            // Wake the receiver so it can observe the closed state
            let mut guard = self.shared.inner.lock();
            let waker = core::mem::take(&mut guard.recv_waker);
            drop(guard);
            unsafe {
                ManuallyDrop::drop(&mut self.shared);
            }

            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    impl<T> super::Sender<T> for Sender<T> {
        async fn send(&mut self, value: T) -> Result<(), T> {
            let mut value = Some(value);
            core::future::poll_fn(|cx| {
                if !self.shared.is_alive() {
                    return Poll::Ready(Err(value.take().unwrap()));
                }

                let mut guard = self.shared.inner.lock();

                if guard.value.is_none() {
                    guard.value = value.take();
                    if let Some(waker) = guard.recv_waker.take() {
                        drop(guard);
                        waker.wake();
                    }
                    return Poll::Ready(Ok(()));
                }

                // Slot full — register waker for when receiver drains
                guard.send_waker = Some(cx.waker().clone());
                Poll::Pending
            })
            .await
        }
    }

    pub struct Receiver<T> {
        shared: ManuallyDrop<Arc<Shared<T>>>,
    }

    impl<T> Drop for Receiver<T> {
        fn drop(&mut self) {
            // Wake the receiver so it can observe the closed state
            let mut guard = self.shared.inner.lock();
            let waker = core::mem::take(&mut guard.send_waker);
            drop(guard);
            unsafe {
                ManuallyDrop::drop(&mut self.shared);
            }

            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    impl<T> super::Receiver<T> for Receiver<T> {
        fn poll_recv(&mut self, cx: &mut core::task::Context<'_>) -> Poll<Option<T>> {
            let mut guard = self.shared.inner.lock();
            if let Some(value) = guard.value.take() {
                if let Some(waker) = guard.send_waker.take() {
                    drop(guard);
                    waker.wake();
                }
                return Poll::Ready(Some(value));
            }
            if !self.shared.is_alive() {
                return Poll::Ready(None);
            }
            guard.recv_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

// ── Priority merging receiver ──────────────────────────────────────────────

/// Merges multiple receivers, always polling the highest-priority (lowest-index)
/// channel first.
///
/// When `poll_recv` is called, receivers are checked in order from index 0
/// (highest priority) to the last. The first `Ready(Some(..))` value is returned.
/// If all receivers return `Pending`, `Pending` is returned. If all receivers
/// are closed (`Ready(None)`), `Ready(None)` is returned.
pub struct Priority<R> {
    receivers: Vec<R>,
}

impl<R> Priority<R> {
    pub fn new(receivers: Vec<R>) -> Self {
        Self { receivers }
    }
}

impl<T, R: Receiver<T>> Receiver<T> for Priority<R> {
    fn poll_recv(&mut self, cx: &mut core::task::Context<'_>) -> Poll<Option<T>> {
        let mut all_closed = true;
        for rx in &mut self.receivers {
            match rx.poll_recv(cx) {
                Poll::Ready(Some(value)) => return Poll::Ready(Some(value)),
                Poll::Pending => {
                    all_closed = false;
                }
                Poll::Ready(None) => {
                    // This channel is closed, continue to lower priority
                }
            }
        }
        if all_closed {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

// ── Flatten adapter ────────────────────────────────────────────────────────

/// Wraps a `Receiver<Queue<T>>` and implements `Receiver<Entry<T>>`.
///
/// When `recv` is called, it first drains any buffered entries from the
/// current queue. Once the queue is exhausted, it pulls the next queue
/// from the inner receiver.
pub struct Flatten<T, R> {
    inner: R,
    queue: crate::intrusive_queue::Queue<T>,
}

impl<T, R> Flatten<T, R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            queue: Default::default(),
        }
    }
}

impl<T, R> Receiver<crate::intrusive_queue::Entry<T>> for Flatten<T, R>
where
    R: Receiver<crate::intrusive_queue::Queue<T>>,
{
    fn poll_recv(
        &mut self,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<Option<crate::intrusive_queue::Entry<T>>> {
        loop {
            // Drain any buffered entries first
            if let Some(entry) = self.queue.pop_front() {
                return Poll::Ready(Some(entry));
            }

            // Try to pull the next queue from the inner receiver
            match self.inner.poll_recv(cx) {
                Poll::Ready(Some(queue)) => {
                    self.queue = queue;
                    continue;
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Calls an inspect function when the inner receiver returns a value.
///
/// Useful for side effects like calling `wheel.on_send()` after a flatten,
/// before the priority merge.
pub struct Inspect<T, R, F> {
    inner: R,
    inspect: F,
    _value: PhantomData<T>,
}

impl<T, R, F> Inspect<T, R, F> {
    pub fn new(inner: R, inspect: F) -> Self {
        Self {
            inner,
            inspect,
            _value: PhantomData,
        }
    }
}

impl<T, R, F> Receiver<T> for Inspect<T, R, F>
where
    R: Receiver<T>,
    F: Fn(&T),
{
    fn poll_recv(&mut self, cx: &mut core::task::Context<'_>) -> Poll<Option<T>> {
        match self.inner.poll_recv(cx) {
            Poll::Ready(Some(value)) => {
                (self.inspect)(&value);
                Poll::Ready(Some(value))
            }
            other => other,
        }
    }
}

// ── Reporter adapter ───────────────────────────────────────────────────────

/// Wraps a `Receiver<Entry<Info, Meta, Completion>>` and periodically logs
/// the send throughput. Passes entries through transparently.
///
/// Uses a precision `Clock` to get wall-clock time for rate calculation,
/// since entry timestamps may lag due to wheel backpressure.
pub struct Reporter<Info, Meta, Completion, R, Clk> {
    inner: R,
    clock: Clk,
    last_emit: crate::clock::precision::Timestamp,
    next_emit: crate::clock::precision::Timestamp,
    sent: u64,
    _phantom: core::marker::PhantomData<(Info, Meta, Completion)>,
}

impl<Info, Meta, Completion, R, Clk> Reporter<Info, Meta, Completion, R, Clk>
where
    Clk: crate::clock::precision::Clock,
{
    pub fn new(inner: R, clock: Clk) -> Self {
        let now = clock.now();
        Self {
            inner,
            clock,
            last_emit: now,
            next_emit: now + std::time::Duration::from_secs(1),
            sent: 0,
            _phantom: core::marker::PhantomData,
        }
    }

    fn on_send(&mut self, len: u16) {
        // if !cfg!(debug_assertions) {
        //     return;
        // }

        self.sent += len as u64;

        let now = self.clock.now();
        if now < self.next_emit {
            return;
        }
        if self.sent > 0 {
            let elapsed_nanos = now.nanos_since(self.last_emit) as f64;
            let elapsed = elapsed_nanos / 1_000_000_000.0;
            let mut rate = self.sent as f64 * 8.0 / elapsed;
            let prefixes = [("G", 1e9), ("M", 1e6), ("K", 1e3)];
            let mut prefix = "";
            for (pref, divisor) in prefixes {
                if rate > divisor {
                    rate /= divisor;
                    prefix = pref;
                    break;
                }
            }
            tracing::info!("{now}: {rate:.2} {prefix}bps");
        }
        self.last_emit = now;
        self.next_emit = now + std::time::Duration::from_secs(1);
        self.sent = 0;
    }
}

impl<Info, Meta, C, R, Clk> Receiver<crate::socket::send::transmission::Entry<Info, Meta, C>>
    for Reporter<Info, Meta, C, R, Clk>
where
    R: Receiver<crate::socket::send::transmission::Entry<Info, Meta, C>>,
    Clk: crate::clock::precision::Clock,
{
    fn poll_recv(
        &mut self,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<Option<crate::socket::send::transmission::Entry<Info, Meta, C>>> {
        match self.inner.poll_recv(cx) {
            Poll::Ready(Some(entry)) => {
                self.on_send(entry.total_len);
                Poll::Ready(Some(entry))
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{intrusive_queue, testing::sim};
    use core::{future::Future, pin::pin};

    fn noop_cx() -> core::task::Context<'static> {
        let waker = s2n_quic_core::task::waker::noop();
        let waker = Box::leak(Box::new(waker));
        core::task::Context::from_waker(waker)
    }

    // ── cell tests (poll-based only, since cell is !Send) ──────────────

    #[test]
    fn cell_poll_recv_empty_returns_pending() {
        let (_tx, mut rx) = cell::new::<u32>();
        let mut cx = noop_cx();
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Pending));
    }

    #[test]
    fn cell_sender_drop_closes_receiver() {
        let (tx, mut rx) = cell::new::<u32>();
        drop(tx);
        let mut cx = noop_cx();
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(None)));
    }

    #[test]
    fn cell_receiver_drop_closes_sender() {
        // Sender's poll sees closed when receiver drops
        let (mut tx, rx) = cell::new::<u32>();
        drop(rx);
        // Use poll_fn to test send
        let mut cx = noop_cx();
        let mut fut = pin!(tx.send(42));
        assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(Err(42)));
    }

    #[test]
    fn cell_send_recv_poll_roundtrip() {
        let (mut tx, mut rx) = cell::new::<u32>();
        let mut cx = noop_cx();

        // Empty — pending
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Pending));

        // Send via poll
        {
            let mut fut = pin!(tx.send(42));
            assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
        }

        // Receive
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(Some(42))));

        // Empty again
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Pending));

        // Send again
        {
            let mut fut = pin!(tx.send(99));
            assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
        }
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(Some(99))));

        // Drop sender → closed
        drop(tx);
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(None)));
    }

    #[test]
    fn cell_backpressure_poll() {
        let (mut tx, mut rx) = cell::new::<u32>();
        let mut cx = noop_cx();

        // Send one value
        {
            let mut fut = pin!(tx.send(1));
            assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
        }

        // Slot is full — send returns Pending
        {
            let mut fut = pin!(tx.send(2));
            assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
        }

        // Drain the slot
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(Some(1))));

        // Now send succeeds
        {
            let mut fut = pin!(tx.send(3));
            assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
        }
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(Some(3))));
    }

    // ── slot tests ─────────────────────────────────────────────────────

    #[test]
    fn slot_poll_recv_empty_returns_pending() {
        let (_tx, mut rx) = slot::new::<u32>();
        let mut cx = noop_cx();
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Pending));
    }

    #[test]
    fn slot_sender_drop_closes_receiver() {
        let (tx, mut rx) = slot::new::<u32>();
        drop(tx);
        let mut cx = noop_cx();
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(None)));
    }

    #[test]
    fn slot_receiver_drop_closes_sender() {
        sim(|| {
            use crate::testing::ext::*;

            async {
                let (mut tx, rx) = slot::new::<u32>();
                drop(rx);
                let result = tx.send(42).await;
                assert_eq!(result, Err(42));
            }
            .primary()
            .spawn();
        });
    }

    #[test]
    fn slot_send_recv_roundtrip() {
        sim(|| {
            use crate::testing::ext::*;

            async {
                let (mut tx, mut rx) = slot::new::<u32>();

                tx.send(42).await.unwrap();
                let val = rx.recv().await;
                assert_eq!(val, Some(42));

                tx.send(99).await.unwrap();
                let val = rx.recv().await;
                assert_eq!(val, Some(99));

                drop(tx);
                let val = rx.recv().await;
                assert_eq!(val, None);
            }
            .primary()
            .spawn();
        });
    }

    #[test]
    fn slot_backpressure() {
        let (mut tx, mut rx) = slot::new::<u32>();
        let mut cx = noop_cx();

        // Send one value via poll
        {
            let mut fut = pin!(tx.send(1));
            assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
        }

        assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(Some(1))));
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Pending));

        {
            let mut fut = pin!(tx.send(2));
            assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
        }
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(Some(2))));
    }

    #[test]
    fn slot_concurrent_send_recv() {
        sim(|| {
            use crate::testing::ext::*;

            let (mut tx, mut rx) = slot::new::<u32>();

            crate::testing::spawn(async move {
                for i in 0..10 {
                    tx.send(i).await.unwrap();
                }
            });

            async move {
                for expected in 0..10 {
                    let val = rx.recv().await.unwrap();
                    assert_eq!(val, expected);
                }
                let val = rx.recv().await;
                assert_eq!(val, None);
            }
            .primary()
            .spawn();
        });
    }

    // ── Priority tests (using slot since sim requires Send) ────────────

    #[test]
    fn priority_empty_returns_pending() {
        let (_tx0, rx0) = slot::new::<u32>();
        let (_tx1, rx1) = slot::new::<u32>();
        let mut priority = Priority::new(vec![rx0, rx1]);

        let mut cx = noop_cx();
        assert!(matches!(priority.poll_recv(&mut cx), Poll::Pending));
    }

    #[test]
    fn priority_all_closed_returns_none() {
        let (tx0, rx0) = slot::new::<u32>();
        let (tx1, rx1) = slot::new::<u32>();
        drop(tx0);
        drop(tx1);
        let mut priority = Priority::new(vec![rx0, rx1]);

        let mut cx = noop_cx();
        assert!(matches!(priority.poll_recv(&mut cx), Poll::Ready(None)));
    }

    #[test]
    fn priority_high_wins_over_low() {
        sim(|| {
            use crate::testing::ext::*;

            async {
                let (mut tx0, rx0) = slot::new::<u32>();
                let (mut tx1, rx1) = slot::new::<u32>();
                let mut priority = Priority::new(vec![rx0, rx1]);

                tx0.send(10).await.unwrap();
                tx1.send(20).await.unwrap();

                let val = priority.recv().await;
                assert_eq!(val, Some(10));

                let val = priority.recv().await;
                assert_eq!(val, Some(20));
            }
            .primary()
            .spawn();
        });
    }

    #[test]
    fn priority_low_priority_works_when_high_empty() {
        sim(|| {
            use crate::testing::ext::*;

            async {
                let (_tx0, rx0) = slot::new::<u32>();
                let (mut tx1, rx1) = slot::new::<u32>();
                let mut priority = Priority::new(vec![rx0, rx1]);

                tx1.send(99).await.unwrap();

                let val = priority.recv().await;
                assert_eq!(val, Some(99));
            }
            .primary()
            .spawn();
        });
    }

    #[test]
    fn priority_partial_close() {
        sim(|| {
            use crate::testing::ext::*;

            async {
                let (tx0, rx0) = slot::new::<u32>();
                let (mut tx1, rx1) = slot::new::<u32>();
                let mut priority = Priority::new(vec![rx0, rx1]);

                drop(tx0);

                tx1.send(42).await.unwrap();
                let val = priority.recv().await;
                assert_eq!(val, Some(42));

                drop(tx1);
                let val = priority.recv().await;
                assert_eq!(val, None);
            }
            .primary()
            .spawn();
        });
    }

    // ── Flatten tests (using slot since sim requires Send) ─────────────

    #[test]
    fn flatten_drains_queue_then_fetches_next() {
        sim(|| {
            use crate::testing::ext::*;

            async {
                let (mut tx, rx) = slot::new::<intrusive_queue::Queue<u32>>();
                let mut flat = Flatten::new(rx);

                let mut queue = intrusive_queue::Queue::default();
                queue.push_back(intrusive_queue::Entry::new(10));
                queue.push_back(intrusive_queue::Entry::new(20));
                queue.push_back(intrusive_queue::Entry::new(30));

                assert!(tx.send(queue).await.is_ok());

                assert_eq!(*flat.recv().await.unwrap(), 10);
                assert_eq!(*flat.recv().await.unwrap(), 20);
                assert_eq!(*flat.recv().await.unwrap(), 30);

                let mut queue2 = intrusive_queue::Queue::default();
                queue2.push_back(intrusive_queue::Entry::new(40));
                assert!(tx.send(queue2).await.is_ok());

                assert_eq!(*flat.recv().await.unwrap(), 40);

                drop(tx);
                let val = flat.recv().await;
                assert!(val.is_none());
            }
            .primary()
            .spawn();
        });
    }

    #[test]
    fn flatten_empty_queue_skipped() {
        sim(|| {
            use crate::testing::ext::*;

            let (mut tx, rx) = slot::new::<intrusive_queue::Queue<u32>>();

            // Sender task: send empty queue, then non-empty queue
            crate::testing::spawn(async move {
                let empty_queue = intrusive_queue::Queue::default();
                assert!(tx.send(empty_queue).await.is_ok());

                let mut queue = intrusive_queue::Queue::default();
                queue.push_back(intrusive_queue::Entry::new(42));
                assert!(tx.send(queue).await.is_ok());
            });

            // Receiver task: should skip the empty queue and return 42
            async move {
                let mut flat = Flatten::new(rx);
                assert_eq!(*flat.recv().await.unwrap(), 42);
            }
            .primary()
            .spawn();
        });
    }
}
