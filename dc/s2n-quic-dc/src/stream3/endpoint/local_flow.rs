// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::waker;
use crate::socket::channel::UnboundedSender;
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::Waker,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StreamPriority {
    High = 0,
    #[default]
    Normal = 1,
    Low = 2,
}

impl StreamPriority {
    pub const LEVELS: usize = 3;
    pub const ALL: [Self; Self::LEVELS] = [Self::High, Self::Normal, Self::Low];

    #[inline]
    pub const fn as_index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CreditState {
    Queued,
    Inflight,
    Released,
}

pub struct Credit {
    controller: Arc<Controller>,
    priority: StreamPriority,
    bytes: u64,
    state: CreditState,
}

impl Credit {
    #[inline]
    pub fn new(controller: Arc<Controller>, priority: StreamPriority, bytes: u64) -> Self {
        Self {
            controller,
            priority,
            bytes,
            state: CreditState::Queued,
        }
    }

    #[inline]
    pub fn on_transmit(&mut self) {
        if self.bytes == 0 || self.state != CreditState::Queued {
            return;
        }

        self.controller
            .move_queued_to_inflight(self.priority, self.bytes);
        self.state = CreditState::Inflight;
    }

    #[inline]
    pub fn on_requeue(&mut self) {
        if self.bytes == 0 || self.state != CreditState::Inflight {
            return;
        }

        self.controller
            .move_inflight_to_queued(self.priority, self.bytes);
        self.state = CreditState::Queued;
    }

    #[inline]
    pub fn release(&mut self) {
        if self.bytes == 0 || self.state == CreditState::Released {
            return;
        }

        match self.state {
            CreditState::Queued => self.controller.release_queued(self.priority, self.bytes),
            CreditState::Inflight => self.controller.release_inflight(self.priority, self.bytes),
            CreditState::Released => {}
        }

        self.state = CreditState::Released;
    }
}

impl Drop for Credit {
    #[inline]
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug)]
struct Waiter {
    id: u64,
    requested: u64,
    waker: Waker,
}

pub struct Controller {
    queued_bytes: AtomicU64,
    inflight_bytes: AtomicU64,
    max_queued_bytes: u64,
    max_inflight_bytes: u64,
    next_waiter_id: AtomicU64,
    waiters: [Mutex<VecDeque<Waiter>>; StreamPriority::LEVELS],
    wake_sink: Mutex<Option<waker::Sink>>,
}

impl std::fmt::Debug for Controller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Controller")
            .field("queued_bytes", &self.queued_bytes())
            .field("inflight_bytes", &self.inflight_bytes())
            .field("max_queued_bytes", &self.max_queued_bytes)
            .field("max_inflight_bytes", &self.max_inflight_bytes)
            .finish_non_exhaustive()
    }
}

impl Controller {
    pub const DEFAULT_MAX_QUEUED_BYTES: u64 = 8 * 1024 * 1024;
    pub const DEFAULT_MAX_INFLIGHT_BYTES: u64 = 8 * 1024 * 1024;

    pub fn new(max_queued_bytes: u64, max_inflight_bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            queued_bytes: AtomicU64::new(0),
            inflight_bytes: AtomicU64::new(0),
            max_queued_bytes,
            max_inflight_bytes,
            next_waiter_id: AtomicU64::new(1),
            waiters: core::array::from_fn(|_| Mutex::new(VecDeque::new())),
            wake_sink: Mutex::new(None),
        })
    }

    #[inline]
    pub fn allocate_waiter_id(&self) -> u64 {
        self.next_waiter_id.fetch_add(1, Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn set_waker_sink(&self, wake_sink: waker::Sink) {
        *self.wake_sink.lock() = Some(wake_sink);
    }

    #[inline]
    pub fn queued_bytes(&self) -> u64 {
        self.queued_bytes.load(Ordering::Acquire)
    }

    #[inline]
    pub fn inflight_bytes(&self) -> u64 {
        self.inflight_bytes.load(Ordering::Acquire)
    }

    #[inline]
    pub fn clear_waiter(&self, waiter_id: u64, priority: StreamPriority) {
        let queue = &mut *self.waiters[priority.as_index()].lock();
        if let Some(pos) = queue.iter().position(|waiter| waiter.id == waiter_id) {
            queue.remove(pos);
        }
    }

    pub fn register_waiter(
        &self,
        waiter_id: u64,
        priority: StreamPriority,
        requested: u64,
        waker: &Waker,
    ) {
        if requested == 0 {
            return;
        }

        let queue = &mut *self.waiters[priority.as_index()].lock();
        if let Some(waiter) = queue.iter_mut().find(|waiter| waiter.id == waiter_id) {
            waiter.requested = requested;
            if !waiter.waker.will_wake(waker) {
                waiter.waker = waker.clone();
            }
            return;
        }

        queue.push_back(Waiter {
            id: waiter_id,
            requested,
            waker: waker.clone(),
        });
    }

    pub fn try_acquire(&self, waiter_id: u64, priority: StreamPriority, requested: u64) -> u64 {
        if requested == 0 {
            self.clear_waiter(waiter_id, priority);
            return 0;
        }

        let priority_idx = priority.as_index();
        for higher in StreamPriority::ALL[..priority_idx].iter().copied() {
            if !self.waiters[higher.as_index()].lock().is_empty() {
                return 0;
            }
        }

        {
            let queue = self.waiters[priority_idx].lock();
            if let Some(front) = queue.front() {
                if front.id != waiter_id {
                    return 0;
                }
            }
        }

        loop {
            let inflight = self.inflight_bytes.load(Ordering::Acquire);
            if inflight >= self.max_inflight_bytes {
                return 0;
            }

            let queued = self.queued_bytes.load(Ordering::Acquire);
            let available = self.max_queued_bytes.saturating_sub(queued);
            if available == 0 {
                return 0;
            }

            let granted = available.min(requested);
            let next = queued.saturating_add(granted);

            if self
                .queued_bytes
                .compare_exchange(queued, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.clear_waiter(waiter_id, priority);
                return granted;
            }
        }
    }

    #[inline]
    pub fn move_queued_to_inflight(&self, priority: StreamPriority, bytes: u64) {
        if bytes == 0 {
            return;
        }

        self.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.inflight_bytes.fetch_add(bytes, Ordering::AcqRel);
        self.wake_waiters(priority);
    }

    #[inline]
    pub fn move_inflight_to_queued(&self, _priority: StreamPriority, bytes: u64) {
        if bytes == 0 {
            return;
        }

        self.inflight_bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.queued_bytes.fetch_add(bytes, Ordering::AcqRel);
    }

    #[inline]
    pub fn release_queued(&self, priority: StreamPriority, bytes: u64) {
        if bytes == 0 {
            return;
        }

        self.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.wake_waiters(priority);
    }

    #[inline]
    pub fn release_inflight(&self, priority: StreamPriority, bytes: u64) {
        if bytes == 0 {
            return;
        }

        self.inflight_bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.wake_waiters(priority);
    }

    fn wake_waiters(&self, released_priority: StreamPriority) {
        let inflight = self.inflight_bytes.load(Ordering::Acquire);
        if inflight >= self.max_inflight_bytes {
            return;
        }

        let mut available = self
            .max_queued_bytes
            .saturating_sub(self.queued_bytes.load(Ordering::Acquire));
        if available == 0 {
            return;
        }

        let mut to_wake = Vec::new();
        for priority in StreamPriority::ALL {
            let queue = &mut *self.waiters[priority.as_index()].lock();
            while let Some(waiter) = queue.front() {
                if waiter.requested > available {
                    break;
                }

                let waiter = queue.pop_front().expect("front waiter must exist");
                available = available.saturating_sub(waiter.requested);
                to_wake.push(waiter.waker);
            }

            if priority.as_index() <= released_priority.as_index() && !to_wake.is_empty() {
                break;
            }
        }

        if to_wake.is_empty() {
            return;
        }

        if let Some(mut sink) = self.wake_sink.lock().clone() {
            for waker in to_wake {
                let _ = sink.send(waker);
            }
        } else {
            for waker in to_wake {
                waker.wake();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::task::{RawWaker, RawWakerVTable, Waker};
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    fn counting_waker(counter: Arc<AtomicUsize>) -> Waker {
        unsafe fn clone(data: *const ()) -> RawWaker {
            let counter = Arc::<AtomicUsize>::from_raw(data.cast());
            let cloned = counter.clone();
            let _ = Arc::into_raw(counter);
            RawWaker::new(Arc::into_raw(cloned).cast(), &VTABLE)
        }

        unsafe fn wake(data: *const ()) {
            let counter = Arc::<AtomicUsize>::from_raw(data.cast());
            counter.fetch_add(1, Ordering::Relaxed);
        }

        unsafe fn wake_by_ref(data: *const ()) {
            let counter = Arc::<AtomicUsize>::from_raw(data.cast());
            counter.fetch_add(1, Ordering::Relaxed);
            let _ = Arc::into_raw(counter);
        }

        unsafe fn drop(data: *const ()) {
            let _ = Arc::<AtomicUsize>::from_raw(data.cast());
        }

        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

        let raw = RawWaker::new(Arc::into_raw(counter).cast(), &VTABLE);
        unsafe { Waker::from_raw(raw) }
    }

    #[test]
    fn priority_waiters_gate_lower_priority_acquire() {
        let controller = Controller::new(64, 64);
        let high_id = controller.allocate_waiter_id();
        let low_id = controller.allocate_waiter_id();
        let high_wakes = Arc::new(AtomicUsize::new(0));

        controller.register_waiter(
            high_id,
            StreamPriority::High,
            32,
            &counting_waker(high_wakes.clone()),
        );

        assert_eq!(controller.try_acquire(low_id, StreamPriority::Low, 16), 0);

        controller.release_queued(StreamPriority::High, 1);

        assert_eq!(high_wakes.load(Ordering::Relaxed), 1);
        assert_eq!(controller.try_acquire(high_id, StreamPriority::High, 32), 32);
    }

    #[test]
    fn credit_moves_between_queued_and_inflight() {
        let controller = Controller::new(64, 64);
        let mut credit = Credit::new(controller.clone(), StreamPriority::Normal, 10);

        assert_eq!(controller.queued_bytes(), 0);
        controller.queued_bytes.fetch_add(10, Ordering::Relaxed);
        credit.on_transmit();
        assert_eq!(controller.queued_bytes(), 0);
        assert_eq!(controller.inflight_bytes(), 10);

        credit.on_requeue();
        assert_eq!(controller.queued_bytes(), 10);
        assert_eq!(controller.inflight_bytes(), 0);

        credit.release();
        assert_eq!(controller.queued_bytes(), 0);
        assert_eq!(controller.inflight_bytes(), 0);
    }

    #[test]
    fn same_priority_waiters_are_fifo() {
        let controller = Controller::new(32, 32);
        let first_id = controller.allocate_waiter_id();
        let second_id = controller.allocate_waiter_id();
        let first_wakes = Arc::new(AtomicUsize::new(0));
        let second_wakes = Arc::new(AtomicUsize::new(0));

        controller.register_waiter(
            first_id,
            StreamPriority::Normal,
            16,
            &counting_waker(first_wakes.clone()),
        );
        controller.register_waiter(
            second_id,
            StreamPriority::Normal,
            16,
            &counting_waker(second_wakes.clone()),
        );

        controller.release_queued(StreamPriority::Normal, 16);

        assert_eq!(first_wakes.load(Ordering::Relaxed), 1);
        assert_eq!(second_wakes.load(Ordering::Relaxed), 0);
        assert_eq!(controller.try_acquire(second_id, StreamPriority::Normal, 16), 0);
        assert_eq!(controller.try_acquire(first_id, StreamPriority::Normal, 16), 16);
    }
}
