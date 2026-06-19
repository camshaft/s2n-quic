// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Devices and execution lanes — the resource and the executor.
//!
//! A **device** is the limited resource (a disk / EBS volume / NVMe namespace): it owns the credit
//! pool(s) and the cost model that govern how much work may be in flight against it, mirroring how
//! the network endpoint's *sender* owns a send-credit pool. A device is referred to by
//! `Arc<Device>` — the application registers one with the scheduler, gets the `Arc` back, holds it,
//! and passes it to reads/writes; the op carries the same `Arc` so it can finish itself. There is no
//! device table, numeric id, or lookup: the `Arc` *is* the device handle (the storage analog of
//! `endpoint::frame::Frame` carrying `Arc<PathSecretEntry>`). One scheduler serves every device, so
//! adding a device adds data structures, not threads.
//!
//! An **execution lane** ([`LocalRingId`]) is the analog of a send socket: a worker's io_uring ring
//! or a slot in the shared blocking pool. Lanes are decoupled from devices — ops for many devices
//! flow through one lane — which is what keeps the blocking-thread count tied to worker count, not
//! device count.

use crate::{
    fs::{
        config::{CostModel, DeviceConfig, OpWeights, PoolMode},
        op::IoKind,
    },
    sched::Pool,
    sync::Arc,
};

/// A unique, per-[`Scheduler`](crate::fs::scheduler::Scheduler) origin stamp, used **only** to catch
/// the footgun of passing a device registered with one scheduler to a different scheduler's handle
/// (which would pace against the wrong pool and key the wrong materialize slot).
///
/// Now that a device is a free-floating `Arc<Device>` with no owning table, nothing structurally ties
/// it to its scheduler — so each scheduler stamps its devices with its `SchedulerId`, and the submit
/// chokepoint `debug_assert`s the op's device came from the same scheduler. The id is carried only in
/// `#[cfg(debug_assertions)]` builds (a process-wide monotonic `u64`); in release it is a zero-sized
/// type and every comparison compiles to nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulerId {
    #[cfg(debug_assertions)]
    id: u64,
}

impl SchedulerId {
    /// Mint the next process-unique scheduler id (debug builds only; a no-op ZST in release).
    pub fn next() -> Self {
        #[cfg(debug_assertions)]
        {
            use core::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            Self {
                id: NEXT.fetch_add(1, Ordering::Relaxed),
            }
        }
        #[cfg(not(debug_assertions))]
        {
            Self {}
        }
    }
}

/// Identifies an execution lane (worker ring / blocking-pool slot).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalRingId(pub u32);

impl LocalRingId {
    /// Sentinel for "not yet routed". `PickRing` overwrites it.
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

/// A device: its budget pool(s), cost model, and weights — carried by `Arc<Device>` on every
/// [`IoOp`](crate::fs::op::IoOp) (the storage analog of `Arc<PathSecretEntry>`), so an op holds
/// everything it needs to validate, pace, and finish itself with no table lookup. The pacer
/// ([`crate::sched::Rate`]) is applied by the pipeline's load-balancing stage; the rate is stored
/// here so the pipeline can read it.
pub struct Device {
    /// Application-supplied label (e.g. `"nvme0"`, `"ebs-data"`). Diagnostics only — gauge prefixes
    /// and logs. There is no application-visible numeric id and no lookup table; a device *is* its
    /// `Arc<Device>`, which the application holds and passes to reads/writes, and which the op carries
    /// through the pipeline.
    pub label: Arc<str>,
    /// Dense, monotonic, **crate-internal** registration index, stamped by the scheduler. Not part of
    /// the public API (the application deals in `Arc<Device>` + label) — it exists only so internal
    /// per-device indexers can do O(1) direct array indexing instead of a map lookup or pointer scan.
    /// Its sole consumer is [`MaterializeStream`](crate::fs::materialize)'s per-device acquire slots.
    pub(crate) index: usize,
    /// Which scheduler registered this device. Used by the submit chokepoint to `debug_assert` the
    /// device belongs to the handle's scheduler (a free-floating `Arc<Device>` is otherwise easy to
    /// misroute). Zero-sized in release builds.
    pub(crate) origin: SchedulerId,
    pub pools: DevicePools,
    pub cost_model: CostModel,
    pub op_weights: OpWeights,
    pub rate: crate::sched::Rate,
    /// Per-device nominal counters (`fs.device.*{device=label}`). Because the op carries this
    /// `Arc<Device>`, both the submit path and the in-place completion path bump these directly — no
    /// extra plumbing — giving per-device IOPS / bytes / failure visibility.
    pub counters: crate::fs::counters::DeviceCounters,
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
        f.debug_struct("Device").field("label", &self.label).finish()
    }
}

impl Device {
    /// Build a device (and its not-yet-spawned credit pools) from config under `label`, with the
    /// crate-internal registration `index` and `origin` scheduler stamp the scheduler assigned, and
    /// its per-device nominal counters registered against `registry`.
    pub fn new(
        label: Arc<str>,
        index: usize,
        origin: SchedulerId,
        registry: &crate::counter::Registry,
        cfg: &DeviceConfig,
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
            origin,
            pools,
            cost_model: cfg.cost_model,
            op_weights: cfg.op_weights,
            rate: cfg.rate,
            counters,
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
    /// credit cost. The hot-path replacement for the old `SubmitHandle::prepare` + `DeviceTable`
    /// index: the caller already holds the `Arc<Device>`, so there is no lookup here.
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
        if offset > i64::MAX as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "io scheduler: offset exceeds i64::MAX",
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
}

/// Force **atomic** credit grants for a storage pool by flooring `min_grant_slice` at
/// `max_single_acquire` per priority.
///
/// The credit pool's demand-elastic fair share normally splits a parked waiter's request into
/// `free / num_waiters` slices (great for QUIC byte-streams, which send the partial and release it).
/// But an [`IoOp`](crate::fs::op::IoOp) is **indivisible** — it cannot execute, and therefore cannot
/// release, until it holds its *full* cost. If two contending ops each pinned a partial slice,
/// neither could run, nothing would release, and the pool would wedge (exactly the deadlock the
/// `Writer` docs warn about, but with no partial-progress escape). Flooring the slice at the per-op
/// ceiling makes every grant all-or-nothing: a waiter is served only when its whole request fits,
/// so an admitted op (cost ≤ capacity, enforced at submit time) always gets granted in one wake.
///
/// Fairness across streams is preserved — the distributor still walks waiters FIFO within a tier and
/// serves whole ops round-robin; it just never hands out a sub-op sliver.
fn atomic_grant(config: crate::sched::CreditConfig) -> crate::sched::CreditConfig {
    let caps = config.max_single_acquire;
    config.with_min_grant_slice_per_priority(caps)
}

