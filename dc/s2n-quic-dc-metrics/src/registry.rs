// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

use crate::{
    backend::{Backend, MetricInfo, MetricKind, QuerylogBackend, ReportOptions, Sparsity},
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
    /// Namespace prepended to the name of every metric registered through this handle.
    ///
    /// This is a per-handle property, *not* part of the shared [`RegistryInner`]: cloning a
    /// `Registry` keeps the same prefix, but [`child`](Registry::child) produces a handle over the
    /// same underlying storage with an extended prefix. It affects only the metric *name* recorded
    /// into storage (and therefore what backends emit); the aggregation/variant dimension is left
    /// untouched.
    prefix: Option<Arc<str>>,
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
            let sparsity = entry.sparsity;
            let info = |unit, kind| {
                let mut info = MetricInfo::new(name, aggregation, unit, kind);
                info.sparsity = sparsity;
                info
            };
            match &mut entry.value {
                MetricValue::Counter(c) => {
                    c.report(&info(Unit::Count, MetricKind::Counter), backend);
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
                    MetricValue::Counter(_) => (MetricKind::Counter, Unit::Count),
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
                }
            })
            .collect()
    }

    /// Folds any buffered per-CPU pages into the aggregate without draining or reporting. See
    /// [`Registry::absorb`].
    pub fn absorb(&self) {
        if !self.is_open {
            return;
        }

        self.counters.steal_pages();
        self.histograms.steal_pages();
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
            Sparsity::Inherit,
        )
    }

    #[track_caller]
    fn register_counter_inner(
        &self,
        metric: Arc<str>,
        aggregation: Option<Arc<str>>,
        sparsity: Sparsity,
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
                value: MetricValue::Counter(Counter::new(inner.counters.clone())),
                sparsity,
            });

        assert_sparsity(entry, sparsity, &metric, &aggregation);
        if let MetricValue::Counter(c) = &entry.value {
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
        self.register_summary_inner(
            metric.into(),
            aggregation.map(Into::into),
            display_unit,
            Sparsity::Inherit,
        )
    }

    #[track_caller]
    fn register_summary_inner(
        &self,
        metric: Arc<str>,
        aggregation: Option<Arc<str>>,
        display_unit: Unit,
        sparsity: Sparsity,
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
                value: MetricValue::Summary(Summary::new(inner.histograms.clone(), display_unit)),
                sparsity,
            });

        assert_sparsity(entry, sparsity, &metric, &aggregation);
        if let MetricValue::Summary(s) = &entry.value {
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
        )
    }

    #[track_caller]
    fn register_bool_inner(
        &self,
        metric: Arc<str>,
        aggregation: Option<Arc<str>>,
        sparsity: Sparsity,
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
            });

        assert_sparsity(entry, sparsity, &metric, &aggregation);
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
                });
            }
            std::collections::btree_map::Entry::Occupied(mut o) => {
                assert_eq!(
                    o.get().sparsity,
                    sparsity,
                    "Callback metric name={metric:?}, aggregation={aggregation:?} already registered with different sparsity"
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
}

impl<'a> MetricBuilder<'a> {
    /// Sets the aggregation/variant string (e.g. `Variant|ect0`).
    pub fn aggregation(mut self, aggregation: impl Into<Arc<str>>) -> Self {
        self.aggregation = Some(aggregation.into());
        self
    }

    /// Sets the per-metric [`Sparsity`] policy. Defaults to [`Sparsity::Inherit`].
    pub fn sparsity(mut self, sparsity: Sparsity) -> Self {
        self.sparsity = sparsity;
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

    /// Registers the metric as a [`Counter`].
    #[track_caller]
    pub fn counter(self) -> Counter {
        self.registry
            .register_counter_inner(self.name, self.aggregation, self.sparsity)
    }

    /// Registers the metric as a [`Summary`] with the given display unit.
    #[track_caller]
    pub fn summary(self, display_unit: Unit) -> Summary {
        self.registry.register_summary_inner(
            self.name,
            self.aggregation,
            display_unit,
            self.sparsity,
        )
    }

    /// Registers the metric as a [`BoolCounter`].
    #[track_caller]
    pub fn bool(self) -> BoolCounter {
        self.registry
            .register_bool_inner(self.name, self.aggregation, self.sparsity)
    }

    /// Registers the metric as a callback list (see [`Registry::register_list_callback`]).
    #[track_caller]
    pub fn list_callback<V, F>(self, unit: Unit, callback: F)
    where
        V: crate::backend::CallbackValue,
        F: FnMut() -> V + 'static + Send,
    {
        self.registry.register_list_callback_inner(
            self.name,
            self.aggregation,
            unit,
            self.sparsity,
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

/// A registered metric: its accumulating value plus the per-metric sparse policy applied to it at
/// report time. The `sparsity` is fixed at first registration (subsequent registrations under the
/// same key dedup to the existing entry and keep the original policy).
struct MetricEntry {
    value: MetricValue,
    sparsity: Sparsity,
}

/// Asserts a repeat registration under an existing key requested the same [`Sparsity`], so a metric
/// can't be silently registered with two conflicting policies.
#[track_caller]
fn assert_sparsity(
    entry: &MetricEntry,
    sparsity: Sparsity,
    metric: &Arc<str>,
    aggregation: &Option<Arc<str>>,
) {
    assert_eq!(
        entry.sparsity, sparsity,
        "metric name={metric:?}, aggregation={aggregation:?} already registered with different sparsity"
    );
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
