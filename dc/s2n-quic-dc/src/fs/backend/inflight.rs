// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A slab of in-flight io_uring operations, keyed by the SQE/CQE `user_data` token.
//!
//! The io_uring backend must keep every submitted op (and therefore its buffer) alive and pinned at a
//! stable address until the op's final CQE is reaped — submitting a buffer the kernel may still write
//! to and then freeing it is a use-after-free. [`InFlightSlab`] is that owning store: each op lives in
//! a `Vec<Option<_>>` slot indexed by a small integer `id` that is handed to the kernel as
//! `user_data`, so a CQE maps back to its op in O(1) with no per-op allocation beyond the op itself.
//! Freed slot indices are recycled.
//!
//! This type holds **no** io_uring state — it is the bookkeeping the ring loop hangs its SQE/CQE
//! syscalls off of, deliberately factored out so the slab lifecycle (insert / progress on short
//! completion / take on final completion / outstanding count / idle detection) is unit-testable on any
//! platform, not just where `io_uring` links. The ring loop ([`super::uring`]) supplies the actual
//! `submission().push` / `submit_and_wait` / `completion()` calls.

use crate::intrusive::Entry;

/// One in-flight op: the owned op plus its short-completion progress cursor. A short read/write
/// advances `done` and the remainder is re-submitted, so the op stays pinned (and its buffer valid
/// for the kernel) until the FULL transfer finishes or it errors.
pub(super) struct InFlight<T> {
    pub(super) op: Entry<T>,
    /// Bytes transferred so far (for read/write progress across short completions).
    pub(super) done: usize,
    /// Total bytes requested (the op's transfer length); `done == want` means complete.
    pub(super) want: usize,
}

/// An owning, index-keyed slab of in-flight ops with recycled slot ids.
pub(super) struct InFlightSlab<T> {
    slots: Vec<Option<InFlight<T>>>,
    free_ids: Vec<usize>,
    outstanding: usize,
}

impl<T> InFlightSlab<T> {
    /// A slab sized to comfortably hold `capacity` concurrent ops (it still grows on demand).
    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: Vec::with_capacity(capacity),
            free_ids: Vec::with_capacity(capacity),
            outstanding: 0,
        }
    }

    /// Number of ops currently in flight (inserted, not yet taken).
    #[inline]
    pub(super) fn outstanding(&self) -> usize {
        self.outstanding
    }

    /// True when nothing is in flight — the ring thread may then block waiting for new work.
    #[inline]
    pub(super) fn is_idle(&self) -> bool {
        self.outstanding == 0
    }

    /// Park `op` (with `want` total bytes) in a free slot and return its `id` (the `user_data` token).
    /// The slot owns the op — and thus its buffer — until [`take`](Self::take).
    pub(super) fn insert(&mut self, op: Entry<T>, want: usize) -> usize {
        let id = self.free_ids.pop().unwrap_or_else(|| {
            self.slots.push(None);
            self.slots.len() - 1
        });
        self.slots[id] = Some(InFlight { op, done: 0, want });
        self.outstanding += 1;
        id
    }

    /// Borrow an in-flight op by id (for building a re-submit SQE / inspecting progress). `None` if
    /// the id is unknown (already completed/reaped) — a CQE for such an id is a harmless tombstone.
    #[inline]
    pub(super) fn get(&self, id: usize) -> Option<&InFlight<T>> {
        self.slots.get(id).and_then(|s| s.as_ref())
    }

    /// Mutably borrow an in-flight op by id (to advance its progress cursor on a short completion).
    #[inline]
    pub(super) fn get_mut(&mut self, id: usize) -> Option<&mut InFlight<T>> {
        self.slots.get_mut(id).and_then(|s| s.as_mut())
    }

    /// Remove the completed op at `id`, recycle its slot, and decrement the outstanding count.
    /// Returns the owned op (its buffer is now safe to hand back — its final CQE was reaped). `None`
    /// if the id was already taken.
    pub(super) fn take(&mut self, id: usize) -> Option<Entry<T>> {
        let slot = self.slots.get_mut(id)?;
        let inflight = slot.take()?;
        self.free_ids.push(id);
        self.outstanding -= 1;
        Some(inflight.op)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fs::{
            config::{CostModel, DeviceConfig, OpWeights, PoolMode},
            device::Device,
            op::{IoBuf, IoKind, IoOp, IoStatus},
        },
        sched::{CreditConfig, Rate},
        sync::Arc,
    };

    /// A throwaway `Arc<Device>` for the slab tests — they only need the op to carry *a* device, never
    /// touching its pool (the slab is pure id/buffer bookkeeping, independent of the device).
    fn device() -> Arc<Device> {
        Arc::new(Device::new(
            "inflight-test".into(),
            0,
            crate::fs::device::SchedulerId::next(),
            &crate::counter::Registry::default(),
            &DeviceConfig {
                pool_mode: PoolMode::Shared(CreditConfig::new(1 << 20)),
                rate: Rate::new(100.0),
                cost_model: CostModel::Bytes,
                op_weights: OpWeights::default(),
            },
        ))
    }

    fn op(offset: u64) -> Entry<IoOp> {
        Entry::new(IoOp {
            kind: IoKind::Read,
            device: device(),
            fd: 0,
            offset,
            len: 0,
            buf: IoBuf::None,
            completion: None,
            status: IoStatus::Pending,
            flow_credits: 0,
            ring_id: crate::fs::device::LocalRingId(0),
            user_data: 0,
            enqueued_at: None,
        })
    }

    #[test]
    fn insert_take_recycles_ids_and_tracks_outstanding() {
        let mut slab = InFlightSlab::<IoOp>::with_capacity(4);
        assert!(slab.is_idle());

        let a = slab.insert(op(1), 100);
        let b = slab.insert(op(2), 200);
        assert_eq!(slab.outstanding(), 2);
        assert!(!slab.is_idle());
        assert_eq!(a, 0);
        assert_eq!(b, 1);

        // Take `a`; its id should be recycled by the next insert.
        let taken = slab.take(a).expect("a in flight");
        assert_eq!(taken.offset, 1);
        assert_eq!(slab.outstanding(), 1);
        let c = slab.insert(op(3), 300);
        assert_eq!(c, a, "freed id must be recycled");
        assert_eq!(slab.outstanding(), 2);

        // A second take of the same id is a no-op (tombstone).
        assert!(slab.take(a).is_some()); // c lives at id a now
        assert!(slab.take(a).is_none());
        assert_eq!(slab.outstanding(), 1);

        slab.take(b).unwrap();
        assert!(slab.is_idle());
    }

    #[test]
    fn progress_cursor_advances_via_get_mut() {
        let mut slab = InFlightSlab::<IoOp>::with_capacity(1);
        let id = slab.insert(op(0), 1000);
        let f = slab.get_mut(id).unwrap();
        assert_eq!(f.done, 0);
        assert_eq!(f.want, 1000);
        f.done += 400;
        assert_eq!(slab.get(id).unwrap().done, 400);
        // unknown id
        assert!(slab.get(999).is_none());
        assert!(slab.get_mut(999).is_none());
    }
}
