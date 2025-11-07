// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::metrics::MetricsSubscriber;
use parking_lot::RwLock;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// Shared state between server/client and TUI
#[derive(Clone)]
pub struct SharedState {
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    /// Current status message
    status: String,
    /// Number of active connections/streams
    active_connections: u64,
    /// Total connections/streams processed
    total_connections: u64,
    /// Start time
    start_time: Instant,
    /// Goodput history (timestamp, percentage)
    goodput_history: Vec<(Instant, f64)>,
    /// Latency history (timestamp, average latency in ms)
    latency_history: Vec<(Instant, f64)>,
    /// Packet loss history (timestamp, percentage)
    loss_history: Vec<(Instant, f64)>,
    /// Log messages
    logs: Vec<String>,
    /// Panic messages
    panics: Vec<String>,
    /// Stream errors
    stream_errors: u64,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                status: "Initializing".to_string(),
                active_connections: 0,
                total_connections: 0,
                start_time: Instant::now(),
                goodput_history: Vec::new(),
                latency_history: Vec::new(),
                loss_history: Vec::new(),
                logs: Vec::new(),
                panics: Vec::new(),
                stream_errors: 0,
            })),
        }
    }

    pub fn increment_stream_errors(&self) {
        self.inner.write().stream_errors += 1;
    }

    pub fn get_stream_errors(&self) -> u64 {
        self.inner.read().stream_errors
    }

    pub fn set_status(&self, status: String) {
        self.inner.write().status = status;
    }

    pub fn get_status(&self) -> String {
        self.inner.read().status.clone()
    }

    pub fn set_active_connections(&self, count: u64) {
        self.inner.write().active_connections = count;
    }

    pub fn increment_total_connections(&self) {
        self.inner.write().total_connections += 1;
    }

    pub fn get_connection_stats(&self) -> (u64, u64) {
        let inner = self.inner.read();
        (inner.active_connections, inner.total_connections)
    }

    pub fn get_uptime(&self) -> Duration {
        self.inner.read().start_time.elapsed()
    }

    pub fn add_log(&self, message: String) {
        let mut inner = self.inner.write();
        inner.logs.push(message);
        // Keep only last 1000 logs
        if inner.logs.len() > 1000 {
            inner.logs.remove(0);
        }
    }

    pub fn get_logs(&self) -> Vec<String> {
        self.inner.read().logs.clone()
    }

    pub fn add_panic(&self, message: String) {
        self.inner.write().panics.push(message);
    }

    pub fn get_panics(&self) -> Vec<String> {
        self.inner.read().panics.clone()
    }

    pub fn update_metrics(&self, metrics: &MetricsSubscriber) {
        // Don't snapshot yet - we'll accumulate over 1 second
        // This is just a polling call to check if we should update
        let now = Instant::now();

        let mut inner = self.inner.write();

        // Check if it's been at least 1 second since last update
        let should_update = if let Some((last_time, _)) = inner.goodput_history.last() {
            now.duration_since(*last_time).as_secs_f64() >= 1.0
        } else {
            true // First update
        };

        if !should_update {
            return;
        }

        // Now snapshot after 1 second has passed
        drop(inner); // Release lock before snapshot
        let snapshot = metrics.snapshot();

        let goodput_pct = snapshot.goodput.goodput_percentage();
        let loss_rate = snapshot.packet_loss_rate();
        let avg_latency_ms = snapshot
            .average_latency()
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);

        let mut inner = self.inner.write();

        // Update goodput history (now at 1-second intervals)
        inner.goodput_history.push((now, goodput_pct));
        if inner.goodput_history.len() > 300 {
            // Keep last 300 points (300 seconds = 5 minutes)
            inner.goodput_history.remove(0);
        }

        // Update latency history
        if avg_latency_ms > 0.0 {
            inner.latency_history.push((now, avg_latency_ms));
            if inner.latency_history.len() > 300 {
                inner.latency_history.remove(0);
            }
        }

        // Update loss history
        inner.loss_history.push((now, loss_rate));
        if inner.loss_history.len() > 300 {
            inner.loss_history.remove(0);
        }
    }

    pub fn get_goodput_history(&self) -> Vec<(Instant, f64)> {
        self.inner.read().goodput_history.clone()
    }

    pub fn get_latency_history(&self) -> Vec<(Instant, f64)> {
        self.inner.read().latency_history.clone()
    }

    pub fn get_loss_history(&self) -> Vec<(Instant, f64)> {
        self.inner.read().loss_history.clone()
    }

    pub fn get_latest_metrics(&self) -> Option<MetricsSnapshot> {
        let inner = self.inner.read();

        let goodput = inner.goodput_history.last().map(|(_, v)| *v);
        let latency = inner.latency_history.last().map(|(_, v)| *v);
        let loss = inner.loss_history.last().map(|(_, v)| *v);

        if goodput.is_some() || latency.is_some() || loss.is_some() {
            Some(MetricsSnapshot {
                goodput_pct: goodput.unwrap_or(0.0),
                avg_latency_ms: latency.unwrap_or(0.0),
                loss_rate_pct: loss.unwrap_or(0.0),
            })
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub goodput_pct: f64,
    pub avg_latency_ms: f64,
    pub loss_rate_pct: f64,
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}
