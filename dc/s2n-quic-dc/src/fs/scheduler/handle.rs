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

use crate::{
    fs::{
        op::{CompletionReceiver, CompletionSender, IoBuf, IoKind, IoOp, IoStatus},
        scheduler::{alloc::SubmitterAlloc, BlockRef, SchedulerInner},
    },
    intrusive::Entry,
    sched::{Budget, TierPriority},
    sync::Arc,
};
use core::{future::poll_fn, sync::atomic::Ordering, task::Context};

/// A cloneable, **`Send + Sync`** handle for submitting IO. Clones share the scheduler's devices,
/// submission channel, and spawned tasks — so a single process-wide scheduler can be handed to and
/// used concurrently by any number of threads, like `stream::Client`.
#[derive(Clone)]
pub struct SubmitHandle {
    pub(super) inner: Arc<SchedulerInner>,
}

impl SubmitHandle {
    /// Submit a read of `len` bytes at `offset` on `fd`/`device` at `priority`; resolve with the
    /// filled buffer. The returned future is `'static` (captures a handle clone).
    pub fn read(
        &self,
        device: crate::fs::device::DeviceId,
        fd: i32,
        offset: u64,
        len: u32,
        priority: TierPriority,
    ) -> impl core::future::Future<Output = std::io::Result<bytes::BytesMut>> + 'static {
        let handle = self.clone();
        async move {
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
        device: crate::fs::device::DeviceId,
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
        device: crate::fs::device::DeviceId,
        fd: i32,
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
        device: crate::fs::device::DeviceId,
        fd: i32,
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
    /// Returns only **successfully completed** ops — a `Failed` completion, an out-of-range device,
    /// an oversized op, or a closed lane all surface as `Err`, so callers never have to re-inspect
    /// `op.status`.
    pub async fn submit(
        &self,
        kind: IoKind,
        device_id: crate::fs::device::DeviceId,
        fd: i32,
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
            device_id,
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
    /// `Ok(())` means the op was admitted (credit acquired) and enqueued. An `Err` (bad device,
    /// misaligned/oversized op, closed scheduler) means no completion will arrive and any acquired
    /// credit was released.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn submit_with(
        &self,
        alloc: &mut SubmitterAlloc,
        kind: IoKind,
        device_id: crate::fs::device::DeviceId,
        fd: i32,
        offset: u64,
        len: u32,
        buf: IoBuf,
        priority: TierPriority,
        completion: CompletionSender,
        user_data: u64,
    ) -> std::io::Result<()> {
        // Validate + resolve the device pool and cost (shared with the materialize reactor).
        let (pool, _cost) = self.prepare(kind, device_id, offset, len, buf.is_direct())?;

        // Acquire credit, parking cooperatively if the device is at capacity. This is the
        // backpressure that prevents the blocking-pool deadlock.
        alloc.acquire(&pool, _cost, priority).await?;

        // Move the granted credit (>= cost) onto the op so the alloc resets clean and the completion
        // dispatcher releases it exactly once.
        let granted = alloc.take_all();
        debug_assert!(granted >= _cost, "acquire returned less than cost");

        self.enqueue(
            kind, device_id, fd, offset, len, buf, completion, user_data, granted,
        )
    }

    /// Validate a prospective op and resolve the device pool it draws from plus its credit cost.
    /// Shared by [`submit_with`](Self::submit_with) and the [`MaterializeStream`] reactor, which
    /// drives the credit acquire itself (one slot per device, to avoid cross-device head-of-line
    /// blocking) and so needs the pool + cost without committing to an acquire.
    ///
    /// Rejects (with `InvalidInput`) an out-of-range device, an offset that would wrap a signed
    /// `off_t`, a misaligned direct op, or a cost exceeding the pool capacity (an atomic `IoOp` has
    /// no partial-submit escape, so an over-capacity op could never be admitted).
    ///
    /// [`MaterializeStream`]: crate::fs::materialize::MaterializeStream
    pub(crate) fn prepare(
        &self,
        kind: IoKind,
        device_id: crate::fs::device::DeviceId,
        offset: u64,
        len: u32,
        is_direct: bool,
    ) -> std::io::Result<(Arc<crate::credit::Pool>, u64)> {
        let counters = &self.inner.counters;
        let device = self.inner.devices.get(device_id).ok_or_else(|| {
            counters.submit_rejected.add(1);
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "io scheduler: device id out of range",
            )
        })?;

        if offset > i64::MAX as u64 {
            counters.submit_rejected.add(1);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "io scheduler: offset exceeds i64::MAX",
            ));
        }

        if is_direct {
            let align = crate::fs::direct::ALIGNMENT as u64;
            if !offset.is_multiple_of(align) || !(len as u64).is_multiple_of(align) {
                counters.submit_rejected.add(1);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "io scheduler: direct op offset/length is not block-aligned",
                ));
            }
        }

        let cost = device.cost(kind, len);
        if cost > device.capacity_for(kind) {
            counters.submit_rejected.add(1);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "io scheduler: op cost exceeds device pool capacity",
            ));
        }
        Ok((device.pool_for(kind).clone(), cost))
    }

    /// Build an admitted op (credit already `granted`) and push it into the submission channel; the
    /// dispatch task routes it to a lane. On a closed channel (scheduler torn down) the credit is
    /// released and an error returned, since no completion can arrive. Shared by `submit_with` and
    /// the materialize reactor.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn enqueue(
        &self,
        kind: IoKind,
        device_id: crate::fs::device::DeviceId,
        fd: i32,
        offset: u64,
        len: u32,
        buf: IoBuf,
        completion: CompletionSender,
        user_data: u64,
        granted: u64,
    ) -> std::io::Result<()> {
        let counters = &self.inner.counters;
        counters.submit_ok.add(1);
        counters.submit_bytes.add(len as u64);

        // Round-robin lane hint (the dispatch task makes the final choice).
        let hint = self.inner.round_robin.fetch_add(1, Ordering::Relaxed) % self.inner.lane_count;
        let op = IoOp {
            kind,
            device: device_id,
            fd,
            offset,
            len,
            buf,
            completion: Some(completion),
            status: IoStatus::Pending,
            flow_credits: granted,
            ring_id: crate::fs::device::LocalRingId(hint as u32),
            user_data,
            enqueued_at: Some(self.inner.clock.now()),
        };

        if let Err(mut undelivered) = self.inner.submission.send_entry(Entry::new(op)) {
            let credits = undelivered.take_credits();
            if credits > 0 {
                if let Some(device) = self.inner.devices.get(undelivered.device) {
                    device.pool_for(undelivered.kind).release(credits);
                }
            }
            counters.submit_rejected.add(1);
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "io scheduler: submission channel closed",
            ));
        }
        Ok(())
    }

    /// Number of registered devices (the materialize reactor sizes one acquire slot per device).
    pub(crate) fn device_count(&self) -> usize {
        self.inner.devices.len()
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
