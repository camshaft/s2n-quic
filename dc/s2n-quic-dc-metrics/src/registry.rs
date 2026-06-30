// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

use crate::{
    backend::{Backend, MetricInfo, MetricKind, QuerylogBackend, ReportOptions},
    rseq::Channels,
    BoolCounter, Counter, Summary, Unit,
};

/// A `Registry` allows registering metrics for emission and can be asked to periodically emit
/// them.
///
/// `Clone` for `Registry` will share the underlying storage. This can make it easier to put
/// recorders into various structures, though callers should prefer to register individual metrics
/// up front (rather than repeatedly doing so).
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<RegistryInner>>,
}

pub(crate) struct RegistryInner {
    // Use a BTreeMap so that we automatically get consistent ordering of the reported metrics.
    // Consistent ordering makes it easier to analyze them locally visually or with ad-hoc scripts.
    metrics: BTreeMap<MetricKey, MetricValue>,

    counters: Arc<Channels<crate::counter::SharedCounter>>,
    histograms: Arc<Channels<crate::summary::SharedSummary>>,

    task_monitors: HashMap<String, crate::TaskMonitor>,

    is_open: bool,
}

impl RegistryInner {
    /// Drives a [`Backend`] over every registered metric.
    ///
    /// Steals per-CPU pages once, then visits every metric in `BTreeMap` order, draining each and
    /// handing the backend its native value. Draining is unconditional (a zero counter/histogram
    /// drains to a no-op); whether a zero is *emitted* is the backend's decision.
    pub fn report(&mut self, options: &ReportOptions, backend: &mut dyn Backend) {
        if !self.is_open {
            return;
        }

        // Ensure that all per-CPU data is aggregated.
        self.counters.steal_pages();
        self.histograms.steal_pages();

        backend.report_start(options);

        for (key, value) in self.metrics.iter_mut() {
            let name = key.name.as_str();
            let aggregation = key.aggregation.as_deref();
            match value {
                MetricValue::Counter(c) => {
                    let info = MetricInfo::new(name, aggregation, Unit::Count, MetricKind::Counter);
                    c.report(&info, backend);
                }
                MetricValue::Summary(s) => {
                    let info =
                        MetricInfo::new(name, aggregation, s.display_unit(), MetricKind::Histogram);
                    s.report(&info, backend);
                }
                MetricValue::BoolCounter(b) => {
                    let info =
                        MetricInfo::new(name, aggregation, Unit::Count, MetricKind::BoolCounter);
                    b.report(&info, backend);
                }
                MetricValue::ValueList(c) => {
                    let mut info = MetricInfo::new(
                        name,
                        aggregation,
                        c.unit(),
                        MetricKind::CallbackScalar,
                    );
                    info.zero_suppressed = c.zero_suppressed();
                    c.report(&info, backend);
                }
            }
        }

        backend.report_end();
    }

    pub fn try_take_current_metrics_line(&mut self, include_sparse: bool) -> Option<String> {
        if !self.is_open {
            return None;
        }

        let mut backend = QuerylogBackend::new();
        self.report(&ReportOptions::new(include_sparse), &mut backend);
        Some(backend.into_line())
    }
}

impl Registry {
    pub fn new() -> Registry {
        Registry {
            inner: Arc::new(Mutex::new(RegistryInner {
                metrics: BTreeMap::new(),
                counters: Arc::new(Channels::new()),
                histograms: Arc::new(Channels::new()),
                task_monitors: HashMap::new(),
                is_open: true,
            })),
        }
    }

    pub fn register_task_monitor(&self, task: &str) -> crate::TaskMonitor {
        let aggregation = format!("Task|{task}");

        let guard = self.inner.lock().unwrap();
        if let Some(monitor) = guard.task_monitors.get(&aggregation) {
            monitor.clone()
        } else {
            drop(guard);
            let monitor = crate::TaskMonitor::new(self, aggregation.clone());
            let mut guard = self.inner.lock().unwrap();
            guard.task_monitors.insert(aggregation, monitor.clone());
            monitor
        }
    }

    /// Registers a given metric (name, aggregation) with the recorder as a `Counter`.
    ///
    /// This will deduplicate calls, but is somewhat expensive, so prefer to call just once and
    /// then reuse the returned type.
    #[track_caller]
    pub fn register_counter(&self, metric: String, aggregation: Option<String>) -> Counter {
        let mut inner = self.inner.lock().unwrap();
        let inner = &mut *inner;

        let entry = inner
            .metrics
            .entry(MetricKey {
                name: metric.clone(),
                aggregation: aggregation.clone(),
            })
            .or_insert_with(|| MetricValue::Counter(Counter::new(inner.counters.clone())));

        if let MetricValue::Counter(c) = &*entry {
            c.clone()
        } else {
            panic!(
                "Non-counter metric name={metric:?}, aggregation={aggregation:?} already registered"
            )
        }
    }

    /// Registers a given metric (name, class, instance) with the recorder as a `Summary`.
    ///
    /// This will deduplicate calls, but is somewhat expensive, so prefer to call just once and
    /// then reuse the returned type.
    #[track_caller]
    pub fn register_summary(
        &self,
        metric: String,
        aggregation: Option<String>,
        display_unit: Unit,
    ) -> Summary {
        let mut inner = self.inner.lock().unwrap();
        let inner = &mut *inner;

        let entry = inner
            .metrics
            .entry(MetricKey {
                name: metric.clone(),
                aggregation: aggregation.clone(),
            })
            .or_insert_with(|| {
                MetricValue::Summary(Summary::new(inner.histograms.clone(), display_unit))
            });

        if let MetricValue::Summary(s) = &*entry {
            s.clone()
        } else {
            panic!(
                "Non-summary metric name={metric:?}, aggregation={aggregation:?} already registered"
            )
        }
    }

    /// Registers a given metric with the recorder as a `BoolCounter`.
    ///
    /// This will deduplicate calls, but is somewhat expensive, so prefer to call just once and
    /// then reuse the returned type.
    #[track_caller]
    pub fn register_bool(&self, metric: String, aggregation: Option<String>) -> BoolCounter {
        let mut inner = self.inner.lock().unwrap();
        let inner = &mut *inner;

        let entry = inner
            .metrics
            .entry(MetricKey {
                name: metric.clone(),
                aggregation: aggregation.clone(),
            })
            .or_insert_with(|| MetricValue::BoolCounter(BoolCounter::new(inner.counters.clone())));

        if let MetricValue::BoolCounter(b) = &*entry {
            b.clone()
        } else {
            panic!(
                "Non-bool metric name={metric:?}, aggregation={aggregation:?} already registered"
            )
        }
    }

    /// Registers a given metric with the recorder, where the value is obtained by calling the
    /// provided function.
    ///
    /// The callback returns a [`CallbackValue`](crate::backend::CallbackValue) (any numeric type),
    /// which exposes both the native value (for structured backends) and the querylog display form.
    /// A zero value is always reported (use [`register_list_callback_zero_suppressed`] for
    /// gauge-style metrics that should omit zeros).
    ///
    /// The provided callback type must match across all calls (we store and confirm this via
    /// `Any`). On repeat calls with matching metric name and aggregation, the new callback is
    /// appended; all callbacks under one name are joined with `+` in the querylog output.
    ///
    /// [`register_list_callback_zero_suppressed`]: Self::register_list_callback_zero_suppressed
    #[track_caller]
    pub fn register_list_callback<V, F>(
        &self,
        metric: String,
        aggregation: Option<String>,
        unit: Unit,
        callback: F,
    ) where
        V: crate::backend::CallbackValue,
        F: FnMut() -> V + 'static + Send,
    {
        self.register_list_callback_inner(metric, aggregation, unit, false, callback);
    }

    /// Like [`register_list_callback`](Self::register_list_callback), but a zero value is suppressed
    /// from output entirely. Use for gauge-style metrics (e.g. queue depth).
    #[track_caller]
    pub fn register_list_callback_zero_suppressed<V, F>(
        &self,
        metric: String,
        aggregation: Option<String>,
        unit: Unit,
        callback: F,
    ) where
        V: crate::backend::CallbackValue,
        F: FnMut() -> V + 'static + Send,
    {
        self.register_list_callback_inner(metric, aggregation, unit, true, callback);
    }

    #[track_caller]
    fn register_list_callback_inner<V, F>(
        &self,
        metric: String,
        aggregation: Option<String>,
        unit: Unit,
        zero_suppressed: bool,
        callback: F,
    ) where
        V: crate::backend::CallbackValue,
        F: FnMut() -> V + 'static + Send,
    {
        let mut inner = self.inner.lock().unwrap();

        let entry = inner.metrics.entry(MetricKey {
            name: metric.clone(),
            aggregation: aggregation.clone(),
        });

        match entry {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(MetricValue::ValueList(Box::new(
                    crate::callback::CallbackList {
                        callbacks: vec![callback],
                        unit,
                        zero_suppressed,
                    },
                )));
            }
            std::collections::btree_map::Entry::Occupied(mut o) => {
                if let MetricValue::ValueList(previous) = o.get_mut() {
                    if let Some(previous) = previous
                        .as_any()
                        .downcast_mut::<crate::callback::CallbackList<F>>()
                    {
                        assert_eq!(previous.unit, unit);
                        assert_eq!(previous.zero_suppressed, zero_suppressed);
                        previous.callbacks.push(callback);
                    } else {
                        panic!(
                            "Callback metric name={metric:?}, aggregation={aggregation:?} already registered with different type"
                        );
                    }
                } else {
                    panic!(
                        "Non-callback metric name={metric:?}, aggregation={aggregation:?} already registered"
                    )
                }
            }
        }
    }

    pub fn has_metrics(&self) -> bool {
        !self.inner.lock().unwrap().metrics.is_empty()
    }

    /// Compute and return the latest metrics line.
    ///
    /// This returns the text which should be placed after `Metrics=` into the service log.
    ///
    /// Note that this will reset various counters, so this shouldn't be called unless emitting
    /// into logs.
    ///
    /// # Panics
    ///
    /// * If the registry has been closed
    pub fn take_current_metrics_line(&self) -> String {
        self.try_take_current_metrics_line()
            .expect("cannot take metrics from closed registry")
    }

    /// Compute and return the latest metrics line if the registry is open.
    ///
    /// This returns the text which should be placed after `Metrics=` into the service log.
    ///
    /// Note that this will reset various counters, so this shouldn't be called unless emitting
    /// into logs.
    pub fn try_take_current_metrics_line(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_take_current_metrics_line(true)
    }

    /// Computes and returns the latest metrics line (or None if the registry has been closed).
    ///
    /// This function, unlike the non `_sparse` variants, supports omitting registered providers
    /// that don't currently have any values to report. This avoids skewing collected data by
    /// polluting with zeros, but does mean that aggregation systems which timeout metrics not
    /// being emitted may lose track of metrics as a result, or it may make it harder to ensure
    /// alarms fire if they're misconfigured (i.e., treating missing data as breaching).
    ///
    /// Depending on the deployment, different strategies for setting `include_sparse` may make
    /// sense. For example, it might be best to only include sparse metrics from one host (if the
    /// fleet is large) or with some low probability, depending on frequency of calls and
    /// sparseness support in the underlying data store.
    pub fn try_take_current_metrics_line_sparse(&self, include_sparse: bool) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_take_current_metrics_line(include_sparse)
    }

    /// Drives a [`Backend`] over every registered metric, draining each.
    ///
    /// This is the native-value reporting path: the backend receives a `u64` counter, an `i64`
    /// gauge, a borrowed histogram with real buckets, etc., rather than a re-parseable string.
    /// Because the take is destructive, compose backends (e.g. `(A, B)`) to feed several from one
    /// snapshot.
    ///
    /// Uses [`ReportOptions::default`] (`include_sparse = false`, so metrics with no recorded value
    /// are omitted). Note this differs from [`take_current_metrics_line`](Self::take_current_metrics_line),
    /// which reports sparsely. Use [`report_with`](Self::report_with) to pass options.
    ///
    /// The backend must not re-enter this `Registry` from its callbacks; see [`Backend`].
    pub fn report(&self, backend: &mut dyn Backend) {
        self.report_with(&ReportOptions::default(), backend);
    }

    /// Like [`report`](Self::report), but with explicit [`ReportOptions`].
    ///
    /// The backend must not re-enter this `Registry` from its callbacks; see [`Backend`].
    pub fn report_with(&self, options: &ReportOptions, backend: &mut dyn Backend) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .report(options, backend);
    }

    /// Returns `true` if the registry is open
    pub fn is_open(&self) -> bool {
        self.inner.lock().is_ok_and(|inner| inner.is_open)
    }

    /// Closes the registry
    ///
    /// This is used as a mechanism to notify and background workers that metrics are no longer being
    /// updated and should shut down.
    pub fn close(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.is_open = false;
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Registry::new()
    }
}

/// This represents a single entry in our emitted service log, with optional aggregation along the
/// two class/instance dimensions.
#[derive(PartialEq, Eq, Hash, PartialOrd, Ord)]
struct MetricKey {
    name: String,
    aggregation: Option<String>,
}

/// This represents metric state. Note that a single metric may collect many different values
/// between emissions; so a "value" represents potentially multiple recorded points.
///
/// (FIXME: rename this to something else?)
enum MetricValue {
    Counter(Counter),
    Summary(Summary),
    BoolCounter(BoolCounter),
    ValueList(Box<dyn crate::callback::ValueList + Send>),
}

#[cfg(test)]
mod test {
    use super::*;

    /// Golden snapshot covering one of every metric kind plus the structural conventions (queue
    /// enq/drain/depth, nominal variant aggregation). This locks the exact querylog line format
    /// end-to-end through the `Backend` driver, proving the refactor stays byte-identical.
    #[test]
    fn querylog_line_is_byte_identical_across_all_kinds() {
        let registry = Registry::new();

        // Plain counter.
        let rx_data = registry.register_counter("rx.data".into(), None);
        // Nominal counters sharing one name, via the `Variant|` aggregation convention.
        let ecn_ect0 = registry.register_counter("rx.ecn".into(), Some("Variant|ect0".into()));
        let ecn_ect1 = registry.register_counter("rx.ecn".into(), Some("Variant|ect1".into()));
        // Bool counter.
        let connect = registry.register_bool("connect".into(), None);
        // Histogram in microseconds.
        let decrypt = registry.register_summary("rx.decrypt_time".into(), None, Unit::Microsecond);
        // Always-on callback (runtime-style): emits even when zero.
        registry.register_list_callback("workers".into(), None, Unit::Count, || 4usize);
        // Zero-suppressed gauge callback at a non-zero value.
        registry.register_list_callback_zero_suppressed(
            "q.depth".into(),
            None,
            Unit::Count,
            || 5i64,
        );
        // Zero-suppressed gauge callback at zero: must be omitted entirely.
        registry.register_list_callback_zero_suppressed(
            "q.empty".into(),
            None,
            Unit::Count,
            || 0i64,
        );

        rx_data.increment(255);
        ecn_ect0.increment(500);
        ecn_ect1.increment(3);
        connect.record(true);
        connect.record(true);
        connect.record(false);
        decrypt.record_duration(std::time::Duration::from_micros(2));

        let line = registry.take_current_metrics_line();

        // BTreeMap ordering: keys sorted by (name, aggregation).
        // - connect (bool): 1*2+0*1
        // - q.depth (gauge): 5 (zero-suppressed but non-zero)
        // - q.empty: omitted (zero-suppressed, zero)
        // - rx.data: 255
        // - rx.decrypt_time: one sample at ~2us -> "2*1 us"
        // - rx.ecn ect0/ect1: 500/3 with Variant| aggregation suffix
        // - workers (callback): 4
        assert_eq!(
            line,
            "connect=1*2+0*1,q.depth=5,rx.data=255,rx.decrypt_time=2*1 us,rx.ecn=500 Variant|ect0,rx.ecn=3 Variant|ect1,workers=4"
        );

        // The take is destructive: counters/histograms/bools drained to zero, gauges/callbacks
        // re-read live state. Reading non-sparse (the drained scalars/bools/histogram are omitted;
        // the always-on callback and live gauge remain).
        let line2 = registry
            .try_take_current_metrics_line_sparse(false)
            .unwrap();
        assert_eq!(line2, "q.depth=5,workers=4");

        // Reading sparse instead re-includes the drained zeros (matching historical
        // `take_current_metrics_line`, which passes include_sparse=true).
        let line3 = registry.take_current_metrics_line();
        assert_eq!(
            line3,
            "q.depth=5,rx.data=0,rx.decrypt_time=0 us,rx.ecn=0 Variant|ect0,rx.ecn=0 Variant|ect1,workers=4"
        );
    }

    /// With `include_sparse`, drained counters and histograms still emit their zero.
    #[test]
    fn sparse_includes_zeroed_metrics() {
        let registry = Registry::new();
        registry.register_counter("a".into(), None);
        registry.register_summary("h".into(), None, Unit::Byte);

        let line = registry
            .try_take_current_metrics_line_sparse(true)
            .unwrap();
        assert_eq!(line, "a=0,h=0 B");
    }
}
