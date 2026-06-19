// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The application-facing scheduler and submit handle.
//!
//! [`Scheduler::new`] builds the device table, spawns one credit distributor per device pool, wires
//! the backend's execution lanes to a completion dispatcher, and hands back a [`SubmitHandle`].
//!
//! [`SubmitHandle::submit`] is the atomic single-op primitive and the heart of the deadlock fix: it
//! acquires the op's credit from its device pool **before** the op is handed to a lane. A submitter
//! that cannot get credit parks cooperatively on a waker (never a thread), so admitted work can
//! never exceed the pool capacity and the blocking-pool hold-and-wait deadlock cannot form. The
//! credit dance mirrors `stream::writer`'s `WriterAlloc` slot lifecycle exactly.

use crate::{
    credit::{AbandonResult, GrantResult, Slot},
    fs::{
        backend::{Backend, LaneSetup, LaneSubmit},
        combinator::{completion_dispatcher, ChannelCompletionSink},
        config::Config,
        device::{Device, DeviceId, DeviceTable, LocalRingId},
        op::{CompletionReceiver, IoBuf, IoKind, IoOp, IoStatus},
        SpawnHandle,
    },
    sched::{Distributor, Pool, TierPriority, WakerSink},
    sync::Arc,
    time::precision,
};
use core::{
    cell::{Cell, RefCell},
    future::poll_fn,
    task::{Context, Poll},
};
use std::{
    alloc::{self, Layout},
    collections::VecDeque,
    ptr::NonNull,
    rc::Rc,
    task::Waker,
};

/// A reference to one block of a logical object: which device/fd it lives on, its byte range, and
/// optional head/tail trim for edge blocks of a sub-range read. The unit of a [`materialize`] spray.
///
/// [`materialize`]: SubmitHandle::materialize
#[derive(Clone, Copy, Debug)]
pub struct BlockRef {
    pub device: DeviceId,
    pub fd: i32,
    pub offset: u64,
    pub len: u32,
    /// Bytes to trim from the head of the read result (for a range starting mid-block).
    pub head_trim: u32,
    /// Bytes to trim from the tail of the read result (for a range ending mid-block).
    pub tail_trim: u32,
}

impl BlockRef {
    /// A whole-block read with no trimming.
    pub fn whole(device: DeviceId, fd: i32, offset: u64, len: u32) -> Self {
        Self {
            device,
            fd,
            offset,
            len,
            head_trim: 0,
            tail_trim: 0,
        }
    }
}

// ── Submitter allocation (slot @ offset 0) ──────────────────────────────────

/// Per-acquire state: leftover credits to release on drop, and the pool to release them to.
struct SubmitterInner {
    pending_credits: u64,
    pool: Arc<Pool>,
}

/// `#[repr(C)]` with `Slot` at offset 0 — the credit pool casts `NonNull<Slot>` back to
/// `NonNull<SubmitterAlloc>` via the registered `drop_fn`. One per in-flight `submit` future so
/// concurrent acquires (e.g. a materialize spray) each park independently.
#[repr(C)]
struct SubmitterAlloc {
    slot: Slot,
    inner: SubmitterInner,
}

crate::assert_slot_at_offset_zero!(SubmitterAlloc);

/// Owning pointer to a heap `SubmitterAlloc`. Drop is staged through the slot's abandon/grant state
/// machine, mirroring `stream::writer::WriterAllocPtr`.
struct SubmitterAllocPtr(NonNull<SubmitterAlloc>);

impl SubmitterAllocPtr {
    fn new(pool: Arc<Pool>) -> Self {
        let layout = Layout::new::<SubmitterAlloc>();
        let raw = unsafe { alloc::alloc(layout) } as *mut SubmitterAlloc;
        let ptr = NonNull::new(raw).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        unsafe {
            std::ptr::write(
                ptr.as_ptr(),
                SubmitterAlloc {
                    slot: Slot::new(drop_submitter_alloc),
                    inner: SubmitterInner {
                        pending_credits: 0,
                        pool,
                    },
                },
            );
        }
        Self(ptr)
    }

    #[inline]
    fn slot_ptr(&self) -> NonNull<Slot> {
        self.0.cast()
    }

    #[inline]
    fn inner(&mut self) -> &mut SubmitterInner {
        unsafe { &mut (*self.0.as_ptr()).inner }
    }
}

impl Drop for SubmitterAllocPtr {
    fn drop(&mut self) {
        // Mirror WriterAllocPtr::drop: `abandon`'s CAS is the single source of truth for ownership.
        let slot = unsafe { &(*self.0.as_ptr()).slot };
        match unsafe { slot.abandon() } {
            AbandonResult::Abandoned => {
                // Slot was LINKED, now DEAD: the pool's pop walk calls `drop_submitter_alloc`.
                return;
            }
            AbandonResult::Granted(n) => {
                let inner = unsafe { &(*self.0.as_ptr()).inner };
                let to_release = n.saturating_add(inner.pending_credits);
                if to_release > 0 {
                    inner.pool.release(to_release);
                }
            }
            AbandonResult::Closed => {
                // Pool gone; do not touch it.
            }
        }
        unsafe {
            std::ptr::drop_in_place(&raw mut (*self.0.as_ptr()).inner);
            alloc::dealloc(self.0.as_ptr().cast(), Layout::new::<SubmitterAlloc>());
        }
    }
}

/// `drop_fn` the pool invokes when it pops a dead slot (submit future dropped while parked).
unsafe fn drop_submitter_alloc(ptr: NonNull<Slot>) {
    let ptr = ptr.cast::<SubmitterAlloc>();
    let inner = &(*ptr.as_ptr()).inner;
    if inner.pending_credits > 0 {
        inner.pool.release(inner.pending_credits);
    }
    std::ptr::drop_in_place(&raw mut (*ptr.as_ptr()).inner);
    alloc::dealloc(ptr.as_ptr().cast(), Layout::new::<SubmitterAlloc>());
}

// SAFETY: the alloc is owned exclusively; the pool only touches the `Slot` under its state machine.
unsafe impl Send for SubmitterAllocPtr {}

// ── Scheduler ───────────────────────────────────────────────────────────────

/// Shared scheduler state behind the handle. `!Send` (lane senders are unsync), pinned to one
/// worker like the network endpoint's per-worker tasks.
struct SchedulerInner {
    devices: Arc<DeviceTable>,
    lanes: RefCell<Vec<LaneSubmit>>,
    round_robin: Cell<usize>,
    clock: crate::time::DefaultClock,
}

/// The IO scheduler. Owns the device table, the spawned distributor/dispatcher/lane tasks, and
/// hands out [`SubmitHandle`]s.
pub struct Scheduler {
    inner: Rc<SchedulerInner>,
}

impl Scheduler {
    /// Build the scheduler: device table, per-pool distributors, backend lanes, completion
    /// dispatcher. `spawn` drives all internal tasks; `clock` is the precision clock used by the
    /// distributors and the backend.
    pub fn new<B, Clk>(config: &Config, backend: &B, spawn: SpawnHandle, clock: Clk) -> Self
    where
        B: Backend,
        Clk: precision::Clock + Clone,
    {
        // 1. Build devices and spawn one distributor per pool.
        let devices: Vec<Device> = config.devices.iter().map(Device::new).collect();
        for device in &devices {
            for pool in device.pools.all() {
                spawn_distributor(pool.clone(), &spawn, clock.clone());
            }
        }
        let devices = Arc::new(DeviceTable::new(devices));

        // 2. Wire the completion path: backend lanes push into the sink; the dispatcher drains it,
        //    releases credit, and routes each op to its submitter.
        let (sink, completion_rx) = ChannelCompletionSink::new();
        spawn.spawn(completion_dispatcher(completion_rx, devices.clone()));

        // 3. Build the backend's execution lanes.
        let lanes = backend.spawn_lanes(LaneSetup {
            devices: devices.clone(),
            lane_count: config.ring_count.max(1),
            completion: Box::new(sink),
            spawn: spawn.clone(),
        });

        Self {
            inner: Rc::new(SchedulerInner {
                devices,
                lanes: RefCell::new(lanes),
                round_robin: Cell::new(0),
                clock: crate::time::DefaultClock::default(),
            }),
        }
    }

    /// A handle for submitting work.
    pub fn handle(&self) -> SubmitHandle {
        SubmitHandle {
            inner: self.inner.clone(),
        }
    }

    /// The device table, for tests/introspection (conservation checks).
    #[cfg(test)]
    pub(crate) fn devices(&self) -> &Arc<DeviceTable> {
        &self.inner.devices
    }
}

/// Spawn a credit distributor for `pool`, delivering its grant wakers inline.
fn spawn_distributor<Clk>(pool: Arc<Pool>, spawn: &SpawnHandle, clock: Clk)
where
    Clk: precision::Clock,
{
    let dist = Distributor::new(pool);
    let budget = crate::sched::Budget::new(1 << 20);
    spawn.spawn(dist.distribute(budget, InlineWakers, clock));
}

/// A [`WakerSink`] that wakes every granted waker inline. Sufficient for the single-threaded mock
/// runtime; production routes through the per-worker waker drain.
struct InlineWakers;

impl WakerSink for InlineWakers {
    fn append_wakers(&mut self, batch: &mut VecDeque<Waker>) {
        for w in batch.drain(..) {
            w.wake();
        }
    }
}

// ── Submit handle ───────────────────────────────────────────────────────────

/// A cloneable handle for submitting IO. Clones share the scheduler's devices, lanes, and tasks.
#[derive(Clone)]
pub struct SubmitHandle {
    inner: Rc<SchedulerInner>,
}

impl SubmitHandle {
    /// Submit a read of `len` bytes at `offset` on `fd`/`device` at `priority`; resolve with the
    /// filled buffer. The returned future is `'static` (captures a handle clone), so it can be
    /// stored in a read-ahead window.
    pub fn read(
        &self,
        device: DeviceId,
        fd: i32,
        offset: u64,
        len: u32,
        priority: TierPriority,
    ) -> impl core::future::Future<Output = std::io::Result<bytes::BytesMut>> + 'static {
        let handle = self.clone();
        async move {
            // `submit` returns only successfully-completed ops (it maps `Failed`/closed paths to
            // `Err`), so a returned read op always carries its filled buffer.
            let op = handle
                .submit(
                    IoKind::Read,
                    device,
                    fd,
                    offset,
                    len,
                    IoBuf::Read(bytes::BytesMut::new()),
                    priority,
                )
                .await?;
            match op.buf {
                IoBuf::Read(buf) => Ok(buf),
                _ => unreachable!("read op returned non-read buffer"),
            }
        }
    }

    /// Submit a write of `data` at `offset` on `fd`/`device` at `priority`; resolve with the number
    /// of bytes written.
    pub fn write(
        &self,
        device: DeviceId,
        fd: i32,
        offset: u64,
        data: bytes::Bytes,
        priority: TierPriority,
    ) -> impl core::future::Future<Output = std::io::Result<usize>> + 'static {
        let handle = self.clone();
        async move {
            let len = data.len() as u32;
            let op = handle
                .submit(
                    IoKind::Write,
                    device,
                    fd,
                    offset,
                    len,
                    IoBuf::Write(data),
                    priority,
                )
                .await?;
            // `submit` only returns `Done` ops; surface the byte count.
            match op.status {
                IoStatus::Done(n) => Ok(n),
                other => unreachable!("submit returned non-Done op: {other:?}"),
            }
        }
    }

    /// The atomic submit primitive: acquire credit, hand the op to a lane, await completion.
    ///
    /// Returns only **successfully completed** ops — a `Failed` completion, an out-of-range device,
    /// an oversized op, or a closed lane all surface as `Err`, so callers never have to re-inspect
    /// `op.status` (and can't silently mistake a failure for success).
    pub async fn submit(
        &self,
        kind: IoKind,
        device_id: DeviceId,
        fd: i32,
        offset: u64,
        len: u32,
        buf: IoBuf,
        priority: TierPriority,
    ) -> std::io::Result<IoOp> {
        // 0. Resolve the device. A caller-supplied id is unchecked, so a bad one is an error, not a
        //    worker panic.
        let device = self.inner.devices.get(device_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "io scheduler: device id out of range",
            )
        })?;
        let cost = device.cost(kind, len);

        // An `IoOp` is atomic — it has no partial-submit path — so an op costing more than its pool
        // could ever hold can never be admitted and would park forever. Reject it up front. (M2 may
        // split oversized ops; fail-fast is the safe minimum.)
        let capacity = device.capacity_for(kind);
        if cost > capacity {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "io scheduler: op cost exceeds device pool capacity",
            ));
        }
        let pool = device.pool_for(kind).clone();

        // 1. Acquire `cost` credit, parking cooperatively if the device is at capacity. This is the
        //    backpressure that prevents the blocking-pool deadlock.
        let mut alloc = SubmitterAllocPtr::new(pool.clone());
        acquire_credit(&mut alloc, &pool, cost, priority).await?;

        // 2. The acquire left at least `cost` in pending_credits. Move it onto the op so the alloc
        //    drops clean (releasing nothing), and the completion dispatcher releases it once.
        let granted = core::mem::take(&mut alloc.inner().pending_credits);
        debug_assert!(granted >= cost, "acquire returned less than cost");

        // 3. Build the op with a fresh completion channel.
        let completion_rx: CompletionReceiver =
            crate::socket::channel::intrusive::datagram_completion::new::<IoOp>();
        let ring_id = self.next_lane();
        let op = IoOp {
            kind,
            device: device_id,
            fd,
            offset,
            len,
            buf,
            completion: Some(completion_rx.sender()),
            status: IoStatus::Pending,
            flow_credits: granted,
            ring_id,
            enqueued_at: Some(self.inner.clock.now()),
        };

        // 4. Hand the op to its lane. The alloc can drop now — the credit lives on the op. If the
        //    lane is closed the op never reaches the dispatcher and no completion can arrive, so
        //    `push_to_lane` released the credit and we must fail the submit directly rather than
        //    await a completion that will never come (the completion channel cannot signal a dropped
        //    sender to the waiter).
        drop(alloc);
        if self.push_to_lane(ring_id, op).is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "io scheduler: execution lane closed",
            ));
        }

        // 5. Await the completion: the dispatcher routes the completed op back here.
        let op = await_completion(completion_rx).await?;
        match op.status {
            IoStatus::Done(_) => Ok(op),
            IoStatus::Failed(kind) => Err(kind.into()),
            IoStatus::Pending => unreachable!("dispatcher routed a still-pending op"),
        }
    }

    /// Stream an ordered object whose blocks scatter across devices, delivering blocks in FIFO
    /// order with bounded read-ahead. See [`crate::fs::materialize::MaterializeStream`].
    pub fn materialize<I>(
        &self,
        blocks: I,
        priority: TierPriority,
        read_ahead: usize,
    ) -> crate::fs::materialize::MaterializeStream<I::IntoIter>
    where
        I: IntoIterator<Item = BlockRef>,
    {
        crate::fs::materialize::MaterializeStream::new(
            self.clone(),
            blocks.into_iter(),
            priority,
            read_ahead,
        )
    }

    /// Round-robin the next execution lane.
    fn next_lane(&self) -> LocalRingId {
        let n = self.inner.lanes.borrow().len().max(1);
        let idx = self.inner.round_robin.get();
        self.inner.round_robin.set((idx + 1) % n);
        LocalRingId((idx % n) as u32)
    }

    /// Hand `op` to its execution lane. On a closed lane channel, release the op's borrowed credit
    /// (an `IoOp` has no `Drop` that does it, so the pool would otherwise leak) and return `Err` so
    /// the caller can fail the submit — the op will never reach the completion dispatcher.
    fn push_to_lane(&self, ring_id: LocalRingId, op: IoOp) -> Result<(), ()> {
        let mut lanes = self.inner.lanes.borrow_mut();
        let lane = &mut lanes[ring_id.as_usize()];
        match lane.send(crate::intrusive::Entry::new(op)) {
            Ok(()) => Ok(()),
            Err(mut entry) => {
                let credits = entry.take_credits();
                if credits > 0 {
                    if let Some(device) = self.inner.devices.get(entry.device) {
                        device.pool_for(entry.kind).release(credits);
                    }
                }
                Err(())
            }
        }
    }

    /// Number of execution lanes.
    pub fn lane_count(&self) -> usize {
        self.inner.lanes.borrow().len()
    }
}

/// Drive a credit acquire to completion, mirroring `Writer::poll_acquire_credits` with a full-op
/// target. With `min_grant_slice >= cost` the distributor grants the full `cost` in one wake, so
/// this parks at most once.
async fn acquire_credit(
    alloc: &mut SubmitterAllocPtr,
    pool: &Arc<Pool>,
    cost: u64,
    priority: TierPriority,
) -> std::io::Result<()> {
    let slot_ptr = alloc.slot_ptr();
    poll_fn(|cx: &mut Context<'_>| {
        // Drain any grant delivered while parked.
        let slot_ref = unsafe { slot_ptr.as_ref() };
        match slot_ref.poll_granted() {
            GrantResult::Pending => return Poll::Pending,
            GrantResult::Closed => {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "io scheduler credit pool closed",
                )));
            }
            GrantResult::Granted(n) => {
                alloc.inner().pending_credits = alloc.inner().pending_credits.saturating_add(n);
            }
        }

        if alloc.inner().pending_credits >= cost {
            return Poll::Ready(Ok(()));
        }

        let need = cost - alloc.inner().pending_credits;
        // SAFETY: `slot_ptr` is this future's stable, idle slot.
        match unsafe { pool.poll_acquire(cx, slot_ptr, need, priority) } {
            Poll::Ready(n) => {
                alloc.inner().pending_credits = alloc.inner().pending_credits.saturating_add(n);
                if alloc.inner().pending_credits >= cost {
                    Poll::Ready(Ok(()))
                } else {
                    // The fast path granted a partial because `need` exceeded `max_single_acquire`
                    // for this tier. Self-wake to re-acquire the remainder; this re-poll loop runs
                    // at most ceil(cost / max_single_acquire) times (bounded — `cost <= capacity` is
                    // enforced by the caller), and a re-acquire that can't be satisfied immediately
                    // parks and registers the waker genuinely.
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

/// Await the single completion of a submitted op.
async fn await_completion(mut rx: CompletionReceiver) -> std::io::Result<IoOp> {
    let mut budget = crate::sched::Budget::new(1);
    // Disambiguate: `datagram_completion::Receiver` is `Receiver` for both `Entry<T>` and
    // `Queue<T>`; we want the single-entry form.
    let entry: Option<crate::intrusive::Entry<IoOp>> = poll_fn(|cx: &mut Context<'_>| {
        budget.reset();
        crate::sched::Receiver::<crate::intrusive::Entry<IoOp>>::poll_recv(&mut rx, cx, &mut budget)
    })
    .await;

    match entry {
        Some(entry) => Ok(entry.into_inner()),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "io scheduler completion channel closed before completion",
        )),
    }
}
