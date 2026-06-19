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
//! pipeline (undeliverable route, backend error, completion dispatch) must release the credit back
//! to the op's device pool exactly once and zero the field. See [`IoOp::take_credits`].

use crate::fs::device::{DeviceId, LocalRingId};

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
    /// Whether this op reads from the device (true) or writes to / mutates it (false). Used to
    /// route the op to the correct pool in split mode and to apply the read-vs-write cost weight.
    #[inline]
    pub fn is_read(self) -> bool {
        matches!(self, IoKind::Read)
    }
}

/// The buffer attached to an op: a mutable, (eventually) page-aligned destination for reads, or an
/// immutable source for writes. Carrying it by value through the backend and back on completion is
/// what lets the buffer double as the correlation token — no separate `user_data` is needed at the
/// abstraction boundary.
#[derive(Debug)]
pub enum IoBuf {
    /// Buffered read destination (scheduler-allocated, grown to the op length). Used for buffered
    /// (non-`O_DIRECT`) IO where the kernel page cache absorbs the copy anyway.
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
    /// Which device's budget and pacer this op draws from. Assigned at submit time.
    pub device: DeviceId,
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
