// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A push-based [`Backend`] that encodes metrics as OTLP protobuf payloads.
//!
//! # Design
//!
//! Like [`StatsdBackend`](super::StatsdBackend), this backend is push-based and follows the
//! format-vs-emit split: the backend formats each report into a binary
//! `ExportMetricsServiceRequest` protobuf payload, then hands it to an [`OtlpSink`] in
//! [`report_end`](Backend::report_end). The sink decides how to transmit the bytes (HTTP POST to
//! an OTLP collector, gRPC, a queue, etc.). This keeps the heavy transport concern — connection
//! pooling, retry, TLS, gRPC framing — out of the crate entirely.
//!
//! The protobuf types are hand-written using [`prost`]'s derive macros (no build-time code
//! generation or `.proto` files). They cover exactly the OTLP metrics wire format fields this
//! backend uses, so there is no dependency on the OpenTelemetry SDK.
//!
//! # Metric mapping
//!
//! | s2n-quic-dc-metrics kind | OTLP type          | Notes                                    |
//! |--------------------------|--------------------|-----------------------------------------|
//! | counter                  | Sum, delta, mono   | Delta temporality; one data point        |
//! | gauge                    | Gauge              | Instantaneous signed value               |
//! | bool counter             | Sum, delta, mono   | Two data points: `result="true/false"`   |
//! | histogram                | Histogram, delta   | Explicit bounds (per-unit fixed set)     |
//! | callback scalar          | Gauge              | Sum of all callback values               |
//!
//! # Histogram encoding
//!
//! The crate histogram has thousands of fine-grained buckets. To produce OTLP payloads of a
//! manageable size, the backend collapses them onto a compact, per-unit fixed set of explicit
//! `le` boundaries — the same boundaries used by the Prometheus backend for consistency. The
//! overflow bucket (`+Inf`) captures any observations above the highest bound.
//!
//! # Units
//!
//! Time metrics (stored internally as nanoseconds) are reported as **seconds** in OTLP, matching
//! the Prometheus backend. The OTLP unit field is set accordingly: `"s"` for time, `"By"` for
//! bytes, `"1"` for dimensionless counts, `"%"` for percentages.
//!
//! # Timestamps
//!
//! Set the per-report timestamps with [`set_start_time_ns`](OtlpBackend::set_start_time_ns) and
//! [`set_time_ns`](OtlpBackend::set_time_ns) (Unix nanoseconds). The backend never reads a wall
//! clock itself, which keeps it deterministic under simulation and avoids a hidden clock source.
//!
//! # Aggregation → attributes
//!
//! The `aggregation` convention string is mapped to OTLP data-point attributes:
//! - `"Variant|ect0"` → `{variant="ect0"}`
//! - `"Task|foo"` → `{task="foo"}`
//! - `"bare_string"` → `{aggregation="bare_string"}`

use crate::{
    backend::{Backend, CallbackValue, Histogram, MetricInfo, ReportOptions},
    Unit,
};
use prost::Message as _;

/// A transport for OTLP protobuf payloads.
///
/// [`OtlpBackend`] calls [`send`](Self::send) once per report in
/// [`report_end`](Backend::report_end), passing a single `ExportMetricsServiceRequest` payload.
/// The implementation transmits it however it likes (HTTP POST, gRPC, a ring buffer for
/// testing, etc.). The slice is only valid for the duration of the call.
pub trait OtlpSink: Send {
    /// Receives one OTLP `ExportMetricsServiceRequest` protobuf payload per report.
    fn send(&mut self, payload: &[u8]);
}

impl<S: OtlpSink + ?Sized> OtlpSink for &mut S {
    fn send(&mut self, payload: &[u8]) {
        (**self).send(payload);
    }
}

/// A [`Backend`] that encodes metrics as OTLP protobuf payloads and forwards them to an
/// [`OtlpSink`].
///
/// Reusable across reports: [`report_start`](Backend::report_start) clears the accumulated metric
/// list while retaining its `Vec` capacity, so after a few intervals the backend stops
/// allocating in the steady state.
///
/// Set the per-report timestamps once before each [`Registry::report`](crate::Registry::report)
/// call via [`set_start_time_ns`](Self::set_start_time_ns) and [`set_time_ns`](Self::set_time_ns).
pub struct OtlpBackend<S> {
    sink: S,
    /// Instrumentation scope name embedded in every payload.
    scope_name: String,
    /// Instrumentation scope version embedded in every payload.
    scope_version: String,
    /// Start of the measurement interval (Unix nanoseconds). Maps to
    /// `start_time_unix_nano` on each data point.
    start_time_ns: u64,
    /// End of the measurement interval (Unix nanoseconds). Maps to `time_unix_nano` on each
    /// data point.
    time_ns: u64,
    /// Metrics accumulated during the in-progress report.
    metrics: Vec<proto::Metric>,
    /// Scratch buffer for protobuf encoding; cleared and reused each report.
    buf: Vec<u8>,
    include_sparse: bool,
}

impl<S: OtlpSink> OtlpBackend<S> {
    /// Creates a backend that forwards OTLP payloads to `sink`.
    ///
    /// `scope_name` and `scope_version` are embedded in the `InstrumentationScope` of every
    /// payload; use the service/library name and its version (e.g. `"s2n-quic-dc"`, `"1.2.3"`).
    pub fn new(sink: S, scope_name: impl Into<String>, scope_version: impl Into<String>) -> Self {
        Self {
            sink,
            scope_name: scope_name.into(),
            scope_version: scope_version.into(),
            start_time_ns: 0,
            time_ns: 0,
            metrics: Vec::new(),
            buf: Vec::new(),
            include_sparse: false,
        }
    }

    /// Sets the start-of-interval timestamp stamped onto every data point (Unix nanoseconds).
    ///
    /// Call once before each report. For the first report this can be the process start time;
    /// for subsequent delta reports it is the end time of the previous interval.
    pub fn set_start_time_ns(&mut self, ns: u64) {
        self.start_time_ns = ns;
    }

    /// Sets the end-of-interval timestamp stamped onto every data point (Unix nanoseconds).
    ///
    /// Call once before each report, typically right before
    /// [`Registry::report`](crate::Registry::report).
    pub fn set_time_ns(&mut self, ns: u64) {
        self.time_ns = ns;
    }
}

impl<S: OtlpSink> Backend for OtlpBackend<S> {
    fn report_start(&mut self, options: &ReportOptions) {
        self.metrics.clear();
        self.include_sparse = options.include_sparse;
    }

    fn record_counter(&mut self, info: &MetricInfo<'_>, value: u64) {
        if value == 0 && !self.include_sparse {
            return;
        }
        let attrs = aggregation_attrs(info.aggregation);
        let point = proto::NumberDataPoint {
            attributes: attrs,
            start_time_unix_nano: self.start_time_ns,
            time_unix_nano: self.time_ns,
            value: Some(proto::NumberValue::AsInt(value as i64)),
        };
        self.metrics.push(proto::Metric {
            name: info.name.to_owned(),
            unit: otlp_unit(info.unit).to_owned(),
            data: Some(proto::MetricData::Sum(proto::Sum {
                data_points: vec![point],
                aggregation_temporality: proto::AggregationTemporality::Delta as i32,
                is_monotonic: true,
            })),
            ..Default::default()
        });
    }

    fn record_gauge(&mut self, info: &MetricInfo<'_>, value: i64) {
        if value == 0 && (info.zero_suppressed || !self.include_sparse) {
            return;
        }
        let attrs = aggregation_attrs(info.aggregation);
        let point = proto::NumberDataPoint {
            attributes: attrs,
            start_time_unix_nano: self.start_time_ns,
            time_unix_nano: self.time_ns,
            value: Some(proto::NumberValue::AsInt(value)),
        };
        self.metrics.push(proto::Metric {
            name: info.name.to_owned(),
            unit: otlp_unit(info.unit).to_owned(),
            data: Some(proto::MetricData::Gauge(proto::Gauge {
                data_points: vec![point],
            })),
            ..Default::default()
        });
    }

    fn record_bool(&mut self, info: &MetricInfo<'_>, true_count: u64, false_count: u64) {
        let mut data_points = Vec::new();
        for (label, count) in [("true", true_count), ("false", false_count)] {
            if count == 0 && !self.include_sparse {
                continue;
            }
            let mut attrs = aggregation_attrs(info.aggregation);
            attrs.push(string_kv("result", label));
            data_points.push(proto::NumberDataPoint {
                attributes: attrs,
                start_time_unix_nano: self.start_time_ns,
                time_unix_nano: self.time_ns,
                value: Some(proto::NumberValue::AsInt(count as i64)),
            });
        }
        if data_points.is_empty() {
            return;
        }
        self.metrics.push(proto::Metric {
            name: info.name.to_owned(),
            unit: otlp_unit(info.unit).to_owned(),
            data: Some(proto::MetricData::Sum(proto::Sum {
                data_points,
                aggregation_temporality: proto::AggregationTemporality::Delta as i32,
                is_monotonic: true,
            })),
            ..Default::default()
        });
    }

    fn record_histogram(&mut self, info: &MetricInfo<'_>, hist: Histogram<'_>) {
        let total = hist.count();
        if total == 0 && !self.include_sparse {
            return;
        }
        let unit = hist.unit();
        let bounds = default_bounds(unit);

        // Collapse the fine-grained crate histogram onto the fixed `le` boundary set.
        // Each non-empty crate bucket contributes its `count` to the smallest OTLP slot whose
        // upper bound covers the bucket's reported value; the overflow slot captures the rest.
        let mut bucket_counts = vec![0u64; bounds.len() + 1];
        let mut sum = 0.0f64;
        for (value, count) in hist.buckets() {
            let reported = to_otlp_value(value, unit);
            let slot = bounds
                .iter()
                .position(|&le| reported <= le)
                .unwrap_or(bounds.len());
            bucket_counts[slot] += count;
            sum += reported * count as f64;
        }

        let attrs = aggregation_attrs(info.aggregation);
        let (min, max) = if total > 0 {
            (
                Some(to_otlp_value(hist.min(), unit)),
                Some(to_otlp_value(hist.max(), unit)),
            )
        } else {
            (None, None)
        };

        let point = proto::HistogramDataPoint {
            attributes: attrs,
            start_time_unix_nano: self.start_time_ns,
            time_unix_nano: self.time_ns,
            count: total,
            sum: Some(sum),
            explicit_bounds: bounds.to_vec(),
            bucket_counts,
            min,
            max,
        };
        self.metrics.push(proto::Metric {
            name: info.name.to_owned(),
            unit: otlp_unit(unit).to_owned(),
            data: Some(proto::MetricData::Histogram(proto::OtlpHistogram {
                data_points: vec![point],
                aggregation_temporality: proto::AggregationTemporality::Delta as i32,
            })),
            ..Default::default()
        });
    }

    fn record_callback(&mut self, info: &MetricInfo<'_>, values: &[&dyn CallbackValue]) {
        if values.is_empty() {
            return;
        }
        let sum: f64 = values.iter().map(|v| v.as_f64()).sum();
        if sum == 0.0 && (info.zero_suppressed || !self.include_sparse) {
            return;
        }
        let attrs = aggregation_attrs(info.aggregation);
        let point = proto::NumberDataPoint {
            attributes: attrs,
            start_time_unix_nano: self.start_time_ns,
            time_unix_nano: self.time_ns,
            value: Some(proto::NumberValue::AsDouble(sum)),
        };
        self.metrics.push(proto::Metric {
            name: info.name.to_owned(),
            unit: otlp_unit(info.unit).to_owned(),
            data: Some(proto::MetricData::Gauge(proto::Gauge {
                data_points: vec![point],
            })),
            ..Default::default()
        });
    }

    fn report_end(&mut self) {
        if self.metrics.is_empty() {
            return;
        }

        let request = proto::ExportMetricsServiceRequest {
            resource_metrics: vec![proto::ResourceMetrics {
                resource: None,
                scope_metrics: vec![proto::ScopeMetrics {
                    scope: Some(proto::InstrumentationScope {
                        name: self.scope_name.clone(),
                        version: self.scope_version.clone(),
                    }),
                    metrics: core::mem::take(&mut self.metrics),
                }],
            }],
        };

        self.buf.clear();
        request.encode(&mut self.buf).expect("OTLP encode error");
        self.sink.send(&self.buf);

        // Restore the metrics Vec (now empty but with retained capacity) after the move.
        self.metrics = request
            .resource_metrics
            .into_iter()
            .next()
            .and_then(|rm| rm.scope_metrics.into_iter().next())
            .map(|sm| {
                let mut v = sm.metrics;
                v.clear();
                v
            })
            .unwrap_or_default();
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────────────────────

/// Parses the `aggregation` convention string into OTLP `KeyValue` attributes.
///
/// Mirrors the Prometheus backend's label-parsing convention:
/// - `"Kind|value"` → `{kind: value}`
/// - bare non-empty string → `{aggregation: string}`
/// - `None` or empty → no attributes
fn aggregation_attrs(aggregation: Option<&str>) -> Vec<proto::KeyValue> {
    let Some(agg) = aggregation.filter(|a| !a.is_empty()) else {
        return Vec::new();
    };
    match agg.split_once('|') {
        Some((kind, value)) => vec![string_kv(kind.to_ascii_lowercase().as_str(), value)],
        None => vec![string_kv("aggregation", agg)],
    }
}

fn string_kv(key: &str, value: &str) -> proto::KeyValue {
    proto::KeyValue {
        key: key.to_owned(),
        value: Some(proto::AnyValue {
            value: Some(proto::AnyValueData::StringValue(value.to_owned())),
        }),
    }
}

/// Maps a native histogram bucket value to the OTLP-reported magnitude.
///
/// Time units are stored as nanoseconds (both `Microsecond` and `Second` use `record_duration`
/// which calls `as_nanos`). OTLP reports them as **seconds**. `Count`/`Byte` are raw. `Percent`
/// is stored scaled by `FLOAT_INT_MULTIPLIER` (1000) and is reported as a whole percentage.
fn to_otlp_value(value: u64, unit: Unit) -> f64 {
    match unit {
        Unit::Microsecond | Unit::Second => value as f64 / 1_000_000_000.0,
        Unit::Count | Unit::Byte => value as f64,
        Unit::Percent => value as f64 / crate::summary::FLOAT_INT_MULTIPLIER_U64 as f64,
    }
}

/// Maps a crate [`Unit`] to an [OTLP unit string] (UCUM-inspired).
///
/// [OTLP unit string]: https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/metrics/semantic_conventions/README.md#instrument-units
fn otlp_unit(unit: Unit) -> &'static str {
    match unit {
        Unit::Microsecond | Unit::Second => "s",
        Unit::Byte => "By",
        Unit::Count => "1",
        Unit::Percent => "%",
    }
}

/// Fixed `le` explicit-bound boundary sets per unit, in the OTLP-reported magnitude.
///
/// The crate histogram has thousands of fine-grained buckets; emitting them all would produce very
/// large payloads. Instead the backend collapses them onto this bounded set (same boundaries as the
/// Prometheus backend for cross-backend consistency). The overflow bucket captures observations
/// above the highest bound.
fn default_bounds(unit: Unit) -> &'static [f64] {
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

// ── minimal OTLP protobuf types ───────────────────────────────────────────────────────────────
//
// Hand-written Rust structs matching the OTLP metrics wire format. Prost's `#[derive(Message)]`
// provides `encode`/`decode` without a `.proto` build step or the OpenTelemetry SDK.
//
// Field numbers and types match the canonical proto files at:
//   opentelemetry/proto/collector/metrics/v1/metrics_service.proto
//   opentelemetry/proto/metrics/v1/metrics.proto
//   opentelemetry/proto/common/v1/common.proto
mod proto {
    /// `opentelemetry.proto.collector.metrics.v1.ExportMetricsServiceRequest`
    #[derive(Clone, prost::Message)]
    pub struct ExportMetricsServiceRequest {
        #[prost(message, repeated, tag = "1")]
        pub resource_metrics: Vec<ResourceMetrics>,
    }

    /// `opentelemetry.proto.metrics.v1.ResourceMetrics`
    #[derive(Clone, prost::Message)]
    pub struct ResourceMetrics {
        /// Omitted: `resource` carries host/process attributes that the caller sets externally via
        /// the OTLP collector or SDK. Emitting `None` here is valid; the collector will attach its
        /// own resource.
        #[prost(message, optional, tag = "1")]
        pub resource: Option<Resource>,
        #[prost(message, repeated, tag = "2")]
        pub scope_metrics: Vec<ScopeMetrics>,
    }

    /// `opentelemetry.proto.resource.v1.Resource` (stub for future use)
    #[derive(Clone, prost::Message)]
    pub struct Resource {
        #[prost(message, repeated, tag = "1")]
        pub attributes: Vec<KeyValue>,
    }

    /// `opentelemetry.proto.metrics.v1.ScopeMetrics`
    #[derive(Clone, prost::Message)]
    pub struct ScopeMetrics {
        #[prost(message, optional, tag = "1")]
        pub scope: Option<InstrumentationScope>,
        #[prost(message, repeated, tag = "2")]
        pub metrics: Vec<Metric>,
    }

    /// `opentelemetry.proto.common.v1.InstrumentationScope`
    #[derive(Clone, prost::Message)]
    pub struct InstrumentationScope {
        #[prost(string, tag = "1")]
        pub name: String,
        #[prost(string, tag = "2")]
        pub version: String,
    }

    /// `opentelemetry.proto.metrics.v1.Metric`
    #[derive(Clone, prost::Message)]
    pub struct Metric {
        #[prost(string, tag = "1")]
        pub name: String,
        #[prost(string, tag = "2")]
        pub description: String,
        #[prost(string, tag = "3")]
        pub unit: String,
        #[prost(oneof = "MetricData", tags = "5, 7, 9")]
        pub data: Option<MetricData>,
    }

    /// Oneof `data` field of [`Metric`].
    #[derive(Clone, prost::Oneof)]
    pub enum MetricData {
        #[prost(message, tag = "5")]
        Gauge(Gauge),
        #[prost(message, tag = "7")]
        Sum(Sum),
        #[prost(message, tag = "9")]
        Histogram(OtlpHistogram),
    }

    /// `opentelemetry.proto.metrics.v1.Gauge`
    #[derive(Clone, prost::Message)]
    pub struct Gauge {
        #[prost(message, repeated, tag = "1")]
        pub data_points: Vec<NumberDataPoint>,
    }

    /// `opentelemetry.proto.metrics.v1.Sum`
    #[derive(Clone, prost::Message)]
    pub struct Sum {
        #[prost(message, repeated, tag = "1")]
        pub data_points: Vec<NumberDataPoint>,
        /// [`AggregationTemporality`] as its `i32` discriminant.
        #[prost(enumeration = "AggregationTemporality", tag = "2")]
        pub aggregation_temporality: i32,
        #[prost(bool, tag = "3")]
        pub is_monotonic: bool,
    }

    /// `opentelemetry.proto.metrics.v1.Histogram`
    ///
    /// Renamed to avoid a collision with [`crate::backend::Histogram`].
    #[derive(Clone, prost::Message)]
    pub struct OtlpHistogram {
        #[prost(message, repeated, tag = "1")]
        pub data_points: Vec<HistogramDataPoint>,
        #[prost(enumeration = "AggregationTemporality", tag = "2")]
        pub aggregation_temporality: i32,
    }

    /// `opentelemetry.proto.metrics.v1.NumberDataPoint`
    #[derive(Clone, prost::Message)]
    pub struct NumberDataPoint {
        #[prost(message, repeated, tag = "1")]
        pub attributes: Vec<KeyValue>,
        #[prost(uint64, tag = "2")]
        pub start_time_unix_nano: u64,
        #[prost(uint64, tag = "3")]
        pub time_unix_nano: u64,
        /// Tags 4 (`as_double`) and 6 (`as_int`). Note: `as_int` is `sint64` (zigzag-encoded)
        /// in the OTLP proto, hence `#[prost(sint64)]`.
        #[prost(oneof = "NumberValue", tags = "4, 6")]
        pub value: Option<NumberValue>,
    }

    /// Oneof `value` of [`NumberDataPoint`].
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum NumberValue {
        #[prost(double, tag = "4")]
        AsDouble(f64),
        /// Zigzag-encoded `sint64` — must use `#[prost(sint64)]`, not `int64`.
        #[prost(sint64, tag = "6")]
        AsInt(i64),
    }

    /// `opentelemetry.proto.metrics.v1.HistogramDataPoint`
    #[derive(Clone, prost::Message)]
    pub struct HistogramDataPoint {
        #[prost(message, repeated, tag = "1")]
        pub attributes: Vec<KeyValue>,
        #[prost(uint64, tag = "2")]
        pub start_time_unix_nano: u64,
        #[prost(uint64, tag = "3")]
        pub time_unix_nano: u64,
        #[prost(uint64, tag = "4")]
        pub count: u64,
        /// `optional double` in proto3; omitted when the histogram is empty.
        #[prost(double, optional, tag = "5")]
        pub sum: Option<f64>,
        /// N counts for N-1 `explicit_bounds` + 1 overflow bucket.
        #[prost(uint64, repeated, tag = "6")]
        pub bucket_counts: Vec<u64>,
        /// N-1 explicit upper bounds defining N buckets.
        #[prost(double, repeated, tag = "7")]
        pub explicit_bounds: Vec<f64>,
        /// `optional double`; omitted when the histogram is empty.
        #[prost(double, optional, tag = "9")]
        pub min: Option<f64>,
        /// `optional double`; omitted when the histogram is empty.
        #[prost(double, optional, tag = "11")]
        pub max: Option<f64>,
    }

    /// `opentelemetry.proto.common.v1.KeyValue`
    #[derive(Clone, prost::Message)]
    pub struct KeyValue {
        #[prost(string, tag = "1")]
        pub key: String,
        #[prost(message, optional, tag = "2")]
        pub value: Option<AnyValue>,
    }

    /// `opentelemetry.proto.common.v1.AnyValue` (string variant only — sufficient for attributes)
    #[derive(Clone, prost::Message)]
    pub struct AnyValue {
        #[prost(oneof = "AnyValueData", tags = "1, 2, 3, 4")]
        pub value: Option<AnyValueData>,
    }

    /// Oneof `value` of [`AnyValue`].
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum AnyValueData {
        #[prost(string, tag = "1")]
        StringValue(String),
        #[prost(bool, tag = "2")]
        BoolValue(bool),
        #[prost(int64, tag = "3")]
        IntValue(i64),
        #[prost(double, tag = "4")]
        DoubleValue(f64),
    }

    /// `opentelemetry.proto.metrics.v1.AggregationTemporality`
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
    #[repr(i32)]
    pub enum AggregationTemporality {
        Unspecified = 0,
        Delta = 1,
        Cumulative = 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Registry, Unit};

    /// Collects all OTLP payloads sent during a test.
    #[derive(Default)]
    struct Capture {
        payloads: Vec<Vec<u8>>,
    }

    impl OtlpSink for Capture {
        fn send(&mut self, payload: &[u8]) {
            self.payloads.push(payload.to_vec());
        }
    }

    fn decode(payload: &[u8]) -> proto::ExportMetricsServiceRequest {
        proto::ExportMetricsServiceRequest::decode(payload).expect("OTLP decode failed")
    }

    fn metrics(payload: &[u8]) -> Vec<proto::Metric> {
        decode(payload)
            .resource_metrics
            .into_iter()
            .flat_map(|rm| rm.scope_metrics)
            .flat_map(|sm| sm.metrics)
            .collect()
    }

    // ── counter ──────────────────────────────────────────────────────────────────────────────

    #[test]
    fn counter_emits_delta_sum() {
        let registry = Registry::new();
        let c = registry.register_counter("rx.data".into(), None);
        c.increment(42);

        let mut cap = Capture::default();
        let mut backend = OtlpBackend::new(&mut cap, "test", "1.0");
        backend.set_start_time_ns(1_000);
        backend.set_time_ns(2_000);
        registry.report_with(&ReportOptions::new(false), &mut backend);

        assert_eq!(cap.payloads.len(), 1);
        let ms = metrics(&cap.payloads[0]);
        assert_eq!(ms.len(), 1);

        assert_eq!(ms[0].name, "rx.data");
        assert_eq!(ms[0].unit, "1");

        let Some(proto::MetricData::Sum(sum)) = &ms[0].data else {
            panic!("expected Sum, got {:?}", ms[0].data);
        };
        assert_eq!(sum.aggregation_temporality, proto::AggregationTemporality::Delta as i32);
        assert!(sum.is_monotonic);
        assert_eq!(sum.data_points.len(), 1);
        let dp = &sum.data_points[0];
        assert_eq!(dp.start_time_unix_nano, 1_000);
        assert_eq!(dp.time_unix_nano, 2_000);
        assert_eq!(dp.value, Some(proto::NumberValue::AsInt(42)));
    }

    #[test]
    fn zero_counter_suppressed_by_default() {
        let registry = Registry::new();
        registry.register_counter("c".into(), None);

        let mut cap = Capture::default();
        let mut backend = OtlpBackend::new(&mut cap, "test", "0");
        registry.report(&mut backend); // default options: include_sparse=false

        // A zero counter with include_sparse=false should produce no payload.
        assert!(cap.payloads.is_empty(), "expected no payload for zero counter");
    }

    #[test]
    fn zero_counter_included_when_sparse() {
        let registry = Registry::new();
        registry.register_counter("c".into(), None);

        let mut cap = Capture::default();
        let mut backend = OtlpBackend::new(&mut cap, "test", "0");
        registry.report_with(&ReportOptions::new(true), &mut backend);

        assert_eq!(cap.payloads.len(), 1);
        let ms = metrics(&cap.payloads[0]);
        assert_eq!(ms.len(), 1);
        let Some(proto::MetricData::Sum(sum)) = &ms[0].data else {
            panic!("expected Sum");
        };
        assert_eq!(sum.data_points[0].value, Some(proto::NumberValue::AsInt(0)));
    }

    // ── bool counter ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn bool_counter_produces_result_attributes() {
        let registry = Registry::new();
        let b = registry.register_bool("connect".into(), None);
        b.record(true);
        b.record(true);
        b.record(false);

        let mut cap = Capture::default();
        let mut backend = OtlpBackend::new(&mut cap, "test", "0");
        registry.report_with(&ReportOptions::new(true), &mut backend);

        let ms = metrics(&cap.payloads[0]);
        assert_eq!(ms.len(), 1);
        let Some(proto::MetricData::Sum(sum)) = &ms[0].data else {
            panic!("expected Sum");
        };
        assert_eq!(sum.data_points.len(), 2);

        let find = |label: &str| -> &proto::NumberDataPoint {
            sum.data_points
                .iter()
                .find(|dp| {
                    dp.attributes
                        .iter()
                        .any(|kv| kv.key == "result" && kv.value.as_ref().map_or(false, |v| {
                            v.value == Some(proto::AnyValueData::StringValue(label.to_owned()))
                        }))
                })
                .unwrap_or_else(|| panic!("no result={label} data point"))
        };

        assert_eq!(find("true").value, Some(proto::NumberValue::AsInt(2)));
        assert_eq!(find("false").value, Some(proto::NumberValue::AsInt(1)));
    }

    // ── histogram ─────────────────────────────────────────────────────────────────────────────

    #[test]
    fn histogram_emits_delta_histogram_with_explicit_bounds() {
        let registry = Registry::new();
        let h = registry.register_summary("latency".into(), None, Unit::Microsecond);
        h.record_duration(std::time::Duration::from_millis(10)); // 10ms = 0.01s

        let mut cap = Capture::default();
        let mut backend = OtlpBackend::new(&mut cap, "test", "0");
        registry.report_with(&ReportOptions::new(false), &mut backend);

        let ms = metrics(&cap.payloads[0]);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].unit, "s");

        let Some(proto::MetricData::Histogram(hist)) = &ms[0].data else {
            panic!("expected Histogram");
        };
        assert_eq!(hist.aggregation_temporality, proto::AggregationTemporality::Delta as i32);
        assert_eq!(hist.data_points.len(), 1);
        let dp = &hist.data_points[0];
        assert_eq!(dp.count, 1);

        // explicit_bounds has N-1 entries for N bucket_counts.
        assert_eq!(dp.bucket_counts.len(), dp.explicit_bounds.len() + 1);

        // The 10ms sample (0.01 s) should fall in the slot for le=0.01.
        let bounds = default_bounds(Unit::Microsecond);
        let slot = bounds.iter().position(|&le| 0.01 <= le).unwrap();
        assert_eq!(dp.bucket_counts[slot], 1, "10ms should land in the 0.01 le bucket");
        // All other buckets are zero.
        let total_in_bounds: u64 = dp.bucket_counts.iter().sum();
        assert_eq!(total_in_bounds, 1);

        assert!(dp.sum.is_some());
        assert!(dp.min.is_some());
        assert!(dp.max.is_some());
    }

    // ── callback scalar (gauge) ────────────────────────────────────────────────────────────────

    #[test]
    fn callback_emits_gauge() {
        let registry = Registry::new();
        registry.register_list_callback("workers".into(), None, Unit::Count, || 8u64);

        let mut cap = Capture::default();
        let mut backend = OtlpBackend::new(&mut cap, "test", "0");
        registry.report_with(&ReportOptions::new(false), &mut backend);

        let ms = metrics(&cap.payloads[0]);
        assert_eq!(ms.len(), 1);
        let Some(proto::MetricData::Gauge(gauge)) = &ms[0].data else {
            panic!("expected Gauge");
        };
        assert_eq!(gauge.data_points.len(), 1);
        assert_eq!(
            gauge.data_points[0].value,
            Some(proto::NumberValue::AsDouble(8.0))
        );
    }

    // ── aggregation → attributes ───────────────────────────────────────────────────────────────

    #[test]
    fn variant_aggregation_becomes_attribute() {
        let registry = Registry::new();
        let c = registry.register_counter("rx.ecn".into(), Some("Variant|ect0".into()));
        c.increment(5);

        let mut cap = Capture::default();
        let mut backend = OtlpBackend::new(&mut cap, "test", "0");
        registry.report_with(&ReportOptions::new(false), &mut backend);

        let ms = metrics(&cap.payloads[0]);
        let Some(proto::MetricData::Sum(sum)) = &ms[0].data else {
            panic!("expected Sum");
        };
        let attrs = &sum.data_points[0].attributes;
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].key, "variant");
        assert_eq!(
            attrs[0].value.as_ref().and_then(|v| v.value.as_ref()),
            Some(&proto::AnyValueData::StringValue("ect0".to_owned()))
        );
    }

    #[test]
    fn bare_aggregation_becomes_aggregation_attribute() {
        let registry = Registry::new();
        let c = registry.register_counter("metric".into(), Some("custom".into()));
        c.increment(1);

        let mut cap = Capture::default();
        let mut backend = OtlpBackend::new(&mut cap, "test", "0");
        registry.report_with(&ReportOptions::new(false), &mut backend);

        let ms = metrics(&cap.payloads[0]);
        let Some(proto::MetricData::Sum(sum)) = &ms[0].data else {
            panic!("expected Sum");
        };
        let attrs = &sum.data_points[0].attributes;
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].key, "aggregation");
    }

    // ── scope metadata ────────────────────────────────────────────────────────────────────────

    #[test]
    fn scope_name_and_version_are_embedded() {
        let registry = Registry::new();
        registry.register_counter("c".into(), None).increment(1);

        let mut cap = Capture::default();
        let mut backend = OtlpBackend::new(&mut cap, "s2n-quic-dc", "1.2.3");
        registry.report_with(&ReportOptions::new(false), &mut backend);

        let req = decode(&cap.payloads[0]);
        let scope = req.resource_metrics[0].scope_metrics[0]
            .scope
            .as_ref()
            .expect("scope should be set");
        assert_eq!(scope.name, "s2n-quic-dc");
        assert_eq!(scope.version, "1.2.3");
    }

    // ── empty report ───────────────────────────────────────────────────────────────────────────

    #[test]
    fn empty_report_sends_nothing() {
        let registry = Registry::new();
        // Register a counter but don't increment it; with default options (no sparse) nothing emits.
        registry.register_counter("c".into(), None);

        let mut cap = Capture::default();
        let mut backend = OtlpBackend::new(&mut cap, "test", "0");
        registry.report(&mut backend);

        assert!(cap.payloads.is_empty(), "no payload expected for all-zero report");
    }

    // ── reuse across reports ───────────────────────────────────────────────────────────────────

    #[test]
    fn backend_reusable_across_reports() {
        let registry = Registry::new();
        let c = registry.register_counter("c".into(), None);

        let mut cap = Capture::default();
        let mut backend = OtlpBackend::new(&mut cap, "test", "0");

        c.increment(1);
        registry.report_with(&ReportOptions::new(false), &mut backend);
        c.increment(2);
        registry.report_with(&ReportOptions::new(false), &mut backend);

        assert_eq!(cap.payloads.len(), 2);

        let first = metrics(&cap.payloads[0]);
        let second = metrics(&cap.payloads[1]);

        let val = |ms: &[proto::Metric]| {
            if let Some(proto::MetricData::Sum(s)) = &ms[0].data {
                if let Some(proto::NumberValue::AsInt(v)) = s.data_points[0].value {
                    return v;
                }
            }
            panic!("unexpected metric structure");
        };

        assert_eq!(val(&first), 1);
        assert_eq!(val(&second), 2);
    }
}
