// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-device counters for the storage IO scheduler.
//!
//! There is **no scheduler-wide aggregate**: every counter is a **nominal** metric whose variant is
//! the device's label, so `fs.device.*{device="nvme0"}` gives per-device insight and the metrics
//! backend sums across variants for a fleet-wide view (the same pattern the network endpoint uses).
//! This is sound here because *every* increment site already holds the device — the submit path and
//! `prepare` take `&Arc<Device>`, and the in-place completion path has `op.device` — so per-device
//! attribution costs no extra plumbing.
//!
//! Naming follows the crate convention: `namespace.name`, lower-snake, with a leading `!` on metrics
//! that indicate a problem. The namespaces are `fs.device.op.{kind}.*` (per-op-kind submit/complete
//! counts + byte-size histograms), `fs.device.{rejected,orphaned,lane_closed}` (error signals),
//! `fs.device.sojourn` (submit→complete latency), and `fs.credit.*` (the credit pools' own gauges,
//! registered separately via [`crate::credit::Pool::register_gauges`]).

use crate::{
    counter::{Counter, Registry, Summary, Unit},
    fs::op::IoKind,
};

/// Per-op-kind metrics for one device: how many ops of this kind were submitted and completed, and —
/// for data ops (read/write) — a histogram of their transfer sizes on each path. The kind is encoded
/// in the metric *name* (`fs.device.op.read.*`); the device is the nominal *variant*, so a dashboard
/// can slice by either dimension.
pub struct OpKindCounters {
    /// Ops of this kind admitted (credit acquired + enqueued).
    pub submitted: Counter,
    /// Ops of this kind completed successfully.
    pub completed: Counter,
    /// Byte-size histogram of this kind at submit (data ops only; `None` for control ops).
    pub submit_bytes: Option<Summary>,
    /// Byte-size histogram of bytes actually transferred at completion (data ops only).
    pub complete_bytes: Option<Summary>,
}

impl OpKindCounters {
    fn register(registry: &Registry, kind: IoKind, label: &str) -> Self {
        let name = kind.name();
        let (submit_bytes, complete_bytes) = if kind.is_data() {
            (
                Some(registry.register_nominal_summary(
                    format!("fs.device.op.{name}.submit_bytes"),
                    label,
                    Unit::Byte,
                )),
                Some(registry.register_nominal_summary(
                    format!("fs.device.op.{name}.complete_bytes"),
                    label,
                    Unit::Byte,
                )),
            )
        } else {
            (None, None)
        };
        Self {
            submitted: registry.register_nominal(format!("fs.device.op.{name}.submitted"), label),
            completed: registry.register_nominal(format!("fs.device.op.{name}.completed"), label),
            submit_bytes,
            complete_bytes,
        }
    }
}

/// All per-device counters, registered as nominal metrics keyed by the device's label. One lives on
/// each [`Device`](crate::fs::device::Device), built at registration; every op carries its
/// `Arc<Device>`, so the submit and in-place completion paths bump these directly (no plumbing).
pub struct DeviceCounters {
    /// Per-op-kind counts + byte-size histograms, indexed by [`IoKind::index`].
    op: [OpKindCounters; IoKind::ALL.len()],
    /// `!` Submissions rejected before admission (bad offset, misaligned/oversized, closed channel).
    pub rejected: Counter,
    /// `!` Ops whose backend execution failed (errored syscall / CQE).
    pub failed: Counter,
    /// `!` Completed ops whose submitter had already dropped its receiver (cancelled read-ahead,
    /// occasionally a bug) — the result is discarded.
    pub orphaned: Counter,
    /// Ops a backend skipped before execution because the submitter had already dropped its receiver
    /// — the IO (syscall / SQE / processing) was never issued, saving the work. Not an error (it is
    /// the optimization working); a high rate just signals heavy upstream cancellation/churn.
    pub cancelled: Counter,
    /// [`Reservation`](crate::fs::device::Reservation)s dropped without `submit` — credit was acquired
    /// but the op was **never** enqueued: no IO issued, the credit released, no buffer ever committed.
    /// Not an error (the two-phase reserve/cancel working); a spike is expected right after the upstream
    /// drops the data (e.g. a deleted shuffle in the spill layer). Distinct from
    /// [`cancelled`](Self::cancelled), which counts a backend skipping an **already-enqueued** op whose
    /// receiver vanished — by then the op (and its fixed disk offset) is owned downstream and the write
    /// must still complete; this counter is the only point where the write is genuinely *not* issued.
    pub cancelled_before_submit: Counter,
    /// `!` Ops the dispatcher could not hand to a lane (lane/backend closed); surfaced as failed.
    pub lane_closed: Counter,
    /// Sojourn: submit→complete latency in microseconds (the end-to-end time an op spent in the
    /// scheduler, queueing + execution). The headline health signal for a device.
    pub sojourn_us: Summary,
}

impl DeviceCounters {
    /// Register this device's nominal counters under `label` (the variant). Called once per device by
    /// [`DeviceRegistry::register_device`](crate::fs::scheduler::DeviceRegistry::register_device).
    pub fn register(registry: &Registry, label: &str) -> Self {
        Self {
            op: IoKind::ALL.map(|kind| OpKindCounters::register(registry, kind, label)),
            rejected: registry.register_nominal("fs.device.!rejected", label),
            failed: registry.register_nominal("fs.device.!failed", label),
            orphaned: registry.register_nominal("fs.device.!orphaned", label),
            cancelled: registry.register_nominal("fs.device.cancelled", label),
            cancelled_before_submit: registry
                .register_nominal("fs.device.cancelled_before_submit", label),
            lane_closed: registry.register_nominal("fs.device.!lane_closed", label),
            sojourn_us: registry.register_nominal_summary(
                "fs.device.sojourn",
                label,
                Unit::Microsecond,
            ),
        }
    }

    /// The per-kind counters for `kind`.
    #[inline]
    pub fn op(&self, kind: IoKind) -> &OpKindCounters {
        &self.op[kind.index()]
    }
}
