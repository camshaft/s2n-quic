// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use parking_lot::RwLock;
use s2n_quic_dc::event::{self, api, Subscriber};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

/// Statistics for calculating goodput across all streams
#[derive(Debug, Default, Clone)]
pub struct GoodputStats {
    /// Total payload bytes that were acknowledged
    pub acked_payload_bytes: u64,
    /// Total bytes transmitted in stream packets (including retransmissions)
    pub stream_packet_bytes: u64,
    /// Total bytes received in control packets
    pub control_packet_bytes: u64,
}

impl GoodputStats {
    /// Calculate the goodput percentage
    ///
    /// Goodput is the ratio of application bytes successfully delivered to total bytes transmitted
    pub fn goodput_percentage(&self) -> f64 {
        let total = self.stream_packet_bytes + self.control_packet_bytes;
        if total > 0 {
            (self.acked_payload_bytes as f64 / total as f64) * 100.0
        } else {
            0.0
        }
    }
}

/// Subscriber that tracks goodput statistics and other metrics
#[derive(Clone, Debug, Default)]
pub struct MetricsSubscriber(Arc<Inner>);

#[derive(Debug, Default)]
struct Inner {
    acked_payload_bytes: AtomicU64,
    stream_packet_bytes: AtomicU64,
    control_packet_bytes: AtomicU64,
    packets_lost: AtomicU64,
    packets_transmitted: AtomicU64,
    packets_acked: AtomicU64,
    latencies: RwLock<Vec<Duration>>,
}

impl MetricsSubscriber {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a snapshot of the current goodput statistics and reset counters
    pub fn snapshot(&self) -> Snapshot {
        let goodput = GoodputStats {
            acked_payload_bytes: self.0.acked_payload_bytes.swap(0, Ordering::Relaxed),
            stream_packet_bytes: self.0.stream_packet_bytes.swap(0, Ordering::Relaxed),
            control_packet_bytes: self.0.control_packet_bytes.swap(0, Ordering::Relaxed),
        };

        let packets_lost = self.0.packets_lost.swap(0, Ordering::Relaxed);
        let packets_transmitted = self.0.packets_transmitted.swap(0, Ordering::Relaxed);
        let packets_acked = self.0.packets_acked.swap(0, Ordering::Relaxed);

        let mut latencies_guard = self.0.latencies.write();
        let latencies = std::mem::take(&mut *latencies_guard);

        Snapshot {
            goodput,
            packets_lost,
            packets_transmitted,
            packets_acked,
            latencies,
        }
    }
}

#[derive(Debug)]
pub struct Snapshot {
    pub goodput: GoodputStats,
    pub packets_lost: u64,
    pub packets_transmitted: u64,
    pub packets_acked: u64,
    pub latencies: Vec<Duration>,
}

impl Snapshot {
    pub fn packet_loss_rate(&self) -> f64 {
        if self.packets_transmitted > 0 {
            (self.packets_lost as f64 / self.packets_transmitted as f64) * 100.0
        } else {
            0.0
        }
    }

    pub fn average_latency(&self) -> Option<Duration> {
        if self.latencies.is_empty() {
            return None;
        }

        let total: Duration = self.latencies.iter().sum();
        Some(total / self.latencies.len() as u32)
    }
}

impl event::Subscriber for MetricsSubscriber {
    type ConnectionContext = ();

    fn create_connection_context(
        &self,
        _meta: &event::api::ConnectionMeta,
        _info: &event::api::ConnectionInfo,
    ) -> Self::ConnectionContext {
    }

    fn on_stream_packet_acked(
        &self,
        _context: &Self::ConnectionContext,
        _meta: &event::api::ConnectionMeta,
        event: &event::api::StreamPacketAcked,
    ) {
        self.0
            .acked_payload_bytes
            .fetch_add(event.payload_len as u64, Ordering::Relaxed);
        self.0.packets_acked.fetch_add(1, Ordering::Relaxed);

        // Track latency
        self.0.latencies.write().push(event.lifetime);
    }

    fn on_stream_packet_transmitted(
        &self,
        _context: &Self::ConnectionContext,
        _meta: &event::api::ConnectionMeta,
        event: &event::api::StreamPacketTransmitted,
    ) {
        self.0
            .stream_packet_bytes
            .fetch_add(event.packet_len as u64, Ordering::Relaxed);
        self.0.packets_transmitted.fetch_add(1, Ordering::Relaxed);
    }

    fn on_stream_control_packet_received(
        &self,
        _context: &Self::ConnectionContext,
        _meta: &event::api::ConnectionMeta,
        event: &event::api::StreamControlPacketReceived,
    ) {
        self.0
            .control_packet_bytes
            .fetch_add(event.packet_len as u64, Ordering::Relaxed);
    }

    fn on_stream_packet_lost(
        &self,
        _context: &Self::ConnectionContext,
        _meta: &event::api::ConnectionMeta,
        _event: &event::api::StreamPacketLost,
    ) {
        self.0.packets_lost.fetch_add(1, Ordering::Relaxed);
    }
}
