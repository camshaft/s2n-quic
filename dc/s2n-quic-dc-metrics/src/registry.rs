// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    thread,
    time::Duration,
};

use crate::{
    backend::{Backend, MetricInfo, MetricKind, QuerylogBackend, ReportOptions, Sparsity},
    rseq::Channels,
    BoolCounter, Counter, Gauge, Summary, Unit,
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
    /// Namespace prepended to the name of every metric registered through this handle.
    ///
    /// This is a per-handle property, *not* part of the shared [`RegistryInner`]: cloning a
    /// `Registry` keeps the same prefix, but [`child`](Registry::child) produces a handle over the
    /// same underlying storage with an extended prefix. It affects only the metric *name* recorded
    /// into storage (and therefore what backends emit); the aggregation/variant dimension is left
    /// untouched.
    prefix: Option<Arc<str>>,
}

/// Handle for the background drain thread spawned by [`Registry::spawn_default_drain_reporter`].
///
/// Stops and joins the thread on drop, so the drainer's lifetime is tied to this handle — keep it
/// alive for as long as the registry should be drained (typically the endpoint's lifetime).
pub struct DrainReporter {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for DrainReporter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            // The thread wakes at most one `interval` after `stop` is set (it sleeps between
            // drains); joining keeps teardown deterministic. A drain already in flight just
            // finishes its `absorb`.
            let _ = handle.join();
        }
    }
}

pub(crate) struct RegistryInner {
    // Use a BTreeMap so that we automatically get consistent ordering of the reported metrics.
    // Consistent ordering makes it easier to analyze them locally visually or with ad-hoc scripts.
    metrics: BTreeMap<MetricKey, MetricEntry>,

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

        for (key, entry) in self.metrics.iter_mut() {
            let name = &key.name;
            let aggregation = key.aggregation.as_ref();
            // Destructure into disjoint field borrows so the metadata (sparsity, tags) can be read
            // alongside the mutable `value` borrow the drain needs, without cloning the tag Arc.
            let MetricEntry {
                value,
                sparsity,
                tags,
            } = entry;
            let sparsity = *sparsity;
            let tags = crate::MetricTags::new(tags);
            let info = |unit, kind| {
                let mut info = MetricInfo::new(name, aggregation, unit, kind);
                info.sparsity = sparsity;
                info.tags = tags;
                info
            };
            match value {
                MetricValue::Counter(c) => {
                    let unit = c.unit();
                    c.report(&info(unit, MetricKind::Counter), backend);
                }
                MetricValue::Gauge(g) => {
                    let unit = g.unit();
                    g.report(&info(unit, MetricKind::Gauge), backend);
                }
                MetricValue::Summary(s) => {
                    let unit = s.display_unit();
                    s.report(&info(unit, MetricKind::Histogram), backend);
                }
                MetricValue::BoolCounter(b) => {
                    b.report(&info(Unit::Count, MetricKind::BoolCounter), backend);
                }
                MetricValue::ValueList(c) => {
                    let unit = c.unit();
                    c.report(&info(unit, MetricKind::CallbackScalar), backend);
                }
            }
        }

        backend.report_end();
    }

    /// Snapshots the shape of every registered metric without touching its value. See
    /// [`Registry::descriptors`].
    ///
    /// Unlike [`report`](Self::report), this reads no per-CPU pages, invokes no callbacks, and
    /// drains nothing: it walks the metric map and copies each entry's static metadata. The kind and
    /// unit are derived identically to the report path, so a descriptor describes exactly the
    /// [`MetricInfo`] a backend would see for that metric.
    pub fn descriptors(&self) -> Vec<MetricDescriptor> {
        self.metrics
            .iter()
            .map(|(key, entry)| {
                let (kind, unit) = match &entry.value {
                    MetricValue::Counter(c) => (MetricKind::Counter, c.unit()),
                    MetricValue::Gauge(g) => (MetricKind::Gauge, g.unit()),
                    MetricValue::Summary(s) => (MetricKind::Histogram, s.display_unit()),
                    MetricValue::BoolCounter(_) => (MetricKind::BoolCounter, Unit::Count),
                    MetricValue::ValueList(c) => (MetricKind::CallbackScalar, c.unit()),
                };
                MetricDescriptor {
                    name: key.name.clone(),
                    aggregation: key.aggregation.clone(),
                    kind,
                    unit,
                    sparsity: entry.sparsity,
                    tags: entry.tags.clone(),
                }
            })
            .collect()
    }

    /// Folds the buffered *full pages* into the aggregate without draining or reporting. See
    /// [`Registry::absorb`].
    ///
    /// This deliberately compacts only the full-page buffer (the part that grows without bound
    /// between reports), not the per-CPU pages — those are bounded to one page per CPU and are only
    /// swept by [`report`](Self::report), which pays for the `membarrier` that makes reading them
    /// sound. Folding is additive, so a later report still observes every event.
    pub fn absorb(&self) {
        if !self.is_open {
            return;
        }

        self.counters.absorb_full_pages();
        self.histograms.absorb_full_pages();
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
            prefix: None,
        }
    }

    /// Returns a handle over the *same* underlying storage that prepends `prefix` to the name of
    /// every metric registered through it.
    ///
    /// The prefix is joined to each metric name with a `.` (e.g. a child with prefix `myapp` turns
    /// `rx.data` into `myapp.rx.data`), matching the dotted-namespace convention used for metric
    /// names. An empty prefix yields a handle equivalent to this one.
    ///
    /// Children compose: calling `child` on a child concatenates the prefixes (`a` then `b`
    /// produces `a.b.`). Because only the name is namespaced, metrics registered through the
    /// child share the parent's counters/histograms storage and are reported together — a child is
    /// purely a naming view, not a separate registry.
    ///
    /// This is the mechanism an application embedding the endpoint uses to keep the transport's
    /// metrics in a namespace relative to its own: construct the endpoint with `registry.child(...)`
    /// and every metric the endpoint registers — through any path — is prefixed.
    pub fn child(&self, prefix: impl AsRef<str>) -> Registry {
        let prefix = prefix.as_ref();
        let prefix = match &self.prefix {
            _ if prefix.is_empty() => self.prefix.clone(),
            Some(existing) => Some(format!("{existing}.{prefix}").into()),
            None => Some(prefix.into()),
        };
        Registry {
            inner: self.inner.clone(),
            prefix,
        }
    }

    /// Applies this handle's [`prefix`](Self::prefix) to a metric name, returning the name unchanged
    /// when no prefix is set (so the common, un-prefixed path allocates nothing extra).
    fn prefixed_name(&self, metric: Arc<str>) -> Arc<str> {
        match &self.prefix {
            Some(prefix) => format!("{prefix}.{metric}").into(),
            None => metric,
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
        self.register_counter_inner(
            metric.into(),
            aggregation.map(Into::into),
            Unit::Count,
            Sparsity::Inherit,
            crate::tags::empty_metric_tag_set(),
        )
    }

    /// Like [`register_counter`](Self::register_counter), but records the counter's display
    /// [`Unit`].
    ///
    /// The unit is metadata a counter carries independent of any backend: it surfaces through
    /// [`MetricInfo::unit`](crate::MetricInfo) at report time and through
    /// [`MetricDescriptor::unit`] for introspection. A backend that renders units (e.g. the querylog
    /// line's ` B` suffix) uses it; one that doesn't (e.g. statsd) simply ignores it. This is the
    /// unit-carrying counterpart to [`register_summary`](Self::register_summary).
    #[track_caller]
    pub fn register_counter_with_unit(
        &self,
        metric: String,
        aggregation: Option<String>,
        unit: Unit,
    ) -> Counter {
        self.register_counter_inner(
            metric.into(),
            aggregation.map(Into::into),
            unit,
            Sparsity::Inherit,
            crate::tags::empty_metric_tag_set(),
        )
    }

    #[track_caller]
    fn register_counter_inner(
        &self,
        metric: Arc<str>,
        aggregation: Option<Arc<str>>,
        unit: Unit,
        sparsity: Sparsity,
        tags: crate::MetricTagSet,
    ) -> Counter {
        let metric = self.prefixed_name(metric);
        let mut inner = self.inner.lock().unwrap();
        let inner = &mut *inner;

        let entry = inner
            .metrics
            .entry(MetricKey {
                name: metric.clone(),
                aggregation: aggregation.clone(),
            })
            .or_insert_with(|| MetricEntry {
                value: MetricValue::Counter(Counter::new(inner.counters.clone(), unit)),
                sparsity,
                tags: tags.clone(),
            });

        assert_metadata(entry, sparsity, &tags, &metric, &aggregation);
        if let MetricValue::Counter(c) = &entry.value {
            // A repeat registration dedups to the existing counter; its unit is fixed at first
            // registration. Guard against a conflicting unit so a metric can't be silently
            // registered with two different display units.
            assert_eq!(
                c.unit(),
                unit,
                "counter metric name={metric:?}, aggregation={aggregation:?} already registered with a different unit"
            );
            c.clone()
        } else {
            panic!(
                "Non-counter metric name={metric:?}, aggregation={aggregation:?} already registered"
            )
        }
    }

    /// Registers a given metric (name, aggregation) with the recorder as a [`Gauge`].
    ///
    /// A gauge is a last-write-wins level reported as an exact `i64` (no float conversion), for
    /// values that are set rather than accumulated (queue depth, a timestamp marker, a byte size).
    /// This dedups on repeat calls; prefer to call once and reuse the returned handle.
    #[track_caller]
    pub fn register_gauge(&self, metric: String, aggregation: Option<String>, unit: Unit) -> Gauge {
        self.register_gauge_inner(
            metric.into(),
            aggregation.map(Into::into),
            unit,
            Sparsity::Inherit,
            crate::tags::empty_metric_tag_set(),
        )
    }

    #[track_caller]
    fn register_gauge_inner(
        &self,
        metric: Arc<str>,
        aggregation: Option<Arc<str>>,
        unit: Unit,
        sparsity: Sparsity,
        tags: crate::MetricTagSet,
    ) -> Gauge {
        let metric = self.prefixed_name(metric);
        let mut inner = self.inner.lock().unwrap();
        let inner = &mut *inner;

        let entry = inner
            .metrics
            .entry(MetricKey {
                name: metric.clone(),
                aggregation: aggregation.clone(),
            })
            .or_insert_with(|| MetricEntry {
                value: MetricValue::Gauge(Gauge::new(unit)),
                sparsity,
                tags: tags.clone(),
            });

        assert_metadata(entry, sparsity, &tags, &metric, &aggregation);
        if let MetricValue::Gauge(g) = &entry.value {
            // A repeat registration dedups to the existing gauge; its unit is fixed at first
            // registration. Guard against a conflicting unit so a metric can't be silently
            // registered with two different display units.
            assert_eq!(
                g.unit(),
                unit,
                "gauge metric name={metric:?}, aggregation={aggregation:?} already registered with a different unit"
            );
            g.clone()
        } else {
            panic!(
                "Non-gauge metric name={metric:?}, aggregation={aggregation:?} already registered"
            )
        }
    }

    /// Registers a given metric (name, class, instance) with the recorder as a `Summary`.
    ///
    /// This will deduplicate calls, but is somewhat expensive, so prefer to call just once and
    /// then reuse the returned type.
    ///
    /// The summary records integer samples ([`scale`](Summary::scale) `1.0`). To record fractional
    /// values with [`Summary::record_f64`], register through the [`metric`](Self::metric) builder
    /// with [`MetricBuilder::scale`].
    #[track_caller]
    pub fn register_summary(
        &self,
        metric: String,
        aggregation: Option<String>,
        display_unit: Unit,
    ) -> Summary {
        self.register_summary_inner(
            metric.into(),
            aggregation.map(Into::into),
            display_unit,
            Sparsity::Inherit,
            1.0,
            crate::tags::empty_metric_tag_set(),
        )
    }

    #[track_caller]
    fn register_summary_inner(
        &self,
        metric: Arc<str>,
        aggregation: Option<Arc<str>>,
        display_unit: Unit,
        sparsity: Sparsity,
        scale: f64,
        tags: crate::MetricTagSet,
    ) -> Summary {
        let metric = self.prefixed_name(metric);
        let mut inner = self.inner.lock().unwrap();
        let inner = &mut *inner;

        let entry = inner
            .metrics
            .entry(MetricKey {
                name: metric.clone(),
                aggregation: aggregation.clone(),
            })
            .or_insert_with(|| MetricEntry {
                value: MetricValue::Summary(Summary::new(
                    inner.histograms.clone(),
                    display_unit,
                    scale,
                )),
                sparsity,
                tags: tags.clone(),
            });

        assert_metadata(entry, sparsity, &tags, &metric, &aggregation);
        if let MetricValue::Summary(s) = &entry.value {
            // A repeat registration dedups to the existing summary; its scale is fixed at first
            // registration. Guard against a conflicting scale so a metric can't be silently
            // registered with two different fixed-point resolutions.
            assert_eq!(
                s.scale(),
                scale,
                "summary metric name={metric:?}, aggregation={aggregation:?} already registered with a different scale"
            );
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
        self.register_bool_inner(
            metric.into(),
            aggregation.map(Into::into),
            Sparsity::Inherit,
            crate::tags::empty_metric_tag_set(),
        )
    }

    #[track_caller]
    fn register_bool_inner(
        &self,
        metric: Arc<str>,
        aggregation: Option<Arc<str>>,
        sparsity: Sparsity,
        tags: crate::MetricTagSet,
    ) -> BoolCounter {
        let metric = self.prefixed_name(metric);
        let mut inner = self.inner.lock().unwrap();
        let inner = &mut *inner;

        let entry = inner
            .metrics
            .entry(MetricKey {
                name: metric.clone(),
                aggregation: aggregation.clone(),
            })
            .or_insert_with(|| MetricEntry {
                value: MetricValue::BoolCounter(BoolCounter::new(inner.counters.clone())),
                sparsity,
                tags: tags.clone(),
            });

        assert_metadata(entry, sparsity, &tags, &metric, &aggregation);
        if let MetricValue::BoolCounter(b) = &entry.value {
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
        // Documented contract: a zero value is always reported. That is `AlwaysDense`, not
        // `Inherit` — historically only the querylog backend honored this (statsd/prometheus gated
        // it on `include_sparse`); pinning it here makes every backend consistent.
        self.register_list_callback_inner(
            metric.into(),
            aggregation.map(Into::into),
            unit,
            Sparsity::AlwaysDense,
            crate::tags::empty_metric_tag_set(),
            callback,
        );
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
        self.register_list_callback_inner(
            metric.into(),
            aggregation.map(Into::into),
            unit,
            Sparsity::AlwaysSparse,
            crate::tags::empty_metric_tag_set(),
            callback,
        );
    }

    #[track_caller]
    fn register_list_callback_inner<V, F>(
        &self,
        metric: Arc<str>,
        aggregation: Option<Arc<str>>,
        unit: Unit,
        sparsity: Sparsity,
        tags: crate::MetricTagSet,
        callback: F,
    ) where
        V: crate::backend::CallbackValue,
        F: FnMut() -> V + 'static + Send,
    {
        let metric = self.prefixed_name(metric);
        let mut inner = self.inner.lock().unwrap();

        let entry = inner.metrics.entry(MetricKey {
            name: metric.clone(),
            aggregation: aggregation.clone(),
        });

        match entry {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(MetricEntry {
                    value: MetricValue::ValueList(Box::new(crate::callback::CallbackList {
                        callbacks: vec![callback],
                        unit,
                    })),
                    sparsity,
                    tags,
                });
            }
            std::collections::btree_map::Entry::Occupied(mut o) => {
                assert_eq!(
                    o.get().sparsity,
                    sparsity,
                    "Callback metric name={metric:?}, aggregation={aggregation:?} already registered with different sparsity"
                );
                assert_eq!(
                    &o.get().tags,
                    &tags,
                    "Callback metric name={metric:?}, aggregation={aggregation:?} already registered with different tags"
                );
                if let MetricValue::ValueList(previous) = &mut o.get_mut().value {
                    if let Some(previous) = previous
                        .as_any()
                        .downcast_mut::<crate::callback::CallbackList<F>>()
                    {
                        assert_eq!(previous.unit, unit);
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

    /// Begins registering a metric with an explicit per-metric [`Sparsity`] override.
    ///
    /// The returned [`MetricBuilder`] collects the aggregation and sparsity, then a terminal method
    /// ([`counter`](MetricBuilder::counter), [`summary`](MetricBuilder::summary),
    /// [`bool`](MetricBuilder::bool), or the callback variants) actually registers it. This is the
    /// way to opt a metric into [`Sparsity::AlwaysDense`] (emit even a zero every interval) or
    /// [`Sparsity::AlwaysSparse`] (never emit a zero); the plain `register_*` methods default to
    /// [`Sparsity::Inherit`].
    pub fn metric(&self, name: impl Into<Arc<str>>) -> MetricBuilder<'_> {
        MetricBuilder {
            registry: self,
            name: name.into(),
            aggregation: None,
            sparsity: Sparsity::Inherit,
            scale: 1.0,
            unit: Unit::Count,
            tags: Vec::new(),
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

    /// Compacts buffered per-CPU metric pages into the aggregate, without draining or reporting.
    ///
    /// Recording is a two-layer process. On the hot path, each CPU fills a fixed-size page of raw
    /// events; when a page fills (or a CPU goes idle) it is set aside, and folding those pages into
    /// the compact per-metric aggregate (a counter's running sum, a summary's bucket array) only
    /// happens when something steals them. Normally the only stealer is [`report`](Self::report),
    /// so between reports the set-aside pages accumulate: at a high event rate and a long report
    /// interval that is a large, growing buffer, and the eventual report pays to fold all of it at
    /// once.
    ///
    /// `absorb` performs just the fold. Calling it on a short interval (e.g. every second) while
    /// reporting on a longer one (e.g. every ten seconds) keeps the outstanding page buffer bounded
    /// to roughly one absorb interval and makes each report cheap, without changing what a report
    /// observes: the fold is purely additive and the per-metric drain still happens only in
    /// `report`, so the reported values are identical to never having absorbed. Callback/gauge
    /// metrics are evaluated lazily at report time and are unaffected.
    ///
    /// This is a no-op on a closed registry. It takes the same internal locks as `report`, so a
    /// background absorb may briefly contend with a concurrent report. Like `report`, this is
    /// designed to be driven by the caller on its own schedule rather than by an internal timer, so
    /// the crate needs no wall-clock source of its own.
    pub fn absorb(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .absorb();
    }

    /// Spawn a background thread that drains this registry every `interval` by calling
    /// [`absorb`](Self::absorb), and return a handle that stops+joins the thread on drop.
    ///
    /// # When to use
    ///
    /// A `Registry` records into a per-CPU page pool that is only recycled when something drains it
    /// (`absorb`/`report`). Draining is normally the job of the exporting reporter a consumer runs.
    /// A consumer that stands up a registry but installs **no** reporter therefore never recycles
    /// the pool — historically that leaked without bound (now bounded by the emergency fold in
    /// `send_event_slow`, but the metrics still never get exported and the pool churns).
    ///
    /// This is the "always register a drainer" primitive: at the **root** registry, install either
    /// the consumer's real exporting reporter **or**, when there is none, this default no-op drainer
    /// — so there is a single uniform always-drained path and no reporter-less branch that can
    /// silently misbehave. Install it **once at the root**; child handles ([`child`](Self::child))
    /// share the same storage and need no drainer of their own.
    ///
    /// The thread holds only a [`Weak`] reference, so it does not keep the registry alive: once the
    /// last owning `Registry` is dropped the next tick sees the upgrade fail and the thread exits.
    /// Dropping the returned [`DrainReporter`] also stops it promptly.
    #[must_use = "dropping the DrainReporter stops the drain thread; keep it alive for the registry's lifetime"]
    pub fn spawn_default_drain_reporter(&self, interval: Duration) -> DrainReporter {
        let weak: Weak<Mutex<RegistryInner>> = Arc::downgrade(&self.inner);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = thread::Builder::new()
            .name("dc-metrics-drain".into())
            .spawn(move || {
                while !stop_thread.load(Ordering::Relaxed) {
                    thread::sleep(interval);
                    match weak.upgrade() {
                        // Registry still alive: recycle its page pool. `absorb` is itself a no-op
                        // on a closed registry.
                        Some(inner) => inner.lock().unwrap_or_else(|e| e.into_inner()).absorb(),
                        // Registry dropped — nothing left to drain, exit.
                        None => break,
                    }
                }
            })
            .expect("failed to spawn dc-metrics drain thread");
        DrainReporter {
            stop,
            handle: Some(handle),
        }
    }

    /// Returns a [`MetricDescriptor`] for every metric registered so far, in the registry's stable
    /// `(name, aggregation)` order, without recording, draining, or invoking any callback.
    ///
    /// This is the introspection counterpart to [`report`](Self::report): where a report observes
    /// metric *values*, this observes their *shape* — the exact name a backend emits (prefix
    /// already applied), the aggregation/variant string, the [`MetricKind`], the display [`Unit`],
    /// and the [`Sparsity`] policy. It lets a consumer enumerate the full catalog of what has been
    /// registered — to document it, validate it against an expected set, export a schema, or drive
    /// code generation — without scraping a reported line and without perturbing any value.
    ///
    /// Because a metric only appears once it has been registered, call this after the endpoint (or
    /// whatever owns the metrics) has performed its registrations. Nominal metrics that share a name
    /// across variants appear as one descriptor per `(name, aggregation)` pair, mirroring how they
    /// are stored and reported.
    pub fn descriptors(&self) -> Vec<MetricDescriptor> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .descriptors()
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

/// Builder for registering a metric with an explicit [`Sparsity`] override.
///
/// Created by [`Registry::metric`]. Set the optional aggregation and sparsity, then call a terminal
/// method to register (and return the handle for) the metric of the desired kind.
#[must_use = "the metric is only registered once a terminal method (counter/summary/bool/...) is called"]
pub struct MetricBuilder<'a> {
    registry: &'a Registry,
    name: Arc<str>,
    aggregation: Option<Arc<str>>,
    sparsity: Sparsity,
    scale: f64,
    unit: Unit,
    /// Accumulated [metadata tags](crate::MetricTags); finalized into a sorted
    /// [`MetricTagSet`](crate::MetricTagSet) by the terminal method.
    tags: Vec<(Arc<str>, Arc<str>)>,
}

impl<'a> MetricBuilder<'a> {
    /// Sets the aggregation/variant string (e.g. `Variant|ect0`).
    pub fn aggregation(mut self, aggregation: impl Into<Arc<str>>) -> Self {
        self.aggregation = Some(aggregation.into());
        self
    }

    /// Attaches a `(key, value)` [metadata tag](crate::MetricTags) to the metric (repeatable).
    ///
    /// Tags are metadata a filtering policy matches on to route/filter/collapse — e.g.
    /// `.tag("level", "debug")`. They are **not** emitted by existing backends and are distinct
    /// from the [`aggregation`](Self::aggregation) wire dimension, so they add no cardinality.
    /// Setting the same key twice keeps the last value.
    pub fn tag(mut self, key: impl Into<Arc<str>>, value: impl Into<Arc<str>>) -> Self {
        self.tags.push((key.into(), value.into()));
        self
    }

    /// Sets the per-metric [`Sparsity`] policy. Defaults to [`Sparsity::Inherit`].
    pub fn sparsity(mut self, sparsity: Sparsity) -> Self {
        self.sparsity = sparsity;
        self
    }

    /// Sets the display [`Unit`] for a [`counter`](Self::counter). Defaults to [`Unit::Count`].
    ///
    /// Applies only to [`counter`](Self::counter); [`summary`](Self::summary) and the callback
    /// variants take their unit as an explicit argument and ignore this.
    pub fn unit(mut self, unit: Unit) -> Self {
        self.unit = unit;
        self
    }

    /// Shorthand for `.sparsity(Sparsity::AlwaysDense)`: always emit, even a zero.
    pub fn dense(self) -> Self {
        self.sparsity(Sparsity::AlwaysDense)
    }

    /// Shorthand for `.sparsity(Sparsity::AlwaysSparse)`: never emit a zero.
    pub fn sparse(self) -> Self {
        self.sparsity(Sparsity::AlwaysSparse)
    }

    /// Sets the fixed-point [`scale`](Summary::scale) for a [`summary`](Self::summary) that records
    /// fractional samples via [`Summary::record_f64`].
    ///
    /// A float sample `v` is stored as `round(scale * v)`, so `scale` sets the resolution: a value
    /// of `10^d` keeps `d` fractional digits (the querylog line prints that many places), with a
    /// resolution floor of `~0.5/scale` and a ceiling of `u64::MAX/scale`. Defaults to `1.0` (a
    /// plain integer summary). Applies only to [`summary`](Self::summary); ignored by the other
    /// terminal methods.
    ///
    /// # Panics
    ///
    /// Panics if `scale` is not finite and positive.
    pub fn scale(mut self, scale: f64) -> Self {
        assert!(
            scale.is_finite() && scale > 0.0,
            "summary scale must be finite and positive, got {scale}"
        );
        self.scale = scale;
        self
    }

    /// Finalizes the accumulated builder tags into a [`MetricTagSet`](crate::MetricTagSet),
    /// returning the shared empty set for the common untagged builder (no per-metric allocation).
    fn finish_tags(tags: Vec<(Arc<str>, Arc<str>)>) -> crate::MetricTagSet {
        if tags.is_empty() {
            crate::tags::empty_metric_tag_set()
        } else {
            crate::tags::metric_tag_set(tags)
        }
    }

    /// Registers the metric as a [`Counter`] with the configured [`unit`](Self::unit) (default
    /// [`Unit::Count`]).
    #[track_caller]
    pub fn counter(self) -> Counter {
        let tags = Self::finish_tags(self.tags);
        self.registry.register_counter_inner(
            self.name,
            self.aggregation,
            self.unit,
            self.sparsity,
            tags,
        )
    }

    /// Registers the metric as a [`Gauge`] with the given display unit.
    #[track_caller]
    pub fn gauge(self, unit: Unit) -> Gauge {
        let tags = Self::finish_tags(self.tags);
        self.registry
            .register_gauge_inner(self.name, self.aggregation, unit, self.sparsity, tags)
    }

    /// Registers the metric as a [`Summary`] with the given display unit, at the configured
    /// [`scale`](Self::scale) (default `1.0`).
    #[track_caller]
    pub fn summary(self, display_unit: Unit) -> Summary {
        let tags = Self::finish_tags(self.tags);
        self.registry.register_summary_inner(
            self.name,
            self.aggregation,
            display_unit,
            self.sparsity,
            self.scale,
            tags,
        )
    }

    /// Registers the metric as a [`BoolCounter`].
    #[track_caller]
    pub fn bool(self) -> BoolCounter {
        let tags = Self::finish_tags(self.tags);
        self.registry
            .register_bool_inner(self.name, self.aggregation, self.sparsity, tags)
    }

    /// Registers the metric as a callback list (see [`Registry::register_list_callback`]).
    #[track_caller]
    pub fn list_callback<V, F>(self, unit: Unit, callback: F)
    where
        V: crate::backend::CallbackValue,
        F: FnMut() -> V + 'static + Send,
    {
        let tags = Self::finish_tags(self.tags);
        self.registry.register_list_callback_inner(
            self.name,
            self.aggregation,
            unit,
            self.sparsity,
            tags,
            callback,
        );
    }
}

/// A read-only description of one registered metric, produced by [`Registry::descriptors`].
///
/// This is the static shape a backend sees for the metric — its emitted name (with any child
/// prefix already applied), aggregation/variant string, [`MetricKind`], display [`Unit`], and
/// [`Sparsity`] — captured without recording or draining any value. It exists so the set of
/// registered metrics can be inspected (documented, validated, exported as a schema, or used to
/// drive code generation) independently of any reported value.
///
/// `#[non_exhaustive]`: further shape metadata (e.g. structured variant data) may be added, so this
/// is constructed only by the crate and matched with a `..` rest pattern by consumers.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct MetricDescriptor {
    /// The emitted metric name (e.g. `rx.data`), including any [`child`](Registry::child) prefix.
    pub name: Arc<str>,
    /// The aggregation/variant string, if any (e.g. `Variant|ect0`, `Task|foo`).
    pub aggregation: Option<Arc<str>>,
    /// The kind of metric a backend would record it as.
    pub kind: MetricKind,
    /// The display unit.
    pub unit: Unit,
    /// The per-metric sparse policy.
    pub sparsity: Sparsity,
    /// The metric's [metadata tags](crate::MetricTags) — the same `(key, value)` pairs a backend
    /// sees on [`MetricInfo::tags`](crate::MetricInfo), so a catalog/validation consumer sees what a
    /// filtering policy would match on. Empty for an untagged metric.
    pub tags: crate::MetricTagSet,
}

impl MetricDescriptor {
    /// The variant name for a nominal metric, i.e. the part after `Variant|` in the aggregation
    /// (`Variant|ect0` yields `ect0`), or `None` for a non-nominal metric or a differently-tagged
    /// aggregation (`Task|`, `Runtime|`).
    ///
    /// Nominal metrics share one `name` across several variants; this recovers the per-variant
    /// discriminator a structured catalog keys on.
    pub fn variant(&self) -> Option<&str> {
        self.aggregation
            .as_deref()
            .and_then(|a| a.strip_prefix("Variant|"))
    }
}

/// This represents a single entry in our emitted service log, with optional aggregation along the
/// two class/instance dimensions.
#[derive(PartialEq, Eq, Hash, PartialOrd, Ord)]
struct MetricKey {
    name: Arc<str>,
    aggregation: Option<Arc<str>>,
}

/// A registered metric: its accumulating value plus per-metric metadata applied at report time.
/// The `sparsity` and `tags` are fixed at first registration (subsequent registrations under the
/// same key dedup to the existing entry and keep the original metadata).
///
/// `tags` are the metric's [metadata tags](crate::MetricTags) — `(key, value)` pairs a filtering
/// policy matches on — and are deliberately **not** part of [`MetricKey`]: they do not affect dedup
/// identity, storage, or report ordering. Empty (a zero-length shared slice) for the common
/// untagged metric.
struct MetricEntry {
    value: MetricValue,
    sparsity: Sparsity,
    tags: crate::MetricTagSet,
}

/// Asserts a repeat registration under an existing key requested the same [`Sparsity`] and tag set,
/// so a metric can't be silently registered with two conflicting sets of metadata.
#[track_caller]
fn assert_metadata(
    entry: &MetricEntry,
    sparsity: Sparsity,
    tags: &crate::MetricTagSet,
    metric: &Arc<str>,
    aggregation: &Option<Arc<str>>,
) {
    assert_eq!(
        entry.sparsity, sparsity,
        "metric name={metric:?}, aggregation={aggregation:?} already registered with different sparsity"
    );
    assert_eq!(
        &entry.tags, tags,
        "metric name={metric:?}, aggregation={aggregation:?} already registered with different tags"
    );
}

/// This represents metric state. Note that a single metric may collect many different values
/// between emissions; so a "value" represents potentially multiple recorded points.
///
/// (FIXME: rename this to something else?)
enum MetricValue {
    Counter(Counter),
    Gauge(Gauge),
    Summary(Summary),
    BoolCounter(BoolCounter),
    ValueList(Box<dyn crate::callback::ValueList + Send>),
}

#[cfg(test)]
mod test {
    use super::*;

    /// A native gauge: repeat registration dedups to the same handle (last-write-wins), gauges are
    /// live (not drained at report), and the per-metric sparsity governs zero emission — a dense
    /// gauge emits its zero under a non-sparse report while a plain (Inherit) one is dropped.
    #[test]
    fn native_gauge_dedups_and_reports() {
        let registry = Registry::new();

        // Repeat registration returns a handle to the same underlying atomic.
        let g1 = registry.register_gauge("depth".into(), None, Unit::Count);
        let g2 = registry.register_gauge("depth".into(), None, Unit::Count);
        g1.set(3);
        g2.set(9); // last write wins
        assert_eq!(registry.take_current_metrics_line(), "depth=9");

        // Gauges are live readings, not drained: the value persists until the next `set`, so we set
        // it to zero explicitly rather than expecting the prior report to have zeroed it.
        g2.set(0);

        // A dense gauge emits its zero even under a non-sparse report; the plain (Inherit) `depth`
        // zero is dropped.
        let dense = registry.metric("mem").dense().gauge(Unit::Count);
        dense.set(0);
        assert_eq!(
            registry
                .try_take_current_metrics_line_sparse(false)
                .unwrap(),
            "mem=0"
        );
    }

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

    /// The default drain reporter runs `absorb` in the background so a registry with no exporting
    /// reporter still recycles its pool — and its handle stops+joins the thread on drop. Verifies
    /// the drainer runs without losing data (absorb is additive, so the eventual report still sees
    /// every increment) and that dropping the handle tears the thread down cleanly (a leaked or
    /// deadlocked thread would hang the join here).
    #[test]
    fn default_drain_reporter_drains_and_stops() {
        let registry = Registry::new();
        let counter = registry.register_counter("a".into(), None);
        let reporter = registry.spawn_default_drain_reporter(Duration::from_millis(5));
        counter.increment(7);
        // Give the background drainer several intervals to fold the buffered events.
        thread::sleep(Duration::from_millis(30));
        counter.increment(4);
        thread::sleep(Duration::from_millis(30));
        // Stop the drainer: this joins the thread, so a leaked/deadlocked drainer hangs the test.
        drop(reporter);
        // absorb is additive, so the report still observes every increment (7 + 4) despite the
        // background folds in between — the drainer recycled the pool without losing data.
        assert_eq!(registry.take_current_metrics_line(), "a=11");
    }

    /// `absorb` folds buffered events into the aggregate without draining, so intervening absorbs
    /// don't change what the eventual report observes: the value spans every increment since the
    /// last report, regardless of how many times it was compacted in between.
    #[test]
    fn absorb_is_additive_and_non_draining() {
        let registry = Registry::new();
        let counter = registry.register_counter("a".into(), None);
        let summary = registry.register_summary("h".into(), None, Unit::Count);

        // Record, compact, record more, compact again -- all before a single report.
        counter.increment(3);
        summary.record_value(10);
        registry.absorb();
        counter.increment(4);
        summary.record_value(20);
        registry.absorb();
        // A redundant absorb with nothing new buffered is a harmless no-op.
        registry.absorb();

        // The report sees the full span across both intervals (3 + 4 = 7, two histogram samples),
        // identical to never having absorbed.
        let line = registry.take_current_metrics_line();
        assert_eq!(line, "a=7,h=10*1+20*1");

        // The report drained; absorbing again finds nothing and the next report is zeroed.
        registry.absorb();
        assert_eq!(registry.take_current_metrics_line(), "a=0,h=0");
    }

    /// `absorb` on a closed registry is a no-op and does not panic.
    #[test]
    fn absorb_closed_registry_is_noop() {
        let registry = Registry::new();
        registry.register_counter("a".into(), None).increment(1);
        registry.close();
        registry.absorb();
    }

    /// With `include_sparse`, drained counters and histograms still emit their zero.
    #[test]
    fn sparse_includes_zeroed_metrics() {
        let registry = Registry::new();
        registry.register_counter("a".into(), None);
        registry.register_summary("h".into(), None, Unit::Byte);

        let line = registry.try_take_current_metrics_line_sparse(true).unwrap();
        assert_eq!(line, "a=0,h=0 B");
    }

    /// The per-metric [`Sparsity`] override pins a metric's zero-emission independent of the report
    /// policy: `AlwaysDense` emits a zero even under a sparse report, `AlwaysSparse` drops a zero
    /// even under a dense one, and `Inherit` follows the report.
    #[test]
    fn per_metric_sparsity_overrides_report_policy() {
        let registry = Registry::new();
        registry.metric("dense").dense().counter();
        registry.metric("sparse").sparse().counter();
        registry.register_counter("inherit".into(), None);

        // Sparse report: only the dense metric's zero survives.
        assert_eq!(
            registry
                .try_take_current_metrics_line_sparse(false)
                .unwrap(),
            "dense=0"
        );

        // Dense report: the dense and inherit zeros survive; the sparse override still drops.
        assert_eq!(
            registry.try_take_current_metrics_line_sparse(true).unwrap(),
            "dense=0,inherit=0"
        );
    }

    /// Registering the same metric key twice with conflicting sparsity is a programming error.
    #[test]
    #[should_panic(expected = "different sparsity")]
    fn conflicting_sparsity_panics() {
        let registry = Registry::new();
        registry.metric("a").dense().counter();
        registry.metric("a").sparse().counter();
    }

    /// A child registry prefixes the emitted name of every metric registered through it, across
    /// every registration path (plain register, the `metric(...)` builder, callbacks), while
    /// leaving the aggregation/variant dimension untouched. Metrics registered on the parent keep
    /// their bare names, and both report together because the child shares the parent's storage.
    #[test]
    fn child_prefixes_metric_names() {
        let parent = Registry::new();
        let child = parent.child("myapp");

        parent.register_counter("rx.data".into(), None).increment(1);
        child.register_counter("rx.data".into(), None).increment(2);
        child
            .register_counter("rx.ecn".into(), Some("Variant|ect0".into()))
            .increment(3);
        child.metric("built").counter().increment(4);
        child.register_list_callback("cb".into(), None, Unit::Count, || 5usize);

        let line = parent.take_current_metrics_line();
        assert_eq!(
            line,
            "myapp.built=4,myapp.cb=5,myapp.rx.data=2,myapp.rx.ecn=3 Variant|ect0,rx.data=1"
        );
    }

    /// Children compose: `child("a").child("b")` prefixes with `a.b`. An empty prefix is a no-op.
    #[test]
    fn child_prefixes_compose() {
        let registry = Registry::new();
        let nested = registry.child("a").child("b");
        nested.register_counter("m".into(), None).increment(1);
        // An empty prefix yields an equivalent handle (no extra segment).
        registry
            .child("")
            .register_counter("n".into(), None)
            .increment(2);

        assert_eq!(registry.take_current_metrics_line(), "a.b.m=1,n=2");
    }

    /// `descriptors` snapshots the shape of every registered metric — name (prefix applied),
    /// aggregation, kind, unit, sparsity — in stable `(name, aggregation)` order, without recording
    /// or draining any value and without invoking callbacks.
    #[test]
    fn descriptors_snapshots_every_metric_shape() {
        let registry = Registry::new();

        let counter = registry.register_counter("rx.data".into(), None);
        registry.register_counter("rx.ecn".into(), Some("Variant|ect0".into()));
        registry.register_counter("rx.ecn".into(), Some("Variant|ect1".into()));
        registry.register_bool("connect".into(), None);
        registry.register_summary("rx.decrypt_time".into(), None, Unit::Microsecond);

        // Snapshotting a descriptor must never invoke the callback; a report is what evaluates it.
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_cb = calls.clone();
        registry
            .metric("q.depth")
            .sparse()
            .list_callback(Unit::Byte, move || {
                calls_cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                0i64
            });

        // Record into a metric first: descriptors describe shape, not value, and must not drain it.
        counter.increment(42);

        let descriptors = registry.descriptors();
        let shapes: Vec<_> = descriptors
            .iter()
            .map(|d| (&*d.name, d.variant(), d.kind, d.unit, d.sparsity))
            .collect();

        assert_eq!(
            shapes,
            vec![
                (
                    "connect",
                    None,
                    MetricKind::BoolCounter,
                    Unit::Count,
                    Sparsity::Inherit
                ),
                (
                    "q.depth",
                    None,
                    MetricKind::CallbackScalar,
                    Unit::Byte,
                    Sparsity::AlwaysSparse
                ),
                (
                    "rx.data",
                    None,
                    MetricKind::Counter,
                    Unit::Count,
                    Sparsity::Inherit
                ),
                (
                    "rx.decrypt_time",
                    None,
                    MetricKind::Histogram,
                    Unit::Microsecond,
                    Sparsity::Inherit
                ),
                (
                    "rx.ecn",
                    Some("ect0"),
                    MetricKind::Counter,
                    Unit::Count,
                    Sparsity::Inherit
                ),
                (
                    "rx.ecn",
                    Some("ect1"),
                    MetricKind::Counter,
                    Unit::Count,
                    Sparsity::Inherit
                ),
            ]
        );

        // Snapshotting the shape never evaluated the callback.
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);

        // Introspection is non-destructive: rx.data retains its 42 in a subsequent sparse report.
        // (The all-zero bool and the sparse zero gauge are dropped even under a sparse report.)
        assert_eq!(
            registry.take_current_metrics_line(),
            "rx.data=42,rx.decrypt_time=0 us,rx.ecn=0 Variant|ect0,rx.ecn=0 Variant|ect1"
        );
    }

    /// A counter's display [`Unit`] flows through to both the reported querylog line and its
    /// descriptor, while a plain (default `Unit::Count`) counter stays byte-identical.
    #[test]
    fn counter_unit_surfaces_in_report_and_descriptor() {
        let registry = Registry::new();
        let count = registry.register_counter("rx.pkts".into(), None);
        let bytes = registry.register_counter_with_unit("rx.bytes".into(), None, Unit::Byte);

        count.increment(3);
        bytes.increment(1500);

        // Descriptors report each counter's real unit (not a hardcoded `Count`).
        let shapes: Vec<_> = registry
            .descriptors()
            .iter()
            .map(|d| (d.name.to_string(), d.kind, d.unit))
            .collect();
        assert_eq!(
            shapes,
            vec![
                ("rx.bytes".to_string(), MetricKind::Counter, Unit::Byte),
                ("rx.pkts".to_string(), MetricKind::Counter, Unit::Count),
            ]
        );

        // The querylog line renders the byte suffix but leaves the plain counter bare.
        assert_eq!(
            registry.take_current_metrics_line(),
            "rx.bytes=1500 B,rx.pkts=3"
        );
    }

    /// A metric's [metadata tags](crate::MetricTags) set via the builder flow through to the
    /// `MetricInfo` a backend sees at report time, sorted by key — but are **not** rendered on the
    /// querylog line (they are metadata, distinct from the emitted aggregation), so output stays
    /// byte-identical to an untagged metric.
    #[test]
    fn metric_tags_reach_backend_but_are_not_emitted() {
        use crate::backend::{Backend, CallbackValue, Histogram, MetricInfo, ReportOptions};

        // Captures the tags observed for each recorded counter name.
        #[derive(Default)]
        struct TagCapture {
            seen: Vec<(String, Vec<(String, String)>)>,
        }
        impl Backend for TagCapture {
            fn record_counter(&mut self, info: &MetricInfo<'_>, _value: u64) {
                let tags = info
                    .tags
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                self.seen.push((info.name.to_string(), tags));
            }
            fn record_gauge(&mut self, _: &MetricInfo<'_>, _: i64) {}
            fn record_bool(&mut self, _: &MetricInfo<'_>, _: u64, _: u64) {}
            fn record_histogram(&mut self, _: &MetricInfo<'_>, _: Histogram<'_>) {}
            fn record_callback(&mut self, _: &MetricInfo<'_>, _: &[&dyn CallbackValue]) {}
        }

        let registry = Registry::new();
        let tagged = registry
            .metric("rx.data")
            .tag("level", "debug")
            .tag("component", "tx")
            .counter();
        let untagged = registry.register_counter("rx.other".into(), None);
        tagged.increment(5);
        untagged.increment(9);

        let mut capture = TagCapture::default();
        registry.report_with(&ReportOptions::new(false), &mut capture);

        // The tagged metric's tags reach the backend, sorted by key; the untagged one is empty.
        assert_eq!(
            capture.seen,
            vec![
                (
                    "rx.data".to_string(),
                    vec![
                        ("component".to_string(), "tx".to_string()),
                        ("level".to_string(), "debug".to_string()),
                    ]
                ),
                ("rx.other".to_string(), vec![]),
            ]
        );

        // Tags do not appear on the querylog line — byte-identical to plain counters.
        let tagged2 = registry
            .metric("rx.data")
            .tag("level", "debug")
            .tag("component", "tx")
            .counter();
        tagged2.increment(5);
        registry
            .register_counter("rx.other".into(), None)
            .increment(9);
        assert_eq!(registry.take_current_metrics_line(), "rx.data=5,rx.other=9");
    }

    /// Registering the same metric key twice with a conflicting tag set is a programming error,
    /// mirroring the sparsity/unit guards.
    #[test]
    #[should_panic(expected = "different tags")]
    fn conflicting_tags_panics() {
        let registry = Registry::new();
        registry.metric("a").tag("level", "debug").counter();
        registry.metric("a").tag("level", "info").counter();
    }

    /// A child registry's prefix is applied to the descriptor name, exactly as it is to the emitted
    /// name — so a generated catalog matches what the backend emits.
    #[test]
    fn descriptors_apply_child_prefix() {
        let parent = Registry::new();
        parent.register_counter("rx.data".into(), None);
        parent
            .child("myapp")
            .register_counter("tx.data".into(), None);

        let names: Vec<_> = parent
            .descriptors()
            .iter()
            .map(|d| d.name.to_string())
            .collect();
        assert_eq!(names, vec!["myapp.tx.data", "rx.data"]);
    }

    /// The builder's `aggregation` flows through to the reported line.
    #[test]
    fn builder_sets_aggregation() {
        let registry = Registry::new();
        registry
            .metric("rx.ecn")
            .aggregation("Variant|ect0")
            .dense()
            .counter();
        assert_eq!(
            registry
                .try_take_current_metrics_line_sparse(false)
                .unwrap(),
            "rx.ecn=0 Variant|ect0"
        );
    }
}
