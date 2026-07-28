// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Periodic metrics reporter for dc-tester.
//!
//! The metrics crate no longer ships a reporter; a consumer owns its own export loop. dc-tester's
//! is deliberately simple: on a fixed interval it drives one destructive report of the endpoint's
//! metric registry through a [`QuerylogBackend`], then emits the assembled line to `tracing` as
//! `[METRICS]` (or `[METRICS:{prefix}]`). Those lines are what the `xtask local` / `xtask cwlogs`
//! tooling scrapes (from stdout or CloudWatch Logs) and converts to Parquet — so the line format is
//! load-bearing and must not change.
//!
//! Driving the report is also what **drains** the registry's metric page pool; without this loop
//! the pool would grow without bound, so the reporter is required whenever dc-tester records
//! metrics, not merely for observability.

use core::time::Duration;
use s2n_quic_dc::endpoint::counters::os;
use s2n_quic_dc_metrics::{
    backend::{QuerylogBackend, ReportOptions},
    Registry,
};

/// Controls whether an interval emits sparse (zero-valued) metrics in addition to the ones that
/// changed.
// `Always`/`Every` aren't wired to a CLI flag today, but are cheap to keep as the obvious knobs a
// future flag would set.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum SparseMode {
    /// Never emit zeros.
    Never,
    /// Always emit zeros.
    Always,
    /// Emit zeros only on the first report (primes downstream time series without ongoing noise).
    Once,
    /// Emit zeros every `n`-th report.
    Every(u64),
}

impl SparseMode {
    fn include_sparse(&self, tick: u64) -> bool {
        match self {
            SparseMode::Never => false,
            SparseMode::Always => true,
            SparseMode::Once => tick == 0,
            SparseMode::Every(n) => tick.is_multiple_of(*n),
        }
    }
}

/// Configuration for [`spawn`].
#[derive(Clone, Debug)]
pub struct Config {
    /// How often to report.
    pub interval: Duration,
    /// Optional prefix, emitted as `[METRICS:{prefix}]`.
    pub prefix: Option<String>,
    /// Sparse-emission policy.
    pub sparse_mode: SparseMode,
    /// When `true`, collect OS-level networking stats from `/proc` each interval before reporting
    /// (Linux only; ignored elsewhere).
    pub os_stats: bool,
}

impl Config {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            prefix: None,
            sparse_mode: SparseMode::Never,
            os_stats: false,
        }
    }
}

/// Spawns a background thread that reports `counters`' metrics on `config.interval`, emitting each
/// assembled querylog line to `tracing` as `[METRICS]`. The loop exits when the registry is closed.
pub fn spawn(counters: &s2n_quic_dc::counter::Registry, config: Config) {
    let registry = counters.metrics().clone();
    let os_collector = if config.os_stats {
        Some(os::Collector::new(counters.clone()))
    } else {
        None
    };

    if let Err(error) = std::thread::Builder::new()
        .name("dc-tester-reporter".into())
        .spawn(move || run(registry, config, os_collector))
    {
        tracing::warn!(%error, "failed to spawn dc-tester metrics reporter thread");
    }
}

fn run(registry: Registry, config: Config, mut os_collector: Option<os::Collector>) {
    // Reused across reports so the querylog line buffer keeps its capacity (no per-interval realloc
    // once it reaches its high-water mark).
    let mut backend = QuerylogBackend::new();
    let prefix = config.prefix.as_deref().filter(|p| !p.is_empty());
    let mut tick: u64 = 0;

    loop {
        std::thread::sleep(config.interval);
        if !registry.is_open() {
            break;
        }
        if let Some(collector) = &mut os_collector {
            collector.record_delta();
        }
        let options = ReportOptions::new(config.sparse_mode.include_sparse(tick));
        registry.report_with(&options, &mut backend);
        // Emit the framed line (see `metrics_line` for the exact format the xtask tooling scrapes).
        // `tracing::info!` needs a literal format string, so the two framings are spelled out here.
        let raw = backend.line();
        match prefix {
            Some(prefix) => tracing::info!("[METRICS:{prefix}] {raw}"),
            None => tracing::info!("[METRICS] {raw}"),
        }
        tick += 1;
    }
}

/// The `[METRICS]` line framing emitted each interval, as a pure function of the prefix and the
/// assembled querylog line — `[METRICS] {line}`, or `[METRICS:{prefix}] {line}` when a non-empty
/// prefix is set. This exact framing is what `xtask local` / `xtask cwlogs` scrape, so it must not
/// change; the reporter's `tracing::info!` calls reproduce it (they can't call this directly because
/// `tracing` requires a literal format string). Defined for the framing test that guards the format.
#[cfg(test)]
fn metrics_line(prefix: Option<&str>, line: &str) -> String {
    match prefix.filter(|p| !p.is_empty()) {
        Some(prefix) => format!("[METRICS:{prefix}] {line}"),
        None => format!("[METRICS] {line}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_line_framing() {
        // No prefix (and empty prefix) → `[METRICS] ...`.
        assert_eq!(metrics_line(None, "a=1,b=2"), "[METRICS] a=1,b=2");
        assert_eq!(metrics_line(Some(""), "a=1"), "[METRICS] a=1");
        // Non-empty prefix → `[METRICS:{prefix}] ...`.
        assert_eq!(
            metrics_line(Some("membrain.storage"), "a=1"),
            "[METRICS:membrain.storage] a=1"
        );
        // An empty report line still frames (the tick emitted nothing but the marker is present).
        assert_eq!(metrics_line(None, ""), "[METRICS] ");
    }

    #[test]
    fn sparse_mode_schedule() {
        assert!(!SparseMode::Never.include_sparse(0));
        assert!(SparseMode::Always.include_sparse(3));
        // `Once` only on the first tick.
        assert!(SparseMode::Once.include_sparse(0));
        assert!(!SparseMode::Once.include_sparse(1));
        // `Every(n)` on multiples of n.
        assert!(SparseMode::Every(3).include_sparse(0));
        assert!(!SparseMode::Every(3).include_sparse(1));
        assert!(SparseMode::Every(3).include_sparse(6));
    }
}
