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
use std::fmt::Write as _;

/// Percentiles emitted for histogram metrics.
const HISTOGRAM_PERCENTILES: [u32; 4] = [50, 90, 95, 99];

/// Default maximum UDP payload size (bytes) for a single datagram.
pub const DEFAULT_MAX_PAYLOAD_SIZE: usize = 1200;

/// A transport for finished StatsD UDP datagrams.
///
/// The backend formats and chunks records into datagram-sized payloads and hands them off here in
/// `report_end`, as an iterator of `&str` slices that borrow the backend's internal buffer.
/// Implementations send them however they like (e.g. a rate-paced UDP socket); each `&str` is one
/// ready-to-send datagram. The slices are only valid for the duration of the call, so an
/// implementation that defers sending (e.g. queues to another task) must copy. `send_batch` should
/// not block the reporting thread; drop or queue as appropriate.
pub trait StatsdSink: Send {
    /// Sends a batch of datagrams. Each item is one complete, newline-joined UDP payload.
    fn send_batch<'a>(&mut self, payloads: impl Iterator<Item = &'a str>);
}

/// A [`Backend`] that encodes metrics as StatsD lines and forwards datagram payloads to a
/// [`StatsdSink`].
///
/// Reusable across reports: per-report state is cleared in
/// [`report_start`](Backend::report_start) while capacity is retained. All records for a report are
/// written into a single `buffer`, separated by `\n`; `bounds` holds each record's end offset so
/// chunking can yield datagrams as contiguous slices of `buffer` without copying.
pub struct StatsdBackend<S> {
    sink: S,
    prefix: Option<String>,
    max_payload_size: usize,
    include_sparse: bool,
    /// All records for the in-progress report, `\n`-separated.
    buffer: String,
    /// End offset (exclusive) of each record in `buffer`. Record `i` spans
    /// `[bounds[i-1] + 1, bounds[i])` (or `[0, bounds[0])` for `i == 0`); the byte at each
    /// `bounds[i-1]` is the `\n` separator.
    bounds: Vec<usize>,
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
            buffer: String::new(),
            bounds: Vec::new(),
        }
    }

    /// Sets the maximum UDP payload size used when chunking records into datagrams.
    pub fn with_max_payload_size(mut self, max_payload_size: usize) -> Self {
        self.max_payload_size = max_payload_size;
        self
    }

    /// Starts a new record: writes the `\n` separator (after the first) and the sanitized,
    /// prefixed metric name plus `name_suffix`, returning the buffer for the caller to append the
    /// value into. The matching [`end_record`](Self::end_record) records the boundary.
    ///
    /// Free-function-style field access (`&mut self.buffer` alongside `&self.prefix`) keeps the
    /// borrows disjoint so we never allocate an intermediate name string.
    fn begin_record(&mut self, name: &str, name_suffix: &str) -> &mut String {
        if !self.buffer.is_empty() {
            self.buffer.push('\n');
        }
        write_name(&mut self.buffer, self.prefix.as_deref(), name);
        self.buffer.push_str(name_suffix);
        &mut self.buffer
    }

    /// Finishes the current record: appends the optional `|#variant:` tag and records the boundary.
    fn end_record(&mut self, aggregation: Option<&str>) {
        write_tag(&mut self.buffer, aggregation);
        self.bounds.push(self.buffer.len());
    }
}

impl<S: StatsdSink> Backend for StatsdBackend<S> {
    fn report_start(&mut self, options: &ReportOptions) {
        // Clear retains the allocated capacity (the backend is long-lived).
        self.buffer.clear();
        self.bounds.clear();
        self.include_sparse = options.include_sparse;
    }

    fn record_counter(&mut self, info: &MetricInfo<'_>, value: u64) {
        // Suppress zeros unless sparse output is requested, so steady-state reporting isn't flooded
        // with `:0|c` lines (matching the querylog policy).
        if value == 0 && !self.include_sparse {
            return;
        }
        write!(self.begin_record(info.name, ":"), "{value}|c").unwrap();
        self.end_record(info.aggregation);
    }

    fn record_gauge(&mut self, info: &MetricInfo<'_>, value: i64) {
        // Honor the zero-suppression policy: drop a zero when the metric is zero-suppressed or when
        // sparse output is off.
        if value == 0 && (info.zero_suppressed || !self.include_sparse) {
            return;
        }
        write!(self.begin_record(info.name, ":"), "{value}|g").unwrap();
        self.end_record(info.aggregation);
    }

    fn record_bool(&mut self, info: &MetricInfo<'_>, true_count: u64, false_count: u64) {
        // Bool counters always suppress the all-zero case (matching the querylog policy).
        if (true_count, false_count) == (0, 0) {
            return;
        }
        write!(self.begin_record(info.name, ".true:"), "{true_count}|c").unwrap();
        self.end_record(info.aggregation);
        write!(self.begin_record(info.name, ".false:"), "{false_count}|c").unwrap();
        self.end_record(info.aggregation);
    }

    fn record_histogram(&mut self, info: &MetricInfo<'_>, hist: Histogram<'_>) {
        if hist.count() == 0 {
            return;
        }
        let unit = hist.unit();

        // Histogram bucket values are the native recorded magnitude: nanoseconds for time units
        // (record_duration stores `as_nanos`), bytes/counts as-is, and percents scaled by
        // FLOAT_INT_MULTIPLIER. `statsd_value` maps each to a sensible StatsD integer.
        write!(self.begin_record(info.name, ".count:"), "{}|c", hist.count()).unwrap();
        self.end_record(info.aggregation);
        write!(
            self.begin_record(info.name, ".min:"),
            "{}|g",
            statsd_value(hist.min(), unit)
        )
        .unwrap();
        self.end_record(info.aggregation);
        for pct in HISTOGRAM_PERCENTILES {
            let value = statsd_value(hist.quantile(pct as f64 / 100.0), unit);
            // Write the name, then the `.p{pct}:` label and value directly into the buffer.
            let out = self.begin_record(info.name, ".p");
            write!(out, "{pct}:{value}|g").unwrap();
            self.end_record(info.aggregation);
        }
        write!(
            self.begin_record(info.name, ".max:"),
            "{}|g",
            statsd_value(hist.max(), unit)
        )
        .unwrap();
        self.end_record(info.aggregation);
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
        write!(self.begin_record(info.name, ":"), "{sum}|g").unwrap();
        self.end_record(info.aggregation);
    }

    fn report_end(&mut self) {
        if self.bounds.is_empty() {
            return;
        }
        // Chunk the records (delimited by `bounds`) into datagram-sized `&str` slices of `buffer`,
        // and hand them to the sink without copying.
        let chunks = Chunks::new(&self.buffer, &self.bounds, self.max_payload_size);
        self.sink.send_batch(chunks);
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

/// Appends the sanitized, prefixed metric name to `out`.
///
/// Sanitization: `[a-zA-Z0-9_.-]` is preserved, `:` becomes `.`, everything else becomes `_`.
fn write_name(out: &mut String, prefix: Option<&str>, name: &str) {
    if let Some(prefix) = prefix.filter(|p| !p.is_empty()) {
        let before = out.len();
        write_sanitized(out, prefix);
        // Only add the separating `.` if the prefix produced something.
        if out.len() != before {
            out.push('.');
        }
    }
    write_sanitized(out, name);
}

/// Appends the `|#variant:{aggregation}` tag to `out`, if there is an aggregation.
fn write_tag(out: &mut String, aggregation: Option<&str>) {
    if let Some(agg) = aggregation {
        out.push_str("|#variant:");
        write_sanitized(out, agg);
    }
}

/// Appends `input` to `out`, sanitized for StatsD (see [`write_name`]).
fn write_sanitized(out: &mut String, input: &str) {
    out.reserve(input.len());
    for c in input.chars() {
        out.push(match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '.' | '-' => c,
            ':' => '.',
            _ => '_',
        });
    }
}

/// Iterator over datagram-sized `&str` slices of a record buffer.
///
/// Records are already `\n`-separated in `buffer` and delimited by `bounds` (each entry is a
/// record's end offset). A datagram is a maximal run of whole records whose byte length fits in
/// `max_payload_size`; because records are contiguous and newline-joined in `buffer`, each datagram
/// is just a sub-slice of `buffer` — no copying. Records longer than `max_payload_size` (which
/// cannot fit a datagram) are skipped.
struct Chunks<'a> {
    buffer: &'a str,
    bounds: &'a [usize],
    max_payload_size: usize,
    /// Index into `bounds` of the next record to consider.
    next: usize,
}

impl<'a> Chunks<'a> {
    fn new(buffer: &'a str, bounds: &'a [usize], max_payload_size: usize) -> Self {
        Self {
            buffer,
            bounds,
            max_payload_size,
            next: 0,
        }
    }

    /// The byte range `[start, end)` of record `i` in `buffer` (excluding its leading separator).
    fn record_range(&self, i: usize) -> (usize, usize) {
        // Record 0 starts at 0; record i (>0) starts one byte after the previous record's end
        // (skipping the `\n` separator).
        let start = if i == 0 { 0 } else { self.bounds[i - 1] + 1 };
        (start, self.bounds[i])
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.max_payload_size == 0 {
            return None;
        }

        // Skip records that can't fit a datagram on their own. This should not happen in practice:
        // records are short (`prefix.name.suffix:value|c|#variant:agg`) against a ~1200 byte
        // default, so an oversized record means a misconfiguration (tiny payload size or a
        // pathologically long metric name). Such records are silently dropped — like the previous
        // implementation, which also dropped them (it surfaced a dropped-count the reporter logged;
        // that diagnostic is not reproduced here to keep this crate free of a logging dependency).
        while self.next < self.bounds.len() {
            let (start, end) = self.record_range(self.next);
            if end - start <= self.max_payload_size {
                break;
            }
            self.next += 1;
        }
        if self.next >= self.bounds.len() {
            return None;
        }

        // Greedily extend the datagram with whole records (joined by their existing `\n`) while it
        // fits. `datagram_start` is the first record's start; the datagram spans to the last
        // included record's end.
        let (datagram_start, mut datagram_end) = self.record_range(self.next);
        self.next += 1;
        while self.next < self.bounds.len() {
            let (start, end) = self.record_range(self.next);
            if end - start > self.max_payload_size {
                // This record can't fit anywhere; let the next iteration's skip-loop drop it.
                break;
            }
            // Including it means spanning from datagram_start..end, which absorbs the `\n` at
            // `datagram_end` (== start - 1) for free.
            if end - datagram_start > self.max_payload_size {
                break;
            }
            datagram_end = end;
            self.next += 1;
        }

        Some(&self.buffer[datagram_start..datagram_end])
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::backend::MetricKind;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct CaptureSink(Arc<Mutex<Vec<String>>>);

    impl CaptureSink {
        /// The datagrams sent, each a complete newline-joined UDP payload.
        fn datagrams(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }

        /// All individual records across all datagrams.
        fn lines(&self) -> Vec<String> {
            self.datagrams()
                .iter()
                .flat_map(|d| d.split('\n').map(str::to_string).collect::<Vec<_>>())
                .collect()
        }
    }

    impl StatsdSink for CaptureSink {
        fn send_batch<'a>(&mut self, payloads: impl Iterator<Item = &'a str>) {
            // Copy the borrowed slices into owned Strings (the slices are only valid for the call).
            self.0
                .lock()
                .unwrap()
                .extend(payloads.map(str::to_string));
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
        assert!(sink.datagrams().is_empty());
    }

    /// Drives the `Chunks` iterator directly over a hand-built buffer + bounds, exercising packing,
    /// the oversized-record skip, and the zero-size case.
    #[test]
    fn chunks_pack_records_and_skip_oversized() {
        // Three records "aaaa", "bbbb", "toolong" laid out newline-separated as the backend would.
        // Offsets: aaaa=0..4, '\n'@4, bbbb=5..9, '\n'@9, toolong=10..17. Bounds are record ends.
        let buffer = "aaaa\nbbbb\ntoolong";
        let bounds = vec![4usize, 9, 17];

        // max 9 fits "aaaa\nbbbb" (9 bytes); "toolong" (7) fits alone in the next datagram.
        let chunks: Vec<&str> = Chunks::new(buffer, &bounds, 9).collect();
        assert_eq!(chunks, vec!["aaaa\nbbbb", "toolong"]);

        // A record longer than the limit is skipped entirely.
        let buffer2 = "waytoolongline";
        let bounds2 = vec![14usize];
        assert_eq!(Chunks::new(buffer2, &bounds2, 5).count(), 0);

        // Zero size drops everything.
        assert_eq!(Chunks::new(buffer, &bounds, 0).count(), 0);

        // Each record alone when the limit only fits one at a time.
        let chunks: Vec<&str> = Chunks::new(buffer, &bounds, 7).collect();
        assert_eq!(chunks, vec!["aaaa", "bbbb", "toolong"]);

        // A huge limit packs everything into one datagram (separators included).
        let chunks: Vec<&str> = Chunks::new(buffer, &bounds, 1000).collect();
        assert_eq!(chunks, vec!["aaaa\nbbbb\ntoolong"]);

        // An oversized record in the middle is skipped while neighbors still pack.
        // "aa"=0..2, '\n'@2, "huge"=3..7, '\n'@7, "bb"=8..10
        let buffer = "aa\nhuge\nbb";
        let bounds = vec![2usize, 7, 10];
        let chunks: Vec<&str> = Chunks::new(buffer, &bounds, 3).collect();
        assert_eq!(chunks, vec!["aa", "bb"]);
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
        assert!(sink.datagrams().is_empty());

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
