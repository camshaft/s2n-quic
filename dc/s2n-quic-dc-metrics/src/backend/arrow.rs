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
//! # Labels
//!
//! Applications add their own dimensions (host, region, run id, ...) via
//! [`with_labels`](ArrowBackend::with_labels): each becomes an extra dictionary-encoded column on
//! every row, so a glob query over the produced files can filter/group on it directly. This is
//! preferred over file-level schema metadata, which query engines do not project as a queryable
//! column across a glob.
//!
//! # Reuse
//!
//! Like every [`Backend`], this is meant to be long-lived. The intended flow is to drain each
//! report with [`finish`](Self::finish), which produces the batch and resets the builders (while
//! retaining their capacity) for the next report. As a safety net, [`report_start`](Backend::report_start)
//! also discards any un-`finish`ed rows, so a caller that defers or forgets `finish` gets a dropped
//! partial rather than two reports mixed into one batch. Set the per-report timestamp with
//! [`set_timestamp`](Self::set_timestamp) before each report; it is stamped onto every row so the
//! crate never needs a wall-clock source of its own (which also keeps it deterministic under
//! simulation).

use crate::{
    backend::{Backend, CallbackValue, Histogram, MetricInfo, ReportOptions},
    Unit,
};
use arrow::{
    array::{
        ArrayRef, DictionaryArray, Float64Builder, Int32Array, Int64Builder, ListBuilder,
        MapBuilder, RecordBatch, StringArray, StringBuilder, UInt64Builder,
    },
    datatypes::{DataType, Field, Int32Type, Schema, SchemaRef},
};
use std::sync::{Arc, OnceLock};

/// The Arrow `DataType` of a label column: a dictionary-encoded UTF-8 string.
///
/// Labels are constant for a backend's lifetime, so every row repeats the same value. A plain
/// `Utf8` column would store that string N times; dictionary encoding stores the string once (in a
/// 1-entry values dictionary) plus an N-entry `Int32` keys buffer of all-zeros. This both shrinks
/// the in-memory batch and maps 1:1 onto Parquet's own dictionary encoding, so the write is a near
/// direct handoff. The dictionary is a physical encoding detail scoped below the logical value:
/// readers (e.g. DuckDB over a glob of files, each with its own local dictionary) decode to the
/// logical string before anything sees it, so cross-file queries behave identically to a plain
/// string column. The one caveat is in-memory Arrow-to-Arrow concat, which must unify dictionaries
/// — not a concern for the write-Parquet-then-query path, but worth knowing if a consumer streams
/// these batches over IPC/Flight and concatenates them.
fn label_data_type() -> DataType {
    DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8))
}

/// The clean, convention-free base Arrow schema (no [labels](ArrowBackend::with_labels)) produced by
/// [`ArrowBackend`]. Cached so repeated calls reuse one `SchemaRef`.
///
/// A labeled backend appends one dictionary column per label after these fields; use
/// [`ArrowBackend::schema`] to get the full schema (including labels) for constructing a writer.
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
                Field::new("sparsity", DataType::Utf8, false),
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
                Field::new(
                    "hist_buckets",
                    DataType::Map(Arc::new(bucket_entries), false),
                    true,
                ),
            ]))
        })
        .clone()
}

/// A precomputed constant label column source: the immutable 1-entry values dictionary, built once
/// in [`ArrowBackend::with_labels`] and cloned (refcount bump) into every batch.
struct Label {
    /// The 1-entry `StringArray` dictionary holding the constant label value.
    values: ArrayRef,
}

/// A [`Backend`] that accumulates native metric values into an [`arrow::RecordBatch`].
///
/// See the [module docs](self) for the schema and the accumulate/finish contract.
pub struct ArrowBackend {
    timestamp: f64,

    /// Constant labels appended as one dictionary column per label to every batch (e.g.
    /// `("host", "box-01")`). Fixed for the backend's lifetime, so both the `schema` and each
    /// label's 1-entry values dictionary are precomputed here and only cheaply reused per `finish`:
    /// the immutable `values` `Arc` is cloned (a refcount bump) and paired with a fresh all-zero
    /// keys buffer sized to the report's row count.
    labels: Vec<Label>,
    /// Full schema including the label columns, computed once from `labels`.
    schema: SchemaRef,

    ts: Float64Builder,
    name: StringBuilder,
    kind: StringBuilder,
    unit: StringBuilder,
    aggregation: StringBuilder,
    sparsity: StringBuilder,

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
            labels: Vec::new(),
            schema: schema(),
            ts: Float64Builder::new(),
            name: StringBuilder::new(),
            kind: StringBuilder::new(),
            unit: StringBuilder::new(),
            aggregation: StringBuilder::new(),
            sparsity: StringBuilder::new(),
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

    /// Attaches constant `(name, value)` labels, each emitted as an extra column on every row of
    /// every batch this backend produces (e.g. `("host", "box-01")`, `("region", "us-east-1")`).
    ///
    /// This is the recommended way for an application to add its own dimensions — host, region,
    /// run id, etc. — so that a glob query over the resulting Parquet files can filter/group on them
    /// with no per-file manipulation (unlike file-level schema metadata, which is not projected as a
    /// queryable column). Label columns are dictionary-encoded (see [`label_data_type`]), so the
    /// repeated constant value costs little in memory and maps directly onto Parquet's dictionary
    /// encoding.
    ///
    /// Labels are fixed for the backend's lifetime; call this once at construction. Label columns
    /// follow the base columns in order, so the schema is stable. A label whose name collides with a
    /// base column is still appended as a separate column (Arrow permits duplicate field names);
    /// avoid base names like `name`/`kind`/`unit` to keep queries unambiguous.
    pub fn with_labels(mut self, labels: impl IntoIterator<Item = (String, String)>) -> Self {
        // Precompute the full schema (base fields + one dictionary column per label, in order) and
        // each label's immutable 1-entry values dictionary, so `finish` only clones an `Arc` and
        // builds the row-length keys buffer.
        let mut fields: Vec<Field> = schema().fields().iter().map(|f| (**f).clone()).collect();
        self.labels = labels
            .into_iter()
            .map(|(name, value)| {
                // Non-nullable: every row always carries the constant label value.
                fields.push(Field::new(&name, label_data_type(), false));
                Label {
                    values: Arc::new(StringArray::from(vec![value])),
                }
            })
            .collect();
        self.schema = Arc::new(Schema::new(fields));
        self
    }

    /// The full schema of the batches this backend produces, including any
    /// [labels](Self::with_labels). Use this to construct the downstream writer.
    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
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
        let rows = self.rows;
        let mut columns: Vec<ArrayRef> = vec![
            Arc::new(self.ts.finish()),
            Arc::new(self.name.finish()),
            Arc::new(self.kind.finish()),
            Arc::new(self.unit.finish()),
            Arc::new(self.aggregation.finish()),
            Arc::new(self.sparsity.finish()),
            Arc::new(self.counter.finish()),
            Arc::new(self.gauge.finish()),
            Arc::new(self.bool_true.finish()),
            Arc::new(self.bool_false.finish()),
            Arc::new(self.callback.finish()),
            Arc::new(self.hist_count.finish()),
            Arc::new(self.hist_buckets.finish()),
        ];

        // Append one constant dictionary column per label. A constant column needs no builder loop
        // (and no per-row hashmap dedup): the values dictionary was built once in `with_labels` and
        // is cloned here, and the keys are all-zero — every row points at that single entry.
        for label in &self.labels {
            let keys = Int32Array::from(vec![0; rows]);
            let dict = DictionaryArray::<Int32Type>::try_new(keys, label.values.clone())
                .expect("constant label dictionary is well-formed");
            columns.push(Arc::new(dict));
        }

        self.rows = 0;
        RecordBatch::try_new(self.schema.clone(), columns).expect("schema mismatch in ArrowBackend")
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
        self.sparsity.append_value(info.sparsity.as_str());
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
        // Per the `Backend` contract, `report_start` is the per-report reset hook. In the normal
        // flow the caller drains each report with `finish` (which already resets the builders), so
        // `rows` is 0 here and this is a no-op that retains capacity. But if a caller defers or
        // forgets `finish`, resetting here prevents the next report's rows from being appended onto
        // the stale ones — a mixed batch is worse than a dropped partial. `finish` drains the
        // builders and we discard its batch, keeping the retained-capacity property.
        if self.rows != 0 {
            let _ = self.finish();
        }

        // Note: we intentionally ignore `include_sparse`. A tabular time series wants gap-free rows,
        // and the null-vs-zero distinction is preserved in the columns, so downstream queries can
        // filter zeros themselves rather than the backend dropping them irreversibly.
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
        let arr = col(batch, name)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
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
        let ts = col(&batch, "ts")
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
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
        assert!(
            buckets.value(hist_row).len() >= 1,
            "at least one non-empty bucket"
        );

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
        // sparse policy is preserved as the `sparsity` column.
        let g_row = (0..batch.num_rows())
            .find(|&r| names.value(r) == "g")
            .expect("gauge callback row");
        let sparsity = col(&batch, "sparsity")
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(sparsity.value(g_row), "sparse");
        // The always-on callback (`cb`) is dense.
        assert_eq!(sparsity.value(cb_row), "dense");
        // A plain counter inherits the report policy.
        assert_eq!(sparsity.value(counter_row), "inherit");
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
        let name: Arc<str> = Arc::from("q.depth");
        let info = MetricInfo::new(&name, None, Unit::Count, MetricKind::Gauge);
        backend.record_gauge(&info, -5);
        let batch = backend.finish();

        assert_eq!(batch.num_rows(), 1);
        let gauge = col(&batch, "gauge")
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(gauge.value(0), -5);
        assert!(col(&batch, "counter").is_null(0));
    }

    /// If a caller forgets `finish` between reports, `report_start` must discard the stale partial
    /// rather than let the next report's rows be appended onto it (producing a mixed batch).
    #[test]
    fn report_start_discards_unfinished_rows() {
        let registry = Registry::new();
        registry.register_counter("c".into(), None).increment(1);

        let mut backend = ArrowBackend::new();
        // First report — but the caller "forgets" to call finish().
        registry.report(&mut backend);
        assert_eq!(backend.rows(), 1);

        // Second report begins: report_start drops the un-finished first report.
        registry.report(&mut backend);
        let batch = backend.finish();

        // Exactly one report's worth of rows (the counter drained to 0 on the first report), not
        // two reports mixed together.
        assert_eq!(batch.num_rows(), 1);
    }

    /// `with_labels` adds a dictionary column per label, carrying its constant value on every row,
    /// and the label columns follow the base columns in the schema.
    #[test]
    fn labels_add_constant_dictionary_columns() {
        use arrow::{array::DictionaryArray, datatypes::Int32Type};

        let registry = Registry::new();
        registry.register_counter("c".into(), None).increment(1);
        registry.register_counter("d".into(), None).increment(2);

        let mut backend = ArrowBackend::new().with_labels([
            ("host".to_string(), "box-01".to_string()),
            ("region".to_string(), "us-east-1".to_string()),
        ]);
        registry.report(&mut backend);
        let batch = backend.finish();

        assert_eq!(batch.num_rows(), 2);

        // Label columns follow the base columns and are dictionary-typed.
        assert_eq!(batch.schema(), backend.schema());
        assert_eq!(
            batch.schema().field_with_name("host").unwrap().data_type(),
            &label_data_type()
        );

        // Every row carries the constant label value (decoded through the dictionary).
        let host = col(&batch, "host")
            .as_any()
            .downcast_ref::<DictionaryArray<Int32Type>>()
            .unwrap();
        let host_values = host
            .values()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        for row in 0..batch.num_rows() {
            assert_eq!(host_values.value(host.keys().value(row) as usize), "box-01");
        }

        // A no-label backend keeps the base schema exactly.
        let plain = ArrowBackend::new();
        assert_eq!(plain.schema(), schema());
    }
}
