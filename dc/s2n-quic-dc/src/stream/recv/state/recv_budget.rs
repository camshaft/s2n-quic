// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Dynamic receive-buffer window based on the application's drain rate and
//! the feedback-loop RTT.
//!
//! Instead of advertising a fixed MAX_DATA window for every stream, this module
//! computes how much receive buffer is actually needed so the sender has enough
//! credits to keep the pipe full while waiting for the next MAX_DATA update:
//!
//! ```text
//!   window = clamp(drain_rate × min_rtt × MULTIPLIER,
//!                  min_window,
//!                  max_window)
//! ```
//!
//! The RTT here is the *feedback-loop RTT*: the time from when the receiver
//! sends a control packet until the sender echoes it back via
//! `next_expected_control_packet` in a data packet.  This captures the full
//! round-trip including sender processing delay — which is exactly the latency
//! we need to buffer through.
//!
//! We use `min_rtt` (not smoothed_rtt) and only sample during active transfer
//! to avoid inflating the window when the sender is idle or app-limited.

use core::time::Duration;
use s2n_quic_core::{time::Timestamp, varint::VarInt};

/// Multiplier applied to drain_rate × min_rtt.
///
/// A value of 4 provides enough headroom beyond what's strictly needed,
/// absorbing jitter in both the drain rate and the RTT measurement.
const WINDOW_MULTIPLIER: u64 = 4;

/// Minimum number of max-datagram-size segments for the window floor.
const MIN_WINDOW_SEGMENTS: u64 = 4;

/// Maximum age for a min_rtt sample before it's considered stale and reset.
/// This allows min_rtt to recover if the true RTT decreases.
const MIN_RTT_EXPIRY: Duration = Duration::from_secs(10);

/// Minimum interval between drain rate samples to avoid noisy measurements
/// from tiny reads.
const DRAIN_RATE_SAMPLE_INTERVAL: Duration = Duration::from_millis(1);

#[derive(Clone, Debug)]
pub struct RecvBudget {
    /// Maximum window (from transport parameters).
    max_window: u64,
    /// Minimum window floor (MIN_WINDOW_SEGMENTS × max_datagram_size).
    min_window: u64,
    /// Feedback-loop RTT tracker.
    rtt: FeedbackRtt,
    /// Application drain rate tracker.
    drain: DrainRate,
    /// Slow-start window used during bootstrap before BDP measurements are
    /// available.  Starts at `min_window` and doubles each time the
    /// application makes progress, capped at `max_window`.  Once both drain
    /// rate and RTT are known, the BDP-based window takes over.
    slow_start_window: u64,
}

/// Tracks the minimum feedback-loop RTT from control packet echo timestamps.
#[derive(Clone, Debug, Default)]
struct FeedbackRtt {
    /// The minimum observed RTT sample.
    min_rtt: Option<Duration>,
    /// When the current min_rtt was recorded (for expiry).
    min_rtt_stamp: Option<Timestamp>,
    /// The control PN and timestamp of the most recently sent control packet.
    /// Used to compute RTT when the sender echoes it back.
    last_sent: Option<(VarInt, Timestamp)>,
    /// Timestamp of the most recently received data packet with payload.
    /// Used for active-transfer filtering.
    last_data_received: Option<Timestamp>,
}

/// Tracks the application's data consumption rate.
#[derive(Clone, Debug, Default)]
struct DrainRate {
    /// Total bytes consumed by the application so far.
    total_bytes: u64,
    /// Bytes consumed at the start of the current measurement window.
    window_bytes: u64,
    /// Timestamp at the start of the current measurement window.
    window_start: Option<Timestamp>,
    /// The current estimated drain rate in bytes per second.
    rate: u64,
}

impl RecvBudget {
    /// Creates a new `RecvBudget`.
    ///
    /// * `max_window` – the upper bound on the receive window (from transport parameters).
    /// * `max_datagram_size` – used to compute the minimum window floor.
    pub fn new(max_window: VarInt, max_datagram_size: u16) -> Self {
        let min_window = MIN_WINDOW_SEGMENTS * max_datagram_size as u64;
        Self {
            max_window: *max_window as u64,
            min_window,
            rtt: FeedbackRtt::default(),
            drain: DrainRate::default(),
            slow_start_window: min_window,
        }
    }

    /// Records that a control packet was sent at the given time.
    ///
    /// Call this from `on_packet_sent()` in the receiver state.
    #[inline]
    pub fn on_control_sent(&mut self, packet_number: VarInt, now: Timestamp) {
        self.rtt.on_control_sent(packet_number, now);
    }

    /// Records that a data packet with payload was received.
    ///
    /// This is used for active-transfer filtering — we only accept RTT samples
    /// when data is actively flowing.
    #[inline]
    pub fn on_data_received(&mut self, now: Timestamp) {
        self.rtt.last_data_received = Some(now);
    }

    /// Processes the sender's echo of our control packet number.
    ///
    /// If we have a matching sent timestamp and the transfer is active,
    /// this produces an RTT sample and updates min_rtt.
    #[inline]
    pub fn on_control_ack(&mut self, largest_delivered: VarInt, now: Timestamp) {
        self.rtt.on_control_ack(largest_delivered, now);
    }

    /// Records the current absolute stream offset consumed by the application.
    ///
    /// The drain rate is computed from how fast this offset advances over time.
    /// Also advances the slow-start window when the application makes progress.
    #[inline]
    pub fn on_consume(&mut self, current_offset: u64, now: Timestamp) {
        let prev = self.drain.total_bytes;
        self.drain.on_consume(current_offset, now);

        // Double the slow-start window each time the application reads new data
        if self.drain.total_bytes > prev && self.slow_start_window < self.max_window {
            self.slow_start_window =
                (self.slow_start_window.saturating_mul(2)).min(self.max_window);
        }
    }

    /// Returns the dynamic receive window in bytes.
    ///
    /// During bootstrap (before we have both drain rate and RTT), returns
    /// the slow-start window which doubles each time the app reads data.
    /// Once both signals are available, switches to the BDP-based window.
    pub fn window(&self) -> u64 {
        let rate = self.drain.rate;
        let Some(min_rtt) = self.rtt.min_rtt else {
            // No RTT samples yet — use the slow-start window.
            return self.slow_start_window;
        };

        if rate == 0 {
            // The application hasn't started reading yet — use slow-start.
            return self.slow_start_window;
        }

        // window = drain_rate × min_rtt × MULTIPLIER
        let rtt_secs_numer = min_rtt.as_nanos() as u128;
        let window = (rate as u128)
            .saturating_mul(rtt_secs_numer)
            .saturating_mul(WINDOW_MULTIPLIER as u128)
            / 1_000_000_000u128;

        let window = window.min(u64::MAX as u128) as u64;
        window.clamp(self.min_window, self.max_window)
    }
}

impl FeedbackRtt {
    fn on_control_sent(&mut self, packet_number: VarInt, now: Timestamp) {
        self.last_sent = Some((packet_number, now));
    }

    fn on_control_ack(&mut self, largest_delivered: VarInt, now: Timestamp) {
        let Some((sent_pn, sent_time)) = self.last_sent else {
            return;
        };

        // Only take a sample if the echo covers our last sent control packet.
        if largest_delivered < sent_pn {
            return;
        }

        // Active-transfer filter: only accept RTT samples if we received
        // data recently. This avoids inflating min_rtt when the sender was
        // idle before echoing. We use the existing min_rtt (if any) to
        // calibrate "recently", falling back to a conservative 100ms.
        let rtt_sample = now.saturating_duration_since(sent_time);
        if let Some(last_data) = self.last_data_received {
            let recency_threshold = self
                .min_rtt
                .map(|rtt| rtt.saturating_mul(10))
                .unwrap_or(Duration::from_millis(100));
            if now.saturating_duration_since(last_data) > recency_threshold {
                // Data hasn't arrived recently — sender was likely idle.
                return;
            }
        } else {
            // No data received yet — can't validate this sample.
            return;
        }

        // Expire stale min_rtt so it can react if conditions change.
        if let Some(stamp) = self.min_rtt_stamp {
            if now.saturating_duration_since(stamp) > MIN_RTT_EXPIRY {
                self.min_rtt = None;
            }
        }

        // Update min_rtt.
        if self.min_rtt.map_or(true, |prev| rtt_sample < prev) {
            self.min_rtt = Some(rtt_sample);
            self.min_rtt_stamp = Some(now);
        }

        // Clear the sent timestamp so we don't re-sample the same control packet.
        self.last_sent = None;
    }
}

impl DrainRate {
    fn on_consume(&mut self, current_offset: u64, now: Timestamp) {
        // current_offset is an absolute stream position, not a delta
        self.total_bytes = current_offset;

        let Some(start) = self.window_start else {
            // First sample — start the measurement window.
            self.window_bytes = self.total_bytes;
            self.window_start = Some(now);
            return;
        };

        let elapsed = now.saturating_duration_since(start);
        if elapsed < DRAIN_RATE_SAMPLE_INTERVAL {
            return;
        }

        // Compute rate over this window.
        let bytes_in_window = self.total_bytes.saturating_sub(self.window_bytes);
        let elapsed_nanos = elapsed.as_nanos() as u64;
        if elapsed_nanos > 0 {
            // rate = bytes_in_window / elapsed_seconds
            // = bytes_in_window * 1_000_000_000 / elapsed_nanos
            let new_rate = (bytes_in_window as u128)
                .saturating_mul(1_000_000_000)
                .checked_div(elapsed_nanos as u128)
                .unwrap_or(0) as u64;

            // EWMA: rate = (rate * 3 + new_rate) / 4
            // This smooths out bursts while still being responsive.
            self.rate = (self.rate.saturating_mul(3).saturating_add(new_rate)) / 4;
        }

        // Reset measurement window.
        self.window_bytes = self.total_bytes;
        self.window_start = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;

    const MTU: u16 = 1500;

    fn budget(max_window: u64) -> RecvBudget {
        RecvBudget::new(VarInt::new(max_window).unwrap_or(VarInt::MAX), MTU)
    }

    fn ts(millis: u64) -> Timestamp {
        unsafe { Timestamp::from_duration(Duration::from_millis(millis)) }
    }

    #[test]
    fn returns_min_window_with_no_samples() {
        let b = budget(1_000_000);
        assert_eq!(b.window(), MIN_WINDOW_SEGMENTS * MTU as u64);
    }

    #[test]
    fn returns_min_window_when_drain_rate_is_zero() {
        let mut b = budget(1_000_000);
        // Give it an RTT sample but no drain rate
        b.rtt.min_rtt = Some(Duration::from_millis(5));
        assert_eq!(b.window(), MIN_WINDOW_SEGMENTS * MTU as u64);
    }

    #[test]
    fn basic_window_calculation() {
        let mut b = budget(10_000_000);
        // drain rate = 100 MB/s, min_rtt = 10ms
        // window = 100_000_000 * 0.010 * 2 = 2_000_000
        b.drain.rate = 100_000_000;
        b.rtt.min_rtt = Some(Duration::from_millis(10));

        assert_eq!(b.window(), 2_000_000);
    }

    #[test]
    fn clamped_to_max_window() {
        let mut b = budget(1_000_000);
        b.drain.rate = 1_000_000_000;
        b.rtt.min_rtt = Some(Duration::from_millis(100));

        assert_eq!(b.window(), 1_000_000);
    }

    #[test]
    fn clamped_to_min_window() {
        let mut b = budget(1_000_000);
        b.drain.rate = 100; // 100 bytes/s
        b.rtt.min_rtt = Some(Duration::from_millis(1));

        assert_eq!(b.window(), MIN_WINDOW_SEGMENTS * MTU as u64);
    }

    #[test]
    fn rtt_sample_during_active_transfer() {
        let mut b = budget(10_000_000);

        let t0 = ts(100);
        let t1 = ts(105); // data received
        let t2 = ts(110); // control echoed back → 10ms RTT

        b.on_control_sent(VarInt::from_u8(1), t0);
        b.on_data_received(t1);
        b.on_control_ack(VarInt::from_u8(1), t2);

        assert_eq!(b.rtt.min_rtt, Some(Duration::from_millis(10)));
    }

    #[test]
    fn rtt_sample_rejected_when_no_recent_data() {
        let mut b = budget(10_000_000);

        let t0 = ts(100);
        let t1 = ts(200); // data long ago
        let t2 = ts(600); // echo comes back 500ms later

        b.on_data_received(t1);
        b.on_control_sent(VarInt::from_u8(1), t0);
        b.on_control_ack(VarInt::from_u8(1), t2);

        // Should be rejected — data was 400ms before the echo
        assert_eq!(b.rtt.min_rtt, None);
    }

    #[test]
    fn rtt_takes_minimum() {
        let mut b = budget(10_000_000);

        // First sample: 10ms
        b.on_data_received(ts(105));
        b.on_control_sent(VarInt::from_u8(1), ts(100));
        b.on_control_ack(VarInt::from_u8(1), ts(110));
        assert_eq!(b.rtt.min_rtt, Some(Duration::from_millis(10)));

        // Second sample: 5ms (better)
        b.on_data_received(ts(203));
        b.on_control_sent(VarInt::from_u8(2), ts(200));
        b.on_control_ack(VarInt::from_u8(2), ts(205));
        assert_eq!(b.rtt.min_rtt, Some(Duration::from_millis(5)));

        // Third sample: 20ms (worse — ignored)
        b.on_data_received(ts(312));
        b.on_control_sent(VarInt::from_u8(3), ts(300));
        b.on_control_ack(VarInt::from_u8(3), ts(320));
        assert_eq!(b.rtt.min_rtt, Some(Duration::from_millis(5)));
    }

    #[test]
    fn drain_rate_tracks_consumption() {
        let mut b = budget(10_000_000);

        // Consume 1 MB over 10ms → 100 MB/s
        let t0 = ts(100);
        b.on_consume(0, t0); // start window

        let t1 = ts(110);
        b.on_consume(1_000_000, t1);

        // First sample: EWMA = (0 * 3 + 100_000_000) / 4 = 25_000_000
        assert!(
            b.drain.rate > 0,
            "drain rate should be positive: {}",
            b.drain.rate
        );
    }

    #[test]
    fn drain_rate_smooths_bursts() {
        let mut b = budget(10_000_000);

        // Establish a steady rate
        b.drain.rate = 100_000_000; // 100 MB/s

        // Set up: app has consumed 50 MB so far
        b.drain.total_bytes = 50_000_000;
        b.drain.window_bytes = 50_000_000;
        b.drain.window_start = Some(ts(100));

        // Sudden burst: 10 MB more in 1ms → 10 GB/s instantaneous
        // Absolute offset is now 60 MB
        b.on_consume(60_000_000, ts(101));

        // EWMA should smooth: (100M * 3 + 10G) / 4 = much less than 10 GB/s
        assert!(b.drain.rate < 5_000_000_000);
        assert!(b.drain.rate > 100_000_000);
    }

    #[test]
    fn shared_receivers_get_smaller_windows() {
        let mut b = budget(10_000_000);
        b.rtt.min_rtt = Some(Duration::from_millis(5));

        // Single fast reader: 100 MB/s
        b.drain.rate = 100_000_000;
        let w_fast = b.window();

        // Slow reader (many streams sharing CPU): 1 MB/s
        b.drain.rate = 1_000_000;
        let w_slow = b.window();

        assert!(
            w_fast > w_slow,
            "faster drain should get larger window: fast={w_fast}, slow={w_slow}"
        );
    }

    #[test]
    fn min_rtt_expires_after_timeout() {
        let mut b = budget(10_000_000);

        // Set initial min_rtt
        b.rtt.min_rtt = Some(Duration::from_millis(5));
        b.rtt.min_rtt_stamp = Some(ts(100));

        // 15 seconds later, take a new (worse) sample
        let late = ts(15_100);
        b.on_data_received(ts(15_095));
        b.on_control_sent(VarInt::from_u8(10), ts(15_080));
        b.on_control_ack(VarInt::from_u8(10), late);

        // min_rtt should have been reset and updated to the new sample (20ms)
        assert_eq!(b.rtt.min_rtt, Some(Duration::from_millis(20)));
    }

    // ── slow-start tests ──────────────────────────────────────────────

    #[test]
    fn slow_start_initial_window_is_min() {
        let b = budget(1_000_000);
        // Before any data is consumed, the slow-start window equals min_window
        assert_eq!(b.slow_start_window, MIN_WINDOW_SEGMENTS * MTU as u64);
        assert_eq!(b.window(), MIN_WINDOW_SEGMENTS * MTU as u64);
    }

    #[test]
    fn slow_start_doubles_on_each_consume() {
        let mut b = budget(1_000_000);
        let min = MIN_WINDOW_SEGMENTS * MTU as u64;

        // First read: 0 → 1000 bytes
        b.on_consume(1_000, ts(100));
        assert_eq!(b.slow_start_window, min * 2);

        // Second read: 1000 → 2000 bytes
        b.on_consume(2_000, ts(110));
        assert_eq!(b.slow_start_window, min * 4);

        // Third read
        b.on_consume(3_000, ts(120));
        assert_eq!(b.slow_start_window, min * 8);
    }

    #[test]
    fn slow_start_does_not_grow_on_duplicate_offset() {
        let mut b = budget(1_000_000);
        let min = MIN_WINDOW_SEGMENTS * MTU as u64;

        b.on_consume(1_000, ts(100));
        assert_eq!(b.slow_start_window, min * 2);

        // Same offset again — no progress, no doubling
        b.on_consume(1_000, ts(110));
        assert_eq!(b.slow_start_window, min * 2);
    }

    #[test]
    fn slow_start_capped_at_max_window() {
        let max = 50_000u64;
        let mut b = RecvBudget::new(VarInt::new(max).unwrap(), MTU);

        // Keep consuming to drive slow-start to the cap
        let mut offset = 0u64;
        for i in 0..30 {
            offset += 10_000;
            b.on_consume(offset, ts(100 + i));
        }

        assert_eq!(b.slow_start_window, max);
        // window() should also respect the cap
        assert!(b.window() <= max);
    }

    #[test]
    fn slow_start_window_used_before_bdp() {
        let mut b = budget(10_000_000);
        let min = MIN_WINDOW_SEGMENTS * MTU as u64;

        // No RTT, no drain rate → slow-start window
        assert_eq!(b.window(), min);

        // Consume some data → slow-start grows
        b.on_consume(10_000, ts(100));
        assert_eq!(b.window(), min * 2);

        b.on_consume(20_000, ts(110));
        assert_eq!(b.window(), min * 4);
    }

    #[test]
    fn slow_start_replaced_by_bdp_when_available() {
        let mut b = budget(10_000_000);

        // Drive slow-start up high
        let mut offset = 0u64;
        for i in 0..20 {
            offset += 100_000;
            b.on_consume(offset, ts(100 + i));
        }
        let ss_window = b.slow_start_window;
        assert!(
            ss_window > 100_000,
            "slow-start should be large: {ss_window}"
        );

        // Now provide BDP measurements that produce a smaller window
        b.drain.rate = 1_000_000; // 1 MB/s
        b.rtt.min_rtt = Some(Duration::from_millis(5));
        // BDP window = 1_000_000 * 0.005 * 2 = 10_000
        let bdp_window = b.window();
        assert_eq!(bdp_window, 10_000);
        // BDP is much smaller than slow-start — this is the whole point
        assert!(bdp_window < ss_window);
    }

    #[test]
    fn slow_start_growth_rate_is_exponential() {
        let mut b = budget(100_000_000);
        let min = MIN_WINDOW_SEGMENTS * MTU as u64;

        let mut windows = vec![b.slow_start_window];
        for i in 1..=10u64 {
            b.on_consume(i * 1_000, ts(100 + i));
            windows.push(b.slow_start_window);
        }

        // Each step should be exactly 2× the previous (until cap)
        for pair in windows.windows(2) {
            let (prev, next) = (pair[0], pair[1]);
            assert_eq!(
                next,
                (prev * 2).min(100_000_000),
                "slow-start should double: prev={prev}, next={next}"
            );
        }

        // After 10 doublings from 6000: 6000 * 2^10 = 6_144_000
        assert_eq!(b.slow_start_window, min * (1 << 10));
    }
}
