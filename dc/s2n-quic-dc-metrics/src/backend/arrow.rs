// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A [`Backend`] that accumulates native metric values into an [`arrow::RecordBatch`].
//!
//! This is the *accumulation* half of Arrow/Parquet export. Following the same split as
//! [`QuerylogBackend`](super::QuerylogBackend) (which hands back a `String`), this backend hands
//! back an in-memory `RecordBatch`; the caller decides how to serialize it (Parquet, IPC, ...) and
//! owns the file lifecycle — including rotation, which for Parquet means a fresh writer + footer per
//! file rather than the byte-appending [`MetricsWriter`](crate::MetricsWriter). Keeping the heavy
//! `parquet` dependency out of this crate is deliberate: we depend only on `arrow`, behind the
//! opt-in `arrow` feature.
//!
//! # Schema
//!
//! One row per registered metric per report, at the *raw native altitude*: each metric is emitted
//! as-is with no name-convention grouping (unlike the querylog string parser, which folds
//! `.enq`/`.drain`/`.depth` into a single queue row, etc.). The value lives in a per-kind column
//! (`counter`, `gauge`, `bool_true`/`bool_false`, `callback`, or the `hist_*` group) with the other
//! kinds' columns left null, so the native richness the string path can't express — a real signed
//! gauge, both bool sides, every callback value (not their sum), the full native bucket map — is
//! preserved.
//!
//! # Reuse
//!
//! Like every [`Backend`], this is meant to be long-lived: [`report_start`](Backend::report_start)
//! clears the accumulated rows while retaining builder capacity, and [`finish`](Self::finish)
//! produces the batch for the just-completed report (also resetting for the next). Set the
//! per-report timestamp with [`set_timestamp`](Self::set_timestamp) before each report; it is
//! stamped onto every row so the crate never needs a wall-clock source of its own (which also keeps
//! it deterministic under simulation).

use crate::{
    backend::{Backend, CallbackValue, Histogram, MetricInfo, ReportOptions},
    Unit,
};
use arrow::{
    array::{
        ArrayRef, BooleanBuilder, Float64Builder, Int64Builder, ListBuilder, MapBuilder,
        RecordBatch, StringBuilder, UInt64Builder,
    },
    datatypes::{DataType, Field, Schema, SchemaRef},
};
use std::sync::{Arc, OnceLock};

/// The clean, convention-free Arrow schema produced by [`ArrowBackend`]. Cached so repeated
/// [`finish`](ArrowBackend::finish) calls reuse one `SchemaRef`.
pub fn schema() -> SchemaRef {
    static SCHEMA: OnceLock<SchemaRef> = OnceLock::new();
    SCHEMA
        .get_or_init(|| {
            // Map<UInt64, UInt64> for histogram buckets: representative (midpoint) value -> count.
            // The field names (`entries`/`keys`/`values`) match arrow's `MapBuilder` defaults so the
            // built array's type matches this declared type exactly.
            let bucket_entries = Field::new(
                "entries",
                DataType::Struct(
                    vec![
                        Arc::new(Field::new("keys", DataType::UInt64, false)),
                        // `values` is nullable to match arrow's `MapBuilder::finish` default field.
                        Arc::new(Field::new("values", DataType::UInt64, true)),
                    ]
                    .into(),
                ),
                false,
            );

            Arc::new(Schema::new(vec![
                // ── identity ──────────────────────────────────────────────────────────
                Field::new("ts", DataType::Float64, false),
                Field::new("name", DataType::Utf8, false),
                Field::new("kind", DataType::Utf8, false),
                Field::new("unit", DataType::Utf8, false),
                Field::new("aggregation", DataType::Utf8, true),
                Field::new("zero_suppressed", DataType::Boolean, false),
                // ── per-kind value columns (all nullable; exactly one group is set per row) ──
                Field::new("counter", DataType::UInt64, true),
                Field::new("gauge", DataType::Int64, true),
                Field::new("bool_true", DataType::UInt64, true),
                Field::new("bool_false", DataType::UInt64, true),
                Field::new(
                    "callback",
                    // Nullable `item` to match arrow's `ListBuilder::finish` default list field.
                    DataType::List(Arc::new(Field::new("item", DataType::Float64, true))),
                    true,
                ),
                // `hist_count` is the exact total sample count (the histogram's natural weight and
                // the ubiquitous "did anything happen" filter). min/p50/p99/max are deliberately
                // NOT stored: they're bucket-midpoint approximations that a query can reconstruct
                // from `hist_buckets` (min/max = min/max key; quantiles = cumulative-count walk),
                // so baking them in would just be a lossy, convention-encoding denormalization.
                Field::new("hist_count", DataType::UInt64, true),
                Field::new("hist_buckets", DataType::Map(Arc::new(bucket_entries), false), true),
            ]))
        })
        .clone()
}

/// A [`Backend`] that accumulates native metric values into an [`arrow::RecordBatch`].
///
/// See the [module docs](self) for the schema and the accumulate/finish contract.
pub struct ArrowBackend {
    timestamp: f64,

    ts: Float64Builder,
    name: StringBuilder,
    kind: StringBuilder,
    unit: StringBuilder,
    aggregation: StringBuilder,
    zero_suppressed: BooleanBuilder,

    counter: UInt64Builder,
    gauge: Int64Builder,
    bool_true: UInt64Builder,
    bool_false: UInt64Builder,
    callback: ListBuilder<Float64Builder>,
    hist_count: UInt64Builder,
    hist_buckets: MapBuilder<UInt64Builder, UInt64Builder>,

    rows: usize,
}

impl Default for ArrowBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ArrowBackend {
    pub fn new() -> Self {
        Self {
            timestamp: 0.0,
            ts: Float64Builder::new(),
            name: StringBuilder::new(),
            kind: StringBuilder::new(),
            unit: StringBuilder::new(),
            aggregation: StringBuilder::new(),
            zero_suppressed: BooleanBuilder::new(),
            counter: UInt64Builder::new(),
            gauge: Int64Builder::new(),
            bool_true: UInt64Builder::new(),
            bool_false: UInt64Builder::new(),
            callback: ListBuilder::new(Float64Builder::new()),
            hist_count: UInt64Builder::new(),
            hist_buckets: MapBuilder::new(None, UInt64Builder::new(), UInt64Builder::new()),
            rows: 0,
        }
    }

    /// The timestamp (unix-epoch seconds, fractional) stamped onto every row of the current report.
    ///
    /// Set this once per report (typically right before [`Registry::report`](crate::Registry::report)),
    /// so the crate never needs its own wall-clock source and stays deterministic under simulation.
    pub fn set_timestamp(&mut self, ts: f64) {
        self.timestamp = ts;
    }

    /// The number of rows accumulated for the in-progress report.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Builds a [`RecordBatch`] for the accumulated rows and resets for the next report (retaining
    /// builder capacity). An empty report yields a zero-row batch with the same [`schema`].
    ///
    /// `finish` is independent of [`report_start`](Backend::report_start): call it once after each
    /// report pass to obtain that report's batch.
    pub fn finish(&mut self) -> RecordBatch {
        let columns: Vec<ArrayRef> = vec![
            Arc::new(self.ts.finish()),
            Arc::new(self.name.finish()),
            Arc::new(self.kind.finish()),
            Arc::new(self.unit.finish()),
            Arc::new(self.aggregation.finish()),
            Arc::new(self.zero_suppressed.finish()),
            Arc::new(self.counter.finish()),
            Arc::new(self.gauge.finish()),
            Arc::new(self.bool_true.finish()),
            Arc::new(self.bool_false.finish()),
            Arc::new(self.callback.finish()),
            Arc::new(self.hist_count.finish()),
            Arc::new(self.hist_buckets.finish()),
        ];
        self.rows = 0;
        RecordBatch::try_new(schema(), columns).expect("schema mismatch in ArrowBackend")
    }

    /// Appends the identity columns shared by every row. Each `record_*` then fills its value
    /// column via one of the `*_row` helpers, which drive every value column exactly once (setting
    /// this kind's, nulling the rest) so all columns stay the same length.
    fn begin_row(&mut self, info: &MetricInfo<'_>, kind: &str) {
        self.ts.append_value(self.timestamp);
        self.name.append_value(info.name);
        self.kind.append_value(kind);
        self.unit.append_value(unit_str(info.unit));
        self.aggregation.append_option(info.aggregation);
        self.zero_suppressed.append_value(info.zero_suppressed);
    }

    /// Fills the value columns for a row, given each column's value (`None` → null). Centralizing
    /// this guarantees every column is appended to exactly once per row regardless of kind.
    #[allow(clippy::too_many_arguments)]
    fn append_values(
        &mut self,
        counter: Option<u64>,
        gauge: Option<i64>,
        bool_counts: Option<(u64, u64)>,
        callback: Option<&[&dyn CallbackValue]>,
        histogram: Option<Histogram<'_>>,
    ) {
        self.counter.append_option(counter);
        self.gauge.append_option(gauge);
        let (bool_true, bool_false) = match bool_counts {
            Some((t, f)) => (Some(t), Some(f)),
            None => (None, None),
        };
        self.bool_true.append_option(bool_true);
        self.bool_false.append_option(bool_false);

        match callback {
            Some(values) => {
                for value in values {
                    self.callback.values().append_value(value.as_f64());
                }
                self.callback.append(true);
            }
            None => self.callback.append_null(),
        }

        match histogram {
            Some(hist) => {
                // Store only the exact total count plus the raw non-empty buckets. min/p50/p99/max
                // are reconstructable from the bucket map at query time (see `schema`).
                self.hist_count.append_value(hist.count());
                for (value, bucket_count) in hist.buckets() {
                    self.hist_buckets.keys().append_value(value);
                    self.hist_buckets.values().append_value(bucket_count);
                }
                self.hist_buckets
                    .append(true)
                    .expect("map builder state inconsistent");
            }
            None => {
                self.hist_count.append_null();
                // Closes the (empty) map row as null.
                self.hist_buckets
                    .append(false)
                    .expect("map builder state inconsistent");
            }
        }

        self.rows += 1;
    }
}

impl Backend for ArrowBackend {
    fn report_start(&mut self, _options: &ReportOptions) {
        // `finish` already resets the builders; nothing to clear here. (We accept every value the
        // registry hands us regardless of `include_sparse` — a tabular time series wants gap-free
        // rows, and the null-vs-zero distinction is preserved in the columns, so downstream queries
        // can filter zeros themselves rather than the backend dropping them irreversibly.)
    }

    fn record_counter(&mut self, info: &MetricInfo<'_>, value: u64) {
        self.begin_row(info, "counter");
        self.append_values(Some(value), None, None, None, None);
    }

    fn record_gauge(&mut self, info: &MetricInfo<'_>, value: i64) {
        self.begin_row(info, "gauge");
        self.append_values(None, Some(value), None, None, None);
    }

    fn record_bool(&mut self, info: &MetricInfo<'_>, true_count: u64, false_count: u64) {
        self.begin_row(info, "bool");
        self.append_values(None, None, Some((true_count, false_count)), None, None);
    }

    fn record_histogram(&mut self, info: &MetricInfo<'_>, hist: Histogram<'_>) {
        self.begin_row(info, "histogram");
        self.append_values(None, None, None, None, Some(hist));
    }

    fn record_callback(&mut self, info: &MetricInfo<'_>, values: &[&dyn CallbackValue]) {
        self.begin_row(info, "callback");
        self.append_values(None, None, None, Some(values), None);
    }
}

fn unit_str(unit: Unit) -> &'static str {
    match unit {
        Unit::Count => "count",
        Unit::Microsecond => "us",
        Unit::Byte => "B",
        Unit::Second => "s",
        Unit::Percent => "%",
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{backend::MetricKind, Registry};
    use arrow::array::{
        Array, Float64Array, Int64Array, ListArray, MapArray, StringArray, UInt64Array,
    };

    fn col<'a>(batch: &'a RecordBatch, name: &str) -> &'a ArrayRef {
        let idx = batch.schema().index_of(name).unwrap();
        batch.column(idx)
    }

    fn u64_at(batch: &RecordBatch, name: &str, row: usize) -> Option<u64> {
        let arr = col(batch, name).as_any().downcast_ref::<UInt64Array>().unwrap();
        arr.is_valid(row).then(|| arr.value(row))
    }

    /// The full set of kinds must produce one row each, with only that kind's value column set and
    /// every other value column null — so all columns stay the same length and the row is
    /// self-describing.
    #[test]
    fn one_row_per_kind_with_disjoint_value_columns() {
        let registry = Registry::new();
        registry.register_counter("c".into(), None).increment(7);
        registry
            .register_summary("h".into(), Some("v1".into()), Unit::Microsecond)
            .record_duration(std::time::Duration::from_micros(5));
        let boolc = registry.register_bool("b".into(), None);
        boolc.record(true);
        boolc.record(false);
        boolc.record(true);
        registry.register_list_callback("cb".into(), None, Unit::Count, || 3u64);
        registry.register_list_callback_zero_suppressed("g".into(), None, Unit::Count, || 11i64);

        let mut backend = ArrowBackend::new();
        backend.set_timestamp(123.5);
        registry.report_with(&ReportOptions::new(true), &mut backend);
        let batch = backend.finish();

        // 5 metrics registered -> 5 rows (counter, histogram, bool, and two callbacks).
        assert_eq!(batch.num_rows(), 5);

        let names = col(&batch, "name")
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let kinds = col(&batch, "kind")
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();

        // Every row is stamped with the report timestamp.
        let ts = col(&batch, "ts").as_any().downcast_ref::<Float64Array>().unwrap();
        for row in 0..batch.num_rows() {
            assert_eq!(ts.value(row), 123.5);
        }

        // Locate the counter row and confirm only `counter` is populated.
        let counter_row = (0..batch.num_rows())
            .find(|&r| names.value(r) == "c")
            .expect("counter row");
        assert_eq!(kinds.value(counter_row), "counter");
        assert_eq!(u64_at(&batch, "counter", counter_row), Some(7));
        assert!(col(&batch, "gauge").is_null(counter_row));
        assert!(col(&batch, "bool_true").is_null(counter_row));
        assert!(col(&batch, "callback").is_null(counter_row));
        assert!(col(&batch, "hist_count").is_null(counter_row));

        // The bool row splits both sides (2 true, 1 false).
        let bool_row = (0..batch.num_rows())
            .find(|&r| names.value(r) == "b")
            .expect("bool row");
        assert_eq!(kinds.value(bool_row), "bool");
        assert_eq!(u64_at(&batch, "bool_true", bool_row), Some(2));
        assert_eq!(u64_at(&batch, "bool_false", bool_row), Some(1));
        assert!(col(&batch, "counter").is_null(bool_row));

        // The histogram row records the summary and the native bucket map + carries its aggregation.
        let hist_row = (0..batch.num_rows())
            .find(|&r| names.value(r) == "h")
            .expect("histogram row");
        assert_eq!(kinds.value(hist_row), "histogram");
        assert_eq!(u64_at(&batch, "hist_count", hist_row), Some(1));
        let agg = col(&batch, "aggregation")
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(agg.value(hist_row), "v1");
        let buckets = col(&batch, "hist_buckets")
            .as_any()
            .downcast_ref::<MapArray>()
            .unwrap();
        assert!(buckets.is_valid(hist_row));
        assert!(buckets.value(hist_row).len() >= 1, "at least one non-empty bucket");

        // The additive callback exposes its value list (not a scalar sum).
        let cb_row = (0..batch.num_rows())
            .find(|&r| names.value(r) == "cb")
            .expect("callback row");
        assert_eq!(kinds.value(cb_row), "callback");
        let callbacks = col(&batch, "callback")
            .as_any()
            .downcast_ref::<ListArray>()
            .unwrap();
        let values = callbacks.value(cb_row);
        let values = values.as_any().downcast_ref::<Float64Array>().unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values.value(0), 3.0);

        // The zero-suppressed gauge callback still emits its row (the backend keeps zeros); its
        // metadata bit is preserved.
        let g_row = (0..batch.num_rows())
            .find(|&r| names.value(r) == "g")
            .expect("gauge callback row");
        let zero_suppressed = col(&batch, "zero_suppressed")
            .as_any()
            .downcast_ref::<arrow::array::BooleanArray>()
            .unwrap();
        assert!(zero_suppressed.value(g_row));
    }

    /// `finish` resets the builders, so a second report is independent (and an empty report yields a
    /// zero-row batch with the same schema, never an error).
    #[test]
    fn finish_resets_between_reports() {
        let registry = Registry::new();
        registry.register_counter("c".into(), None).increment(1);

        let mut backend = ArrowBackend::new();
        registry.report(&mut backend);
        let first = backend.finish();
        assert_eq!(first.num_rows(), 1);

        // No new values recorded: the counter drained on the first report, so this report visits it
        // with value 0 (still a row — the backend keeps zeros).
        registry.report(&mut backend);
        let second = backend.finish();
        assert_eq!(second.num_rows(), 1);
        assert_eq!(u64_at(&second, "counter", 0), Some(0));
        assert_eq!(second.schema(), schema());

        // A finish with nothing recorded is a valid empty batch.
        let empty = backend.finish();
        assert_eq!(empty.num_rows(), 0);
        assert_eq!(empty.schema(), schema());
    }

    /// A direct native gauge (via `record_gauge`) lands a real signed value in the `gauge` column —
    /// richness the querylog/string path cannot express.
    #[test]
    fn native_gauge_column() {
        let mut backend = ArrowBackend::new();
        backend.report_start(&ReportOptions::default());
        let info = MetricInfo::new("q.depth", None, Unit::Count, MetricKind::Gauge);
        backend.record_gauge(&info, -5);
        let batch = backend.finish();

        assert_eq!(batch.num_rows(), 1);
        let gauge = col(&batch, "gauge").as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(gauge.value(0), -5);
        assert!(col(&batch, "counter").is_null(0));
    }
}
