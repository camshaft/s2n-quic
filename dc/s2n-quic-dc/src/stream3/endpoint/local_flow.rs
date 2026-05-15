// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::waker;
use crate::{
    flow::queue::AutoWake,
    intrusive_queue::{Adapter, Links, List},
    socket::channel::UnboundedSender,
};
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, Weak,
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

    #[inline]
    fn from_u8(value: u8) -> Self {
        debug_assert!(
            (value as usize) < Self::LEVELS,
            "invalid stream priority value: {value}"
        );
        if value == Self::High as u8 {
            Self::High
        } else if value == Self::Normal as u8 {
            Self::Normal
        } else {
            debug_assert_eq!(value, Self::Low as u8);
            Self::Low
        }
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

/// Per-writer local-flow state shared between the application task and the endpoint controller.
///
/// Writers park their waker here when they need more endpoint-local credits. The controller
/// stores the flow in a priority intrusive queue, issues a capped burst into `issued`, and then
/// forwards the stored waker via `AutoWake` so the application can resume and consume credits.
pub struct Flow {
    controller: Weak<Controller>,
    priority: AtomicU8,
    requested: AtomicU64,
    issued: AtomicU64,
    queued: AtomicBool,
    waker: Mutex<Option<Waker>>,
    links: Links,
}

unsafe impl Send for Flow {}
unsafe impl Sync for Flow {}

impl std::fmt::Debug for Flow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Flow")
            .field("priority", &self.priority())
            .field("requested", &self.requested())
            .field("issued", &self.issued())
            .field("queued", &self.is_queued())
            .finish_non_exhaustive()
    }
}

impl Flow {
    fn new(controller: &Arc<Controller>, priority: StreamPriority) -> Arc<Self> {
        Arc::new(Self {
            controller: Arc::downgrade(controller),
            priority: AtomicU8::new(priority as u8),
            requested: AtomicU64::new(0),
            issued: AtomicU64::new(0),
            queued: AtomicBool::new(false),
            waker: Mutex::new(None),
            links: Links::new(),
        })
    }

    #[inline]
    pub fn priority(&self) -> StreamPriority {
        StreamPriority::from_u8(self.priority.load(Ordering::Acquire))
    }

    #[inline]
    fn set_priority(&self, priority: StreamPriority) {
        self.priority.store(priority as u8, Ordering::Release);
    }

    #[inline]
    fn requested(&self) -> u64 {
        self.requested.load(Ordering::Acquire)
    }

    #[inline]
    fn set_requested(&self, requested: u64) {
        self.requested.store(requested, Ordering::Release);
    }

    #[inline]
    fn clear_requested(&self) {
        self.requested.store(0, Ordering::Release);
    }

    #[inline]
    fn issue(&self, credits: u64) {
        self.issued.fetch_add(credits, Ordering::AcqRel);
    }

    #[inline]
    fn issued(&self) -> u64 {
        self.issued.load(Ordering::Acquire)
    }

    #[inline]
    /// Atomically consumes up to `limit` bytes of previously issued credits.
    ///
    /// Returns the actual number of bytes removed from this flow's issued balance.
    pub fn take_issued(&self, limit: u64) -> u64 {
        let mut current = self.issued.load(Ordering::Acquire);
        loop {
            if current == 0 {
                return 0;
            }

            let granted = current.min(limit);
            match self.issued.compare_exchange(
                current,
                current - granted,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return granted,
                Err(updated) => current = updated,
            }
        }
    }

    #[inline]
    /// Stores the current task waker so the controller can resume the writer once credits arrive.
    pub fn register_waker(&self, waker: &Waker) {
        let slot = &mut *self.waker.lock();
        if slot.as_ref().map_or(true, |current| !current.will_wake(waker)) {
            *slot = Some(waker.clone());
        }
    }

    #[inline]
    fn take_waker(&self) -> AutoWake {
        AutoWake::new(self.waker.lock().take())
    }

    #[inline]
    /// Clears any pending request and drops the stored application waker.
    ///
    /// Writers call this when they no longer need queued credits, such as on drop.
    pub fn clear_wait(&self) {
        self.clear_requested();
        *self.waker.lock() = None;
    }

    #[inline]
    fn is_queued(&self) -> bool {
        self.queued.load(Ordering::Acquire)
    }

    #[inline]
    fn mark_enqueued(&self) -> bool {
        if self
            .queued
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        true
    }

    #[inline]
    fn mark_dequeued(&self) {
        self.queued.store(false, Ordering::Release);
    }
}

impl Drop for Flow {
    fn drop(&mut self) {
        let outstanding = self.issued.swap(0, Ordering::AcqRel);
        if outstanding == 0 {
            return;
        }

        if let Some(controller) = self.controller.upgrade() {
            controller.release_queued(self.priority(), outstanding);
        } else {
            // The endpoint/controller has already been dropped, so no remaining writers can
            // observe or reuse the endpoint-local budget anymore.
        }
    }
}

/// Adapter that stores [`Arc<Flow>`] values in the controller's intrusive priority queues.
///
/// This lets each writer keep one stable flow allocation while the controller links and unlinks
/// it across wait queues without additional boxing or per-wait allocations.
#[derive(Clone, Copy, Debug)]
struct FlowAdapter;

impl Adapter for FlowAdapter {
    type Value = Flow;
    type Target = Flow;
    type Pointer = Arc<Flow>;

    unsafe fn links(value: *mut Self::Value) -> *mut Links {
        core::ptr::addr_of_mut!((*value).links)
    }

    unsafe fn target(value: *mut Self::Value) -> *mut Self::Target {
        value
    }

    fn as_ptr(ptr: &Self::Pointer) -> *const Self::Value {
        Arc::as_ptr(ptr)
    }

    fn into_raw(ptr: Self::Pointer) -> *mut Self::Value {
        Arc::into_raw(ptr) as *mut Self::Value
    }

    unsafe fn from_raw(ptr: *mut Self::Value) -> Self::Pointer {
        Arc::from_raw(ptr)
    }
}

pub struct Controller {
    queued_bytes: AtomicU64,
    inflight_bytes: AtomicU64,
    max_queued_bytes: u64,
    max_inflight_bytes: u64,
    max_burst_bytes: u64,
    waiters: [Mutex<List<FlowAdapter>>; StreamPriority::LEVELS],
    wake_sink: Mutex<Option<waker::Sink>>,
}

impl std::fmt::Debug for Controller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Controller")
            .field("queued_bytes", &self.queued_bytes())
            .field("inflight_bytes", &self.inflight_bytes())
            .field("max_queued_bytes", &self.max_queued_bytes)
            .field("max_inflight_bytes", &self.max_inflight_bytes)
            .field("max_burst_bytes", &self.max_burst_bytes)
            .finish_non_exhaustive()
    }
}

impl Controller {
    pub const DEFAULT_MAX_QUEUED_BYTES: u64 = 8 * 1024 * 1024;
    pub const DEFAULT_MAX_INFLIGHT_BYTES: u64 = 8 * 1024 * 1024;
    pub const DEFAULT_MAX_BURST_BYTES: u64 = 64 * 1024;

    /// Creates a controller with endpoint-wide queued/inflight limits and a per-issuance burst cap.
    ///
    /// `max_burst_bytes` limits how many queued bytes a single flow can receive in one issuance so
    /// that other waiting flows have a chance to run before the same writer acquires more credits.
    pub fn new(max_queued_bytes: u64, max_inflight_bytes: u64, max_burst_bytes: u64) -> Arc<Self> {
        assert!(
            max_queued_bytes > 0,
            "max_queued_bytes must be non-zero, got: {max_queued_bytes}"
        );
        assert!(
            max_inflight_bytes > 0,
            "max_inflight_bytes must be non-zero, got: {max_inflight_bytes}"
        );
        assert!(
            max_burst_bytes > 0,
            "max_burst_bytes must be non-zero, got: {max_burst_bytes}"
        );
        Arc::new(Self {
            queued_bytes: AtomicU64::new(0),
            inflight_bytes: AtomicU64::new(0),
            max_queued_bytes,
            max_inflight_bytes,
            max_burst_bytes,
            waiters: core::array::from_fn(|_| Mutex::new(List::new())),
            wake_sink: Mutex::new(None),
        })
    }

    #[inline]
    pub fn allocate_flow(self: &Arc<Self>, priority: StreamPriority) -> Arc<Flow> {
        Flow::new(self, priority)
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

    /// Requests endpoint-local credits for `flow`.
    ///
    /// The controller records the desired byte count, queues the flow in its priority lane if
    /// needed, and attempts to issue a capped burst immediately if endpoint capacity is available.
    pub fn request(&self, flow: &Arc<Flow>, priority: StreamPriority, requested: u64) {
        if requested == 0 {
            return;
        }

        flow.set_priority(priority);
        flow.set_requested(requested);

        if flow.issued() > 0 {
            return;
        }

        self.enqueue(flow.clone(), priority);
        self.issue_waiters();
    }

    /// Clears any pending wait state for `flow`.
    ///
    /// This is used when a writer is dropped or otherwise no longer needs queued credits.
    pub fn clear_waiter(&self, flow: &Arc<Flow>) {
        flow.clear_wait();
    }

    /// Updates the flow's priority for future credit issuance.
    ///
    /// Already-queued flows keep their current queue position until popped, at which point the
    /// controller requeues them onto the new priority lane before issuing credits.
    pub fn update_priority(&self, flow: &Arc<Flow>, priority: StreamPriority) {
        let previous = flow.priority();
        if previous == priority {
            return;
        }

        flow.set_priority(priority);
        self.issue_waiters();
    }

    #[inline]
    pub fn move_queued_to_inflight(&self, _priority: StreamPriority, bytes: u64) {
        if bytes == 0 {
            return;
        }

        Self::sub_counter(&self.queued_bytes, bytes);
        self.inflight_bytes.fetch_add(bytes, Ordering::AcqRel);
        self.issue_waiters();
    }

    #[inline]
    pub fn move_inflight_to_queued(&self, _priority: StreamPriority, bytes: u64) {
        if bytes == 0 {
            return;
        }

        Self::sub_counter(&self.inflight_bytes, bytes);
        self.queued_bytes.fetch_add(bytes, Ordering::AcqRel);
        self.issue_waiters();
    }

    #[inline]
    pub fn release_queued(&self, _priority: StreamPriority, bytes: u64) {
        if bytes == 0 {
            return;
        }

        Self::sub_counter(&self.queued_bytes, bytes);
        self.issue_waiters();
    }

    #[inline]
    pub fn release_inflight(&self, _priority: StreamPriority, bytes: u64) {
        if bytes == 0 {
            return;
        }

        Self::sub_counter(&self.inflight_bytes, bytes);
        self.issue_waiters();
    }

    fn enqueue(&self, flow: Arc<Flow>, priority: StreamPriority) {
        if !flow.mark_enqueued() {
            return;
        }

        self.waiters[priority.as_index()].lock().push_back(flow);
    }

    fn issue_waiters(&self) {
        let mut available = self.available_bytes();
        if available == 0 {
            return;
        }

        let mut wakes = VecDeque::new();

        'issue_loop: while available > 0 {
            let mut progressed = false;

            for queued_priority in StreamPriority::ALL {
                let Some(flow) = self.waiters[queued_priority.as_index()].lock().pop_front() else {
                    continue;
                };
                progressed = true;
                flow.mark_dequeued();

                let desired_priority = flow.priority();
                if desired_priority != queued_priority {
                    self.enqueue(flow, desired_priority);
                    continue;
                }

                let requested = flow.requested();
                if requested == 0 {
                    continue;
                }

                let granted = available.min(self.max_burst_bytes).min(requested);
                self.queued_bytes.fetch_add(granted, Ordering::AcqRel);
                flow.clear_requested();
                flow.issue(granted);
                available = available.saturating_sub(granted);
                wakes.push_back(flow.take_waker());
                continue 'issue_loop;
            }

            if !progressed {
                break;
            }
        }

        self.dispatch_wakes(wakes);
    }

    fn available_bytes(&self) -> u64 {
        let queued = self.queued_bytes.load(Ordering::Acquire);
        let inflight = self.inflight_bytes.load(Ordering::Acquire);
        self.max_queued_bytes
            .saturating_sub(queued)
            .min(self.max_inflight_bytes.saturating_sub(inflight))
    }

    fn dispatch_wakes(&self, wakes: VecDeque<AutoWake>) {
        if wakes.is_empty() {
            return;
        }

        if let Some(mut sink) = self.wake_sink.lock().clone() {
            for auto_wake in wakes {
                let _ = sink.send(auto_wake);
            }
        } else {
            for auto_wake in wakes {
                drop(auto_wake);
            }
        }
    }

    fn sub_counter(counter: &AtomicU64, bytes: u64) {
        let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            Some(current.saturating_sub(bytes))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::task::{RawWaker, RawWakerVTable, Waker};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
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
            let counter = std::mem::ManuallyDrop::new(Arc::<AtomicUsize>::from_raw(data.cast()));
            counter.fetch_add(1, Ordering::Relaxed);
        }

        unsafe fn drop(data: *const ()) {
            let _ = Arc::<AtomicUsize>::from_raw(data.cast());
        }

        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

        let raw = RawWaker::new(Arc::into_raw(counter).cast(), &VTABLE);
        unsafe { Waker::from_raw(raw) }
    }

    #[test]
    fn issues_credits_into_flow_and_wakes() {
        let controller = Controller::new(64, 64, 32);
        let flow = controller.allocate_flow(StreamPriority::Normal);
        let wakes = Arc::new(AtomicUsize::new(0));

        flow.register_waker(&counting_waker(wakes.clone()));
        controller.request(&flow, StreamPriority::Normal, 32);

        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert_eq!(flow.take_issued(u64::MAX), 32);
        assert_eq!(controller.queued_bytes(), 32);
    }

    #[test]
    fn priority_waiters_gate_lower_priority_issue() {
        let controller = Controller::new(64, 64, 32);
        let high = controller.allocate_flow(StreamPriority::High);
        let low = controller.allocate_flow(StreamPriority::Low);
        let high_wakes = Arc::new(AtomicUsize::new(0));
        let low_wakes = Arc::new(AtomicUsize::new(0));

        high.register_waker(&counting_waker(high_wakes.clone()));
        low.register_waker(&counting_waker(low_wakes.clone()));

        controller.request(&low, StreamPriority::Low, 16);
        controller.clear_waiter(&low);
        let _ = low.take_issued(u64::MAX);
        low_wakes.store(0, Ordering::Relaxed);
        controller.queued_bytes.store(64, Ordering::Release);
        controller.request(&high, StreamPriority::High, 16);
        controller.request(&low, StreamPriority::Low, 16);

        controller.release_queued(StreamPriority::High, 16);

        assert_eq!(high_wakes.load(Ordering::Relaxed), 1);
        assert_eq!(low_wakes.load(Ordering::Relaxed), 0);
        assert_eq!(high.take_issued(u64::MAX), 16);
        assert_eq!(low.take_issued(u64::MAX), 0);
    }

    #[test]
    fn caps_grants_to_burst_size() {
        let controller = Controller::new(256, 256, 32);
        let flow = controller.allocate_flow(StreamPriority::Normal);
        let wakes = Arc::new(AtomicUsize::new(0));

        flow.register_waker(&counting_waker(wakes.clone()));
        controller.request(&flow, StreamPriority::Normal, 128);

        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert_eq!(flow.take_issued(u64::MAX), 32);
        assert_eq!(controller.queued_bytes(), 32);
    }

    #[test]
    fn reprioritizes_queued_flows() {
        let controller = Controller::new(64, 64, 32);
        let high = controller.allocate_flow(StreamPriority::High);
        let low = controller.allocate_flow(StreamPriority::Low);
        let high_wakes = Arc::new(AtomicUsize::new(0));
        let low_wakes = Arc::new(AtomicUsize::new(0));

        controller.queued_bytes.store(64, Ordering::Release);
        high.register_waker(&counting_waker(high_wakes.clone()));
        low.register_waker(&counting_waker(low_wakes.clone()));
        controller.request(&high, StreamPriority::High, 16);
        controller.request(&low, StreamPriority::Low, 16);
        controller.update_priority(&low, StreamPriority::High);

        controller.release_queued(StreamPriority::High, 16);
        assert_eq!(high_wakes.load(Ordering::Relaxed), 1);
        assert_eq!(low_wakes.load(Ordering::Relaxed), 0);
        assert_eq!(high.take_issued(u64::MAX), 16);

        controller.release_queued(StreamPriority::High, 16);
        assert_eq!(low_wakes.load(Ordering::Relaxed), 1);
        assert_eq!(low.take_issued(u64::MAX), 16);
    }
}
