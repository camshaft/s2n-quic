// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use arrow::{
    array::*,
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
use std::sync::Arc;

pub const METRICS_BATCH_SIZE: usize = 8192;

pub fn metrics_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("ts", DataType::Float64, false),
        Field::new("source", DataType::Utf8, false),
        Field::new("process", DataType::Utf8, false),
        Field::new("run", DataType::Utf8, false),
        Field::new("workloads", DataType::Utf8, false),
        Field::new("log_group", DataType::Utf8, false),
        Field::new("stream", DataType::Utf8, false),
        Field::new("env", DataType::Utf8, false),
        Field::new("metric", DataType::Utf8, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("variant", DataType::Utf8, true),
        Field::new("unit", DataType::Utf8, true),
        Field::new("value", DataType::Int64, true),
        Field::new("enq", DataType::UInt64, true),
        Field::new("drain", DataType::UInt64, true),
        Field::new("depth", DataType::Int64, true),
        Field::new("hit", DataType::UInt64, true),
        Field::new("miss", DataType::UInt64, true),
        Field::new("bytes", DataType::UInt64, true),
        Field::new("count", DataType::UInt64, true),
        Field::new("p50", DataType::UInt64, true),
        Field::new("p99", DataType::UInt64, true),
        Field::new("max", DataType::UInt64, true),
        Field::new("buckets", DataType::Utf8, true),
    ]))
}

pub struct MetricsBatchBuilder {
    ts: Float64Builder,
    source: StringBuilder,
    process: StringBuilder,
    run: StringBuilder,
    workloads: StringBuilder,
    log_group: StringBuilder,
    stream: StringBuilder,
    env: StringBuilder,
    metric: StringBuilder,
    r#type: StringBuilder,
    variant: StringBuilder,
    unit: StringBuilder,
    value: Int64Builder,
    enq: UInt64Builder,
    drain: UInt64Builder,
    depth: Int64Builder,
    hit: UInt64Builder,
    miss: UInt64Builder,
    bytes: UInt64Builder,
    count: UInt64Builder,
    p50: UInt64Builder,
    p99: UInt64Builder,
    max: UInt64Builder,
    buckets: StringBuilder,
    pub row_count: usize,
}

impl MetricsBatchBuilder {
    pub fn new() -> Self {
        Self {
            ts: Float64Builder::new(),
            source: StringBuilder::new(),
            process: StringBuilder::new(),
            run: StringBuilder::new(),
            workloads: StringBuilder::new(),
            log_group: StringBuilder::new(),
            stream: StringBuilder::new(),
            env: StringBuilder::new(),
            metric: StringBuilder::new(),
            r#type: StringBuilder::new(),
            variant: StringBuilder::new(),
            unit: StringBuilder::new(),
            value: Int64Builder::new(),
            enq: UInt64Builder::new(),
            drain: UInt64Builder::new(),
            depth: Int64Builder::new(),
            hit: UInt64Builder::new(),
            miss: UInt64Builder::new(),
            bytes: UInt64Builder::new(),
            count: UInt64Builder::new(),
            p50: UInt64Builder::new(),
            p99: UInt64Builder::new(),
            max: UInt64Builder::new(),
            buckets: StringBuilder::new(),
            row_count: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        ts: f64,
        source: &str,
        process: Option<&str>,
        run: Option<&str>,
        workloads: Option<&str>,
        log_group: Option<&str>,
        stream: Option<&str>,
        env: Option<&str>,
        row: &serde_json::Value,
    ) {
        let (process, stream) = mirror_context_pair(process, stream);
        let (run, log_group) = mirror_context_pair(run, log_group);
        let (workloads, env) = mirror_context_pair(workloads, env);

        self.ts.append_value(ts);
        self.source.append_value(source);
        self.process.append_value(process);
        self.run.append_value(run);
        self.workloads.append_value(workloads);
        self.log_group.append_value(log_group);
        self.stream.append_value(stream);
        self.env.append_value(env);

        self.metric
            .append_value(row.get("metric").and_then(|v| v.as_str()).unwrap_or(""));
        self.r#type
            .append_value(row.get("type").and_then(|v| v.as_str()).unwrap_or(""));

        append_opt_json_str(&mut self.variant, row.get("variant"));
        append_opt_json_str(&mut self.unit, row.get("unit"));
        append_opt_i64(&mut self.value, row.get("value"));
        append_opt_u64(&mut self.enq, row.get("enq"));
        append_opt_u64(&mut self.drain, row.get("drain"));
        append_opt_i64(&mut self.depth, row.get("depth"));
        append_opt_u64(&mut self.hit, row.get("hit"));
        append_opt_u64(&mut self.miss, row.get("miss"));
        append_opt_u64(&mut self.bytes, row.get("bytes"));
        append_opt_u64(&mut self.count, row.get("count"));
        append_opt_u64(&mut self.p50, row.get("p50"));
        append_opt_u64(&mut self.p99, row.get("p99"));
        append_opt_u64(&mut self.max, row.get("max"));

        match row.get("buckets") {
            Some(b) if !b.is_null() => self.buckets.append_value(b.to_string()),
            _ => self.buckets.append_null(),
        }

        self.row_count += 1;
    }

    pub fn finish(&mut self) -> RecordBatch {
        RecordBatch::try_new(
            metrics_schema(),
            vec![
                Arc::new(self.ts.finish()),
                Arc::new(self.source.finish()),
                Arc::new(self.process.finish()),
                Arc::new(self.run.finish()),
                Arc::new(self.workloads.finish()),
                Arc::new(self.log_group.finish()),
                Arc::new(self.stream.finish()),
                Arc::new(self.env.finish()),
                Arc::new(self.metric.finish()),
                Arc::new(self.r#type.finish()),
                Arc::new(self.variant.finish()),
                Arc::new(self.unit.finish()),
                Arc::new(self.value.finish()),
                Arc::new(self.enq.finish()),
                Arc::new(self.drain.finish()),
                Arc::new(self.depth.finish()),
                Arc::new(self.hit.finish()),
                Arc::new(self.miss.finish()),
                Arc::new(self.bytes.finish()),
                Arc::new(self.count.finish()),
                Arc::new(self.p50.finish()),
                Arc::new(self.p99.finish()),
                Arc::new(self.max.finish()),
                Arc::new(self.buckets.finish()),
            ],
        )
        .expect("schema mismatch in metrics batch builder")
    }
}

fn mirror_context_pair<'a>(left: Option<&'a str>, right: Option<&'a str>) -> (&'a str, &'a str) {
    match (left, right) {
        (Some(left), Some(right)) => (left, right),
        (Some(value), None) | (None, Some(value)) => (value, value),
        (None, None) => ("", ""),
    }
}

fn append_opt_json_str(builder: &mut StringBuilder, val: Option<&serde_json::Value>) {
    match val.and_then(|v| v.as_str()) {
        Some(s) => builder.append_value(s),
        None => builder.append_null(),
    }
}

fn append_opt_u64(builder: &mut UInt64Builder, val: Option<&serde_json::Value>) {
    match val.and_then(|v| v.as_u64()) {
        Some(n) => builder.append_value(n),
        None => builder.append_null(),
    }
}

fn append_opt_i64(builder: &mut Int64Builder, val: Option<&serde_json::Value>) {
    match val.and_then(|v| v.as_i64()) {
        Some(n) => builder.append_value(n),
        None => builder.append_null(),
    }
}
