// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Pluggable metric emission backends.
//!
//! A [`Backend`] receives **native Rust values** (a `u64` counter, an `i64` gauge, a borrowed
//! [`Histogram`] with real buckets) at report time and emits them however it likes -- the querylog
//! string, statsd, OpenTelemetry, Parquet, etc. This replaces the previous design where the
//! registry serialized everything to a single string that each consumer then re-parsed.
//!
//! [`Registry::report`](crate::Registry::report) drives a backend: it drains every registered
//! metric exactly once (the take is destructive) and visits each with its native value. Because the
//! take can only happen once, multiple backends are fed from a single snapshot via composition --
//! `impl Backend for (A, B)` fans out to both. Backends can be toggled at runtime with [`Toggle`].

use crate::{summary::bucket, Unit};
use std::{
    fmt::Write as _,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

mod querylog;

pub use querylog::QuerylogBackend;

mod sealed {
    pub trait Sealed {}
}

/// A value produced by a registered callback.
///
/// Callbacks need to serve two consumers from a single (potentially stateful) invocation: the
/// querylog string and structured backends that want a native number. `CallbackValue` exposes both
/// -- [`as_f64`](CallbackValue::as_f64) for the native value and
/// [`fmt_value`](CallbackValue::fmt_value) for the exact display form. Keeping the original type's
/// `Display` matters: e.g. an `f32` must not be widened to `f64` before formatting, since that
/// changes the printed digits.
///
/// This trait is sealed: it is implemented for all numeric primitive types and cannot be
/// implemented outside this crate. That guarantees `as_f64` and `fmt_value` stay consistent (an
/// external impl that disagreed would silently corrupt output).
pub trait CallbackValue: sealed::Sealed + 'static + Send {
    /// The native numeric value, used by structured backends.
    fn as_f64(&self) -> f64;

    /// Writes the exact display form (the bare number, using the value's own `Display`) into `out`.
    fn fmt_value(&self, out: &mut String);
}

macro_rules! impl_callback_value {
    ($($ty:ty),* $(,)?) => {
        $(
            impl sealed::Sealed for $ty {}

            impl CallbackValue for $ty {
                #[inline]
                fn as_f64(&self) -> f64 {
                    *self as f64
                }

                #[inline]
                fn fmt_value(&self, out: &mut String) {
                    write!(out, "{self}").unwrap();
                }
            }
        )*
    };
}

impl_callback_value!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);

/// Per-report knobs, threaded into the report pass.
///
/// `#[non_exhaustive]` so new emit-policy knobs (sampling rate, name filters, timestamp source,
/// ...) can be added later without changing any backend's method signatures.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct ReportOptions {
    /// Hint that backends should emit metrics which currently have no recorded value (i.e. zeros).
    ///
    /// Backends decide how to honor this. The querylog backend drops zeros (matching historical
    /// behavior) unless this is set; a parquet/otel backend may always keep them for gap-free time
    /// series. See also [`MetricInfo::zero_suppressed`].
    pub include_sparse: bool,
}

impl ReportOptions {
    /// Options with `include_sparse` set to the given value.
    pub fn new(include_sparse: bool) -> Self {
        Self { include_sparse }
    }
}

/// The kind of a metric being reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricKind {
    Counter,
    Gauge,
    BoolCounter,
    Histogram,
    CallbackScalar,
}

/// Metadata describing a single metric, passed by shared reference to every `record_*` call.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct MetricInfo<'a> {
    /// The metric name (e.g. `rx.data`).
    pub name: &'a str,
    /// The aggregation/variant string, if any (e.g. `Variant|prefix-x`, `Task|foo`, `Runtime|bar`).
    ///
    /// This is the raw historical convention string; a later phase replaces it with structured
    /// variant data.
    pub aggregation: Option<&'a str>,
    /// The display unit for this metric.
    pub unit: Unit,
    /// The kind of metric.
    pub kind: MetricKind,
    /// When `true`, a zero value for this metric should be suppressed by default (gauges, queue
    /// depth). This replaces the previous `NonZeroDisplay` empty-string trick with an explicit bit,
    /// so native backends can still choose to emit the real zero.
    pub zero_suppressed: bool,
}

impl<'a> MetricInfo<'a> {
    /// Constructs a `MetricInfo`, defaulting `zero_suppressed` to `false`.
    pub fn new(name: &'a str, aggregation: Option<&'a str>, unit: Unit, kind: MetricKind) -> Self {
        Self {
            name,
            aggregation,
            unit,
            kind,
            zero_suppressed: false,
        }
    }
}

/// A borrowed view over a pre-aggregated histogram's bucket array.
///
/// The view borrows the live bucket storage (valid only for the duration of the
/// [`Backend::record_histogram`] call) and exposes both native bucket access and the
/// quantile/summary helpers backends commonly want, so no backend is forced through a string.
#[derive(Clone, Copy)]
pub struct Histogram<'a> {
    buckets: &'a [u64],
    config: &'a bucket::Config,
    unit: Unit,
}

impl<'a> Histogram<'a> {
    pub(crate) fn new(buckets: &'a [u64], config: &'a bucket::Config, unit: Unit) -> Self {
        Self {
            buckets,
            config,
            unit,
        }
    }

    /// The display unit for this histogram.
    pub fn unit(&self) -> Unit {
        self.unit
    }

    /// The total number of recorded samples.
    pub fn count(&self) -> u64 {
        self.buckets.iter().sum()
    }

    /// Iterates the non-empty buckets as `(representative_value, count)` pairs.
    ///
    /// The representative value is the midpoint of the bucket's value range, matching the value
    /// historically emitted in the querylog `value*count` pairs.
    pub fn buckets(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.buckets
            .iter()
            .enumerate()
            .filter(|(_, count)| **count != 0)
            .map(move |(idx, count)| (self.representative_value(idx), *count))
    }

    /// The representative (midpoint) value of bucket `idx`.
    pub(crate) fn representative_value(&self, idx: usize) -> u64 {
        self.config
            .index_to_lower_bound(idx)
            .midpoint(self.config.index_to_upper_bound(idx))
    }

    /// The lowest non-empty bucket's representative (midpoint) value, or 0 if empty.
    ///
    /// Note this is the bucket midpoint, not the true smallest observed sample — the histogram
    /// only retains bucketed counts, not individual values.
    pub fn min(&self) -> u64 {
        self.buckets
            .iter()
            .position(|count| *count != 0)
            .map(|idx| self.representative_value(idx))
            .unwrap_or(0)
    }

    /// The highest non-empty bucket's representative (midpoint) value, or 0 if empty.
    ///
    /// Note this is the bucket midpoint, not the true largest observed sample.
    pub fn max(&self) -> u64 {
        self.buckets
            .iter()
            .rposition(|count| *count != 0)
            .map(|idx| self.representative_value(idx))
            .unwrap_or(0)
    }

    /// The representative value at the requested quantile in `0.0..=1.0`.
    ///
    /// Uses a `ceil(q * count)` cumulative-walk. `quantile(0.0)` returns the minimum observed
    /// bucket (first non-empty) and `quantile(1.0)` the maximum.
    pub fn quantile(&self, q: f64) -> u64 {
        let total = self.count();
        if total == 0 {
            return 0;
        }
        // Target at least the first sample, so q=0.0 (ceil -> 0) resolves to the minimum observed
        // bucket rather than bucket 0. Empty buckets are skipped so a leading run of zeros never
        // satisfies the threshold.
        let target = ((q * total as f64).ceil() as u64).max(1);
        let mut cumulative = 0u64;
        for (idx, count) in self.buckets.iter().enumerate() {
            if *count == 0 {
                continue;
            }
            cumulative += *count;
            if cumulative >= target {
                return self.representative_value(idx);
            }
        }
        self.max()
    }

    /// Returns `(count, min, p50, p99, max)`.
    pub fn summarize(&self) -> (u64, u64, u64, u64, u64) {
        (
            self.count(),
            self.min(),
            self.quantile(0.5),
            self.quantile(0.99),
            self.max(),
        )
    }

    /// Writes the querylog quantile-walk representation (the `value*count+value*count...` form,
    /// without the trailing unit suffix) into `out`. `total_count` must equal `self.count()`.
    ///
    /// This reproduces the historical summary formatter: at each of a fixed set of quantile
    /// boundaries it emits the count accumulated since the last boundary at the current bucket's
    /// midpoint value (converted per unit).
    pub(crate) fn fmt_querylog_buckets(&self, out: &mut String, total_count: u64) {
        use crate::summary::logging_util_integer_to_float;
        use std::time::Duration;

        let quantiles = [
            0.0f64, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 0.95, 0.99, 0.999, 1.0,
        ]
        .map(|q| (q * total_count as f64).ceil() as u64);
        let mut quantile_idx = 0;

        // Prefix sum up to the current bucket.
        let mut partial_count = 0;
        // Prefix sum excluding already reported counts (i.e., those we've written to `out`).
        let mut since_last_write = 0;
        let mut first = true;
        for (idx, bucket) in self.buckets.iter().enumerate() {
            partial_count += *bucket;
            since_last_write += *bucket;

            // If this bucket hits the next quantile, we write out the current count.
            //
            // Can't panic due to the break below on partial_count == total_count.
            if partial_count >= quantiles[quantile_idx] {
                quantile_idx += 1;

                if since_last_write != 0 {
                    if !first {
                        out.push('+');
                    }
                    first = false;

                    // Use the midpoint of the bucket. We don't know where the actual value was and
                    // this gives a balance between overestimating and under estimating.
                    let new_value = self.representative_value(idx);
                    let count = since_last_write;
                    since_last_write = 0;

                    let formatted_value = match self.unit {
                        Unit::Count | Unit::Byte | Unit::Percent => new_value,
                        Unit::Microsecond => Duration::from_nanos(new_value).as_micros() as u64,
                        Unit::Second => Duration::from_nanos(new_value).as_secs(),
                    };

                    match self.unit {
                        Unit::Percent => {
                            let formatted_value = logging_util_integer_to_float(formatted_value);
                            write!(out, "{formatted_value:.3}*{count}").unwrap();
                        }
                        _ => write!(out, "{formatted_value}*{count}").unwrap(),
                    }
                }
            }

            if partial_count == total_count {
                break;
            }
        }
    }
}

impl std::fmt::Debug for Histogram<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (count, min, p50, p99, max) = self.summarize();
        f.debug_struct("Histogram")
            .field("unit", &self.unit)
            .field("count", &count)
            .field("min", &min)
            .field("p50", &p50)
            .field("p99", &p99)
            .field("max", &max)
            .finish()
    }
}

/// A sink for native metric values at report time.
///
/// `Registry::report` calls `report_start`, then one `record_*` per registered metric, then
/// `report_end`. All methods take `&mut self` and use concrete argument types, so the trait is
/// object-safe (`Box<dyn Backend>`).
///
/// # Reuse across reports
///
/// A `Backend` is intended to be **long-lived and reused across many report passes** rather than
/// constructed per report. `report_start` resets any per-report state while retaining allocated
/// capacity (buffers, maps), so after a few intervals the working set settles and steady-state
/// reporting performs no allocation. Implementations must therefore clear, not reallocate, in
/// `report_start`.
///
/// # Re-entrancy
///
/// `Registry::report` holds the registry's internal lock for the duration of the report pass, so a
/// `Backend` method must **not** call back into the same `Registry` (e.g. `register_*`, `report`,
/// `is_open`). Doing so deadlocks. Record into the backend's own state and do any registry
/// interaction outside the report pass.
pub trait Backend {
    /// Called once at the start of a report pass, before any metric is visited.
    ///
    /// Implementations should reset per-report state here while retaining capacity, so the backend
    /// can be reused across reports without reallocating (see the trait-level note on reuse).
    fn report_start(&mut self, options: &ReportOptions) {
        let _ = options;
    }

    /// A monotonic counter's accumulated value since the last report.
    fn record_counter(&mut self, info: &MetricInfo<'_>, value: u64);

    /// An instantaneous signed gauge value.
    ///
    /// Note: in the current code no metric reports through this method — gauges are registered as
    /// zero-suppressed callbacks and arrive via [`record_callback`](Backend::record_callback). It
    /// exists for backends and for a future first-class gauge metric kind.
    fn record_gauge(&mut self, info: &MetricInfo<'_>, value: i64);

    /// A boolean counter's true/false counts since the last report.
    fn record_bool(&mut self, info: &MetricInfo<'_>, true_count: u64, false_count: u64);

    /// A pre-aggregated distribution. `hist` borrows the live bucket array and is valid only for the
    /// duration of this call.
    fn record_histogram(&mut self, info: &MetricInfo<'_>, hist: Histogram<'_>);

    /// The value(s) produced by a registered callback metric (e.g. a gauge or runtime stat).
    ///
    /// A single name may have multiple callbacks registered under it (e.g. one per worker); all of
    /// their values are passed together as one logical metric. Each value exposes both its native
    /// number ([`CallbackValue::as_f64`]) and its exact display form
    /// ([`CallbackValue::fmt_value`]).
    ///
    /// The `values` slice (and the references in it) borrows a temporary built for this call and is
    /// valid only for its duration — backends must consume it here, not retain it.
    fn record_callback(&mut self, info: &MetricInfo<'_>, values: &[&dyn CallbackValue]);

    /// Called once after all metrics are visited. Backends flush batched output here.
    fn report_end(&mut self) {}
}

/// No-op backend.
impl Backend for () {
    fn record_counter(&mut self, _info: &MetricInfo<'_>, _value: u64) {}
    fn record_gauge(&mut self, _info: &MetricInfo<'_>, _value: i64) {}
    fn record_bool(&mut self, _info: &MetricInfo<'_>, _true_count: u64, _false_count: u64) {}
    fn record_histogram(&mut self, _info: &MetricInfo<'_>, _hist: Histogram<'_>) {}
    fn record_callback(&mut self, _info: &MetricInfo<'_>, _values: &[&dyn CallbackValue]) {}
}

/// Composition: feed both backends from a single snapshot. Nest for more, e.g. `(A, (B, C))`.
impl<A: Backend, B: Backend> Backend for (A, B) {
    fn report_start(&mut self, options: &ReportOptions) {
        self.0.report_start(options);
        self.1.report_start(options);
    }
    fn record_counter(&mut self, info: &MetricInfo<'_>, value: u64) {
        self.0.record_counter(info, value);
        self.1.record_counter(info, value);
    }
    fn record_gauge(&mut self, info: &MetricInfo<'_>, value: i64) {
        self.0.record_gauge(info, value);
        self.1.record_gauge(info, value);
    }
    fn record_bool(&mut self, info: &MetricInfo<'_>, true_count: u64, false_count: u64) {
        self.0.record_bool(info, true_count, false_count);
        self.1.record_bool(info, true_count, false_count);
    }
    fn record_histogram(&mut self, info: &MetricInfo<'_>, hist: Histogram<'_>) {
        self.0.record_histogram(info, hist);
        self.1.record_histogram(info, hist);
    }
    fn record_callback(&mut self, info: &MetricInfo<'_>, values: &[&dyn CallbackValue]) {
        self.0.record_callback(info, values);
        self.1.record_callback(info, values);
    }
    fn report_end(&mut self) {
        self.0.report_end();
        self.1.report_end();
    }
}

/// Setup-time toggle: a `None` backend records nothing.
impl<B: Backend> Backend for Option<B> {
    fn report_start(&mut self, options: &ReportOptions) {
        if let Some(b) = self {
            b.report_start(options);
        }
    }
    fn record_counter(&mut self, info: &MetricInfo<'_>, value: u64) {
        if let Some(b) = self {
            b.record_counter(info, value);
        }
    }
    fn record_gauge(&mut self, info: &MetricInfo<'_>, value: i64) {
        if let Some(b) = self {
            b.record_gauge(info, value);
        }
    }
    fn record_bool(&mut self, info: &MetricInfo<'_>, true_count: u64, false_count: u64) {
        if let Some(b) = self {
            b.record_bool(info, true_count, false_count);
        }
    }
    fn record_histogram(&mut self, info: &MetricInfo<'_>, hist: Histogram<'_>) {
        if let Some(b) = self {
            b.record_histogram(info, hist);
        }
    }
    fn record_callback(&mut self, info: &MetricInfo<'_>, values: &[&dyn CallbackValue]) {
        if let Some(b) = self {
            b.record_callback(info, values);
        }
    }
    fn report_end(&mut self) {
        if let Some(b) = self {
            b.report_end();
        }
    }
}

/// Forwarding impl so a `&mut B` (or a boxed backend) can be passed where a `Backend` is expected.
impl<B: Backend + ?Sized> Backend for &mut B {
    fn report_start(&mut self, options: &ReportOptions) {
        (**self).report_start(options);
    }
    fn record_counter(&mut self, info: &MetricInfo<'_>, value: u64) {
        (**self).record_counter(info, value);
    }
    fn record_gauge(&mut self, info: &MetricInfo<'_>, value: i64) {
        (**self).record_gauge(info, value);
    }
    fn record_bool(&mut self, info: &MetricInfo<'_>, true_count: u64, false_count: u64) {
        (**self).record_bool(info, true_count, false_count);
    }
    fn record_histogram(&mut self, info: &MetricInfo<'_>, hist: Histogram<'_>) {
        (**self).record_histogram(info, hist);
    }
    fn record_callback(&mut self, info: &MetricInfo<'_>, values: &[&dyn CallbackValue]) {
        (**self).record_callback(info, values);
    }
    fn report_end(&mut self) {
        (**self).report_end();
    }
}

/// Forwarding impl for boxed backends, enabling `Vec<Box<dyn Backend>>`-style dynamic sets.
impl<B: Backend + ?Sized> Backend for Box<B> {
    fn report_start(&mut self, options: &ReportOptions) {
        (**self).report_start(options);
    }
    fn record_counter(&mut self, info: &MetricInfo<'_>, value: u64) {
        (**self).record_counter(info, value);
    }
    fn record_gauge(&mut self, info: &MetricInfo<'_>, value: i64) {
        (**self).record_gauge(info, value);
    }
    fn record_bool(&mut self, info: &MetricInfo<'_>, true_count: u64, false_count: u64) {
        (**self).record_bool(info, true_count, false_count);
    }
    fn record_histogram(&mut self, info: &MetricInfo<'_>, hist: Histogram<'_>) {
        (**self).record_histogram(info, hist);
    }
    fn record_callback(&mut self, info: &MetricInfo<'_>, values: &[&dyn CallbackValue]) {
        (**self).record_callback(info, values);
    }
    fn report_end(&mut self) {
        (**self).report_end();
    }
}

/// A runtime on/off switch wrapping any backend.
///
/// While disabled, every method is a cheap atomic load and a no-op; the wrapped backend sees
/// nothing (not even `report_start`/`report_end`). `Toggle` is itself a [`Backend`], so it composes:
/// `(QuerylogBackend, Toggle<StatsdBackend>)`.
pub struct Toggle<B> {
    inner: B,
    enabled: Arc<AtomicBool>,
}

impl<B> Toggle<B> {
    /// Wraps `inner`, returning the toggle and a handle to flip it. `enabled` sets the initial state.
    pub fn new(inner: B, enabled: bool) -> (Self, ToggleHandle) {
        let flag = Arc::new(AtomicBool::new(enabled));
        let handle = ToggleHandle(flag.clone());
        (
            Self {
                inner,
                enabled: flag,
            },
            handle,
        )
    }

    /// Returns a reference to the wrapped backend.
    pub fn inner(&self) -> &B {
        &self.inner
    }

    #[inline]
    fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
}

impl<B: Backend> Backend for Toggle<B> {
    fn report_start(&mut self, options: &ReportOptions) {
        if self.is_enabled() {
            self.inner.report_start(options);
        }
    }
    fn record_counter(&mut self, info: &MetricInfo<'_>, value: u64) {
        if self.is_enabled() {
            self.inner.record_counter(info, value);
        }
    }
    fn record_gauge(&mut self, info: &MetricInfo<'_>, value: i64) {
        if self.is_enabled() {
            self.inner.record_gauge(info, value);
        }
    }
    fn record_bool(&mut self, info: &MetricInfo<'_>, true_count: u64, false_count: u64) {
        if self.is_enabled() {
            self.inner.record_bool(info, true_count, false_count);
        }
    }
    fn record_histogram(&mut self, info: &MetricInfo<'_>, hist: Histogram<'_>) {
        if self.is_enabled() {
            self.inner.record_histogram(info, hist);
        }
    }
    fn record_callback(&mut self, info: &MetricInfo<'_>, values: &[&dyn CallbackValue]) {
        if self.is_enabled() {
            self.inner.record_callback(info, values);
        }
    }
    fn report_end(&mut self) {
        if self.is_enabled() {
            self.inner.report_end();
        }
    }
}

/// A cloneable handle that enables or disables a [`Toggle`] at runtime.
#[derive(Clone)]
pub struct ToggleHandle(Arc<AtomicBool>);

impl ToggleHandle {
    /// Enables the associated toggle.
    pub fn enable(&self) {
        self.set(true);
    }

    /// Disables the associated toggle.
    pub fn disable(&self) {
        self.set(false);
    }

    /// Sets the associated toggle's enabled state.
    pub fn set(&self, enabled: bool) {
        self.0.store(enabled, Ordering::Relaxed);
    }

    /// Returns the associated toggle's current enabled state.
    pub fn is_enabled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::Registry;

    /// A backend that tallies how many of each record call it saw and the total observed value, so
    /// tests can assert fan-out and single-drain behavior.
    #[derive(Default)]
    struct CountingBackend {
        starts: usize,
        ends: usize,
        counters: usize,
        gauges: usize,
        bools: usize,
        histograms: usize,
        callbacks: usize,
        counter_sum: u64,
        callback_sum: f64,
        histogram_sample_sum: u64,
    }

    impl Backend for CountingBackend {
        fn report_start(&mut self, _options: &ReportOptions) {
            self.starts += 1;
        }
        fn record_counter(&mut self, _info: &MetricInfo<'_>, value: u64) {
            self.counters += 1;
            self.counter_sum += value;
        }
        fn record_gauge(&mut self, _info: &MetricInfo<'_>, _value: i64) {
            self.gauges += 1;
        }
        fn record_bool(&mut self, _info: &MetricInfo<'_>, _true_count: u64, _false_count: u64) {
            self.bools += 1;
        }
        fn record_histogram(&mut self, _info: &MetricInfo<'_>, hist: Histogram<'_>) {
            self.histograms += 1;
            self.histogram_sample_sum += hist.count();
        }
        fn record_callback(&mut self, _info: &MetricInfo<'_>, values: &[&dyn CallbackValue]) {
            self.callbacks += 1;
            self.callback_sum += values.iter().map(|v| v.as_f64()).sum::<f64>();
        }
        fn report_end(&mut self) {
            self.ends += 1;
        }
    }

    #[test]
    fn composition_fans_out_with_single_destructive_drain() {
        let registry = Registry::new();
        let counter = registry.register_counter("a".into(), None);
        counter.increment(7);

        // A 2-tuple backend: both halves must see the value from one snapshot.
        let mut backends = (CountingBackend::default(), CountingBackend::default());
        registry.report(&mut backends);

        assert_eq!(backends.0.counters, 1);
        assert_eq!(backends.1.counters, 1);
        assert_eq!(backends.0.counter_sum, 7);
        assert_eq!(backends.1.counter_sum, 7);
        assert_eq!(backends.0.starts, 1);
        assert_eq!(backends.0.ends, 1);

        // The take is destructive: a second report sees zero (the counter drained on the first).
        let mut again = CountingBackend::default();
        registry.report(&mut again);
        assert_eq!(again.counters, 1, "metric still visited");
        assert_eq!(again.counter_sum, 0, "but value drained to zero");
    }

    #[test]
    fn noop_backend_records_nothing() {
        let registry = Registry::new();
        registry.register_counter("a".into(), None).increment(1);
        // The unit backend implements Backend as a no-op; this should not panic and should drain.
        registry.report(&mut ());
        // Confirm the value drained even though the no-op backend ignored it.
        let mut counting = CountingBackend::default();
        registry.report(&mut counting);
        assert_eq!(counting.counter_sum, 0);
    }

    #[test]
    fn toggle_gates_recording() {
        let registry = Registry::new();
        registry.register_counter("a".into(), None).increment(5);

        let (mut toggle, handle) = Toggle::new(CountingBackend::default(), false);
        // Disabled: records nothing (not even start/end).
        registry.report(&mut toggle);
        assert_eq!(toggle.inner().starts, 0);
        assert_eq!(toggle.inner().counters, 0);

        // Note the counter drained above even though the backend was disabled (drain is
        // unconditional). Record a fresh value and enable.
        registry.register_counter("a".into(), None).increment(9);
        handle.enable();
        registry.report(&mut toggle);
        assert_eq!(toggle.inner().starts, 1);
        assert_eq!(toggle.inner().counters, 1);
        assert_eq!(toggle.inner().counter_sum, 9);
    }

    #[test]
    fn visits_every_kind() {
        let registry = Registry::new();

        let counter = registry.register_counter("c".into(), None);
        let summary = registry.register_summary("h".into(), None, Unit::Microsecond);
        let boolc = registry.register_bool("b".into(), None);
        registry.register_list_callback("cb".into(), None, Unit::Count, || 3u64);
        registry.register_list_callback_zero_suppressed("g".into(), None, Unit::Count, || 11i64);

        counter.increment(2);
        summary.record_duration(std::time::Duration::from_micros(5));
        boolc.record(true);
        boolc.record(false);

        let mut backend = CountingBackend::default();
        registry.report_with(&ReportOptions::new(true), &mut backend);

        assert_eq!(backend.counters, 1);
        assert_eq!(backend.histograms, 1);
        assert_eq!(backend.bools, 1);
        // Both callback metrics (cb + g) flow through record_callback.
        assert_eq!(backend.callbacks, 2);
        assert_eq!(backend.counter_sum, 2);
        assert_eq!(backend.histogram_sample_sum, 1);
        // cb=3, g=11 -> 14
        assert_eq!(backend.callback_sum, 14.0);
    }

    /// `quantile(0.0)` must return the minimum observed bucket, not bucket 0 when bucket 0 is empty.
    #[test]
    fn quantile_zero_is_min_not_bucket_zero() {
        // Capture the borrowed Histogram view's quantile/min/max from a real Summary.
        #[derive(Default)]
        struct Capture {
            q0: u64,
            min: u64,
            q1: u64,
            max: u64,
        }
        impl Backend for Capture {
            fn record_counter(&mut self, _: &MetricInfo<'_>, _: u64) {}
            fn record_gauge(&mut self, _: &MetricInfo<'_>, _: i64) {}
            fn record_bool(&mut self, _: &MetricInfo<'_>, _: u64, _: u64) {}
            fn record_histogram(&mut self, _: &MetricInfo<'_>, hist: Histogram<'_>) {
                self.q0 = hist.quantile(0.0);
                self.min = hist.min();
                self.q1 = hist.quantile(1.0);
                self.max = hist.max();
            }
            fn record_callback(&mut self, _: &MetricInfo<'_>, _: &[&dyn CallbackValue]) {}
        }

        let registry = Registry::new();
        let summary = registry.register_summary("h".into(), None, Unit::Count);
        // All samples land well above bucket 0, so bucket 0 is empty.
        summary.record_value(100);
        summary.record_value(100);
        summary.record_value(5000);

        let mut cap = Capture::default();
        registry.report(&mut cap);

        // q0 must equal the minimum observed bucket (non-zero), not bucket 0's midpoint (0).
        assert_eq!(cap.q0, cap.min);
        assert!(cap.q0 > 0, "quantile(0.0) returned bucket 0 ({})", cap.q0);
        assert_eq!(cap.q1, cap.max);
    }
}
