// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A [`Backend`] that encodes metrics into the StatsD line protocol from native values.
//!
//! This is the formatting + batching half of StatsD export. It is transport-agnostic: finished
//! UDP payloads are handed to a [`StatsdSink`], which the consumer implements (e.g. an
//! `s2n-quic-dc` rate-paced UDP socket). The crate intentionally does not depend on any socket or
//! pacing machinery.
//!
//! Dispatch is purely on the generic [`MetricInfo`] (`kind` + `unit`); `name` and `aggregation`
//! are treated as opaque. No metric-name conventions are interpreted here.

use crate::{
    backend::{Backend, CallbackValue, Histogram, MetricInfo, ReportOptions},
    Unit,
};

/// Percentiles emitted for histogram metrics.
const HISTOGRAM_PERCENTILES: [u32; 4] = [50, 90, 95, 99];

/// Default maximum UDP payload size (bytes) for a single datagram.
pub const DEFAULT_MAX_PAYLOAD_SIZE: usize = 1200;

/// A transport for finished StatsD UDP payloads.
///
/// The backend chunks lines into datagram-sized payloads and hands them off here in `report_end`.
/// Implementations send them however they like (e.g. a rate-paced UDP socket). `send_batch` should
/// not block the reporting thread; drop or queue as appropriate.
pub trait StatsdSink: Send {
    /// Sends a batch of already-chunked UDP payloads.
    fn send_batch(&mut self, payloads: Vec<Vec<u8>>);
}

/// A [`Backend`] that encodes metrics as StatsD lines and forwards datagram payloads to a
/// [`StatsdSink`].
///
/// Reusable across reports: per-report line state is cleared in
/// [`report_start`](Backend::report_start) while capacity is retained.
pub struct StatsdBackend<S> {
    sink: S,
    prefix: Option<String>,
    max_payload_size: usize,
    include_sparse: bool,
    /// Accumulated lines for the in-progress report, flushed in `report_end`.
    lines: Vec<String>,
}

impl<S> StatsdBackend<S> {
    /// Creates a backend that forwards to `sink`, optionally prefixing every metric name, with the
    /// default maximum payload size.
    pub fn new(sink: S, prefix: Option<String>) -> Self {
        Self {
            sink,
            prefix,
            max_payload_size: DEFAULT_MAX_PAYLOAD_SIZE,
            include_sparse: false,
            lines: Vec::new(),
        }
    }

    /// Sets the maximum UDP payload size used when chunking lines into datagrams.
    pub fn with_max_payload_size(mut self, max_payload_size: usize) -> Self {
        self.max_payload_size = max_payload_size;
        self
    }

    /// The fully-qualified, sanitized metric name (with the optional prefix applied).
    fn metric_name(&self, name: &str) -> String {
        with_prefix(name, self.prefix.as_deref())
    }

    /// The `|#variant:...` tag for an aggregation string, or empty when there is none.
    fn variant_tag(aggregation: Option<&str>) -> String {
        match aggregation {
            Some(agg) => format!("|#variant:{}", sanitize(agg)),
            None => String::new(),
        }
    }
}

impl<S: StatsdSink> Backend for StatsdBackend<S> {
    fn report_start(&mut self, options: &ReportOptions) {
        // Retain capacity across reports (the backend is long-lived).
        self.lines.clear();
        self.include_sparse = options.include_sparse;
    }

    fn record_counter(&mut self, info: &MetricInfo<'_>, value: u64) {
        // Suppress zeros unless sparse output is requested, so steady-state reporting isn't flooded
        // with `:0|c` lines (matching the querylog policy).
        if value == 0 && !self.include_sparse {
            return;
        }
        let metric = self.metric_name(info.name);
        let tag = Self::variant_tag(info.aggregation);
        self.lines.push(format!("{metric}:{value}|c{tag}"));
    }

    fn record_gauge(&mut self, info: &MetricInfo<'_>, value: i64) {
        // Honor the zero-suppression policy: drop a zero when the metric is zero-suppressed or when
        // sparse output is off.
        if value == 0 && (info.zero_suppressed || !self.include_sparse) {
            return;
        }
        let metric = self.metric_name(info.name);
        let tag = Self::variant_tag(info.aggregation);
        self.lines.push(format!("{metric}:{value}|g{tag}"));
    }

    fn record_bool(&mut self, info: &MetricInfo<'_>, true_count: u64, false_count: u64) {
        // Bool counters always suppress the all-zero case (matching the querylog policy).
        if (true_count, false_count) == (0, 0) {
            return;
        }
        let metric = self.metric_name(info.name);
        let tag = Self::variant_tag(info.aggregation);
        self.lines
            .push(format!("{metric}.true:{true_count}|c{tag}"));
        self.lines
            .push(format!("{metric}.false:{false_count}|c{tag}"));
    }

    fn record_histogram(&mut self, info: &MetricInfo<'_>, hist: Histogram<'_>) {
        if hist.count() == 0 {
            return;
        }
        let metric = self.metric_name(info.name);
        let tag = Self::variant_tag(info.aggregation);
        let unit = hist.unit();

        // Histogram bucket values are the native recorded magnitude: nanoseconds for time units
        // (record_duration stores `as_nanos`), bytes/counts as-is, and percents scaled by
        // FLOAT_INT_MULTIPLIER. `statsd_value` maps each to a sensible StatsD integer.
        let count = hist.count();
        self.lines.push(format!("{metric}.count:{count}|c{tag}"));
        self.lines
            .push(format!("{metric}.min:{}|g{tag}", statsd_value(hist.min(), unit)));
        for pct in HISTOGRAM_PERCENTILES {
            let value = statsd_value(hist.quantile(pct as f64 / 100.0), unit);
            self.lines.push(format!("{metric}.p{pct}:{value}|g{tag}"));
        }
        self.lines
            .push(format!("{metric}.max:{}|g{tag}", statsd_value(hist.max(), unit)));
    }

    fn record_callback(&mut self, info: &MetricInfo<'_>, values: &[&dyn CallbackValue]) {
        // Callback metrics are gauge-like instantaneous readings, emitted as a single gauge.
        //
        // When multiple callbacks share one name (e.g. one per worker) we emit their SUM. The
        // callback list carries no per-element identity (the registry discards which worker each
        // value came from), so a single reduced value is the only faithful StatsD representation.
        // Sum is correct for additive readings (counts, queue depths) but not for intensive ones
        // (e.g. per-worker busy-percent, where a mean would be more meaningful). Lifting these to a
        // first-class, per-variant metric kind — which would let StatsD emit one tagged line per
        // element — is deferred to a later phase; for now sum is the documented behavior.
        if values.is_empty() {
            return;
        }
        let sum: f64 = values.iter().map(|v| v.as_f64()).sum();
        // Honor the zero-suppression policy, as for gauges.
        if sum == 0.0 && (info.zero_suppressed || !self.include_sparse) {
            return;
        }
        let metric = self.metric_name(info.name);
        let tag = Self::variant_tag(info.aggregation);
        self.lines.push(format!("{metric}:{sum}|g{tag}"));
    }

    fn report_end(&mut self) {
        if self.lines.is_empty() {
            return;
        }
        let payloads = chunk_lines(&self.lines, self.max_payload_size);
        if !payloads.is_empty() {
            self.sink.send_batch(payloads);
        }
    }
}

/// Maps a native histogram bucket value to the integer StatsD reports.
///
/// Histogram values arrive in their native recorded magnitude:
/// - time units (`Microsecond`/`Second`) are stored as **nanoseconds** (`record_duration` records
///   `Duration::as_nanos`), and StatsD timers are nanosecond-based, so they pass through unchanged;
/// - `Count`/`Byte` are raw and pass through;
/// - `Percent` is stored scaled by `FLOAT_INT_MULTIPLIER` (1000), so divide back to a whole percent.
fn statsd_value(value: u64, unit: Unit) -> u64 {
    match unit {
        Unit::Microsecond | Unit::Second | Unit::Count | Unit::Byte => value,
        Unit::Percent => value / crate::summary::FLOAT_INT_MULTIPLIER_U64,
    }
}

/// Sanitizes a metric name for StatsD: `[a-zA-Z0-9_.-]` is preserved, `:` becomes `.`, everything
/// else becomes `_`.
fn sanitize(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for c in input.chars() {
        output.push(match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '.' | '-' => c,
            ':' => '.',
            _ => '_',
        });
    }
    output
}

fn with_prefix(name: &str, prefix: Option<&str>) -> String {
    match prefix.filter(|p| !p.is_empty()) {
        Some(prefix) => {
            let prefix = sanitize(prefix);
            if prefix.is_empty() {
                sanitize(name)
            } else {
                format!("{prefix}.{}", sanitize(name))
            }
        }
        None => sanitize(name),
    }
}

/// Batches StatsD lines into newline-delimited UDP payloads up to `max_payload_size`.
///
/// Lines longer than `max_payload_size` are dropped (they cannot fit a datagram). A
/// `max_payload_size` of 0 drops everything.
fn chunk_lines(lines: &[String], max_payload_size: usize) -> Vec<Vec<u8>> {
    if max_payload_size == 0 {
        return Vec::new();
    }

    let mut payloads = Vec::new();
    let mut current: Vec<u8> = Vec::new();

    for line in lines {
        let bytes = line.as_bytes();
        if bytes.len() > max_payload_size {
            // Cannot fit in a single datagram; skip.
            continue;
        }

        let required = if current.is_empty() {
            bytes.len()
        } else {
            current.len() + 1 + bytes.len()
        };

        if required > max_payload_size && !current.is_empty() {
            payloads.push(std::mem::take(&mut current));
        }

        if !current.is_empty() {
            current.push(b'\n');
        }
        current.extend_from_slice(bytes);
    }

    if !current.is_empty() {
        payloads.push(current);
    }

    payloads
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::backend::MetricKind;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct CaptureSink(Arc<Mutex<Vec<Vec<u8>>>>);

    impl CaptureSink {
        fn payloads(&self) -> Vec<Vec<u8>> {
            self.0.lock().unwrap().clone()
        }

        fn lines(&self) -> Vec<String> {
            self.payloads()
                .iter()
                .flat_map(|p| {
                    String::from_utf8(p.clone())
                        .unwrap()
                        .split('\n')
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .collect()
        }
    }

    impl StatsdSink for CaptureSink {
        fn send_batch(&mut self, payloads: Vec<Vec<u8>>) {
            self.0.lock().unwrap().extend(payloads);
        }
    }

    fn info<'a>(name: &'a str, agg: Option<&'a str>, unit: Unit, kind: MetricKind) -> MetricInfo<'a> {
        MetricInfo::new(name, agg, unit, kind)
    }

    #[test]
    fn counter_gauge_bool_encoding() {
        let sink = CaptureSink::default();
        let mut backend = StatsdBackend::new(sink.clone(), Some("svc".into()));

        backend.report_start(&ReportOptions::default());
        backend.record_counter(&info("rx.data", None, Unit::Count, MetricKind::Counter), 255);
        backend.record_counter(
            &info("rx.ecn", Some("ect0"), Unit::Count, MetricKind::Counter),
            500,
        );
        backend.record_gauge(&info("q.depth", None, Unit::Count, MetricKind::Gauge), 7);
        backend.record_bool(&info("connect", None, Unit::Count, MetricKind::BoolCounter), 2, 1);
        backend.report_end();

        let lines = sink.lines();
        assert!(lines.contains(&"svc.rx.data:255|c".to_string()));
        assert!(lines.contains(&"svc.rx.ecn:500|c|#variant:ect0".to_string()));
        assert!(lines.contains(&"svc.q.depth:7|g".to_string()));
        assert!(lines.contains(&"svc.connect.true:2|c".to_string()));
        assert!(lines.contains(&"svc.connect.false:1|c".to_string()));
    }

    fn line_value(lines: &[String], prefix: &str) -> u64 {
        let line = lines.iter().find(|l| l.starts_with(prefix)).expect(prefix);
        line[prefix.len()..line.find('|').unwrap()].parse().unwrap()
    }

    #[test]
    fn histogram_time_values_are_nanoseconds_not_inflated() {
        // Build a histogram via a real Summary so bucketing is genuine. record_duration stores
        // nanoseconds, so the StatsD value must be ~the nanosecond magnitude (5us -> ~5000ns),
        // NOT inflated by a us->ns multiply (which would yield ~5_000_000).
        let registry = crate::Registry::new();
        let summary =
            registry.register_summary("task.time".into(), Some("d.0".into()), Unit::Microsecond);
        summary.record_duration(std::time::Duration::from_micros(5));
        summary.record_duration(std::time::Duration::from_micros(5));
        summary.record_duration(std::time::Duration::from_micros(10));

        let sink = CaptureSink::default();
        let mut backend = StatsdBackend::new(sink.clone(), Some("svc".into()));
        registry.report(&mut backend);

        let lines = sink.lines();
        assert!(lines.contains(&"svc.task.time.count:3|c|#variant:d.0".to_string()));

        // The histogram view reports nanosecond bucket midpoints directly; statsd must pass them
        // through unchanged. min is the 5us bucket (~5000ns, within the ~0.78% bucket error), and
        // critically nowhere near 5_000_000.
        let min = line_value(&lines, "svc.task.time.min:");
        assert!((5000..5100).contains(&min), "min should be ~5000ns, got {min}");
        let max = line_value(&lines, "svc.task.time.max:");
        assert!((10000..10100).contains(&max), "max should be ~10000ns, got {max}");
        // p50/p99 present and within the observed [5000, 10100) range.
        let p50 = line_value(&lines, "svc.task.time.p50:");
        let p99 = line_value(&lines, "svc.task.time.p99:");
        assert!((5000..10100).contains(&p50), "p50 out of range: {p50}");
        assert!((5000..10100).contains(&p99), "p99 out of range: {p99}");
    }

    #[test]
    fn histogram_percent_descaled() {
        // Percent histogram values are stored x1000; statsd must divide back to whole percent.
        let registry = crate::Registry::new();
        let summary = registry.register_summary("ratio".into(), None, Unit::Percent);
        // logging_util_float_to_integer(50.0) == 50_000 stored.
        summary.record_value(crate::logging_util_float_to_integer(50.0));

        let sink = CaptureSink::default();
        let mut backend = StatsdBackend::new(sink.clone(), None);
        registry.report(&mut backend);

        let lines = sink.lines();
        let min = line_value(&lines, "ratio.min:");
        // ~50 (within bucket error), definitely not 50_000.
        assert!((49..=51).contains(&min), "percent should be ~50, got {min}");
    }

    #[test]
    fn empty_histogram_emits_nothing() {
        let sink = CaptureSink::default();
        let mut backend = StatsdBackend::new(sink.clone(), None);
        backend.report_start(&ReportOptions::default());
        // a zero-count histogram view
        let buckets = [0u64; 8];
        let cfg = crate::summary::bucket::Config::new(7, 64);
        let hist = Histogram::new(&buckets, &cfg, Unit::Count);
        backend.record_histogram(&info("h", None, Unit::Count, MetricKind::Histogram), hist);
        backend.report_end();
        assert!(sink.payloads().is_empty());
    }

    #[test]
    fn chunking_splits_and_drops_oversized() {
        let lines = vec!["aaaa".to_string(), "bbbb".to_string(), "toolong".to_string()];
        // max 9 fits "aaaa\nbbbb" (9 bytes); "toolong" (7) fits alone in a new payload.
        let payloads = chunk_lines(&lines, 9);
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0], b"aaaa\nbbbb");
        assert_eq!(payloads[1], b"toolong");

        // a line longer than the limit is dropped
        let payloads = chunk_lines(&["waytoolongline".to_string()], 5);
        assert!(payloads.is_empty());

        // zero size drops everything
        assert!(chunk_lines(&lines, 0).is_empty());
    }

    #[test]
    fn zero_values_suppressed_unless_sparse() {
        // Non-sparse: a zero counter emits nothing.
        let sink = CaptureSink::default();
        let mut backend = StatsdBackend::new(sink.clone(), None);
        backend.report_start(&ReportOptions::new(false));
        backend.record_counter(&info("c", None, Unit::Count, MetricKind::Counter), 0);
        backend.record_bool(&info("b", None, Unit::Count, MetricKind::BoolCounter), 0, 0);
        backend.report_end();
        assert!(sink.payloads().is_empty());

        // Sparse: the zero counter emits (bool all-zero is always suppressed).
        let sink = CaptureSink::default();
        let mut backend = StatsdBackend::new(sink.clone(), None);
        backend.report_start(&ReportOptions::new(true));
        backend.record_counter(&info("c", None, Unit::Count, MetricKind::Counter), 0);
        backend.record_bool(&info("b", None, Unit::Count, MetricKind::BoolCounter), 0, 0);
        backend.report_end();
        let lines = sink.lines();
        assert!(lines.contains(&"c:0|c".to_string()));
        assert!(!lines.iter().any(|l| l.starts_with("b.")));
    }

    #[test]
    fn callback_sums_multiple_values() {
        let sink = CaptureSink::default();
        let mut backend = StatsdBackend::new(sink.clone(), None);
        backend.report_start(&ReportOptions::default());
        let a: u64 = 3;
        let b: u64 = 4;
        let values: [&dyn CallbackValue; 2] = [&a, &b];
        backend.record_callback(
            &info("workers", None, Unit::Count, MetricKind::CallbackScalar),
            &values,
        );
        backend.report_end();
        assert!(sink.lines().contains(&"workers:7|g".to_string()));
    }
}
