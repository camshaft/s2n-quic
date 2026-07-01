// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A pull-based [`Backend`] that maintains cumulative Prometheus series in memory and renders the
//! [text exposition format] on demand.
//!
//! # Why this backend is different
//!
//! Unlike the statsd/querylog backends, which format each report's values and forward them
//! immediately, Prometheus is **pull-based** and its counters/histograms must be **cumulative and
//! monotonic**. But [`Registry::report`](crate::Registry::report) is *destructive*: it drains each
//! counter and histogram to zero every pass, handing the backend only the delta since the last
//! report. This backend therefore keeps its own authoritative, cumulative state and *accumulates*
//! each delta into it:
//!
//! - counters add the delta into a running total;
//! - gauges and callbacks (instantaneous readings) take the latest value (last-write-wins);
//! - histograms accumulate per-bucket counts, plus a running sum and observation count.
//!
//! A scrape endpoint reads the state through a [`PrometheusHandle`]. To keep the scrape path cheap
//! and off the reporting hot path, the backend re-encodes the full snapshot into the exposition
//! string once per report (in [`report_end`](Backend::report_end)) and publishes it behind a shared
//! cell; `encode` just clones the published `Arc<str>`. Values only change at report boundaries
//! anyway (that is when the registry drains), so a per-report snapshot loses no fidelity.
//!
//! # Name and label mapping
//!
//! Metric names are sanitized to the Prometheus grammar (`[a-zA-Z0-9_:]`, e.g. `rx.data` ->
//! `rx_data`). The historical `aggregation` convention string becomes labels: `Variant|ect0` ->
//! `{variant="ect0"}`, `Task|foo` -> `{task="foo"}`, and a bare string with no `|` -> its own
//! `aggregation` label. Bool counters collapse into a single counter family with a `result="true"`
//! / `result="false"` label.
//!
//! [text exposition format]: https://prometheus.io/docs/instrumenting/exposition_formats/

use crate::{
    backend::{Backend, CallbackValue, Histogram, MetricInfo, MetricKind, ReportOptions},
    Unit,
};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    sync::{Arc, Mutex},
};

/// A [`Backend`] that accumulates cumulative Prometheus series and publishes the rendered exposition
/// text for a scrape endpoint to serve.
///
/// Construct with [`new`](Self::new), which also returns a [`PrometheusHandle`] the scrape endpoint
/// holds. The backend is long-lived and reused across reports: its cumulative state persists (that
/// is the whole point), so nothing is cleared in [`report_start`](Backend::report_start) beyond
/// per-report options.
pub struct PrometheusBackend {
    /// Authoritative cumulative state, keyed by sanitized metric name.
    families: BTreeMap<String, Family>,
    /// Optional prefix prepended to every metric name (joined with `_`).
    prefix: Option<String>,
    /// The rendered exposition text most recently published for scraping.
    published: Arc<Mutex<Arc<str>>>,
    include_sparse: bool,
}

/// A cloneable handle a scrape endpoint uses to read the most recently published exposition text.
///
/// Cheap to clone and to read: [`encode`](Self::encode) clones a shared `Arc<str>` under a
/// short-lived lock (no formatting on the scrape path — that happens once per report on the
/// reporting thread).
#[derive(Clone)]
pub struct PrometheusHandle {
    published: Arc<Mutex<Arc<str>>>,
}

impl PrometheusHandle {
    /// The exposition text published by the most recent report, as a shared `Arc<str>`.
    ///
    /// Returns an empty string until the first report completes.
    pub fn encode(&self) -> Arc<str> {
        self.published.lock().unwrap().clone()
    }

    /// The exposition text published by the most recent report, as an owned `String`.
    pub fn render(&self) -> String {
        self.encode().to_string()
    }
}

impl PrometheusBackend {
    /// Creates a backend and the [`PrometheusHandle`] a scrape endpoint reads from.
    ///
    /// `prefix`, when non-empty, is prepended to every metric name (joined with `_`).
    pub fn new(prefix: Option<String>) -> (Self, PrometheusHandle) {
        let published: Arc<Mutex<Arc<str>>> = Arc::new(Mutex::new(Arc::from("")));
        let handle = PrometheusHandle {
            published: published.clone(),
        };
        (
            Self {
                families: BTreeMap::new(),
                prefix,
                published,
                include_sparse: false,
            },
            handle,
        )
    }

    /// Looks up (or creates) the family for `info`, sanitizing the name and applying the prefix.
    ///
    /// The unit and kind are taken from the first record for a given name; a name is assumed to have
    /// a single kind across all its aggregations (as every call site registers it), matching the
    /// per-family `# TYPE` requirement of the exposition format.
    fn family_mut(&mut self, info: &MetricInfo<'_>) -> &mut Family {
        let mut name = String::new();
        write_name(&mut name, self.prefix.as_deref(), info.name);
        self.families.entry(name).or_insert_with(|| Family {
            kind: info.kind,
            unit: info.unit,
            series: BTreeMap::new(),
        })
    }

    /// Renders the full cumulative state into the Prometheus text exposition format.
    fn encode(&self) -> String {
        let mut out = String::new();
        for (name, family) in &self.families {
            if family.series.is_empty() {
                continue;
            }
            writeln!(out, "# TYPE {name} {}", family.prom_type()).unwrap();
            for (aggregation, series) in &family.series {
                let labels = parse_labels(aggregation);
                series.encode(&mut out, name, &labels, family.unit);
            }
        }
        out
    }

    /// Encodes the current state and publishes it for scraping.
    fn publish(&self) {
        let text = self.encode();
        *self.published.lock().unwrap() = Arc::from(text);
    }
}

impl Backend for PrometheusBackend {
    fn report_start(&mut self, options: &ReportOptions) {
        // The cumulative state is deliberately NOT cleared: Prometheus series persist across
        // reports. Only per-report options are refreshed.
        self.include_sparse = options.include_sparse;
    }

    fn record_counter(&mut self, info: &MetricInfo<'_>, value: u64) {
        // Skip a counter that has never been non-zero (avoids clutter). Once a series exists it is
        // retained and keeps rendering its cumulative total, even when a later delta is zero, so the
        // time series has no gaps.
        let key = info.aggregation.unwrap_or("");
        let include_sparse = self.include_sparse;
        let family = self.family_mut(info);
        if value == 0 && !include_sparse && !family.series.contains_key(key) {
            return;
        }
        match family
            .series
            .entry(key.to_string())
            .or_insert(Series::Counter(0))
        {
            Series::Counter(total) => *total += value,
            _ => unreachable!("counter family has non-counter series"),
        }
    }

    fn record_gauge(&mut self, info: &MetricInfo<'_>, value: i64) {
        if value == 0 && (info.zero_suppressed || !self.include_sparse) {
            let key = info.aggregation.unwrap_or("");
            if !self.family_mut(info).series.contains_key(key) {
                return;
            }
        }
        let key = info.aggregation.unwrap_or("").to_string();
        let family = self.family_mut(info);
        *family.series.entry(key).or_insert(Series::Gauge(0.0)) = Series::Gauge(value as f64);
    }

    fn record_bool(&mut self, info: &MetricInfo<'_>, true_count: u64, false_count: u64) {
        // One counter family with a `result` label per side; each side accumulates like a counter.
        let include_sparse = self.include_sparse;
        for (label, delta) in [("true", true_count), ("false", false_count)] {
            let key = bool_key(info.aggregation, label);
            let family = self.family_mut(info);
            if delta == 0 && !include_sparse && !family.series.contains_key(&key) {
                continue;
            }
            match family.series.entry(key).or_insert(Series::Counter(0)) {
                Series::Counter(total) => *total += delta,
                _ => unreachable!("bool family has non-counter series"),
            }
        }
    }

    fn record_histogram(&mut self, info: &MetricInfo<'_>, hist: Histogram<'_>) {
        let key = info.aggregation.unwrap_or("");
        let unit = info.unit;
        let include_sparse = self.include_sparse;
        let family = self.family_mut(info);
        if hist.count() == 0 && !include_sparse && !family.series.contains_key(key) {
            return;
        }
        let bounds = default_buckets(unit);
        let series = family
            .series
            .entry(key.to_string())
            .or_insert_with(|| Series::Histogram(HistState::new(bounds.len())));
        let Series::Histogram(state) = series else {
            unreachable!("histogram family has non-histogram series");
        };
        // Fold this report's drained distribution into the cumulative per-bucket counts. Each
        // non-empty crate bucket contributes its `count` to the smallest `le` slot that covers its
        // representative value; the trailing slot is the `+Inf` overflow. The running sum uses the
        // reported (unit-converted) value, matching the statsd backend's magnitude choice.
        for (value, count) in hist.buckets() {
            let reported = to_reported(value, unit);
            let slot = bounds
                .iter()
                .position(|&le| reported <= le)
                .unwrap_or(bounds.len());
            state.counts[slot] += count;
            state.sum += reported * count as f64;
            state.count += count;
        }
    }

    fn record_callback(&mut self, info: &MetricInfo<'_>, values: &[&dyn CallbackValue]) {
        // Callbacks are gauge-like instantaneous readings. As in statsd, multiple values under one
        // name are summed (the registry discards per-element identity), so a single reduced value is
        // the only faithful representation.
        if values.is_empty() {
            return;
        }
        let sum: f64 = values.iter().map(|v| v.as_f64()).sum();
        if sum == 0.0 && (info.zero_suppressed || !self.include_sparse) {
            let key = info.aggregation.unwrap_or("");
            if !self.family_mut(info).series.contains_key(key) {
                return;
            }
        }
        let key = info.aggregation.unwrap_or("").to_string();
        let family = self.family_mut(info);
        *family.series.entry(key).or_insert(Series::Gauge(0.0)) = Series::Gauge(sum);
    }

    fn report_end(&mut self) {
        self.publish();
    }
}

/// A metric family: all series sharing one name (and therefore one `# TYPE`).
struct Family {
    kind: MetricKind,
    unit: Unit,
    /// Series keyed by their raw aggregation string (`""` for none); for bool counters the key also
    /// carries the `result` side (see [`bool_key`]).
    series: BTreeMap<String, Series>,
}

impl Family {
    /// The Prometheus type string for this family's `# TYPE` line.
    fn prom_type(&self) -> &'static str {
        match self.kind {
            MetricKind::Counter | MetricKind::BoolCounter => "counter",
            MetricKind::Gauge | MetricKind::CallbackScalar => "gauge",
            MetricKind::Histogram => "histogram",
        }
    }
}

/// The cumulative state of a single Prometheus series.
enum Series {
    /// Monotonic cumulative total.
    Counter(u64),
    /// Latest instantaneous value.
    Gauge(f64),
    /// Cumulative classic-histogram state.
    Histogram(HistState),
}

/// Cumulative classic-histogram state: per-`le`-slot observation counts (the last slot is `+Inf`),
/// plus the running sum and total observation count.
struct HistState {
    /// `counts[i]` is the number of observations that fell in `le` slot `i`, accumulated across
    /// reports (non-cumulative; the exposition emits the running prefix sum). `len == bounds + 1`,
    /// the extra slot being `+Inf`.
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl HistState {
    fn new(num_bounds: usize) -> Self {
        Self {
            counts: vec![0; num_bounds + 1],
            sum: 0.0,
            count: 0,
        }
    }
}

impl Series {
    /// Appends this series' sample line(s) to `out`.
    fn encode(&self, out: &mut String, name: &str, labels: &[(String, String)], unit: Unit) {
        match self {
            Series::Counter(v) => {
                write_series(out, name, labels, &[]);
                writeln!(out, " {v}").unwrap();
            }
            Series::Gauge(v) => {
                write_series(out, name, labels, &[]);
                writeln!(out, " {}", fmt_f64(*v)).unwrap();
            }
            Series::Histogram(state) => {
                let bounds = default_buckets(unit);
                // Cumulative bucket counts: each `le` bucket is the count of observations <= le.
                let mut cumulative = 0u64;
                for (i, &le) in bounds.iter().enumerate() {
                    cumulative += state.counts[i];
                    write_series(
                        out,
                        &format!("{name}_bucket"),
                        labels,
                        &[("le", &fmt_f64(le))],
                    );
                    writeln!(out, " {cumulative}").unwrap();
                }
                // The `+Inf` bucket always equals the total observation count.
                write_series(out, &format!("{name}_bucket"), labels, &[("le", "+Inf")]);
                writeln!(out, " {}", state.count).unwrap();
                write_series(out, &format!("{name}_sum"), labels, &[]);
                writeln!(out, " {}", fmt_f64(state.sum)).unwrap();
                write_series(out, &format!("{name}_count"), labels, &[]);
                writeln!(out, " {}", state.count).unwrap();
            }
        }
    }
}

/// Writes `name{labels...}` (no trailing space or value) into `out`.
///
/// `base` are the aggregation-derived labels; `extra` are per-sample labels (`le`, `result`) that
/// are appended after them. When both are empty, no `{}` is written.
fn write_series(out: &mut String, name: &str, base: &[(String, String)], extra: &[(&str, &str)]) {
    out.push_str(name);
    if base.is_empty() && extra.is_empty() {
        return;
    }
    out.push('{');
    let mut first = true;
    for (k, v) in base {
        write_label(out, &mut first, k, v);
    }
    for (k, v) in extra {
        write_label(out, &mut first, k, v);
    }
    out.push('}');
}

/// Writes a single `key="value"` label, prefixing a `,` when it is not the first.
fn write_label(out: &mut String, first: &mut bool, key: &str, value: &str) {
    if !*first {
        out.push(',');
    }
    *first = false;
    out.push_str(key);
    out.push_str("=\"");
    write_escaped_value(out, value);
    out.push('"');
}

/// Composite series key for one side of a bool counter, so the two sides are distinct series under
/// the same family but keep the aggregation for label rendering. Encoded as `result=<side>` prefixed
/// onto the raw aggregation; [`parse_labels`] recovers both.
fn bool_key(aggregation: Option<&str>, side: &str) -> String {
    match aggregation.filter(|a| !a.is_empty()) {
        Some(agg) => format!("result={side}\u{1f}{agg}"),
        None => format!("result={side}"),
    }
}

/// Parses a series key into ordered `(label_name, label_value)` pairs.
///
/// Handles the [`bool_key`] `result=<side>` prefix (delimited by an ASCII unit separator) and the
/// aggregation convention: `Kind|value` -> `{kind="value"}`, a bare non-empty string ->
/// `{aggregation="value"}`, and an empty string -> no labels.
fn parse_labels(key: &str) -> Vec<(String, String)> {
    let mut labels = Vec::new();
    let aggregation = if let Some(rest) = key.strip_prefix("result=") {
        // `result=<side>` optionally followed by `\u{1f}<aggregation>`.
        let (side, agg) = match rest.split_once('\u{1f}') {
            Some((side, agg)) => (side, agg),
            None => (rest, ""),
        };
        labels.push(("result".to_string(), side.to_string()));
        agg
    } else {
        key
    };

    if !aggregation.is_empty() {
        match aggregation.split_once('|') {
            Some((kind, value)) => {
                labels.push((sanitize_label_name(kind), value.to_string()));
            }
            None => labels.push(("aggregation".to_string(), aggregation.to_string())),
        }
    }
    labels
}

/// Default `le` bucket boundaries for a unit, in the reported (converted) magnitude.
///
/// The crate histogram has thousands of fine buckets, far too many to emit per scrape; the reporter
/// collapses them onto this bounded, per-unit boundary set. Time units report **seconds** (values
/// are stored as nanoseconds), bytes and counts report their raw magnitude, and percent reports a
/// whole percentage.
fn default_buckets(unit: Unit) -> &'static [f64] {
    match unit {
        Unit::Microsecond | Unit::Second => &[
            0.000_001, 0.000_01, 0.000_1, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0,
            2.5, 5.0, 10.0, 30.0, 60.0,
        ],
        Unit::Byte => &[
            64.0,
            256.0,
            1024.0,
            4096.0,
            16384.0,
            65536.0,
            262_144.0,
            1_048_576.0,
            4_194_304.0,
            16_777_216.0,
            67_108_864.0,
            268_435_456.0,
            1_073_741_824.0,
        ],
        Unit::Count => &[
            1.0,
            2.0,
            5.0,
            10.0,
            25.0,
            50.0,
            100.0,
            250.0,
            500.0,
            1000.0,
            5000.0,
            10000.0,
            50000.0,
            100_000.0,
            1_000_000.0,
        ],
        Unit::Percent => &[1.0, 5.0, 10.0, 25.0, 50.0, 75.0, 90.0, 95.0, 99.0, 100.0],
    }
}

/// Converts a native histogram bucket value to its reported Prometheus magnitude.
///
/// Mirrors the storage conventions the statsd backend documents: time units store nanoseconds (from
/// `record_duration`) and are reported as seconds; `Count`/`Byte` are raw; `Percent` is stored
/// scaled by `FLOAT_INT_MULTIPLIER` (1000) and is reported as a whole percentage.
fn to_reported(value: u64, unit: Unit) -> f64 {
    match unit {
        Unit::Microsecond | Unit::Second => value as f64 / 1_000_000_000.0,
        Unit::Count | Unit::Byte => value as f64,
        Unit::Percent => value as f64 / crate::summary::FLOAT_INT_MULTIPLIER_U64 as f64,
    }
}

/// Formats an `f64` for the exposition format: integers render without a decimal point, and
/// non-finite values map to the Prometheus spellings (`+Inf`, `-Inf`, `NaN`).
fn fmt_f64(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_string()
    } else if v.is_infinite() {
        if v > 0.0 {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        }
    } else if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Appends the sanitized, prefixed metric name to `out`.
///
/// Sanitization keeps `[a-zA-Z0-9_:]` and maps everything else to `_`, matching the Prometheus
/// metric-name grammar. A prefix, when non-empty, is joined to the name with `_`.
fn write_name(out: &mut String, prefix: Option<&str>, name: &str) {
    if let Some(prefix) = prefix.filter(|p| !p.is_empty()) {
        let before = out.len();
        write_sanitized_name(out, prefix);
        if out.len() != before {
            out.push('_');
        }
    }
    write_sanitized_name(out, name);
}

/// Appends `input` to `out`, sanitized for a Prometheus metric name.
fn write_sanitized_name(out: &mut String, input: &str) {
    out.reserve(input.len());
    for c in input.chars() {
        out.push(match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | ':' => c,
            _ => '_',
        });
    }
}

/// Sanitizes a label name to the Prometheus grammar (`[a-zA-Z_][a-zA-Z0-9_]*`), lowercasing ASCII
/// letters so `Variant` -> `variant`.
fn sanitize_label_name(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        out.push(match c {
            'A'..='Z' => c.to_ascii_lowercase(),
            'a'..='z' | '0'..='9' | '_' => c,
            _ => '_',
        });
    }
    // A label must not start with a digit.
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// Appends `value` to `out`, escaping `\`, `"`, and newlines as the exposition format requires.
fn write_escaped_value(out: &mut String, value: &str) {
    out.reserve(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn info<'a>(
        name: &'a str,
        agg: Option<&'a str>,
        unit: Unit,
        kind: MetricKind,
    ) -> MetricInfo<'a> {
        MetricInfo::new(name, agg, unit, kind)
    }

    /// Collects the rendered lines for easy assertions.
    fn lines(text: &str) -> Vec<&str> {
        text.lines().collect()
    }

    #[test]
    fn counter_accumulates_across_reports() {
        let (mut backend, handle) = PrometheusBackend::new(Some("svc".into()));

        // First report: delta 7.
        backend.report_start(&ReportOptions::default());
        backend.record_counter(&info("rx.data", None, Unit::Count, MetricKind::Counter), 7);
        backend.report_end();
        assert!(lines(&handle.render()).contains(&"svc_rx_data 7"));

        // Second report: delta 5 -> cumulative 12 (proves accumulation, not last-write).
        backend.report_start(&ReportOptions::default());
        backend.record_counter(&info("rx.data", None, Unit::Count, MetricKind::Counter), 5);
        backend.report_end();
        let rendered = handle.render();
        assert!(lines(&rendered).contains(&"svc_rx_data 12"), "{rendered}");
        // TYPE line present exactly once.
        assert_eq!(
            rendered.matches("# TYPE svc_rx_data counter").count(),
            1,
            "{rendered}"
        );
    }

    #[test]
    fn zero_counter_delta_retains_existing_series() {
        let (mut backend, handle) = PrometheusBackend::new(None);
        backend.report_start(&ReportOptions::default());
        backend.record_counter(&info("c", None, Unit::Count, MetricKind::Counter), 4);
        backend.report_end();

        // A zero delta must not drop the series; the cumulative total stays and keeps rendering.
        backend.report_start(&ReportOptions::default());
        backend.record_counter(&info("c", None, Unit::Count, MetricKind::Counter), 0);
        backend.report_end();
        assert!(lines(&handle.render()).contains(&"c 4"));
    }

    #[test]
    fn never_nonzero_counter_is_suppressed_unless_sparse() {
        let (mut backend, handle) = PrometheusBackend::new(None);
        backend.report_start(&ReportOptions::new(false));
        backend.record_counter(&info("c", None, Unit::Count, MetricKind::Counter), 0);
        backend.report_end();
        assert_eq!(handle.render(), "");

        // Sparse keeps it.
        backend.report_start(&ReportOptions::new(true));
        backend.record_counter(&info("c", None, Unit::Count, MetricKind::Counter), 0);
        backend.report_end();
        assert!(lines(&handle.render()).contains(&"c 0"));
    }

    #[test]
    fn aggregation_becomes_a_label() {
        let (mut backend, handle) = PrometheusBackend::new(None);
        backend.report_start(&ReportOptions::default());
        backend.record_counter(
            &info(
                "rx.ecn",
                Some("Variant|ect0"),
                Unit::Count,
                MetricKind::Counter,
            ),
            9,
        );
        backend.record_counter(
            &info(
                "rx.ecn",
                Some("Variant|ce"),
                Unit::Count,
                MetricKind::Counter,
            ),
            3,
        );
        backend.report_end();
        let rendered = handle.render();
        assert!(
            lines(&rendered).contains(&"rx_ecn{variant=\"ect0\"} 9"),
            "{rendered}"
        );
        assert!(
            lines(&rendered).contains(&"rx_ecn{variant=\"ce\"} 3"),
            "{rendered}"
        );
        // Both series share one TYPE line.
        assert_eq!(rendered.matches("# TYPE rx_ecn counter").count(), 1);
    }

    #[test]
    fn bare_aggregation_uses_generic_label() {
        let (mut backend, handle) = PrometheusBackend::new(None);
        backend.report_start(&ReportOptions::default());
        backend.record_counter(
            &info("m", Some("misc"), Unit::Count, MetricKind::Counter),
            1,
        );
        backend.report_end();
        assert!(lines(&handle.render()).contains(&"m{aggregation=\"misc\"} 1"));
    }

    #[test]
    fn gauge_is_last_write_wins() {
        let (mut backend, handle) = PrometheusBackend::new(None);
        backend.report_start(&ReportOptions::default());
        backend.record_gauge(&info("q.depth", None, Unit::Count, MetricKind::Gauge), 7);
        backend.report_end();
        assert!(lines(&handle.render()).contains(&"q_depth 7"));

        // A later report overwrites (does not accumulate).
        backend.report_start(&ReportOptions::default());
        backend.record_gauge(&info("q.depth", None, Unit::Count, MetricKind::Gauge), 3);
        backend.report_end();
        let rendered = handle.render();
        assert!(lines(&rendered).contains(&"q_depth 3"), "{rendered}");
        assert!(!lines(&rendered).contains(&"q_depth 10"));
    }

    #[test]
    fn bool_counter_uses_result_label_and_accumulates() {
        let (mut backend, handle) = PrometheusBackend::new(None);
        backend.report_start(&ReportOptions::default());
        backend.record_bool(
            &info("connect", None, Unit::Count, MetricKind::BoolCounter),
            2,
            1,
        );
        backend.report_end();

        backend.report_start(&ReportOptions::default());
        backend.record_bool(
            &info("connect", None, Unit::Count, MetricKind::BoolCounter),
            3,
            0,
        );
        backend.report_end();

        let rendered = handle.render();
        assert!(
            lines(&rendered).contains(&"connect{result=\"true\"} 5"),
            "{rendered}"
        );
        assert!(
            lines(&rendered).contains(&"connect{result=\"false\"} 1"),
            "{rendered}"
        );
        assert_eq!(rendered.matches("# TYPE connect counter").count(), 1);
    }

    #[test]
    fn callback_sums_and_is_gauge() {
        let (mut backend, handle) = PrometheusBackend::new(None);
        backend.report_start(&ReportOptions::default());
        let a: u64 = 3;
        let b: u64 = 4;
        let values: [&dyn CallbackValue; 2] = [&a, &b];
        backend.record_callback(
            &info("workers", None, Unit::Count, MetricKind::CallbackScalar),
            &values,
        );
        backend.report_end();
        let rendered = handle.render();
        assert!(rendered.contains("# TYPE workers gauge"), "{rendered}");
        assert!(lines(&rendered).contains(&"workers 7"), "{rendered}");
    }

    #[test]
    fn histogram_emits_cumulative_buckets_sum_and_count() {
        // Build a real Summary so bucketing is genuine. record_duration stores nanoseconds; the
        // backend reports seconds, so 5us -> 0.000005s lands in the 0.00001 le bucket.
        let registry = crate::Registry::new();
        let summary = registry.register_summary("task.time".into(), None, Unit::Microsecond);
        summary.record_duration(std::time::Duration::from_micros(5));
        summary.record_duration(std::time::Duration::from_micros(5));
        summary.record_duration(std::time::Duration::from_micros(10));

        let (mut backend, handle) = PrometheusBackend::new(None);
        registry.report(&mut backend);
        let rendered = handle.render();

        assert!(
            rendered.contains("# TYPE task_time histogram"),
            "{rendered}"
        );
        // Three observations total.
        assert!(
            lines(&rendered).contains(&"task_time_count 3"),
            "{rendered}"
        );
        // +Inf bucket equals the count.
        assert!(
            lines(&rendered).contains(&"task_time_bucket{le=\"+Inf\"} 3"),
            "{rendered}"
        );
        // Buckets are cumulative and monotonic: the highest bucket reaches 3, and the smallest that
        // any sample falls into is >= the number of small samples.
        let bucket_lines: Vec<&str> = rendered
            .lines()
            .filter(|l| l.starts_with("task_time_bucket"))
            .collect();
        let values: Vec<u64> = bucket_lines
            .iter()
            .map(|l| l.rsplit(' ').next().unwrap().parse().unwrap())
            .collect();
        assert!(
            values.windows(2).all(|w| w[0] <= w[1]),
            "buckets must be non-decreasing: {values:?}"
        );
    }

    #[test]
    fn histogram_accumulates_across_reports() {
        let registry = crate::Registry::new();
        let summary = registry.register_summary("lat".into(), None, Unit::Count);
        summary.record_value(5);

        let (mut backend, handle) = PrometheusBackend::new(None);
        registry.report(&mut backend);
        assert!(lines(&handle.render()).contains(&"lat_count 1"));

        // A second interval adds another observation -> cumulative count 2.
        summary.record_value(5);
        registry.report(&mut backend);
        assert!(lines(&handle.render()).contains(&"lat_count 2"));
    }

    #[test]
    fn label_values_are_escaped() {
        let (mut backend, handle) = PrometheusBackend::new(None);
        backend.report_start(&ReportOptions::default());
        backend.record_counter(
            &info(
                "m",
                Some("Variant|a\"b\\c"),
                Unit::Count,
                MetricKind::Counter,
            ),
            1,
        );
        backend.report_end();
        assert!(
            handle.render().contains("m{variant=\"a\\\"b\\\\c\"} 1"),
            "{}",
            handle.render()
        );
    }

    #[test]
    fn empty_report_publishes_empty_string() {
        let (mut backend, handle) = PrometheusBackend::new(None);
        backend.report_start(&ReportOptions::default());
        backend.report_end();
        assert_eq!(handle.render(), "");
    }

    #[test]
    fn handle_reflects_latest_publish() {
        let (mut backend, handle) = PrometheusBackend::new(None);
        // Before any report the handle is empty.
        assert_eq!(handle.render(), "");
        backend.report_start(&ReportOptions::default());
        backend.record_counter(&info("c", None, Unit::Count, MetricKind::Counter), 1);
        backend.report_end();
        assert!(!handle.render().is_empty());
    }
}
