// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::Unit;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// A `Gauge` holds a single last-write-wins reading, reported as an exact `i64` (no float
/// conversion). Unlike [`Counter`](crate::Counter) it is not drained at report time — a gauge is a
/// live level, not an accumulated delta — and it uses one shared atomic rather than per-CPU pages,
/// since a store is last-writer-wins and needs no cross-CPU fold.
#[derive(Clone)]
pub struct Gauge {
    value: Arc<AtomicU64>,
    /// The display unit (e.g. [`Unit::Byte`]); surfaced through [`MetricInfo`](crate::MetricInfo).
    unit: Unit,
}

impl Gauge {
    pub(crate) fn new(unit: Unit) -> Gauge {
        Gauge {
            value: Arc::new(AtomicU64::new(0)),
            unit,
        }
    }

    pub(crate) fn unit(&self) -> Unit {
        self.unit
    }

    /// Sets the current reading. Stored as the `u64` bit pattern of `value`; report reads it back as
    /// `i64`.
    pub fn set(&self, value: i64) {
        self.value.store(value as u64, Ordering::Relaxed);
    }

    /// Reports the current reading to `backend` without draining it (a gauge is a live level, re-read
    /// every interval).
    pub(crate) fn report(&self, info: &crate::MetricInfo<'_>, backend: &mut dyn crate::Backend) {
        let value = self.value.load(Ordering::Relaxed) as i64;
        backend.record_gauge(info, value);
    }
}

#[cfg(test)]
mod test {
    /// A gauge reports its last `set` value exactly, including magnitudes above the `u64::MAX/1e6`
    /// float ceiling.
    #[test]
    fn gauge_reports_large_value_exactly() {
        let registry = crate::Registry::new();
        let gauge = registry.register_gauge("queue_depth".into(), None, crate::Unit::Count);
        gauge.set(1_783_972_929_737_661_674);
        assert_eq!(
            registry.take_current_metrics_line(),
            "queue_depth=1783972929737661674"
        );
    }
}
