// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Configuration for the IO scheduler.
//!
//! v1 uses **static caps only** — pool capacity and pacer rate are configured constants per device,
//! with no adaptive estimation loop. The two budgeting axes are orthogonal:
//!
//! * [`CostModel`] decides the *currency* a device's budget is denominated in — raw bytes
//!   (bandwidth-limited) or fixed-unit IOPS (SSD/EBS, where one I/O up to `io_unit` bytes counts as
//!   one operation).
//! * [`OpWeights`] scales a write's cost above a read's within a shared pool, modelling the NAND
//!   write penalty and the read-tail-latency externality (the Seastar `{Mo, Mb·bytes}` model).
//!
//! [`PoolMode`] selects a single shared read+write budget (the correct model for EBS, which
//! enforces one combined IOPS/throughput limit) or two split read/write budgets (for instance/raw
//! NVMe, which publish separate ceilings).

use crate::sched::{CreditConfig, Rate};

/// How a device's budget is denominated.
#[derive(Clone, Copy, Debug)]
pub enum CostModel {
    /// Bandwidth-limited: an op's cost is its transfer length in bytes.
    Bytes,
    /// IOPS-limited: an op's cost is `ceil(len / io_unit)` operations. SSD-backed EBS counts one
    /// I/O per 16 KiB (the IOPS limit) — a 512 KiB read costs 32 IOPS. A control op (len 0) costs 1.
    Iops { io_unit: u32 },
}

impl CostModel {
    /// The raw cost of a `len`-byte op in this model's currency, before the per-kind weight.
    #[inline]
    pub fn raw_cost(&self, len: u32) -> u64 {
        match *self {
            CostModel::Bytes => len.max(1) as u64,
            CostModel::Iops { io_unit } => {
                let io_unit = io_unit.max(1) as u64;
                (len as u64).div_ceil(io_unit).max(1)
            }
        }
    }
}

/// Per-op-kind cost multiplier (fixed-point, in 1/256ths so `256` == weight 1.0). A write debits
/// `cost * write / 256` credits; a read debits `cost * read / 256`.
///
/// The **default weights a write at 2.0× a read** (`read 1.0`, `write 2.0`): our testing shows a
/// write costs roughly twice a read of equal size on the SSD/NVMe-backed devices this scheduler
/// targets (NAND program-vs-read asymmetry plus the read-tail-latency externality a write imposes).
/// There is no universal write multiplier — large sequential writes approach write-amplification 1
/// (weight ~1), small random writes on a full drive are far costlier — so this stays operator-tunable
/// via [`OpWeights::new`], with `2.0` as the calibrated starting point.
///
/// The weight only bites in [`PoolMode::Shared`], where reads and writes contend for one budget. In
/// [`PoolMode::Split`] each pool already has its own capacity, so the read:write ratio is expressed
/// by the two capacities and the weight is typically left at default.
#[derive(Clone, Copy, Debug)]
pub struct OpWeights {
    read_256ths: u32,
    write_256ths: u32,
}

impl Default for OpWeights {
    #[inline]
    fn default() -> Self {
        // Read 1.0×, write 2.0× — writes are ~2× as expensive as reads on our target devices.
        Self {
            read_256ths: 256,
            write_256ths: 512,
        }
    }
}

impl OpWeights {
    /// Build weights from floating-point multipliers (e.g. `1.0` read, `1.5` write).
    #[inline]
    pub fn new(read: f64, write: f64) -> Self {
        Self {
            read_256ths: ((read.max(0.0) * 256.0).round() as u32).max(1),
            write_256ths: ((write.max(0.0) * 256.0).round() as u32).max(1),
        }
    }

    /// Apply the weight for `kind` to a raw cost, flooring at 1 so no op is free.
    #[inline]
    pub fn apply(&self, raw_cost: u64, is_read: bool) -> u64 {
        let w = if is_read {
            self.read_256ths
        } else {
            self.write_256ths
        } as u64;
        ((raw_cost.saturating_mul(w)) / 256).max(1)
    }
}

/// Whether reads and writes share one budget or use two.
#[derive(Clone, Debug)]
pub enum PoolMode {
    /// One combined read+write budget. Correct for EBS (a single enforced IOPS/throughput limit);
    /// the write penalty is expressed via [`OpWeights`].
    Shared(CreditConfig),
    /// Independent read and write budgets. For instance/raw NVMe with separate published ceilings.
    Split {
        read: CreditConfig,
        write: CreditConfig,
    },
}

/// The default number of execution lanes a device gets when [`DeviceConfig`] does not override it.
/// One lane is the simplest shape (a single ring / one blocking thread); raise it for a device that
/// benefits from spreading work across several rings/threads.
pub const DEFAULT_LANE_COUNT: usize = 1;

/// Per-device configuration.
///
/// Each device now **owns its own execution**: its credit pool(s), cost model, pacer rate, and its
/// own set of execution lanes. `lane_count` is therefore a per-device knob — the maximum number of
/// rings / blocking-pool threads this device's ops fan out across — decoupled from any other device.
/// The device's *queue depth* (max ops concurrently in flight) is governed separately by the credit
/// pool capacity ([`pool_mode`](Self::pool_mode)); `lane_count` is how that in-flight work is spread
/// across execution resources.
#[derive(Clone, Debug)]
pub struct DeviceConfig {
    /// The credit budget(s) for this device.
    pub pool_mode: PoolMode,
    /// The pacer rate enforcing the device's IOPS/bandwidth ceiling (in the cost-model currency).
    pub rate: Rate,
    /// How the budget is denominated.
    pub cost_model: CostModel,
    /// Read-vs-write cost weighting (applies within a shared pool; in split mode each pool already
    /// has its own capacity so the weight is typically left at default).
    pub op_weights: OpWeights,
    /// Number of execution lanes (rings / blocking-pool threads) this device's ops fan out across.
    /// Per-device and independent of the credit-pool queue depth. Defaults to [`DEFAULT_LANE_COUNT`].
    pub lane_count: usize,
}

impl DeviceConfig {
    /// A simple bandwidth-limited device: one shared pool of `capacity` bytes, paced at `rate`.
    pub fn shared_bytes(capacity: u64, rate: Rate) -> Self {
        Self {
            pool_mode: PoolMode::Shared(CreditConfig::new(capacity)),
            rate,
            cost_model: CostModel::Bytes,
            op_weights: OpWeights::default(),
            lane_count: DEFAULT_LANE_COUNT,
        }
    }

    /// An IOPS-limited device: one shared pool of `queue_depth` outstanding ops, where each op
    /// counts `ceil(len / io_unit)` operations, paced at `rate` (ops/sec expressed as a [`Rate`]).
    pub fn shared_iops(queue_depth: u64, io_unit: u32, rate: Rate) -> Self {
        Self {
            pool_mode: PoolMode::Shared(CreditConfig::new(queue_depth)),
            rate,
            cost_model: CostModel::Iops { io_unit },
            op_weights: OpWeights::default(),
            lane_count: DEFAULT_LANE_COUNT,
        }
    }

    /// Override the per-device lane count (builder style).
    pub fn with_lane_count(mut self, lane_count: usize) -> Self {
        self.lane_count = lane_count.max(1);
        self
    }
}
