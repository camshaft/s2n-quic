// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Dynamic send-buffer window based on the Bandwidth-Delay Product (BDP).
//!
//! Instead of allocating a fixed-size buffer for every stream, this module
//! computes how much buffer is actually needed to sustain the current delivery
//! rate.  The window is:
//!
//! ```text
//!   window = clamp(bandwidth × smoothed_rtt × BDP_MULTIPLIER,
//!                  min_window,
//!                  max_window)
//! ```
//!
//! When many streams share a single packet sender that caps the aggregate
//! send rate, each stream's observed delivery rate drops proportionally and
//! its buffer budget shrinks — which is exactly the coordination we want.

use s2n_quic_core::{
    recovery::{bandwidth::Bandwidth, RttEstimator},
    varint::VarInt,
};

/// Multiplier applied to the BDP (pacing_rate × smoothed_rtt) when computing
/// the dynamic local send buffer window.
///
/// We use a multiplier of 2 to avoid timing issues where the buffer underflows
/// and doesn't have enough to saturate the 5 Gbps capacity.
const BDP_MULTIPLIER: u64 = 2;

/// Minimum number of max-datagram-size segments the window will allow.
///
/// Even before the first RTT sample arrives, streams need enough buffer to
/// get a few segments in flight so the BDP estimator can bootstrap.  4 segments
/// keeps the floor small while avoiding a cold-start deadlock.
const MIN_WINDOW_SEGMENTS: u64 = 4;

#[derive(Clone, Debug)]
pub struct BufferBudget {
    /// The maximum window size (configured from transport parameters).
    /// The dynamic BDP window will never exceed this.
    max_window: u64,
    /// The minimum window size, computed as `MIN_WINDOW_SEGMENTS × max_datagram_size`.
    min_window: u64,
}

impl BufferBudget {
    /// Creates a new `BufferBudget`.
    ///
    /// * `max_window` – the upper bound on the local send window (from transport parameters).
    /// * `max_datagram_size` – used to compute the minimum window floor.
    pub fn new(max_window: VarInt, max_datagram_size: u16) -> Self {
        let max_window = max_window.as_u64();
        let min_window = (MIN_WINDOW_SEGMENTS * max_datagram_size as u64).min(max_window);
        Self {
            max_window,
            min_window,
        }
    }

    /// Returns the dynamic local send window in bytes.
    ///
    /// The window is based on the BDP (bandwidth × smoothed RTT) scaled by
    /// [`BDP_MULTIPLIER`], clamped between the minimum floor and the configured
    /// maximum.  When bandwidth is zero (no samples yet) or the RTT is zero,
    /// the minimum window is returned so streams can still bootstrap.
    pub fn window(&self, bandwidth: Bandwidth, rtt: &RttEstimator) -> u64 {
        let srtt = rtt.smoothed_rtt();

        // `Bandwidth::ZERO` has nanos_per_kibibyte == u64::MAX which would make
        // the multiplication return 0.  Fall back to the minimum in that case.
        let bdp = bandwidth * srtt;

        if bdp == 0 {
            return self.min_window;
        }

        let window = bdp.saturating_mul(BDP_MULTIPLIER);

        window.clamp(self.min_window, self.max_window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;
    use s2n_quic_core::recovery::RttEstimator;

    const MTU: u16 = 1500;

    fn budget(max_window: u64) -> BufferBudget {
        BufferBudget::new(VarInt::new(max_window).unwrap_or(VarInt::MAX), MTU)
    }

    fn rtt_estimator(rtt: Duration) -> RttEstimator {
        let mut est = RttEstimator::new(rtt);
        // Force a real smoothed_rtt by sending an update.
        let now = unsafe { s2n_quic_core::time::Timestamp::from_duration(Duration::from_secs(1)) };
        est.update_rtt(
            Duration::ZERO,
            rtt,
            now,
            true,
            s2n_quic_core::packet::number::PacketNumberSpace::ApplicationData,
        );
        est
    }

    #[test]
    fn returns_min_window_when_bandwidth_is_zero() {
        let b = budget(1_000_000);
        let rtt = rtt_estimator(Duration::from_millis(10));

        let w = b.window(Bandwidth::ZERO, &rtt);
        assert_eq!(w, MIN_WINDOW_SEGMENTS * MTU as u64);
    }

    #[test]
    fn returns_min_window_when_rtt_is_tiny() {
        // RttEstimator enforces a minimum RTT, so we use the smallest valid
        // value.  With a very small RTT and modest bandwidth the BDP will be
        // tiny, falling below the minimum window floor.
        let b = budget(1_000_000);
        let rtt = rtt_estimator(Duration::from_micros(1));
        let bw = Bandwidth::new(100, Duration::from_secs(1));

        let w = b.window(bw, &rtt);
        // BDP ≈ 0 → clamp to min
        assert_eq!(w, MIN_WINDOW_SEGMENTS * MTU as u64);
    }

    #[test]
    fn basic_bdp_calculation() {
        // 100 MB/s pacing rate, 10ms RTT → BDP = 1 MB, window = 1 MB (multiplier=1)
        let b = budget(10_000_000);
        let rtt = rtt_estimator(Duration::from_millis(10));
        let bw = Bandwidth::new(100_000_000, Duration::from_secs(1));

        let w = b.window(bw, &rtt);
        // BDP = 100_000_000 * 0.010 = 1_000_000
        // window = 1_000_000 * 1 = 1_000_000
        assert_eq!(w, 1_000_000);
    }

    #[test]
    fn clamped_to_max_window() {
        // 1 GB/s, 100ms RTT → BDP = 100 MB, but max is 1 MB
        let b = budget(1_000_000);
        let rtt = rtt_estimator(Duration::from_millis(100));
        let bw = Bandwidth::new(1_000_000_000, Duration::from_secs(1));

        let w = b.window(bw, &rtt);
        assert_eq!(w, 1_000_000);
    }

    #[test]
    fn clamped_to_min_window() {
        // Very low bandwidth: 100 bytes/s, 1ms RTT → BDP ≈ 0
        let b = budget(1_000_000);
        let rtt = rtt_estimator(Duration::from_millis(1));
        let bw = Bandwidth::new(100, Duration::from_secs(1));

        let w = b.window(bw, &rtt);
        assert_eq!(w, MIN_WINDOW_SEGMENTS * MTU as u64);
    }

    #[test]
    fn shared_sender_reduces_window() {
        // Scenario: 100 streams sharing a 1 Gbps sender
        // Each stream gets ~10 Mbps = 1.25 MB/s pacing rate
        // With 5ms RTT → BDP per stream = 6250, window = 6250
        let b = budget(10_000_000);
        let rtt = rtt_estimator(Duration::from_millis(5));
        let per_stream_bw = Bandwidth::new(1_250_000, Duration::from_secs(1));

        let w = b.window(per_stream_bw, &rtt);
        // BDP = 1_250_000 * 0.005 = 6250
        // window = 6250 * 1 = 6250
        assert_eq!(w, 6_250);
        // Much less than the 10 MB max — exactly what we want!
    }

    #[test]
    fn single_stream_gets_full_window() {
        // Single stream saturating a 10 Gbps link, 1ms RTT
        // BDP = 10_000_000_000 * 0.001 = 10_000_000 → window = 20_000_000
        // But max is 10 MB, so clamped
        let b = budget(10_000_000);
        let rtt = rtt_estimator(Duration::from_millis(1));
        let bw = Bandwidth::new(10_000_000_000, Duration::from_secs(1));

        let w = b.window(bw, &rtt);
        assert_eq!(w, 10_000_000);
    }

    #[test]
    fn higher_rtt_means_larger_window() {
        let b = budget(100_000_000);
        let bw = Bandwidth::new(10_000_000, Duration::from_secs(1)); // 10 MB/s

        let rtt_low = rtt_estimator(Duration::from_millis(1));
        let rtt_high = rtt_estimator(Duration::from_millis(50));

        let w_low = b.window(bw, &rtt_low);
        let w_high = b.window(bw, &rtt_high);

        assert!(
            w_high > w_low,
            "higher RTT should produce a larger window: low={w_low}, high={w_high}"
        );
    }

    #[test]
    fn bandwidth_infinity_clamped_to_max() {
        let b = budget(1_000_000);
        let rtt = rtt_estimator(Duration::from_millis(10));

        let w = b.window(Bandwidth::INFINITY, &rtt);
        // INFINITY bandwidth produces a huge BDP, clamped to max
        assert_eq!(w, 1_000_000);
    }
}
