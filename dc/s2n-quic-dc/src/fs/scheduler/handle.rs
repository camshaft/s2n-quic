// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The application-facing [`SubmitHandle`] and the atomic submit primitive.
//!
//! A [`SubmitHandle`] is a cheap, **`Send + Sync`** clone of the scheduler's shared state — any thread
//! can hold one and submit concurrently, exactly like `stream::Client` over an `Arc<Endpoint>`. The
//! `!Send` per-worker machinery (lane senders, dispatch task, distributors) lives behind the
//! submission channel on the worker tasks, never on the handle.
//!
//! [`SubmitHandle::submit`] is the atomic single-op primitive and the heart of the deadlock fix: it
//! acquires the op's credit from its device pool **before** the op is handed to the pipeline. A
//! submitter that cannot get credit parks cooperatively on a waker (never a thread), so admitted work
//! can never exceed the pool capacity and the blocking-pool hold-and-wait deadlock cannot form.
//!
//! The read/write methods take the target device as **`&Arc<Device>`** (obtained once from
//! [`Scheduler::register_device`](crate::fs::scheduler::Scheduler::register_device)), not a
//! `DeviceId` index: the handle validates and acquires against that device directly, and the op
//! carries the `Arc<Device>` through the pipeline so it can finish itself with no table lookup.

use crate::{
    fs::{
        device::Device,
        op::{CompletionReceiver, CompletionSender, Fd, IoBuf, IoKind, IoOp, IoStatus},
        scheduler::{alloc::SubmitterAlloc, BlockRef, SchedulerInner},
    },
    intrusive::Entry,
    sched::{Budget, TierPriority},
    sync::Arc,
};
use core::{future::poll_fn, task::Context};

/// A cloneable, **`Send + Sync`** handle for submitting IO. Clones share the scheduler's devices,
/// submission channel, and spawned tasks — so a single process-wide scheduler can be handed to and
/// used concurrently by any number of threads, like `stream::Client`.
#[derive(Clone)]
pub struct SubmitHandle {
    pub(super) inner: Arc<SchedulerInner>,
}

impl SubmitHandle {
    /// Submit a read of `len` bytes at `offset` on `fd`/`device`, resolving with the filled buffer.
    ///
    /// The scheduler allocates a `BytesMut` with capacity `len` and reads into its uninitialized
    /// spare capacity (no zero-fill); the returned buffer's `len()` is the bytes actually read (a
    /// short read at EOF returns fewer bytes, never a zero-padded tail). The returned future is
    /// `'static` (captures a handle clone).
    pub fn read(
        &self,
        device: Arc<Device>,
        fd: Fd,
        offset: u64,
        len: u32,
        priority: TierPriority,
    ) -> impl core::future::Future<Output = std::io::Result<bytes::BytesMut>> + 'static {
        let handle = self.clone();
        async move {
            // Caller-owned, pre-sized, logically-empty buffer: the backend reads into spare capacity
            // and `set_len`s to bytes read. No allocation or memset happens inside the backend.
            let buf = bytes::BytesMut::with_capacity(len as usize);
            let op = handle
                .submit(
                    IoKind::Read,
                    device,
                    fd,
                    offset,
                    len,
                    IoBuf::Read(buf),
                    priority,
                )
                .await?;
            match op.buf {
                IoBuf::Read(buf) => Ok(buf),
                _ => unreachable!("read op returned non-read buffer"),
            }
        }
    }

    /// Submit a write of `data` at `offset` on `fd`/`device`, resolving with the number of bytes
    /// written.
    pub fn write(
        &self,
        device: Arc<Device>,
        fd: Fd,
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
            match op.status {
                IoStatus::Done(n) => Ok(n),
                other => unreachable!("submit returned non-Done op: {other:?}"),
            }
        }
    }

    /// Submit a **zero-copy** `O_DIRECT` read at `offset` into the caller-owned, page-aligned `buf`,
    /// reading up to `buf.len()` bytes. Resolves with `(buffer, n)` where `n` is the bytes actually
    /// read — the buffer is filled in place and its logical length set to `n`. Both `offset` and
    /// `buf.len()` must be block-aligned (validated; `InvalidInput` otherwise).
    pub fn read_direct(
        &self,
        device: Arc<Device>,
        fd: Fd,
        offset: u64,
        buf: crate::fs::direct::AlignedBuf,
        priority: TierPriority,
    ) -> impl core::future::Future<Output = std::io::Result<(crate::fs::direct::AlignedBuf, usize)>>
           + 'static {
        let handle = self.clone();
        async move {
            let len = buf.len() as u32;
            let op = handle
                .submit(
                    IoKind::Read,
                    device,
                    fd,
                    offset,
                    len,
                    IoBuf::Direct(buf),
                    priority,
                )
                .await?;
            match (op.status, op.buf) {
                (IoStatus::Done(n), IoBuf::Direct(buf)) => Ok((buf, n)),
                other => unreachable!("direct read returned unexpected op: {other:?}"),
            }
        }
    }

    /// Submit a **zero-copy** `O_DIRECT` write at `offset` from the caller-owned, page-aligned `buf`,
    /// writing `buf.len()` bytes in place. Resolves with the buffer returned and the byte count.
    /// `offset` must be block-aligned (validated; `InvalidInput` otherwise).
    pub fn write_direct(
        &self,
        device: Arc<Device>,
        fd: Fd,
        offset: u64,
        buf: crate::fs::direct::AlignedBuf,
        priority: TierPriority,
    ) -> impl core::future::Future<Output = std::io::Result<(crate::fs::direct::AlignedBuf, usize)>>
           + 'static {
        let handle = self.clone();
        async move {
            let len = buf.len() as u32;
            let op = handle
                .submit(
                    IoKind::Write,
                    device,
                    fd,
                    offset,
                    len,
                    IoBuf::Direct(buf),
                    priority,
                )
                .await?;
            match (op.status, op.buf) {
                (IoStatus::Done(n), IoBuf::Direct(buf)) => Ok((buf, n)),
                other => unreachable!("direct write returned unexpected op: {other:?}"),
            }
        }
    }

    /// The atomic submit primitive: acquire credit, hand the op to a lane, await completion.
    ///
    /// Returns only **successfully completed** ops — a `Failed` completion, an oversized op, a
    /// misaligned direct op, or a closed lane all surface as `Err`, so callers never have to
    /// re-inspect `op.status`.
    #[allow(clippy::too_many_arguments)]
    pub async fn submit(
        &self,
        kind: IoKind,
        device: Arc<Device>,
        fd: Fd,
        offset: u64,
        len: u32,
        buf: IoBuf,
        priority: TierPriority,
    ) -> std::io::Result<IoOp> {
        let completion_rx: CompletionReceiver =
            crate::socket::channel::intrusive::datagram_completion::new::<IoOp>();
        // A one-shot submit allocates a fresh acquire context; long-lived submitters reuse one (see
        // `submit_with`).
        let mut alloc = SubmitterAlloc::new();
        self.submit_with(
            &mut alloc,
            kind,
            device,
            fd,
            offset,
            len,
            buf,
            priority,
            completion_rx.sender(),
            0,
        )
        .await?;
        let op = await_completion(completion_rx).await?;
        match op.status {
            IoStatus::Done(_) => Ok(op),
            IoStatus::Failed(kind) => Err(kind.into()),
            IoStatus::Pending => unreachable!("dispatcher routed a still-pending op"),
        }
    }

    /// Acquire credit (reusing the caller's `alloc`) and enqueue an op whose completion is delivered
    /// to the caller-provided `completion` sender (tagged with `user_data`), then return — **without**
    /// awaiting the completion. The building block for a submitter that funnels many ops onto one
    /// shared completion channel and reuses one acquire context across them (e.g.
    /// [`MaterializeStream`](crate::fs::materialize)), avoiding a per-op slot allocation.
    ///
    /// `Ok(())` means the op was admitted (credit acquired) and enqueued. An `Err` (misaligned/
    /// oversized op, closed scheduler) means no completion will arrive and any acquired credit was
    /// released.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn submit_with(
        &self,
        alloc: &mut SubmitterAlloc,
        kind: IoKind,
        device: Arc<Device>,
        fd: Fd,
        offset: u64,
        len: u32,
        buf: IoBuf,
        priority: TierPriority,
        completion: CompletionSender,
        user_data: u64,
    ) -> std::io::Result<()> {
        // Validate + resolve the device pool and cost directly on the carried device (no lookup).
        let (pool, cost) = self.prepare(&device, kind, offset, len, buf.is_direct())?;

        // Acquire credit, parking cooperatively if the device is at capacity. This is the
        // backpressure that prevents the blocking-pool deadlock.
        alloc.acquire(&pool, cost, priority).await?;

        // Move the granted credit (>= cost) onto the op so the alloc resets clean and the completion
        // path releases it exactly once.
        let granted = alloc.take_all();
        debug_assert!(granted >= cost, "acquire returned less than cost");

        self.enqueue(
            kind, &device, fd, offset, len, buf, completion, user_data, granted,
        )
    }

    /// Validate a prospective op against `device` and resolve the pool it draws from plus its credit
    /// cost. A thin wrapper over [`Device::prepare`] that also bumps the rejection counter, shared by
    /// [`submit_with`](Self::submit_with) and the [`MaterializeStream`] reactor.
    ///
    /// [`MaterializeStream`]: crate::fs::materialize::MaterializeStream
    pub(crate) fn prepare(
        &self,
        device: &Arc<Device>,
        kind: IoKind,
        offset: u64,
        len: u32,
        is_direct: bool,
    ) -> std::io::Result<(Arc<crate::credit::Pool>, u64)> {
        // A device is a free-floating `Arc<Device>` with no owning table, so guard the footgun of
        // submitting one scheduler's device to a different scheduler's handle (it would pace against
        // the wrong pool and key the wrong materialize slot). Debug-only — `SchedulerId` is a ZST in
        // release, so this compiles to nothing.
        debug_assert!(
            device.origin == self.inner.origin,
            "io scheduler: device was registered with a different Scheduler than this handle"
        );
        device
            .prepare(kind, offset, len, is_direct)
            .inspect_err(|_| {
                device.counters.rejected.add(1);
            })
    }

    /// Build an admitted op (credit already `granted`) carrying its `Arc<Device>` and push it into
    /// the submission channel; the dispatch task routes it to a lane. On a closed channel (scheduler
    /// torn down) the credit is released and an error returned, since no completion can arrive.
    /// Shared by `submit_with` and the materialize reactor.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn enqueue(
        &self,
        kind: IoKind,
        device: &Arc<Device>,
        fd: Fd,
        offset: u64,
        len: u32,
        buf: IoBuf,
        completion: CompletionSender,
        user_data: u64,
        granted: u64,
    ) -> std::io::Result<()> {
        // Per-device admission counters (the caller holds the `Arc<Device>`, so no extra plumbing):
        // bump this op-kind's submitted count and, for data ops, its submit-size histogram.
        let opk = device.counters.op(kind);
        opk.submitted.add(1);
        if let Some(hist) = &opk.submit_bytes {
            hist.record_value(len as u64);
        }

        let op = IoOp {
            kind,
            device: device.clone(),
            fd,
            offset,
            len,
            buf,
            completion: Some(completion),
            status: IoStatus::Pending,
            flow_credits: granted,
            // `LocalRingId::UNSET` until the dispatch EDT picks a lane (which overwrites it).
            ring_id: crate::fs::device::LocalRingId::UNSET,
            user_data,
            enqueued_at: Some(self.inner.clock.now()),
        };

        if let Err(mut undelivered) = self.inner.submission.send_entry(Entry::new(op)) {
            undelivered.release_credits();
            undelivered.device.counters.rejected.add(1);
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "io scheduler: submission channel closed",
            ));
        }
        Ok(())
    }

    /// Stream an ordered object whose blocks scatter across devices, delivering blocks in FIFO order
    /// via **buffered** reads. See [`crate::fs::materialize::MaterializeStream`].
    pub fn materialize<I>(
        &self,
        blocks: I,
        priority: TierPriority,
    ) -> crate::fs::materialize::MaterializeStream<I::IntoIter>
    where
        I: IntoIterator<Item = BlockRef>,
    {
        crate::fs::materialize::MaterializeStream::new(
            self.clone(),
            blocks.into_iter(),
            priority,
            false,
        )
    }

    /// Like [`materialize`](Self::materialize) but issues **zero-copy `O_DIRECT`** reads into
    /// page-aligned buffers — the mode Membrain uses. Each block's `offset` and `len` must be
    /// block-aligned (a misaligned block surfaces as an `InvalidInput` error in delivery order).
    pub fn materialize_direct<I>(
        &self,
        blocks: I,
        priority: TierPriority,
    ) -> crate::fs::materialize::MaterializeStream<I::IntoIter>
    where
        I: IntoIterator<Item = BlockRef>,
    {
        crate::fs::materialize::MaterializeStream::new(
            self.clone(),
            blocks.into_iter(),
            priority,
            true,
        )
    }

    /// Number of execution lanes.
    pub fn lane_count(&self) -> usize {
        self.inner.lane_count
    }
}

/// Await the single completion of a submitted op.
async fn await_completion(mut rx: CompletionReceiver) -> std::io::Result<IoOp> {
    let mut budget = Budget::new(1);
    // Disambiguate: `datagram_completion::Receiver` is `Receiver` for both `Entry<T>` and `Queue<T>`;
    // we want the single-entry form.
    let entry: Option<Entry<IoOp>> = poll_fn(|cx: &mut Context<'_>| {
        budget.reset();
        crate::sched::Receiver::<Entry<IoOp>>::poll_recv(&mut rx, cx, &mut budget)
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
