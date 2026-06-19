// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Devices and execution lanes — the resource and the executor.
//!
//! A **device** is the limited resource (a disk / EBS volume / NVMe namespace): it owns the credit
//! pool(s) and the cost model that govern how much work may be in flight against it, mirroring how
//! the network endpoint's *sender* owns a send-credit pool. A single scheduler endpoint holds an
//! [`IdMap`]-like table of devices and serves all of them, so adding a device adds data structures,
//! not threads.
//!
//! An **execution lane** ([`LocalRingId`]) is the analog of a send socket: a worker's io_uring ring
//! or a slot in the shared blocking pool. Lanes are decoupled from devices — ops for many devices
//! flow through one lane — which is what keeps the blocking-thread count tied to worker count, not
//! device count.

use crate::{
    fs::config::{CostModel, DeviceConfig, OpWeights, PoolMode},
    fs::op::IoKind,
    sched::Pool,
    sync::Arc,
};

/// Identifies a device (index into the scheduler's device table).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(pub u32);

impl DeviceId {
    #[inline]
    pub fn as_usize(self) -> usize {
        self.0 as usize
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

/// A device: its budget pool(s), cost model, and weights. The pacer ([`crate::sched::Rate`]) is
/// applied by the pipeline's `Paced` stage keyed by [`DeviceId`]; the rate is stored here so the
/// pipeline can look it up.
pub struct Device {
    pub pools: DevicePools,
    pub cost_model: CostModel,
    pub op_weights: OpWeights,
    pub rate: crate::sched::Rate,
    /// Capacity of the read / write pool (in the cost-model currency), recorded so `submit` can
    /// reject an op whose cost exceeds it (an `IoOp` is atomic — it has no partial-submit escape, so
    /// a `cost > capacity` acquire could never be satisfied and would park forever).
    read_capacity: u64,
    write_capacity: u64,
}

impl Device {
    /// Build a device and its (not-yet-spawned) distributors from config.
    pub fn new(cfg: &DeviceConfig) -> Self {
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
            pools,
            cost_model: cfg.cost_model,
            op_weights: cfg.op_weights,
            rate: cfg.rate,
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

/// The scheduler's device registry, indexed by [`DeviceId`].
pub struct DeviceTable {
    devices: Vec<Device>,
}

impl DeviceTable {
    pub fn new(devices: Vec<Device>) -> Self {
        Self { devices }
    }

    /// Look up a device by id, returning `None` for an out-of-range (caller-supplied) id rather
    /// than panicking the shared worker.
    #[inline]
    pub fn get(&self, id: DeviceId) -> Option<&Device> {
        self.devices.get(id.as_usize())
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (DeviceId, &Device)> {
        self.devices
            .iter()
            .enumerate()
            .map(|(i, d)| (DeviceId(i as u32), d))
    }
}
