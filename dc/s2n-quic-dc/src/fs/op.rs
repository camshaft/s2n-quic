// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The IO work-unit — the storage analog of `endpoint::frame::Frame`.
//!
//! An [`IoOp`] is a single atomic device operation (one `pread`/`pwrite`/`fsync`). It carries
//! everything a backend needs to execute it (target fd/offset/len and the buffer), the completion
//! handle that notifies the submitter, the borrowed credit that must be released exactly once, and
//! the disposition the pipeline stamps as the op flows through.
//!
//! Like `Frame`, `IoOp` has **no `Drop` that releases `flow_credits`** — every drop path in the
//! pipeline (undeliverable route, backend error, completion) must release the credit back to the
//! op's device pool exactly once and zero the field. See [`IoOp::take_credits`].
//!
//! Also like `Frame` (which carries `Arc<PathSecretEntry>`), `IoOp` carries its **`Arc<Device>`**
//! rather than a `DeviceId` index: the op holds everything it needs to finish itself — the pool to
//! release credit to and the `Send + Sync` completion sender — so a backend worker thread can
//! complete its own op in place with no table lookup and no cross-thread completion bridge.

use crate::{
    fs::device::{Device, LocalRingId},
    sync::Arc,
};

/// A completion notification handle for a single submitter, parameterized over the work-unit so the
/// completed [`IoOp`] (with its filled buffer and result) is delivered back. This is the exact
/// mechanism the QUIC endpoint uses for frame completions — see
/// [`crate::socket::channel::intrusive::datagram_completion`].
pub type CompletionSender = crate::socket::channel::intrusive::datagram_completion::Sender<IoOp>;

/// Receiver side of a submitter's completion channel.
pub type CompletionReceiver =
    crate::socket::channel::intrusive::datagram_completion::Receiver<IoOp>;

/// The kind of device operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoKind {
    Read,
    Write,
    Fsync,
    Fdatasync,
    Trim,
}

impl IoKind {
    /// Every op kind, in `index` order — for registering one per-kind metric per device.
    pub const ALL: [IoKind; 5] = [
        IoKind::Read,
        IoKind::Write,
        IoKind::Fsync,
        IoKind::Fdatasync,
        IoKind::Trim,
    ];

    /// Whether this op reads from the device (true) or writes to / mutates it (false). Used to
    /// route the op to the correct pool in split mode and to apply the read-vs-write cost weight.
    #[inline]
    pub fn is_read(self) -> bool {
        matches!(self, IoKind::Read)
    }

    /// Whether this op transfers a payload (read/write) rather than being a zero-byte control op
    /// (fsync/fdatasync/trim). Only data ops get a byte-size histogram recorded.
    #[inline]
    pub fn is_data(self) -> bool {
        matches!(self, IoKind::Read | IoKind::Write)
    }

    /// Lower-snake metric-name fragment (`read`, `write`, `fsync`, `fdatasync`, `trim`), used to build
    /// the per-kind device metric names (`fs.device.op.{name}.*`).
    #[inline]
    pub fn name(self) -> &'static str {
        match self {
            IoKind::Read => "read",
            IoKind::Write => "write",
            IoKind::Fsync => "fsync",
            IoKind::Fdatasync => "fdatasync",
            IoKind::Trim => "trim",
        }
    }

    /// Dense index into a per-kind metric array (matches [`ALL`](Self::ALL) order).
    #[inline]
    pub fn index(self) -> usize {
        match self {
            IoKind::Read => 0,
            IoKind::Write => 1,
            IoKind::Fsync => 2,
            IoKind::Fdatasync => 3,
            IoKind::Trim => 4,
        }
    }
}

/// The buffer attached to an op: a mutable, (eventually) page-aligned destination for reads, or an
/// immutable source for writes. Carrying it by value through the backend and back on completion is
/// what lets the buffer double as the correlation token — no separate `user_data` is needed at the
/// abstraction boundary.
#[derive(Debug)]
pub enum IoBuf {
    /// Buffered read destination for non-`O_DIRECT` IO. The buffer arrives **pre-sized with
    /// capacity** (`BytesMut::with_capacity(len)`) and **logically empty** (`len() == 0`): the
    /// backend reads straight into its uninitialized spare capacity and then `set_len`s to the
    /// bytes actually read. The scheduler never `resize`s or zero-fills it — there is no `memset`
    /// on the read path, and a short read at EOF leaves no zero-padded tail. The op carries the
    /// requested transfer length in [`IoOp::len`] (the capacity is `>= len`); on completion the
    /// buffer's `len()` is the bytes read.
    Read(bytes::BytesMut),
    /// Buffered write source.
    Write(bytes::Bytes),
    /// A caller-owned, page-aligned buffer for **zero-copy** `O_DIRECT` IO — used **in place** as
    /// the destination of a direct read or the source of a direct write, with no bounce-buffer copy
    /// in the data path. The caller allocates it (e.g. via [`crate::fs::direct::AlignedBuf`]) and
    /// gets it back on completion. The submit path validates the offset is block-aligned; the
    /// pointer and transfer length are aligned by `AlignedBuf` construction.
    Direct(crate::fs::direct::AlignedBuf),
    /// No buffer (fsync / fdatasync / trim).
    None,
}

impl IoBuf {
    /// The number of payload bytes this op transfers (0 for control ops). Used by the cost model.
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            IoBuf::Read(b) => b.len(),
            IoBuf::Write(b) => b.len(),
            IoBuf::Direct(b) => b.len(),
            IoBuf::None => 0,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether this is a direct (zero-copy, aligned) buffer.
    #[inline]
    pub fn is_direct(&self) -> bool {
        matches!(self, IoBuf::Direct(_))
    }
}

/// Disposition of an op, updated by the pipeline / backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoStatus {
    /// Submitted, not yet completed.
    Pending,
    /// Completed successfully, transferring `usize` bytes.
    Done(usize),
    /// Failed with the given `io::ErrorKind` (stored by kind so `IoOp` stays `Clone`-free-of-Error).
    Failed(std::io::ErrorKind),
}

impl IoStatus {
    #[inline]
    pub fn is_pending(self) -> bool {
        matches!(self, IoStatus::Pending)
    }
}

/// A single atomic device operation flowing through the scheduler pipeline.
#[derive(Debug)]
pub struct IoOp {
    /// Read / write / fsync / ...
    pub kind: IoKind,
    /// The device this op draws budget from, carried by `Arc` (the storage analog of
    /// `Frame::path_secret_entry`). Holding the `Arc` — not a `DeviceId` index — means the op can
    /// release its own credit (`device.pool_for(kind).release(..)`) on whatever thread completes it,
    /// with no `DeviceTable` lookup. Assigned at submit time.
    pub device: Arc<Device>,
    /// Target file descriptor (resolved by the caller; opaque to the scheduler core).
    pub fd: i32,
    /// Byte offset within the file.
    pub offset: u64,
    /// Transfer length in bytes (control ops use 0).
    pub len: u32,
    /// Read destination / write source / none.
    pub buf: IoBuf,
    /// Completion notification handle. `None` for fire-and-forget ops with no waiter.
    pub completion: Option<CompletionSender>,
    /// Disposition, stamped by the backend and read on completion.
    pub status: IoStatus,
    /// Credit borrowed from the device pool, recorded so it is released exactly once. Mirrors
    /// `Frame::flow_credits`.
    pub flow_credits: u64,
    /// Execution lane assigned by `PickRing` (the send-socket analog). `LocalRingId::UNSET` until
    /// routed.
    pub ring_id: LocalRingId,
    /// Opaque caller correlation token, echoed back on completion. Lets a submitter that funnels
    /// many ops onto one shared completion channel (e.g. [`MaterializeStream`](crate::fs::materialize))
    /// identify which op completed without a per-op channel allocation.
    pub user_data: u64,
    /// When the op was submitted, for sojourn metrics. Set by the scheduler.
    pub enqueued_at: Option<crate::time::precision::Timestamp>,
}

impl IoOp {
    /// Take all of `flow_credits`, leaving the field zero, and return the amount. The single accessor
    /// for releasing an op's borrowed credit back to its pool — zeroing the field guarantees the
    /// credit is released exactly once across every drop/complete/closed-lane path.
    #[inline]
    pub fn take_credits(&mut self) -> u64 {
        core::mem::take(&mut self.flow_credits)
    }

    /// Release this op's borrowed credit back to its own device pool, exactly once. Because the op
    /// carries its `Arc<Device>`, this needs no table lookup and works on whatever thread completes
    /// the op — the basis for a backend worker completing its own op in place (no completion bridge).
    /// Idempotent: [`take_credits`](Self::take_credits) zeroes the field, so a second call is a no-op.
    #[inline]
    pub fn release_credits(&mut self) {
        let credits = self.take_credits();
        if credits > 0 {
            self.device.pool_for(self.kind).release(credits);
        }
    }
}

impl crate::sched::ByteCost for IoOp {
    /// The op's cost in the device pool's currency. The actual cost-model arithmetic (bytes vs.
    /// fixed-unit IOPS, times the per-kind weight) is applied at submit time by the device and the
    /// resulting value is recorded in `flow_credits`; this accessor reports it for the pipeline's
    /// pacing/accounting stages.
    #[inline]
    fn byte_cost(&self) -> u64 {
        self.flow_credits
    }
}
