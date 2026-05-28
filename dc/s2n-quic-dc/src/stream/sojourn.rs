// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Sojourn time tracking for application frames.
//!
//! Sojourn time is the duration between when an application frame is enqueued
//! into the pipeline (recorded in [`Frame::enqueued_at`]) and when it reaches
//! its final disposition (acknowledged, peer dead, cancelled, etc.).
//!
//! [`SojournMetrics`] collects per-outcome distributions as microsecond
//! summaries.  A single instance is typically shared between the two `Inner`
//! halves of a stream (Writer and Reader) via [`std::sync::Arc`].
//!
//! [`Frame::enqueued_at`]: crate::endpoint::frame::Frame::enqueued_at

use crate::{
    counter::{Registry, Summary, Unit},
    endpoint::frame::FailureReason,
    time::precision::Timestamp,
};

/// Per-outcome sojourn time distributions for application frames.
///
/// Each variant records the nanosecond duration from frame creation to final
/// disposition.  The underlying [`Summary`] expects nanosecond input and
/// formats output as microseconds.
#[derive(Clone)]
pub struct SojournMetrics {
    /// Frame acknowledged by the peer.
    pub acked: Summary,
    /// Peer declared dead (PTO reached max idle timeout).
    pub peer_dead: Summary,
    /// Frame cancelled (writer dropped or stream cancelled before ACK).
    pub cancelled: Summary,
    /// Transmission error (retransmission TTL exhausted).
    pub transmission_error: Summary,
    /// Unknown path secret (peer rejected the key).
    pub unknown_path_secret: Summary,
}

impl SojournMetrics {
    /// Create a new bundle of sojourn summaries under the given label prefix.
    ///
    /// All five variants are registered as nominal summaries:
    /// `{label}.sojourn` with variants `acked`, `peer_dead`, etc.
    pub fn new(registry: &Registry, label: &str) -> Self {
        let label = format!("{label}.sojourn");
        Self {
            acked: registry.register_nominal_summary(&label, "acked", Unit::Microsecond),
            peer_dead: registry.register_nominal_summary(&label, "peer_dead", Unit::Microsecond),
            cancelled: registry.register_nominal_summary(&label, "cancelled", Unit::Microsecond),
            transmission_error: registry
                .register_nominal_summary(&label, "transmission_error", Unit::Microsecond),
            unknown_path_secret: registry
                .register_nominal_summary(&label, "unknown_path_secret", Unit::Microsecond),
        }
    }

    /// Record a sojourn observation.
    ///
    /// `enqueued_at` is the time the frame entered the pipeline.
    /// `completed_at` is the time of final disposition.
    /// `failure` is `None` for a successful ACK, or `Some(reason)` for failures.
    #[inline]
    pub fn record(
        &self,
        enqueued_at: Timestamp,
        completed_at: Timestamp,
        failure: Option<FailureReason>,
    ) {
        let nanos = completed_at.nanos_since(enqueued_at);
        let summary = match failure {
            None => &self.acked,
            Some(FailureReason::PeerDead) => &self.peer_dead,
            Some(FailureReason::Cancelled) => &self.cancelled,
            Some(FailureReason::TransmissionError) => &self.transmission_error,
            Some(FailureReason::UnknownPathSecret) => &self.unknown_path_secret,
        };
        summary.record_value(nanos);
    }
}
