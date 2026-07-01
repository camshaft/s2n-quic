// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-op flight recorder for the storage IO scheduler, backed by [`backbeat`].
//!
//! Every operation the scheduler processes is recorded as a single [`IoOpEvent`] at each point in its
//! life — admitted, parked/granted on credit, dispatched to a lane, started/finished on the backend,
//! and completed — captured into the process-wide [`backbeat::global`] recorder (a CPU-sharded ring
//! push with no allocation on the hot path). This is the storage analog of
//! [`crate::endpoint::frame_trace`], and it follows the same philosophy with one deliberate divergence.
//!
//! # One event type, two axes (vs. frame_trace's per-kind events)
//!
//! `frame_trace` defines one struct *per wire frame kind* because each kind destructures to genuinely
//! different fields (an `AckFrame` has `largest_acknowledged`; a `QueueFreeFrame` has
//! `free_request_id`). An [`IoOp`](crate::fs::op::IoOp) is the opposite: every
//! [`IoKind`](crate::fs::op::IoKind) (read/write/fsync/…) carries the *identical* structural fields
//! (`fd`/`offset`/`len`/buffer) — the kind only selects which syscall runs, not which fields mean
//! something. So the honest representation is **one** [`IoOpEvent`] with [`kind`](IoOpEvent::kind) and
//! [`lifecycle`](IoOpEvent::lifecycle) as *fields*, not five byte-identical structs differing only by
//! a namespace string. Column-honesty still holds: every field means exactly its name at every
//! lifecycle, and `kind`/`lifecycle` are [`EventEnum`] value→label maps embedded in the schema.
//!
//! # Correlation
//!
//! Each op is stamped with a monotonic [`op_seq`](IoOpEvent::op_seq) at admission (see
//! [`crate::fs::op::IoOp::op_seq`]); `WHERE op_seq = N` stitches one op's whole lifecycle. The
//! recyclable `(device_index, fd, offset)` tuple is also recorded as a coarse fallback for the
//! pre-admission lifecycles ([`Rejected`](IoLifecycle::Rejected), the pre-acquire
//! [`CancelledBeforeSubmit`](IoLifecycle::CancelledBeforeSubmit)) that fire before an `IoOp` — and
//! hence an `op_seq` — exists; those rows carry [`NO_OP_SEQ`].
//!
//! # Gating
//!
//! The whole facility is gated behind [`crate::fs::dbg::ENABLED`]: in a plain release build every
//! entry point folds to nothing — the event is never constructed and the recorder is never built. It
//! is active under `test`, the `testing` feature, or the dedicated `io-dbg` feature.

use crate::fs::op::{IoKind, IoOp, IoStatus};
// `Event`/`EventEnum` name both a trait and a derive macro in backbeat; importing them plainly brings
// the derive macros (and the `#[event(...)]` helper attribute) into scope, plus the traits (so
// `IoOpEvent::ID` resolves).
use backbeat::{Event, EventEnum};
use zerocopy::{Immutable, IntoBytes};

// Register the IO-scheduler correlation views so every `.bb` dump carries the `op_timeline` /
// `device_ops` / `offset_timeline` / `stuck_ops` / `credit_waits` DuckDB macros; `backbeat convert`
// appends them after its generated per-event views.
backbeat::register_views!(include_str!("trace.views.sql"));

/// Stored in [`IoOpEvent::op_seq`] for a lifecycle that fires before an [`IoOp`] (and thus its
/// `op_seq`) exists — [`Rejected`](IoLifecycle::Rejected) and the pre-acquire
/// [`CancelledBeforeSubmit`](IoLifecycle::CancelledBeforeSubmit). The view layer maps it to SQL NULL.
pub(crate) const NO_OP_SEQ: u64 = u64::MAX;

/// Stored in [`IoOpEvent::stream_id`] for an op that does not belong to a multi-op stream — a one-off
/// [`Device::submit`](crate::fs::device::Device) / [`Reservation`](crate::fs::device::Reservation)
/// submit, and every pre-op lifecycle. A
/// streaming submitter (e.g. [`MaterializeStream`](crate::fs::materialize)) stamps a real, shared id so
/// `WHERE stream_id = N` returns every op of that one run. The view layer maps the sentinel to SQL NULL
/// (trace a standalone op by [`op_seq`](IoOpEvent::op_seq) instead).
pub(crate) const NO_STREAM: u64 = u64::MAX;

/// Where in an op's life it was observed. A *field* on every [`IoOpEvent`] (not a type), so one op is
/// tracked across its whole lifecycle with a single column filter.
///
/// Discriminants are stable dump values — append new variants, never renumber. [`backbeat`] embeds
/// the value→label map in the schema (via [`EventEnum`]), so the CLI renders these as names.
#[derive(EventEnum, IntoBytes, Immutable, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum IoLifecycle {
    /// `Device::prepare` rejected the op before admission (offset wrap, misaligned direct op, or cost
    /// exceeding pool capacity). No credit acquired, no `IoOp` built — carries [`NO_OP_SEQ`].
    Rejected = 0,
    /// The caller's predicate cancelled the op after `prepare` but before enqueue — the cancel-before-IO
    /// window. [`IoOpEvent::had_credit`] distinguishes the two sub-windows: `false` = cancelled before
    /// any credit was acquired (carries [`NO_OP_SEQ`]); `true` = cancelled the instant credit was
    /// granted, before enqueue (credit released).
    CancelledBeforeSubmit = 1,
    /// Admitted: credit acquired, `IoOp` built, `op_seq`/`enqueued_at` stamped, pushed to the device's
    /// submission channel. The first sighting of a live op.
    Submitted = 2,
    /// The device's submission channel was closed at enqueue time (device torn down) — the op was never
    /// admitted downstream and its credit was released.
    EnqueueClosed = 3,
    /// A credit acquire parked on a waker after requesting credit from the pool (the device was at
    /// capacity). Correlates by `(device_index, kind, cost)` — see the module docs; the `op_seq` is not
    /// yet known on the one-shot submit path, so it is [`NO_OP_SEQ`].
    CreditPark = 4,
    /// A credit acquire reached its full `cost` and returned — the op may now be enqueued.
    CreditGrant = 5,
    /// A credit acquire took a partial grant (capped at the pool's `max_single_acquire`) and self-woke
    /// to acquire the remainder.
    CreditPartial = 6,
    /// The dispatch task assigned a lane ([`IoOpEvent::ring_id`] now set) and handed the op to that
    /// lane's channel.
    Dispatched = 7,
    /// The dispatch task could not hand the op to a lane (lane/backend closed); the op was stamped
    /// `Failed` and completed in place.
    LaneClosed = 8,
    /// The backend began executing the op (syscall issued / io_uring SQE built and submitted).
    BackendStart = 9,
    /// The backend finished executing the op (syscall returned / io_uring CQE reaped). For the syscall
    /// backend this is adjacent to [`Completed`](Self::Completed)/[`Failed`](Self::Failed); for
    /// io_uring it pins kernel-residency time (SQE submit → CQE reap straddles the kernel).
    BackendDone = 10,
    /// Completed successfully — [`IoOpEvent::bytes_done`] carries the transfer count.
    Completed = 11,
    /// Backend execution failed — [`IoOpEvent::err_kind`] carries the error.
    Failed = 12,
    /// Completed but the submitter's receiver had already dropped — the result was discarded.
    Orphaned = 13,
    /// The backend skipped execution because the submitter's receiver was already gone — no syscall/SQE
    /// was issued.
    CancelledSkipped = 14,
}

/// Dump mirror of [`IoKind`] (which is not `IntoBytes`). Discriminants **must** equal
/// [`IoKind::index`] (guarded by a test), so the dump's kind label aligns with the per-kind metric
/// index. Append, never renumber.
#[derive(EventEnum, IntoBytes, Immutable, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum IoOpKind {
    Read = 0,
    Write = 1,
    Fsync = 2,
    Fdatasync = 3,
    Trim = 4,
}

impl From<IoKind> for IoOpKind {
    #[inline]
    fn from(k: IoKind) -> Self {
        match k {
            IoKind::Read => IoOpKind::Read,
            IoKind::Write => IoOpKind::Write,
            IoKind::Fsync => IoOpKind::Fsync,
            IoKind::Fdatasync => IoOpKind::Fdatasync,
            IoKind::Trim => IoOpKind::Trim,
        }
    }
}

/// A compact, stable mirror of the subset of [`std::io::ErrorKind`] the pipeline produces. The
/// storage analog of [`frame_trace::DropReason`](crate::endpoint::frame_trace). [`None`](Self::None)
/// (0) is the resting value on every non-error row. Append, never renumber.
#[derive(EventEnum, IntoBytes, Immutable, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ErrorKindCode {
    /// Not an error (resting value).
    None = 0,
    /// `InvalidInput` — a `prepare` rejection (offset wrap, misalignment, oversized cost) or a missing
    /// buffer.
    InvalidInput = 1,
    /// `BrokenPipe` — a closed submission/lane channel or a closed credit pool (teardown).
    BrokenPipe = 2,
    /// `WriteZero` — a write made no progress (the backend's zero-byte-write guard).
    WriteZero = 3,
    /// `NotFound` — the target file/path was not found.
    NotFound = 4,
    /// Any other `io::ErrorKind` (e.g. a device `EBADF`/`EIO` surfaced by the backend).
    Other = 255,
}

impl ErrorKindCode {
    /// Map an [`std::io::ErrorKind`] to its dump code. Unknown kinds fold to [`Other`](Self::Other).
    #[inline]
    pub(crate) fn from_io_kind(kind: std::io::ErrorKind) -> Self {
        use std::io::ErrorKind as E;
        match kind {
            E::InvalidInput => ErrorKindCode::InvalidInput,
            E::BrokenPipe => ErrorKindCode::BrokenPipe,
            E::WriteZero => ErrorKindCode::WriteZero,
            E::NotFound => ErrorKindCode::NotFound,
            _ => ErrorKindCode::Other,
        }
    }
}

/// One per-op event. See the module docs for why this is a single struct with `kind`/`lifecycle`
/// fields rather than per-kind structs.
///
/// Field order is 8-byte fields first (naturally aligned, no implicit gap — `zerocopy::IntoBytes`
/// rejects gaps), then 4-byte, then 1-byte enums/bools, then an explicit `_pad` filling out to the
/// struct's 8-byte alignment.
#[derive(Event, IntoBytes, Immutable, Debug)]
#[event(namespace = "s2n_quic_dc::fs::io_op")]
#[repr(C)]
pub struct IoOpEvent {
    /// Monotonic per-op correlation id (the primary join key). [`NO_OP_SEQ`] → SQL NULL for the
    /// pre-admission lifecycles and credit events that fire before the op exists.
    #[event(key, sentinel = u64::MAX)]
    pub op_seq: u64,
    /// Stream grouping key: every op of one multi-op submitter (a
    /// [`MaterializeStream`](crate::fs::materialize)) shares this id, so `WHERE stream_id = N` returns
    /// the whole run. [`NO_STREAM`] → SQL NULL for one-off ops and pre-op lifecycles.
    #[event(key, sentinel = u64::MAX)]
    pub stream_id: u64,
    /// Caller correlation token echoed by the op (e.g. a [`MaterializeStream`](crate::fs::materialize)
    /// block index); `0` when unused.
    #[event(key)]
    pub user_data: u64,
    /// Raw file descriptor at the emit site — part of the coarse `(device_index, fd, offset)` fallback
    /// key.
    #[event(key)]
    pub fd: u64,
    /// Byte offset within the file.
    #[event(unit = "bytes")]
    pub offset: u64,
    /// Credit cost of the op (`== flow_credits == byte_cost`); `0` once released or for pre-credit
    /// lifecycles.
    pub cost: u64,
    /// Submit→this-point latency in microseconds, recorded on the terminal lifecycles
    /// ([`Completed`](IoLifecycle::Completed)/[`Failed`](IoLifecycle::Failed)/
    /// [`Orphaned`](IoLifecycle::Orphaned)/[`CancelledSkipped`](IoLifecycle::CancelledSkipped)); `0`
    /// where not applicable.
    #[event(unit = "us")]
    pub sojourn_us: u64,
    /// Requested transfer length in bytes (`0` for control ops).
    #[event(unit = "bytes")]
    pub len: u32,
    /// Bytes actually transferred at [`Completed`](IoLifecycle::Completed); `0` otherwise.
    #[event(unit = "bytes")]
    pub bytes_done: u32,
    /// The device's dense registration index — part of the fallback key and the per-device grouping
    /// dimension.
    #[event(key)]
    pub device_index: u32,
    /// Execution lane id; `u32::MAX` (`LocalRingId::UNSET`) until [`Dispatched`](IoLifecycle::Dispatched).
    #[event(key)]
    pub ring_id: u32,
    pub kind: IoOpKind,
    pub lifecycle: IoLifecycle,
    pub err_kind: ErrorKindCode,
    /// Whether this op operated on a page-aligned `O_DIRECT` buffer.
    pub is_direct: bool,
    /// For [`CancelledBeforeSubmit`](IoLifecycle::CancelledBeforeSubmit): `true` = cancelled after
    /// credit was acquired (and released), `false` = cancelled before any credit. `false` elsewhere.
    pub had_credit: bool,
    pub _pad: [u8; 3],
}

const _: () = assert!(
    core::mem::size_of::<IoOpEvent>().is_multiple_of(8),
    "IoOpEvent must be 8-byte aligned with no trailing implicit padding"
);

/// Read an op's correlation id in a build-independent way: the real `op_seq` when the field is
/// compiled in (the same builds where the trace is `ENABLED`), else the [`NO_OP_SEQ`] sentinel.
#[inline]
pub(crate) fn op_seq_of(op: &IoOp) -> u64 {
    #[cfg(any(test, feature = "testing", feature = "io-dbg"))]
    {
        op.op_seq
    }
    #[cfg(not(any(test, feature = "testing", feature = "io-dbg")))]
    {
        let _ = op;
        NO_OP_SEQ
    }
}

/// Read an op's stream grouping id in a build-independent way (the real `stream_id` in trace builds,
/// else the [`NO_STREAM`] sentinel).
#[inline]
pub(crate) fn stream_id_of(op: &IoOp) -> u64 {
    #[cfg(any(test, feature = "testing", feature = "io-dbg"))]
    {
        op.stream_id
    }
    #[cfg(not(any(test, feature = "testing", feature = "io-dbg")))]
    {
        let _ = op;
        NO_STREAM
    }
}

/// Mint the next monotonic `op_seq`.
///
/// Under bach (test/simulation) it is a per-thread monotonic counter so ids are deterministic and
/// snapshot-friendly — bach is single-threaded and cooperatively scheduled, so a plain `thread_local`
/// is race-free. Otherwise it is a process-wide atomic. (Mirrors
/// [`crate::endpoint::dbg::next_dump_id`]'s testing/production split.)
#[cfg(any(test, feature = "testing", feature = "io-dbg"))]
pub(crate) fn next_op_seq() -> u64 {
    #[cfg(any(test, feature = "testing"))]
    if ::bach::is_active() {
        use core::cell::Cell;
        thread_local! {
            static COUNTER: Cell<u64> = const { Cell::new(1) };
        }
        return COUNTER.with(|c| {
            let id = c.get();
            c.set(id.wrapping_add(1));
            id
        });
    }

    use core::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Mint the next stream grouping id, for a multi-op submitter that wants its ops grouped in the dump
/// (e.g. a [`MaterializeStream`](crate::fs::materialize)). Same testing/production split as
/// [`next_op_seq`]; a distinct counter space from `op_seq` (they are different key columns).
#[cfg(any(test, feature = "testing", feature = "io-dbg"))]
pub(crate) fn next_stream_id() -> u64 {
    #[cfg(any(test, feature = "testing"))]
    if ::bach::is_active() {
        use core::cell::Cell;
        thread_local! {
            static COUNTER: Cell<u64> = const { Cell::new(1) };
        }
        return COUNTER.with(|c| {
            let id = c.get();
            c.set(id.wrapping_add(1));
            id
        });
    }

    use core::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ──────────────────────────────────────────────────────────────────────────────────────────────
// Recording API. Each entry point is gated by `crate::fs::dbg::on_enabled` (a `false` const in plain
// release builds, so the whole body — including event construction — is stripped). Call sites pass
// plain values from the op (or loose args for the pre-op lifecycles); no closures needed.
// ──────────────────────────────────────────────────────────────────────────────────────────────

/// The shared field-mapping. Every public helper funnels here so the layout lives in one place
/// (analogous to `frame_trace::emit_header`).
#[allow(clippy::too_many_arguments)]
#[inline]
fn emit(
    lifecycle: IoLifecycle,
    op_seq: u64,
    stream_id: u64,
    user_data: u64,
    device_index: u32,
    kind: IoKind,
    fd: u64,
    offset: u64,
    len: u32,
    cost: u64,
    ring_id: u32,
    is_direct: bool,
    had_credit: bool,
    bytes_done: u32,
    err_kind: ErrorKindCode,
    sojourn_us: u64,
) {
    backbeat::global::record(&IoOpEvent {
        op_seq,
        stream_id,
        user_data,
        fd,
        offset,
        cost,
        sojourn_us,
        len,
        bytes_done,
        device_index,
        ring_id,
        kind: kind.into(),
        lifecycle,
        err_kind,
        is_direct,
        had_credit,
        _pad: [0; 3],
    });
}

/// submit→now sojourn in microseconds from the op's `enqueued_at` stamp (0 if unstamped). Mirrors the
/// computation in [`combinator::complete`](crate::fs::combinator).
#[inline]
fn sojourn_us(op: &IoOp) -> u64 {
    match op.enqueued_at {
        Some(enqueued_at) => {
            let now = crate::time::DefaultClock::default().now();
            now.nanos_since(enqueued_at) / 1_000
        }
        None => 0,
    }
}

/// Record the disposition of a live op (one carrying its `Arc<Device>`) at `lifecycle`, pulling every
/// field off the op. `bytes_done`/`err_kind`/`sojourn` are caller-supplied since they depend on the
/// terminal status.
#[inline]
fn emit_op(op: &IoOp, lifecycle: IoLifecycle, bytes_done: u32, err_kind: ErrorKindCode, soj: u64) {
    emit(
        lifecycle,
        op_seq_of(op),
        stream_id_of(op),
        op.user_data,
        op.device.index as u32,
        op.kind,
        op.fd.as_raw() as u64,
        op.offset,
        op.len,
        op.flow_credits,
        op.ring_id.0,
        op.buf.is_direct(),
        false,
        bytes_done,
        err_kind,
        soj,
    );
}

/// [`Rejected`](IoLifecycle::Rejected) — `Device::prepare` failed before any op was built. No
/// `op_seq` yet ([`NO_OP_SEQ`]).
#[allow(clippy::too_many_arguments)]
#[inline]
pub(crate) fn rejected(
    device_index: usize,
    kind: IoKind,
    fd: u64,
    offset: u64,
    len: u32,
    is_direct: bool,
    err_kind: std::io::ErrorKind,
) {
    crate::fs::dbg::on_enabled(|| {
        emit(
            IoLifecycle::Rejected,
            NO_OP_SEQ,
            NO_STREAM,
            0,
            device_index as u32,
            kind,
            fd,
            offset,
            len,
            0,
            crate::fs::device::LocalRingId::UNSET.0,
            is_direct,
            false,
            0,
            ErrorKindCode::from_io_kind(err_kind),
            0,
        )
    });
}

/// [`CancelledBeforeSubmit`](IoLifecycle::CancelledBeforeSubmit) — a [`Reservation`] was dropped without
/// `submit`, cancelling in the pre-enqueue window: credit was granted then released and no op was built.
/// `had_credit` is always `true` (a reservation only exists once credit is in hand).
///
/// [`Reservation`]: crate::fs::device::Reservation
#[allow(clippy::too_many_arguments)]
#[inline]
pub(crate) fn cancelled_before_submit(
    device_index: usize,
    kind: IoKind,
    fd: u64,
    offset: u64,
    len: u32,
    is_direct: bool,
    had_credit: bool,
) {
    crate::fs::dbg::on_enabled(|| {
        emit(
            IoLifecycle::CancelledBeforeSubmit,
            NO_OP_SEQ,
            NO_STREAM,
            0,
            device_index as u32,
            kind,
            fd,
            offset,
            len,
            0,
            crate::fs::device::LocalRingId::UNSET.0,
            is_direct,
            had_credit,
            0,
            ErrorKindCode::None,
            0,
        )
    });
}

/// [`Submitted`](IoLifecycle::Submitted) — the op was admitted and pushed to the submission channel.
#[inline]
pub(crate) fn submitted(op: &IoOp) {
    crate::fs::dbg::on_enabled(|| emit_op(op, IoLifecycle::Submitted, 0, ErrorKindCode::None, 0));
}

/// [`EnqueueClosed`](IoLifecycle::EnqueueClosed) — the submission channel was closed at enqueue.
#[inline]
pub(crate) fn enqueue_closed(op: &IoOp) {
    crate::fs::dbg::on_enabled(|| {
        emit_op(
            op,
            IoLifecycle::EnqueueClosed,
            0,
            ErrorKindCode::BrokenPipe,
            0,
        )
    });
}

/// A credit-acquire transition ([`CreditPark`](IoLifecycle::CreditPark) /
/// [`CreditGrant`](IoLifecycle::CreditGrant) / [`CreditPartial`](IoLifecycle::CreditPartial)).
///
/// Emitted from inside the acquire poll, which predates the op on the one-shot submit path, so it
/// carries [`NO_OP_SEQ`] and correlates by `(device_index, kind, cost)` plus time-adjacency.
///
/// Only reachable from the cfg-gated `SubmitterAlloc::trace_credit`, so it carries the same cfg as
/// that caller (it would be dead code in a non-trace build).
#[cfg(any(test, feature = "testing", feature = "io-dbg"))]
#[inline]
pub(crate) fn credit(lifecycle: IoLifecycle, device_index: u32, kind: IoKind, cost: u64) {
    crate::fs::dbg::on_enabled(|| {
        emit(
            lifecycle,
            NO_OP_SEQ,
            NO_STREAM,
            0,
            device_index,
            kind,
            0,
            0,
            0,
            cost,
            crate::fs::device::LocalRingId::UNSET.0,
            false,
            false,
            0,
            ErrorKindCode::None,
            0,
        )
    });
}

/// [`Dispatched`](IoLifecycle::Dispatched) — a lane was assigned (`ring_id` now set) and the op handed
/// to it.
#[inline]
pub(crate) fn dispatched(op: &IoOp) {
    crate::fs::dbg::on_enabled(|| emit_op(op, IoLifecycle::Dispatched, 0, ErrorKindCode::None, 0));
}

/// [`LaneClosed`](IoLifecycle::LaneClosed) — the dispatcher could not hand the op to a lane.
#[inline]
pub(crate) fn lane_closed(op: &IoOp) {
    crate::fs::dbg::on_enabled(|| {
        emit_op(op, IoLifecycle::LaneClosed, 0, ErrorKindCode::BrokenPipe, 0)
    });
}

/// [`BackendStart`](IoLifecycle::BackendStart) — the backend began executing the op.
#[inline]
pub(crate) fn backend_start(op: &IoOp) {
    crate::fs::dbg::on_enabled(|| {
        emit_op(op, IoLifecycle::BackendStart, 0, ErrorKindCode::None, 0)
    });
}

/// [`BackendDone`](IoLifecycle::BackendDone) — the backend finished executing the op. Carries the
/// stamped status (bytes / error) for the io_uring case where this precedes completion across threads.
#[inline]
pub(crate) fn backend_done(op: &IoOp) {
    crate::fs::dbg::on_enabled(|| {
        let (bytes_done, err_kind) = status_fields(op.status);
        emit_op(op, IoLifecycle::BackendDone, bytes_done, err_kind, 0)
    });
}

/// [`Completed`](IoLifecycle::Completed) / [`Failed`](IoLifecycle::Failed) /
/// [`Orphaned`](IoLifecycle::Orphaned) — terminal disposition in `combinator::complete`. `orphaned`
/// reroutes a `Done` op to [`Orphaned`](IoLifecycle::Orphaned) (the submitter's receiver had dropped).
/// Call **before** `release_credits()` so `cost` is still recorded.
#[inline]
pub(crate) fn completed(op: &IoOp, orphaned: bool) {
    crate::fs::dbg::on_enabled(|| {
        let (bytes_done, err_kind) = status_fields(op.status);
        let lifecycle = if orphaned {
            IoLifecycle::Orphaned
        } else if matches!(op.status, IoStatus::Failed(_)) {
            IoLifecycle::Failed
        } else {
            IoLifecycle::Completed
        };
        emit_op(op, lifecycle, bytes_done, err_kind, sojourn_us(op))
    });
}

/// [`CancelledSkipped`](IoLifecycle::CancelledSkipped) — the backend skipped an op whose receiver was
/// gone. Call **before** `release_credits()`.
#[inline]
pub(crate) fn cancelled_skipped(op: &IoOp) {
    crate::fs::dbg::on_enabled(|| {
        emit_op(
            op,
            IoLifecycle::CancelledSkipped,
            0,
            ErrorKindCode::None,
            sojourn_us(op),
        )
    });
}

/// Split an [`IoStatus`] into the `(bytes_done, err_kind)` event fields.
#[inline]
fn status_fields(status: IoStatus) -> (u32, ErrorKindCode) {
    match status {
        IoStatus::Done(n) => (n as u32, ErrorKindCode::None),
        IoStatus::Failed(kind) => (0, ErrorKindCode::from_io_kind(kind)),
        IoStatus::Pending => (0, ErrorKindCode::None),
    }
}

#[cfg(test)]
impl IoLifecycle {
    pub(crate) fn as_u8(self) -> u8 {
        self as u8
    }
}

#[cfg(test)]
impl IoOpKind {
    pub(crate) fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Test-only snapshot of what is currently resident in the global recorder: the set of
/// [`IoLifecycle`] discriminants and the set of [`IoOpKind`] discriminants across all [`IoOpEvent`]s.
/// Lets an end-to-end test assert the trace points fired during a real transfer without an on-disk
/// dump. Mirrors [`frame_trace::resident_event_kinds`](crate::endpoint::frame_trace).
#[cfg(test)]
pub(crate) fn resident_event_kinds() -> (
    std::collections::BTreeSet<u8>,
    std::collections::BTreeSet<u8>,
) {
    use backbeat::record::RecordView;

    let bytes = backbeat::global::recorder().dump(
        backbeat::registry::schemas(),
        core::iter::empty(),
        backbeat::registry::views(),
        "",
    );

    let mut lifecycles = std::collections::BTreeSet::new();
    let mut kinds = std::collections::BTreeSet::new();

    let Ok(reader) = backbeat::wire::DumpReader::new(bytes) else {
        return (lifecycles, kinds);
    };
    let Ok(shards) = reader.shards() else {
        return (lifecycles, kinds);
    };
    let lifecycle_off = core::mem::offset_of!(IoOpEvent, lifecycle);
    let kind_off = core::mem::offset_of!(IoOpEvent, kind);
    for shard in shards {
        backbeat::ring::walk(
            &shard.region,
            shard.head as usize,
            shard.capacity as usize,
            |payload| {
                let Some(view) = RecordView::parse(&payload) else {
                    return false;
                };
                if view.event_id == IoOpEvent::ID {
                    if let Some(&b) = view.fields.get(lifecycle_off) {
                        lifecycles.insert(b);
                    }
                    if let Some(&b) = view.fields.get(kind_off) {
                        kinds.insert(b);
                    }
                }
                true
            },
        );
    }

    (lifecycles, kinds)
}

/// Test-only: the distinct, non-`NO_STREAM` `stream_id`s resident in the recorder, each with the count
/// of [`IoOpEvent`] rows carrying it. Lets a test assert that all ops of a materialize run grouped
/// under one shared id (and that one-off ops did not get one). `stream_id` is the second `u64` field,
/// read little-endian from the record payload at its `offset_of`.
#[cfg(test)]
pub(crate) fn resident_stream_ids() -> std::collections::BTreeMap<u64, usize> {
    use backbeat::record::RecordView;

    let bytes = backbeat::global::recorder().dump(
        backbeat::registry::schemas(),
        core::iter::empty(),
        backbeat::registry::views(),
        "",
    );

    let mut out: std::collections::BTreeMap<u64, usize> = std::collections::BTreeMap::new();
    let Ok(reader) = backbeat::wire::DumpReader::new(bytes) else {
        return out;
    };
    let Ok(shards) = reader.shards() else {
        return out;
    };
    let off = core::mem::offset_of!(IoOpEvent, stream_id);
    for shard in shards {
        backbeat::ring::walk(
            &shard.region,
            shard.head as usize,
            shard.capacity as usize,
            |payload| {
                let Some(view) = RecordView::parse(&payload) else {
                    return false;
                };
                if view.event_id == IoOpEvent::ID {
                    if let Some(slice) = view.fields.get(off..off + 8) {
                        let mut b = [0u8; 8];
                        b.copy_from_slice(slice);
                        let sid = u64::from_le_bytes(b);
                        if sid != NO_STREAM {
                            *out.entry(sid).or_default() += 1;
                        }
                    }
                }
                true
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dump mirror's discriminants must equal `IoKind::index()` so the trace's `kind` label aligns
    /// with the per-kind metric index. A renumber here would silently mislabel every dump.
    #[test]
    fn io_op_kind_matches_io_kind_index() {
        for kind in IoKind::ALL {
            assert_eq!(
                IoOpKind::from(kind).as_u8() as usize,
                kind.index(),
                "IoOpKind discriminant diverged from IoKind::index for {kind:?}"
            );
        }
    }

    /// `IoLifecycle` discriminants are stable dump values — pin them so an accidental reorder/renumber
    /// (which would reinterpret historical dumps) fails loudly.
    #[test]
    fn lifecycle_discriminants_are_stable() {
        assert_eq!(IoLifecycle::Rejected.as_u8(), 0);
        assert_eq!(IoLifecycle::CancelledBeforeSubmit.as_u8(), 1);
        assert_eq!(IoLifecycle::Submitted.as_u8(), 2);
        assert_eq!(IoLifecycle::EnqueueClosed.as_u8(), 3);
        assert_eq!(IoLifecycle::CreditPark.as_u8(), 4);
        assert_eq!(IoLifecycle::CreditGrant.as_u8(), 5);
        assert_eq!(IoLifecycle::CreditPartial.as_u8(), 6);
        assert_eq!(IoLifecycle::Dispatched.as_u8(), 7);
        assert_eq!(IoLifecycle::LaneClosed.as_u8(), 8);
        assert_eq!(IoLifecycle::BackendStart.as_u8(), 9);
        assert_eq!(IoLifecycle::BackendDone.as_u8(), 10);
        assert_eq!(IoLifecycle::Completed.as_u8(), 11);
        assert_eq!(IoLifecycle::Failed.as_u8(), 12);
        assert_eq!(IoLifecycle::Orphaned.as_u8(), 13);
        assert_eq!(IoLifecycle::CancelledSkipped.as_u8(), 14);
    }

    /// `from_io_kind` maps the kinds the pipeline actually produces; everything else folds to `Other`.
    #[test]
    fn error_kind_code_mapping() {
        use std::io::ErrorKind as E;
        assert_eq!(
            ErrorKindCode::from_io_kind(E::InvalidInput),
            ErrorKindCode::InvalidInput
        );
        assert_eq!(
            ErrorKindCode::from_io_kind(E::BrokenPipe),
            ErrorKindCode::BrokenPipe
        );
        assert_eq!(
            ErrorKindCode::from_io_kind(E::WriteZero),
            ErrorKindCode::WriteZero
        );
        assert_eq!(
            ErrorKindCode::from_io_kind(E::NotFound),
            ErrorKindCode::NotFound
        );
        assert_eq!(
            ErrorKindCode::from_io_kind(E::PermissionDenied),
            ErrorKindCode::Other
        );
    }
}
