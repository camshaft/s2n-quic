// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::Write as _;

use crate::backend::{Backend, CallbackValue, Histogram, MetricInfo, ReportOptions};

/// A [`Backend`] that produces the historical "querylog" metrics line: a comma-separated list of
/// `name=value [aggregation]` entries.
///
/// This is the backward-compatibility anchor -- it reproduces byte-for-byte the string previously
/// built by `Registry::try_take_current_metrics_line`. It also owns the sparse/zero emit policy:
/// counters and histograms honor [`ReportOptions::include_sparse`], while gauges, bool counters, and
/// any [`MetricInfo::zero_suppressed`] callback drop zeros regardless (matching historical
/// behavior).
///
/// Reuse it across reports: [`report_start`](Backend::report_start) clears the line buffer while
/// keeping its capacity, so a backend held by a long-lived reporter stops allocating once the
/// buffer reaches its high-water mark. Read the result with [`line`](Self::line) after each report;
/// [`into_line`](Self::into_line) is for the one-shot case (it moves the buffer out, forfeiting the
/// retained capacity).
#[derive(Default)]
pub struct QuerylogBackend {
    output: String,
    include_sparse: bool,
}

impl QuerylogBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// The metrics line assembled by the most recent report pass.
    ///
    /// Valid until the next [`report_start`](Backend::report_start), which clears it for reuse.
    pub fn line(&self) -> &str {
        &self.output
    }

    /// Consumes the backend, returning the assembled metrics line.
    ///
    /// Prefer [`line`](Self::line) when reusing the backend across reports; this moves the buffer
    /// out and so gives up the retained capacity.
    pub fn into_line(self) -> String {
        self.output
    }

    /// Writes the `,name=` prefix for a new entry, returning a `&mut String` to append the value
    /// (and any aggregation suffix via [`finish_entry`](Self::finish_entry)) directly into.
    ///
    /// Callers must only call this once they've decided the entry will be emitted, since it commits
    /// the separator and name to the output.
    fn begin_entry(&mut self, info: &MetricInfo<'_>) -> &mut String {
        // The first entry has no leading separator; derive that from whether anything has been
        // written yet (rather than a separate flag that could drift from the output state).
        if !self.output.is_empty() {
            self.output.push(',');
        }
        self.output.push_str(info.name);
        self.output.push('=');
        &mut self.output
    }

    /// Appends the optional ` aggregation` suffix after a value has been written.
    fn finish_entry(&mut self, info: &MetricInfo<'_>) {
        if let Some(agg) = info.aggregation {
            self.output.push(' ');
            self.output.push_str(agg);
        }
    }
}

impl Backend for QuerylogBackend {
    fn report_start(&mut self, options: &ReportOptions) {
        // Clear retains the allocated capacity, so a reused backend stops reallocating once the
        // buffer reaches the high-water mark across reports.
        self.output.clear();
        self.include_sparse = options.include_sparse;
    }

    fn record_counter(&mut self, info: &MetricInfo<'_>, value: u64) {
        if value == 0 && !self.include_sparse {
            return;
        }
        write!(self.begin_entry(info), "{value}").unwrap();
        self.finish_entry(info);
    }

    fn record_gauge(&mut self, info: &MetricInfo<'_>, value: i64) {
        // Gauges are zero-suppressed: a zero value is omitted entirely (historically via
        // `NonZeroDisplay`). Today gauges flow through the callback path; this direct method is
        // provided for structured callers and mirrors that zero-suppression rule.
        if value == 0 {
            return;
        }
        write!(self.begin_entry(info), "{value}").unwrap();
        self.finish_entry(info);
    }

    fn record_bool(&mut self, info: &MetricInfo<'_>, true_count: u64, false_count: u64) {
        // Bool counters always suppress the all-zero case (no `include_sparse` override),
        // reproducing `BoolCounter::take_current`.
        if (true_count, false_count) == (0, 0) {
            return;
        }
        let out = self.begin_entry(info);
        match (true_count, false_count) {
            (t, 0) => write!(out, "1*{t}").unwrap(),
            (0, f) => write!(out, "0*{f}").unwrap(),
            (t, f) => write!(out, "1*{t}+0*{f}").unwrap(),
        }
        self.finish_entry(info);
    }

    fn record_histogram(&mut self, info: &MetricInfo<'_>, hist: Histogram<'_>) {
        let total_count = hist.count();
        if total_count == 0 && !self.include_sparse {
            return;
        }

        let out = self.begin_entry(info);
        if total_count == 0 {
            // include_sparse: emit a bare "0" (plus unit suffix) like the historical formatter.
            out.push('0');
        } else {
            hist.fmt_querylog_buckets(out, total_count);
        }
        out.push_str(hist.unit().pmet_str());
        self.finish_entry(info);
    }

    fn record_callback(&mut self, info: &MetricInfo<'_>, values: &[&dyn CallbackValue]) {
        // No callbacks registered under this name: emit nothing (avoids a bare unit suffix),
        // matching the historical `ValueList::take_current` empty-output guard.
        if values.is_empty() {
            return;
        }

        // Zero-suppressed metrics (gauges) drop an all-zero value entirely, reproducing the
        // `NonZeroDisplay` empty-string behavior. Decide this up front with a cheap value-only
        // pre-pass (no formatting) so we can write directly into the output once committed.
        //
        // The historical code suppressed each zero value *individually* (a `NonZeroDisplay(0)`
        // rendered empty while the `+` separator still advanced), so a hypothetical mixed list
        // `[0, 5]` produced `+5`. We instead suppress only when the whole list is zero. This can
        // only differ when two or more zero-suppressed callbacks share one (name, aggregation),
        // which no call site does — every gauge registers under a distinct key — and the old
        // per-value form was malformed (leading `+`) anyway.
        if info.zero_suppressed && values.iter().all(|v| v.as_f64() == 0.0) {
            return;
        }

        // Join each callback's exact display form with `+`, preserving each value's original type
        // formatting (e.g. an `f32` percent is not widened to `f64`).
        let out = self.begin_entry(info);
        for (i, value) in values.iter().enumerate() {
            if i != 0 {
                out.push('+');
            }
            value.fmt_value(out);
        }
        out.push_str(info.unit.pmet_str());
        self.finish_entry(info);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::backend::MetricKind;

    /// `Default` and `new()` must behave identically — in particular, neither may emit a leading
    /// separator on the first entry.
    #[test]
    fn default_matches_new_no_leading_comma() {
        let info = MetricInfo::new("a", None, crate::Unit::Count, MetricKind::Counter);

        let mut from_new = QuerylogBackend::new();
        from_new.record_counter(&info, 5);
        assert_eq!(from_new.into_line(), "a=5");

        let mut from_default = QuerylogBackend::default();
        from_default.record_counter(&info, 5);
        assert_eq!(from_default.into_line(), "a=5");
    }

    /// A backend reused across reports must clear prior output on `report_start` (no stale data,
    /// no leading separator) while retaining its buffer capacity.
    #[test]
    fn reuse_clears_output_and_retains_capacity() {
        let info = MetricInfo::new("a", None, crate::Unit::Count, MetricKind::Counter);
        let opts = ReportOptions::default();

        let mut backend = QuerylogBackend::new();

        backend.report_start(&opts);
        backend.record_counter(&info, 5);
        assert_eq!(backend.line(), "a=5");
        let cap_after_first = backend.output.capacity();

        // Second report: report_start must clear the prior line, not append to it.
        backend.report_start(&opts);
        backend.record_counter(&info, 9);
        assert_eq!(backend.line(), "a=9");

        // Capacity is retained across the reset (clear, not reallocate).
        assert!(backend.output.capacity() >= cap_after_first);
    }
}
