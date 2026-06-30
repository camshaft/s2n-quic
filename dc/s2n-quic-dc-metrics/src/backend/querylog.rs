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
#[derive(Default)]
pub struct QuerylogBackend {
    output: String,
    include_sparse: bool,
}

impl QuerylogBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes the backend, returning the assembled metrics line.
    pub fn into_line(self) -> String {
        self.output
    }

    /// Appends a fully-formatted value for `info` as a `name=value [aggregation]` entry.
    fn push_entry(&mut self, info: &MetricInfo<'_>, value: &str) {
        // The first entry has no leading separator; derive that from whether anything has been
        // written yet (rather than a separate flag that could drift from the output state).
        if !self.output.is_empty() {
            self.output.push(',');
        }

        self.output.push_str(info.name);
        self.output.push('=');
        self.output.push_str(value);

        if let Some(agg) = info.aggregation {
            self.output.push(' ');
            self.output.push_str(agg);
        }
    }
}

impl Backend for QuerylogBackend {
    fn report_start(&mut self, options: &ReportOptions) {
        self.include_sparse = options.include_sparse;
    }

    fn record_counter(&mut self, info: &MetricInfo<'_>, value: u64) {
        if value == 0 && !self.include_sparse {
            return;
        }
        let mut buf = String::new();
        write!(buf, "{value}").unwrap();
        self.push_entry(info, &buf);
    }

    fn record_gauge(&mut self, info: &MetricInfo<'_>, value: i64) {
        // Gauges are zero-suppressed: a zero value is omitted entirely (historically via
        // `NonZeroDisplay`). Today gauges flow through the callback path; this direct method is
        // provided for structured callers and mirrors that zero-suppression rule.
        if value == 0 {
            return;
        }
        let mut buf = String::new();
        write!(buf, "{value}").unwrap();
        self.push_entry(info, &buf);
    }

    fn record_bool(&mut self, info: &MetricInfo<'_>, true_count: u64, false_count: u64) {
        // Bool counters always suppress the all-zero case (no `include_sparse` override),
        // reproducing `BoolCounter::take_current`.
        let value = match (true_count, false_count) {
            (0, 0) => return,
            (t, 0) => format!("1*{t}"),
            (0, f) => format!("0*{f}"),
            (t, f) => format!("1*{t}+0*{f}"),
        };
        self.push_entry(info, &value);
    }

    fn record_histogram(&mut self, info: &MetricInfo<'_>, hist: Histogram<'_>) {
        let total_count = hist.count();
        if total_count == 0 && !self.include_sparse {
            return;
        }

        let mut buf = String::new();
        if total_count == 0 {
            // include_sparse: emit a bare "0" (plus unit suffix) like the historical formatter.
            buf.push('0');
        } else {
            hist.fmt_querylog_buckets(&mut buf, total_count);
        }
        buf.push_str(hist.unit().pmet_str());
        self.push_entry(info, &buf);
    }

    fn record_callback(&mut self, info: &MetricInfo<'_>, values: &[&dyn CallbackValue]) {
        // No callbacks registered under this name: emit nothing (avoids a bare unit suffix),
        // matching the historical `ValueList::take_current` empty-output guard.
        if values.is_empty() {
            return;
        }

        // Join each callback's exact display form with `+`, preserving each value's original type
        // formatting (e.g. an `f32` percent is not widened to `f64`).
        let mut buf = String::new();
        let mut first = true;
        let mut all_zero = true;
        for value in values {
            if value.as_f64() != 0.0 {
                all_zero = false;
            }
            if !first {
                buf.push('+');
            }
            first = false;
            value.fmt_value(&mut buf);
        }

        // Zero-suppressed metrics (gauges) drop an all-zero value entirely, reproducing the
        // `NonZeroDisplay` empty-string behavior.
        //
        // The historical code suppressed each zero value *individually* (a `NonZeroDisplay(0)`
        // rendered empty while the `+` separator still advanced), so a hypothetical mixed list
        // `[0, 5]` produced `+5`. We instead suppress only when the whole list is zero. This can
        // only differ when two or more zero-suppressed callbacks share one (name, aggregation),
        // which no call site does — every gauge registers under a distinct key — and the old
        // per-value form was malformed (leading `+`) anyway.
        if info.zero_suppressed && all_zero {
            return;
        }

        buf.push_str(info.unit.pmet_str());
        self.push_entry(info, &buf);
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
}
