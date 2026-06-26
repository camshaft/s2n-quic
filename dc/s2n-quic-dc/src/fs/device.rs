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

    /// Live load on this device's pool for `kind`, in the cost-model currency: how much credit is
    /// committed right now, `(capacity − available).max(0)`. This rises smoothly from **0 at idle**
    /// (`available == capacity`) through in-flight work and on into parked overflow (`available` goes
    /// negative), so unlike raw parked demand it is a useful balancing signal *before* a device
    /// saturates. Used by the shard picker for pick-two load balancing across drives (lower is better).
    ///
    /// Computed entirely in the `u64` domain with saturating ops so a transient credit-accounting
    /// skew (an over-release pushing `available` past `capacity`, or a deeply-negative parked total)
    /// can never overflow or wrap — it just clamps to `0` or `u64::MAX`. See [`pool_load`].
    #[inline]
    pub fn load(&self, kind: IoKind) -> u64 {
        pool_load(self.capacity_for(kind), self.pool_for(kind).available())
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

    /// The atomic submit primitive: acquire credit, hand the op to one of this device's lanes, await
    /// completion.
    ///
    /// Returns only **successfully completed** ops — a `Failed` completion, an oversized op, a
    /// misaligned direct op, or a closed lane all surface as `Err`, so callers never have to
    /// re-inspect `op.status`. Acquires the op's credit from this device's pool **before** the op is
    /// handed to the pipeline: a submitter that cannot get credit parks cooperatively on a waker
    /// (never a thread), so admitted work can never exceed the pool capacity and the blocking-pool
    /// hold-and-wait deadlock cannot form.
    ///
    /// This is the all-in-one form — it [`reserve`](Self::reserve)s credit and then immediately
    /// [`submit`](Reservation::submit)s the buffer on the reservation. A caller that wants to apply
    /// backpressure *before* committing the buffer (or wants the option to cancel after acquiring
    /// credit) should call [`reserve`](Self::reserve) directly and hold the [`Reservation`].
    pub async fn submit(
        self: &Arc<Self>,
        kind: IoKind,
        fd: Fd,
        offset: u64,
        len: u32,
        buf: IoBuf,
        priority: TierPriority,
    ) -> std::io::Result<IoOp> {
        let is_direct = buf.is_direct();
        self.reserve(kind, fd, offset, len, priority, is_direct)
            .await?
            .submit(buf)
            .await
    }

    /// Phase one of a two-phase submit: validate the op and **acquire its credit**, returning a
    /// [`Reservation`] that holds the granted credit. Phase two is [`Reservation::submit`] (hand over
    /// the buffer, enqueue, await completion) — or, to cancel, simply **drop** the reservation.
    ///
    /// This is the cancellation-safe alternative to a one-shot [`submit`](Self::submit): credit is the
    /// scarce, indivisible resource, so acquiring it is the right place to apply backpressure and the
    /// right granularity to cancel at. Awaiting `reserve` parks cooperatively on a waker (never a
    /// thread) when the device is at capacity, exactly as `submit` does — so a producer can gate its
    /// own fan-out on `reserve().await` and spawn the per-op task only once a credit slot is in hand,
    /// bounding in-flight work to the pool capacity without a separate semaphore.
    ///
    /// **Cancellation = drop.** A `Reservation` that is dropped without [`submit`](Reservation::submit)
    /// releases its credit back to the pool and issues **no** IO — the target fd/offset is never handed
    /// to a backend or the kernel. This holds whether the drop happens after the credit is granted (the
    /// held `Reservation`) or while the `reserve` future is still parked on credit (drop the future):
    /// both abandon the credit slot cleanly. This is the one provably safe cancellation window. Once
    /// [`submit`](Reservation::submit) enqueues the op it is owned by the pipeline — and on io_uring its
    /// buffer is pinned in the in-flight slab until the CQE reaps it — so the write **must** run to
    /// completion; a caller that treated a post-enqueue drop as license to recycle the op's fixed disk
    /// offset would race a stale in-flight write against the new one across unordered lanes and tear it.
    /// (See `spill-write-cancellation` in the Membrain design docs for that offset-reuse race.)
    ///
    /// `is_direct` selects `O_DIRECT` validation: it must match the buffer eventually handed to
    /// [`submit`](Reservation::submit) (a direct op requires `offset`/`len` block-aligned, validated
    /// here, before any credit is acquired, so an op that can never be admitted never contends the pool).
    pub async fn reserve(
        self: &Arc<Self>,
        kind: IoKind,
        fd: Fd,
        offset: u64,
        len: u32,
        priority: TierPriority,
        is_direct: bool,
    ) -> std::io::Result<Reservation> {
        // Validate + resolve the device pool and cost directly on this device (no lookup). Done before
        // any credit is acquired so an op that can never be admitted (oversized / misaligned) is
        // rejected without contending the pool against live ops.
        let (pool, cost) = self.prepare(kind, offset, len, is_direct)?;

        // Acquire credit, parking cooperatively if the device is at capacity. This is the backpressure
        // that prevents the blocking-pool deadlock — and the gate a producer waits on before spawning.
        let mut alloc = SubmitterAlloc::new();
        alloc.set_trace_ctx(self.index, kind);
        alloc.acquire(&pool, cost, priority).await?;

        Ok(Reservation {
            device: self.clone(),
            alloc: Some(alloc),
            kind,
            fd,
            offset,
            len,
            cost,
            is_direct,
        })
    }

    /// Build an admitted op (credit already `granted`) carrying this `Arc<Device>` and push it into
    /// the device's submission channel; the device's dispatch task routes it to one of its lanes. On a
    /// closed channel (device torn down) the credit is released and an error returned, since no
    /// completion can arrive. Shared by the [`Reservation`] submit path and the materialize reactor.
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

/// A credit reservation against a [`Device`]: the held grant from a successful [`Device::reserve`],
/// waiting for its buffer. This is phase two of the two-phase submit — the point where the indivisible
/// scarce resource (credit) has been acquired but the op has **not** yet been built or enqueued.
///
/// # Two things a reservation can do
///
/// * [`submit`](Self::submit) the buffer — build the op, enqueue it to one of the device's lanes, and
///   await its completion. The reservation is consumed; its held credit rides onto the op and is
///   released exactly once when the op completes.
/// * **be dropped** — cancellation. The held credit is released back to the pool and **no IO is
///   issued**: the target fd/offset is never handed to a backend or the kernel, so a caller may safely
///   recycle that offset. This is the only provably safe cancellation window (see [`Device::reserve`]
///   for why post-enqueue is not), and it is free: dropping the owned [`SubmitterAlloc`] releases its
///   grant under the credit slot's abandon machinery.
///
/// # Backpressure
///
/// Because `reserve` parks until credit is in hand, a producer that calls `reserve().await` *before*
/// spawning the per-op task naturally bounds its in-flight fan-out to the device's pool capacity — the
/// reservation *is* the permit. Holding a `Reservation` while doing other work (e.g. snapshotting the
/// buffer to write) keeps that credit slot reserved; dropping it hands the slot back to other writers.
///
/// The reservation carries an `Arc<Device>`, so it is `'static` and `Send` — it can be moved into a
/// spawned task and submitted there.
pub struct Reservation {
    device: Arc<Device>,
    /// The acquire context holding the granted credit. `Some` until [`submit`](Self::submit) consumes
    /// it (moving the credit onto the op) — `None` afterward so [`Drop`] knows not to count a cancel.
    /// On a plain drop (cancellation) the `SubmitterAlloc`'s own `Drop` releases the held credit.
    alloc: Option<SubmitterAlloc>,
    kind: IoKind,
    fd: Fd,
    offset: u64,
    len: u32,
    /// The credit cost `reserve` acquired (a `debug_assert` floor on what the alloc hands back).
    cost: u64,
    /// Whether the eventual buffer is a zero-copy `O_DIRECT` buffer — validated at `reserve` time and
    /// re-checked against the submitted buffer so the two cannot disagree.
    is_direct: bool,
}

impl Reservation {
    /// The device this reservation holds credit against.
    #[inline]
    pub fn device(&self) -> &Arc<Device> {
        &self.device
    }

    /// The op kind (read/write/…) this reservation was acquired for.
    #[inline]
    pub fn kind(&self) -> IoKind {
        self.kind
    }

    /// Phase two: hand over the `buf`, enqueue the op to one of the device's lanes, and await its
    /// completion. Consumes the reservation; the held credit rides onto the op (released exactly once
    /// when it completes). Returns only **successfully completed** ops — a `Failed` completion or a
    /// closed lane surfaces as `Err`, exactly like [`Device::submit`].
    ///
    /// `buf`'s kind must match the reservation: its [`is_direct`](IoBuf::is_direct) must equal the
    /// `is_direct` passed to [`reserve`](Device::reserve), and its length is expected to match the
    /// reserved `len` (the credit was sized for it). A mismatch is a caller bug (`debug_assert`).
    pub async fn submit(self, buf: IoBuf) -> std::io::Result<IoOp> {
        let completion_rx: CompletionReceiver =
            crate::socket::channel::intrusive::datagram_completion::new::<IoOp>();
        self.enqueue(buf, completion_rx.sender(), 0)?;
        let op = await_completion(completion_rx).await?;
        match op.status {
            IoStatus::Done(_) => Ok(op),
            IoStatus::Failed(kind) => Err(kind.into()),
            IoStatus::Pending => unreachable!("dispatcher routed a still-pending op"),
        }
    }

    /// Phase two, non-awaiting: enqueue the op against the caller-provided `completion` sender (tagged
    /// with `user_data`) and return immediately, **without** awaiting completion. Consumes the
    /// reservation. The building block for a submitter that funnels many ops onto one shared completion
    /// channel (e.g. a fire-many/await-elsewhere pattern); [`submit`](Self::submit) is the await-here form.
    ///
    /// On a closed submission channel (device torn down) the credit is released and an `Err` returned —
    /// no completion will arrive.
    pub fn enqueue(
        mut self,
        buf: IoBuf,
        completion: CompletionSender,
        user_data: u64,
    ) -> std::io::Result<()> {
        // The buffer must match the *kind* the reservation validated and sized credit for. We do not
        // assert `buf.len() == self.len`: a buffered `IoBuf::Read` is intentionally logically empty
        // (pre-sized *capacity*, the backend reads into spare capacity and sets the length to bytes
        // read), so its `len()` is 0 at submit time while the reservation's `len` is the requested
        // transfer. The reservation's `len` is the authoritative transfer length carried onto the op.
        debug_assert_eq!(
            buf.is_direct(),
            self.is_direct,
            "Reservation::submit buffer direct-ness must match the reservation"
        );

        // Move the granted credit (>= cost) off the alloc and onto the op so the alloc drops clean (no
        // double release) and the completion path releases it exactly once. Taking the alloc here also
        // marks the reservation consumed, so `Drop` does not count a cancel.
        let mut alloc = self.alloc.take().expect("reservation already consumed");
        let granted = alloc.take_all();
        debug_assert!(granted >= self.cost, "acquire returned less than cost");
        drop(alloc);

        // A one-shot reservation submit is not part of a multi-op stream — `NO_STREAM` (the trace
        // builds map it to SQL NULL); the materialize reactor passes its own shared id.
        self.device.enqueue(
            self.kind,
            self.fd.clone(),
            self.offset,
            self.len,
            buf,
            completion,
            user_data,
            granted,
            crate::fs::trace::NO_STREAM,
        )
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // `submit`/`enqueue` took the alloc → the op carries the credit; nothing to do. Otherwise this
        // is a cancellation: the `SubmitterAlloc`'s own `Drop` releases the held grant back to the pool
        // (no IO was ever issued — the offset is untouched). Record it for observability.
        if self.alloc.take().is_some() {
            self.device.counters.cancelled_before_submit.add(1);
            // Credit was granted then released (`had_credit = true`); no op was ever built.
            crate::fs::trace::cancelled_before_submit(
                self.device.index,
                self.kind,
                self.fd.as_raw() as u64,
                self.offset,
                self.len,
                self.is_direct,
                true,
            );
        }
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

/// Committed credit against a pool of `capacity` whose live counter reads `available`:
/// `(capacity − available)` clamped to `[0, u64::MAX]`. Factored out of [`Device::load`] so the
/// saturating arithmetic — the part that must survive an out-of-range `available` from a transient
/// credit-accounting skew — can be unit-tested without standing up a whole device + distributor.
///
/// `available` is the pool's free credit: it starts at `capacity`, drops toward `0` as ops take
/// credit, and goes **negative** by the parked demand once waiters queue. So load is `0` at idle and
/// rises monotonically into oversubscription. The whole computation stays in `u64` with saturating
/// ops: `available > capacity` (over-release) clamps to `0`, and a pathologically negative
/// `available` (even `i64::MIN`) clamps at `u64::MAX` rather than overflowing.
#[inline]
fn pool_load(capacity: u64, available: i64) -> u64 {
    if available >= 0 {
        capacity.saturating_sub(available as u64)
    } else {
        capacity.saturating_add(available.unsigned_abs())
    }
}

#[cfg(test)]
mod load_tests {
    use super::pool_load;

    #[test]
    fn zero_at_idle_rising_through_capacity() {
        let cap = 32 * 1024 * 1024;
        // Idle: available == capacity ⇒ no load.
        assert_eq!(pool_load(cap, cap as i64), 0);
        // Half the pool committed.
        assert_eq!(pool_load(cap, (cap / 2) as i64), cap / 2);
        // Fully committed, nothing parked yet.
        assert_eq!(pool_load(cap, 0), cap);
    }

    #[test]
    fn parked_overflow_adds_to_capacity() {
        let cap = 1_000u64;
        // 250 bytes of demand parked beyond a full pool ⇒ load = capacity + parked.
        assert_eq!(pool_load(cap, -250), 1_250);
    }

    #[test]
    fn out_of_range_available_saturates_not_overflows() {
        let cap = 1_000u64;
        // Over-release (available > capacity) must clamp to 0, never wrap negative.
        assert_eq!(pool_load(cap, 5_000), 0);
        // The i64::MIN worst case for naive negation must not panic: `unsigned_abs()` yields 2^63
        // and the add (cap + 2^63) still fits in u64, so it is exact, not saturated.
        assert_eq!(pool_load(cap, i64::MIN), cap + (i64::MIN).unsigned_abs());
        // A sum that genuinely exceeds u64::MAX saturates rather than wrapping.
        assert_eq!(pool_load(u64::MAX, -1), u64::MAX);
        assert_eq!(pool_load(u64::MAX, i64::MIN), u64::MAX);
    }
}
