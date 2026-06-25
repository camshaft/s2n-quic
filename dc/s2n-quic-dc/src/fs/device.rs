// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The device — the limited resource **and** its own scheduler.
//!
//! A **device** is the limited resource (a disk / EBS volume / NVMe namespace). It owns the credit
//! pool(s) and the cost model that govern how much work may be in flight against it, *and* it owns
//! its own execution: a submission channel, a dispatch task routing admitted ops to that device's
//! execution lanes, and a credit distributor. There is no global scheduler arbitrating across
//! devices — each device schedules itself, so adding a device adds an independent, self-contained
//! unit, and there is no cross-device footgun (an op physically cannot be submitted to the wrong
//! device's pool, because the submit method *is* the device's).
//!
//! A device is referred to by `Arc<Device>`: the application registers one with the
//! [`DeviceRegistry`](crate::fs::scheduler::DeviceRegistry), gets the `Arc` back, holds it, and
//! calls reads/writes **on it directly** (`device.read(..)`); the op carries the same `Arc` so it can
//! finish itself. There is no device table, numeric id, or lookup: the `Arc` *is* the device handle
//! (the storage analog of `endpoint::frame::Frame` carrying `Arc<PathSecretEntry>`).
//!
//! An **execution lane** ([`LocalRingId`]) is one of a device's worker rings / blocking-pool slots.
//! A device fans its admitted ops across its [`lane_count`](crate::fs::config::DeviceConfig) lanes
//! via its dispatch task's pick-two load balancer. Lane count is a per-device knob, independent of
//! the device's queue depth (which the credit pool capacity governs).

use crate::{
    fs::{
        config::{CostModel, DeviceConfig, OpWeights, PoolMode},
        op::{CompletionReceiver, CompletionSender, Fd, IoBuf, IoKind, IoOp, IoStatus},
        scheduler::alloc::SubmitterAlloc,
    },
    intrusive::Entry,
    sched::{Budget, Pool, TierPriority},
    socket::channel::intrusive::sync as sync_chan,
    sync::Arc,
};
use core::{future::poll_fn, task::Context};

/// Identifies an execution lane (worker ring / blocking-pool slot) within one device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalRingId(pub u32);

impl LocalRingId {
    /// Sentinel for "not yet routed". The device's dispatch task overwrites it.
    pub const UNSET: LocalRingId = LocalRingId(u32::MAX);

    #[inline]
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }

    #[inline]
    pub fn is_set(self) -> bool {
        self != Self::UNSET
    }
}

/// The credit pool(s) backing a device, selected by [`PoolMode`].
pub enum DevicePools {
    Shared(Arc<Pool>),
    Split { read: Arc<Pool>, write: Arc<Pool> },
}

impl DevicePools {
    /// The pool an op of `kind` draws from.
    #[inline]
    pub fn pool_for(&self, kind: IoKind) -> &Arc<Pool> {
        match self {
            DevicePools::Shared(p) => p,
            DevicePools::Split { read, write } => {
                if kind.is_read() {
                    read
                } else {
                    write
                }
            }
        }
    }

    /// All pools, for distributor spawning and conservation checks.
    pub fn all(&self) -> impl Iterator<Item = &Arc<Pool>> {
        match self {
            DevicePools::Shared(p) => vec![p].into_iter(),
            DevicePools::Split { read, write } => vec![read, write].into_iter(),
        }
    }
}

/// The result of a **cancellable** submit ([`Device::submit_cancellable`]).
///
/// The cancellation decision is made in the one window where it is provably safe: **after** the op's
/// credit is acquired but **before** the op is enqueued to a lane. Up to that point no `IoOp` exists
/// downstream, nothing references the target fd/offset, and no buffer has been handed to a backend (or
/// to the kernel, on io_uring) — so cancelling is just "release the credit, hand the buffer back."
///
/// Once an op is enqueued it is owned by the pipeline (and its buffer pinned in the io_uring in-flight
/// slab until the CQE reaps); the write **must** be allowed to complete, because the caller's notion of
/// "this op is dead" may coincide with the caller recycling the op's fixed disk offset to a *new* op —
/// and with multiple unordered lanes/rings the stale in-flight write could land after the new one and
/// tear it. Cancellation is therefore offered *only* in the pre-enqueue window; an enqueued op runs to
/// completion. (See `spill-write-cancellation` in the Membrain design docs for the offset-reuse race
/// this constraint exists to prevent.)
pub enum SubmitOutcome {
    /// The op was enqueued and ran to a successful completion; carries the completed [`IoOp`] (filled
    /// buffer + final status), exactly as [`Device::submit`] returns.
    Completed(IoOp),
    /// The caller's predicate cancelled the op after credit was acquired but before it was enqueued.
    /// No IO was issued and the target offset was never touched; the op's buffer is handed back so the
    /// caller can reuse or drop it. The credit was released back to the device pool.
    Cancelled(IoBuf),
}

/// The result of a cancellable zero-copy `O_DIRECT` write ([`Device::write_direct_cancellable`]). Both
/// arms hand the page-aligned buffer back so it is never leaked, mirroring how [`Device::write_direct`]
/// returns the buffer on success.
pub enum WriteDirectOutcome {
    /// The write completed; the buffer is returned with the number of bytes written.
    Written {
        buf: crate::fs::direct::AlignedBuf,
        len: usize,
    },
    /// The caller's predicate cancelled the write before it was enqueued — no disk IO was issued and
    /// the target offset was never written. The buffer is returned untouched.
    Cancelled { buf: crate::fs::direct::AlignedBuf },
}

/// Whether a credit-acquired op was handed to the pipeline or cancelled in the pre-enqueue window.
/// Internal result of [`Device::submit_with`]; the public [`SubmitOutcome`] / [`WriteDirectOutcome`]
/// are derived from it.
pub(crate) enum Admitted {
    /// The op was enqueued to a lane; a completion will arrive on the caller's channel.
    Enqueued,
    /// The caller's predicate cancelled the op after acquire but before enqueue; the buffer is handed
    /// back and the acquired credit was released. No completion will arrive.
    Cancelled(IoBuf),
}

/// A device: its budget pool(s), cost model, weights, **and its own submission channel** — so a read
/// or write is a method *on the device* and the op it builds carries the `Arc<Device>` straight
/// through the device's own pipeline (the storage analog of `Arc<PathSecretEntry>`), finishing itself
/// with no table lookup. The pacer ([`crate::sched::Rate`]) is applied by the device's dispatch
/// load-balancing stage; the rate is stored here so the pipeline can read it.
///
/// The submit API lives here as methods taking `self: &Arc<Device>` ([`Device::read`],
/// [`Device::write`], [`Device::submit`], …): a holder of the `Arc<Device>` submits against it
/// directly, with no separate handle type and no global scheduler.
pub struct Device {
    /// Application-supplied label (e.g. `"nvme0"`, `"ebs-data"`). Diagnostics only — gauge prefixes
    /// and logs. There is no application-visible numeric id and no lookup table; a device *is* its
    /// `Arc<Device>`, which the application holds and submits against, and which the op carries
    /// through the pipeline.
    pub label: Arc<str>,
    /// Dense, monotonic, **crate-internal** registration index, stamped by the registry. Not part of
    /// the public API (the application deals in `Arc<Device>` + label) — it exists only so internal
    /// per-device indexers can do O(1) direct array indexing instead of a map lookup or pointer scan.
    /// Its sole consumer is [`MaterializeStream`](crate::fs::materialize)'s per-device acquire slots.
    pub(crate) index: usize,
    pub pools: DevicePools,
    pub cost_model: CostModel,
    pub op_weights: OpWeights,
    pub rate: crate::sched::Rate,
    /// Per-device nominal counters (`fs.device.*{device=label}`). Because the op carries this
    /// `Arc<Device>`, both the submit path and the in-place completion path bump these directly — no
    /// extra plumbing — giving per-device IOPS / bytes / failure visibility.
    pub counters: crate::fs::counters::DeviceCounters,
    /// This device's **own** `Send` submission channel: every `submit`/`enqueue` pushes the built
    /// `IoOp` here; the device's dispatch task (on a worker) drains it and routes to *this device's*
    /// lanes. There is no shared cross-device submission path.
    pub(crate) submission: sync_chan::Sender<IoOp>,
    /// Clock used to stamp `enqueued_at` for sojourn metrics.
    pub(crate) clock: crate::time::DefaultClock,
    /// Capacity of the read / write pool (in the cost-model currency), recorded so `prepare` can
    /// reject an op whose cost exceeds it (an `IoOp` is atomic — it has no partial-submit escape, so
    /// a `cost > capacity` acquire could never be satisfied and would park forever).
    read_capacity: u64,
    write_capacity: u64,
}

impl core::fmt::Debug for Device {
    /// Minimal — `IoOp` derives `Debug` and carries `Arc<Device>`, but the pools/cost model are not
    /// usefully printable. Show the label only.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("label", &self.label)
            .finish()
    }
}

impl Device {
    /// Build a device (and its not-yet-spawned credit pools) from config under `label`, with the
    /// crate-internal registration `index` the registry assigned, its per-device nominal counters
    /// registered against `registry`, and its own `submission` channel sender (whose receiver the
    /// registry hands to the registrar to build this device's dispatch task + lanes).
    pub(crate) fn new(
        label: Arc<str>,
        index: usize,
        registry: &crate::counter::Registry,
        cfg: &DeviceConfig,
        submission: sync_chan::Sender<IoOp>,
        clock: crate::time::DefaultClock,
    ) -> Self {
        let counters = crate::fs::counters::DeviceCounters::register(registry, &label);
        let (pools, read_capacity, write_capacity) = match &cfg.pool_mode {
            PoolMode::Shared(c) => {
                let c = atomic_grant(*c);
                (
                    DevicePools::Shared(Arc::new(Pool::new(c))),
                    c.capacity,
                    c.capacity,
                )
            }
            PoolMode::Split { read, write } => {
                let (read, write) = (atomic_grant(*read), atomic_grant(*write));
                (
                    DevicePools::Split {
                        read: Arc::new(Pool::new(read)),
                        write: Arc::new(Pool::new(write)),
                    },
                    read.capacity,
                    write.capacity,
                )
            }
        };
        Self {
            label,
            index,
            pools,
            cost_model: cfg.cost_model,
            op_weights: cfg.op_weights,
            rate: cfg.rate,
            counters,
            submission,
            clock,
            read_capacity,
            write_capacity,
        }
    }

    /// The credit cost of a `kind`/`len` op against this device, in the pool's currency, after the
    /// per-kind weight. This is what `submit` acquires and records in `IoOp::flow_credits`.
    #[inline]
    pub fn cost(&self, kind: IoKind, len: u32) -> u64 {
        let raw = self.cost_model.raw_cost(len);
        self.op_weights.apply(raw, kind.is_read())
    }

    /// The pool an op of `kind` draws from.
    #[inline]
    pub fn pool_for(&self, kind: IoKind) -> &Arc<Pool> {
        self.pools.pool_for(kind)
    }

    /// Capacity of the pool an op of `kind` draws from. An op costing more than this can never be
    /// admitted and must be rejected up front rather than parked.
    #[inline]
    pub fn capacity_for(&self, kind: IoKind) -> u64 {
        if kind.is_read() {
            self.read_capacity
        } else {
            self.write_capacity
        }
    }

    /// Validate a prospective op against this device and resolve the pool it draws from plus its
    /// credit cost, bumping the rejection counter on failure. The hot-path admission check: the caller
    /// already holds the `Arc<Device>`, so there is no lookup here.
    ///
    /// Rejects (with `InvalidInput`) an offset that would wrap a signed `off_t`, a misaligned direct
    /// op, or a cost exceeding the pool capacity (an atomic `IoOp` has no partial-submit escape, so
    /// an over-capacity op could never be admitted and would park forever).
    pub fn prepare(
        &self,
        kind: IoKind,
        offset: u64,
        len: u32,
        is_direct: bool,
    ) -> std::io::Result<(Arc<Pool>, u64)> {
        self.prepare_inner(kind, offset, len, is_direct)
            .inspect_err(|e| {
                self.counters.rejected.add(1);
                // `prepare` does not take the fd (it validates kind/offset/len/alignment only), so the
                // rejected row carries the `u64::MAX` fd sentinel; it has no `op_seq` either (no op was
                // built). The reason is the rejection's `io::ErrorKind`.
                crate::fs::trace::rejected(
                    self.index,
                    kind,
                    u64::MAX,
                    offset,
                    len,
                    is_direct,
                    e.kind(),
                );
            })
    }

    fn prepare_inner(
        &self,
        kind: IoKind,
        offset: u64,
        len: u32,
        is_direct: bool,
    ) -> std::io::Result<(Arc<Pool>, u64)> {
        // The end offset (`offset + len`) must fit in a signed `off_t`: the backends compute
        // `offset + done` and cast to `libc::off_t`, so an end past `i64::MAX` would wrap to a
        // negative offset (kernel `EINVAL`, surfaced confusingly as a `Failed` op). Reject up front.
        if offset
            .checked_add(len as u64)
            .is_none_or(|end| end > i64::MAX as u64)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "io scheduler: offset + len exceeds i64::MAX",
            ));
        }

        if is_direct {
            let align = crate::fs::direct::ALIGNMENT as u64;
            if !offset.is_multiple_of(align) || !(len as u64).is_multiple_of(align) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "io scheduler: direct op offset/length is not block-aligned",
                ));
            }
        }

        let cost = self.cost(kind, len);
        if cost > self.capacity_for(kind) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "io scheduler: op cost exceeds device pool capacity",
            ));
        }
        Ok((self.pool_for(kind).clone(), cost))
    }

    // ── Submit API (moved off the old `SubmitHandle`; submit is now a method on the device) ────────

    /// Submit a read of `len` bytes at `offset` on `fd`, resolving with the filled buffer.
    ///
    /// Allocates a `BytesMut` with capacity `len` and reads into its uninitialized spare capacity (no
    /// zero-fill); the returned buffer's `len()` is the bytes actually read (a short read at EOF
    /// returns fewer bytes, never a zero-padded tail). The returned future is `'static` (captures an
    /// `Arc<Device>` clone).
    pub fn read(
        self: &Arc<Self>,
        fd: Fd,
        offset: u64,
        len: u32,
        priority: TierPriority,
    ) -> impl core::future::Future<Output = std::io::Result<bytes::BytesMut>> + 'static {
        let device = self.clone();
        async move {
            // Caller-owned, pre-sized, logically-empty buffer: the backend reads into spare capacity
            // and `set_len`s to bytes read. No allocation or memset happens inside the backend.
            let buf = bytes::BytesMut::with_capacity(len as usize);
            let op = device
                .submit(IoKind::Read, fd, offset, len, IoBuf::Read(buf), priority)
                .await?;
            match op.buf {
                IoBuf::Read(buf) => Ok(buf),
                _ => unreachable!("read op returned non-read buffer"),
            }
        }
    }

    /// Submit a write of `data` at `offset` on `fd`, resolving with the number of bytes written.
    pub fn write(
        self: &Arc<Self>,
        fd: Fd,
        offset: u64,
        data: bytes::Bytes,
        priority: TierPriority,
    ) -> impl core::future::Future<Output = std::io::Result<usize>> + 'static {
        let device = self.clone();
        async move {
            let len = data.len() as u32;
            let op = device
                .submit(IoKind::Write, fd, offset, len, IoBuf::Write(data), priority)
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
        self: &Arc<Self>,
        fd: Fd,
        offset: u64,
        buf: crate::fs::direct::AlignedBuf,
        priority: TierPriority,
    ) -> impl core::future::Future<Output = std::io::Result<(crate::fs::direct::AlignedBuf, usize)>>
           + 'static {
        let device = self.clone();
        async move {
            let len = buf.len() as u32;
            let op = device
                .submit(IoKind::Read, fd, offset, len, IoBuf::Direct(buf), priority)
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
        self: &Arc<Self>,
        fd: Fd,
        offset: u64,
        buf: crate::fs::direct::AlignedBuf,
        priority: TierPriority,
    ) -> impl core::future::Future<Output = std::io::Result<(crate::fs::direct::AlignedBuf, usize)>>
           + 'static {
        let device = self.clone();
        async move {
            let len = buf.len() as u32;
            let op = device
                .submit(IoKind::Write, fd, offset, len, IoBuf::Direct(buf), priority)
                .await?;
            match (op.status, op.buf) {
                (IoStatus::Done(n), IoBuf::Direct(buf)) => Ok((buf, n)),
                other => unreachable!("direct write returned unexpected op: {other:?}"),
            }
        }
    }

    /// Submit a zero-copy `O_DIRECT` write like [`write_direct`](Self::write_direct), but cancellable
    /// in the pre-enqueue window (the spill-flush use case): `should_cancel` is consulted after credit
    /// is acquired and before the op is enqueued, and if it returns `true` the write is dropped without
    /// touching the disk — the target `offset` is never written, so the caller may safely recycle it.
    ///
    /// Resolves with [`WriteDirectOutcome::Written`] (buffer + bytes written) on completion, or
    /// [`WriteDirectOutcome::Cancelled`] (buffer handed back, untouched) on cancellation. Either way the
    /// page-aligned buffer is returned so it is never leaked. `offset` must be block-aligned (validated).
    ///
    /// This is the API the Membrain spill layer uses to cancel writeback for blocks whose data was
    /// deleted mid-flush: the predicate is the block's "no live owner / not resurrectable" refcount
    /// check, and cancelling strictly before enqueue is what makes freeing-and-recycling the block's
    /// fixed disk offset safe (an enqueued write owns that offset until its CQE reaps). See
    /// [`submit_cancellable`](Self::submit_cancellable) for the full safety rationale.
    pub fn write_direct_cancellable(
        self: &Arc<Self>,
        fd: Fd,
        offset: u64,
        buf: crate::fs::direct::AlignedBuf,
        priority: TierPriority,
        should_cancel: impl FnMut() -> bool + 'static,
    ) -> impl core::future::Future<Output = std::io::Result<WriteDirectOutcome>> + 'static {
        let device = self.clone();
        async move {
            let len = buf.len() as u32;
            match device
                .submit_cancellable(
                    IoKind::Write,
                    fd,
                    offset,
                    len,
                    IoBuf::Direct(buf),
                    priority,
                    should_cancel,
                )
                .await?
            {
                SubmitOutcome::Completed(op) => match (op.status, op.buf) {
                    (IoStatus::Done(n), IoBuf::Direct(buf)) => {
                        Ok(WriteDirectOutcome::Written { buf, len: n })
                    }
                    other => unreachable!("direct write returned unexpected op: {other:?}"),
                },
                SubmitOutcome::Cancelled(IoBuf::Direct(buf)) => {
                    Ok(WriteDirectOutcome::Cancelled { buf })
                }
                SubmitOutcome::Cancelled(other) => {
                    unreachable!("direct write cancelled with non-direct buffer: {other:?}")
                }
            }
        }
    }

    /// The atomic submit primitive: acquire credit, hand the op to one of this device's lanes, await
    /// completion.
    ///
    /// Returns only **successfully completed** ops — a `Failed` completion, an oversized op, a
    /// misaligned direct op, or a closed lane all surface as `Err`, so callers never have to
    /// re-inspect `op.status`. Acquires the op's credit from this device's pool **before** the op is
    /// handed to the pipeline: a submitter that cannot get credit parks cooperatively on a waker
    /// (never a thread), so admitted work can never exceed the pool capacity and the blocking-pool
    /// hold-and-wait deadlock cannot form.
    pub async fn submit(
        self: &Arc<Self>,
        kind: IoKind,
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
        // No cancellation predicate: this op always runs to completion (`|| false` never cancels).
        match self
            .submit_with(
                &mut alloc,
                kind,
                fd,
                offset,
                len,
                buf,
                priority,
                completion_rx.sender(),
                0,
                || false,
            )
            .await?
        {
            Admitted::Enqueued => {}
            Admitted::Cancelled(_) => {
                unreachable!("non-cancellable submit cannot cancel (predicate is `|| false`)")
            }
        }
        let op = await_completion(completion_rx).await?;
        match op.status {
            IoStatus::Done(_) => Ok(op),
            IoStatus::Failed(kind) => Err(kind.into()),
            IoStatus::Pending => unreachable!("dispatcher routed a still-pending op"),
        }
    }

    /// Submit like [`submit`](Self::submit) but allow the caller to **cancel the op in the one window
    /// where cancellation is provably safe**: after its credit is acquired but before it is enqueued.
    ///
    /// `should_cancel` is consulted twice, both **pre-enqueue**: once before any credit is acquired
    /// (an op already dead at submit time never touches the pool) and once the instant credit is
    /// granted (an op that died while parked on credit is dropped before any IO is issued). If it
    /// returns `true` at either point the op is **not** enqueued: its credit is released, the buffer is
    /// handed back in [`SubmitOutcome::Cancelled`], and no syscall/SQE ever references the target
    /// fd/offset. Otherwise the op is enqueued and awaited exactly as [`submit`](Self::submit), yielding
    /// [`SubmitOutcome::Completed`].
    ///
    /// Why only this window: once enqueued, the op (and its buffer) is owned by the pipeline and, on
    /// io_uring, pinned in the in-flight slab until the kernel CQE reaps it — so the write **must**
    /// complete. A caller that treats "this op is dead" as license to recycle the op's fixed disk
    /// offset to a new op would otherwise race a stale in-flight write against the new one across
    /// unordered lanes and tear it. Cancelling strictly pre-enqueue removes that hazard by construction.
    /// (To abandon an op that has *already* been enqueued, drop the returned future — that is
    /// memory-safe, the buffer rides the op to completion downstream — but the write still hits the
    /// device; it is not cancellation.)
    ///
    /// `should_cancel` is **not** re-polled while the op is parked waiting for credit (only on the
    /// grant wake). During a churn event the dead backlog still drains without issuing IO: each parked
    /// op, when finally granted credit, observes the predicate and releases immediately — the device
    /// (the bottleneck) sees zero wasted writes. To cancel a parked op *sooner* (freeing its credit
    /// slot for live writes before it reaches the grant), race this future against a cancellation
    /// signal and drop it — dropping a parked, never-enqueued submit is clean (its credit slot is
    /// abandoned and reclaimed).
    #[allow(clippy::too_many_arguments)]
    pub async fn submit_cancellable(
        self: &Arc<Self>,
        kind: IoKind,
        fd: Fd,
        offset: u64,
        len: u32,
        buf: IoBuf,
        priority: TierPriority,
        should_cancel: impl FnMut() -> bool,
    ) -> std::io::Result<SubmitOutcome> {
        let completion_rx: CompletionReceiver =
            crate::socket::channel::intrusive::datagram_completion::new::<IoOp>();
        let mut alloc = SubmitterAlloc::new();
        match self
            .submit_with(
                &mut alloc,
                kind,
                fd,
                offset,
                len,
                buf,
                priority,
                completion_rx.sender(),
                0,
                should_cancel,
            )
            .await?
        {
            Admitted::Cancelled(buf) => Ok(SubmitOutcome::Cancelled(buf)),
            Admitted::Enqueued => {
                let op = await_completion(completion_rx).await?;
                match op.status {
                    IoStatus::Done(_) => Ok(SubmitOutcome::Completed(op)),
                    IoStatus::Failed(kind) => Err(kind.into()),
                    IoStatus::Pending => unreachable!("dispatcher routed a still-pending op"),
                }
            }
        }
    }

    /// Acquire credit (reusing the caller's `alloc`) and enqueue an op whose completion is delivered
    /// to the caller-provided `completion` sender (tagged with `user_data`), then return — **without**
    /// awaiting the completion. The building block for a submitter that funnels many ops onto one
    /// shared completion channel and reuses one acquire context across them (e.g.
    /// [`MaterializeStream`](crate::fs::materialize)), avoiding a per-op slot allocation.
    ///
    /// `should_cancel` is the pre-enqueue cancellation hook (see [`submit_cancellable`](Self::submit_cancellable)
    /// for the full rationale and the offset-reuse race it guards). It is consulted **only** before the
    /// op is enqueued — once before acquiring credit and once the instant credit is granted — never
    /// after. Read submitters that never cancel pass `|| false` (zero overhead: a never-true closure
    /// the optimizer drops).
    ///
    /// Returns `Ok(Admitted::Enqueued)` when the op was admitted (credit acquired) and handed to a
    /// lane — a completion will arrive on `completion`. Returns `Ok(Admitted::Cancelled(buf))` when the
    /// predicate cancelled it pre-enqueue: the acquired credit was released and the buffer is handed
    /// back; **no** completion will arrive. An `Err` (misaligned/oversized op, or this device's
    /// submission channel closed on teardown) likewise means no completion will arrive and any acquired
    /// credit was released.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn submit_with(
        self: &Arc<Self>,
        alloc: &mut SubmitterAlloc,
        kind: IoKind,
        fd: Fd,
        offset: u64,
        len: u32,
        buf: IoBuf,
        priority: TierPriority,
        completion: CompletionSender,
        user_data: u64,
        mut should_cancel: impl FnMut() -> bool,
    ) -> std::io::Result<Admitted> {
        // Validate + resolve the device pool and cost directly on this device (no lookup).
        let (pool, cost) = self.prepare(kind, offset, len, buf.is_direct())?;

        // Cancel before touching the pool: an op already dead at submit time should never acquire
        // credit (it would needlessly contend the pool against live ops). Nothing was acquired or
        // enqueued, so handing the buffer back is all that is owed.
        if should_cancel() {
            self.counters.cancelled_before_submit.add(1);
            // Pre-acquire window: no credit was ever held (`had_credit = false`), no op built.
            crate::fs::trace::cancelled_before_submit(
                self.index,
                kind,
                fd.as_raw() as u64,
                offset,
                len,
                buf.is_direct(),
                false,
            );
            return Ok(Admitted::Cancelled(buf));
        }

        // Acquire credit, parking cooperatively if the device is at capacity. This is the
        // backpressure that prevents the blocking-pool deadlock.
        alloc.set_trace_ctx(self.index, kind);
        alloc.acquire(&pool, cost, priority).await?;

        // Credit is granted but the op is NOT yet enqueued — the last instant cancellation is safe.
        // An op that died while parked on credit is dropped here before any IO is issued: release the
        // just-granted credit back to the pool (keeping it conserved and freeing the slot for a live
        // op) and hand the buffer back. Crucially this is still pre-enqueue, so the target fd/offset
        // was never handed to a backend/kernel and cannot race a later op reusing that offset.
        if should_cancel() {
            let granted = alloc.take_all();
            if granted > 0 {
                pool.release(granted);
            }
            self.counters.cancelled_before_submit.add(1);
            // Post-acquire window: credit was granted then released (`had_credit = true`), still no op.
            crate::fs::trace::cancelled_before_submit(
                self.index,
                kind,
                fd.as_raw() as u64,
                offset,
                len,
                buf.is_direct(),
                true,
            );
            return Ok(Admitted::Cancelled(buf));
        }

        // Move the granted credit (>= cost) onto the op so the alloc resets clean and the completion
        // path releases it exactly once.
        let granted = alloc.take_all();
        debug_assert!(granted >= cost, "acquire returned less than cost");

        // A one-shot submit is not part of a multi-op stream — `NO_STREAM` (the trace builds map it to
        // SQL NULL); the materialize reactor passes its own shared id.
        self.enqueue(
            kind,
            fd,
            offset,
            len,
            buf,
            completion,
            user_data,
            granted,
            crate::fs::trace::NO_STREAM,
        )?;
        Ok(Admitted::Enqueued)
    }

    /// Build an admitted op (credit already `granted`) carrying this `Arc<Device>` and push it into
    /// the device's submission channel; the device's dispatch task routes it to one of its lanes. On a
    /// closed channel (device torn down) the credit is released and an error returned, since no
    /// completion can arrive. Shared by `submit_with` and the materialize reactor.
    ///
    /// `stream_id` groups this op with the other ops of a multi-op submitter for the
    /// [flight recorder](crate::fs::trace) ([`NO_STREAM`](crate::fs::trace::NO_STREAM) for a one-off);
    /// it is only stored in the trace builds.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn enqueue(
        self: &Arc<Self>,
        kind: IoKind,
        fd: Fd,
        offset: u64,
        len: u32,
        buf: IoBuf,
        completion: CompletionSender,
        user_data: u64,
        granted: u64,
        stream_id: u64,
    ) -> std::io::Result<()> {
        // Per-device admission counters (the caller holds the `Arc<Device>`, so no extra plumbing):
        // bump this op-kind's submitted count and, for data ops, its submit-size histogram.
        let opk = self.counters.op(kind);
        opk.submitted.add(1);
        if let Some(hist) = &opk.submit_bytes {
            hist.record_value(len as u64);
        }

        let op = IoOp {
            kind,
            device: self.clone(),
            fd,
            offset,
            len,
            buf,
            completion: Some(completion),
            status: IoStatus::Pending,
            flow_credits: granted,
            // `LocalRingId::UNSET` until the dispatch task picks a lane (which overwrites it).
            ring_id: LocalRingId::UNSET,
            user_data,
            enqueued_at: Some(self.clock.now()),
            // Mint the per-op correlation id at the single admission funnel (trace builds only).
            #[cfg(any(test, feature = "testing", feature = "io-dbg"))]
            op_seq: crate::fs::trace::next_op_seq(),
            // The caller-supplied stream grouping id (NO_STREAM for one-off submits).
            #[cfg(any(test, feature = "testing", feature = "io-dbg"))]
            stream_id,
        };
        // `stream_id` is only read into the op under the trace cfg; reference it otherwise so the
        // parameter is not dead in a plain build.
        #[cfg(not(any(test, feature = "testing", feature = "io-dbg")))]
        let _ = stream_id;

        // First live sighting of the op (credit acquired, about to enter the pipeline).
        crate::fs::trace::submitted(&op);

        if let Err(mut undelivered) = self.submission.send_entry(Entry::new(op)) {
            crate::fs::trace::enqueue_closed(&undelivered);
            undelivered.release_credits();
            undelivered.device.counters.rejected.add(1);
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "io scheduler: device submission channel closed (device torn down)",
            ));
        }
        Ok(())
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
            "io scheduler: device completion channel closed before completion",
        )),
    }
}

/// Force **atomic** credit grants for a storage pool: raise `max_single_acquire` to the full
/// capacity and floor `min_grant_slice` at that same ceiling, per priority.
///
/// The credit pool's demand-elastic fair share normally splits a parked waiter's request into
/// `free / num_waiters` slices (great for QUIC byte-streams, which send the partial and release it).
/// But an [`IoOp`](crate::fs::op::IoOp) is **indivisible** — it cannot execute, and therefore cannot
/// release, until it holds its *full* cost. If two contending ops each pinned a partial slice,
/// neither could run, nothing would release, and the pool would wedge (exactly the deadlock the
/// `Writer` docs warn about, but with no partial-progress escape).
///
/// Atomicity has **two** sides and we must close both:
/// * the **grant** side — the distributor's `min_grant_slice`, floored here at the per-op ceiling so
///   the distributor never hands a parked waiter a sub-op sliver;
/// * the **acquire** side — the pool clamps every acquire request to `max_single_acquire`
///   ([`Pool::poll_acquire`](crate::credit::Pool) → `clamp_request`), so if `max_single_acquire`
///   stayed at the demand-elastic default (`capacity/256 .. capacity/64`), an op whose
///   `cost > max_single_acquire` would *itself* fragment into capped partial fast-path grants that
///   accumulate below `cost` and pin the pool — the same wedge, reintroduced on the acquire path.
///
/// So we raise `max_single_acquire` to `capacity` first. Combined with the submit-time guard that an
/// op's `cost ≤ capacity`, every admitted op is now requested **and** granted whole in one wake — a
/// true all-or-nothing acquisition. Fairness across streams is preserved: the distributor still walks
/// waiters FIFO within a tier and serves whole ops round-robin.
///
/// (Trade-off: a larger `max_single_acquire` widens the worst-case transient negative `available`
/// excursion to one full op per parked waiter — acceptable for an isolated per-device pool, and the
/// price of indivisible ops.)
fn atomic_grant(config: crate::sched::CreditConfig) -> crate::sched::CreditConfig {
    let cap = config.capacity.max(1);
    let config = config.with_max_single_acquire_uniform(cap);
    let caps = config.max_single_acquire;
    config.with_min_grant_slice_per_priority(caps)
}
