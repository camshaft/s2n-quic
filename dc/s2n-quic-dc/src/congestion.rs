// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use s2n_quic_core::{
    event, random,
    recovery::{
        bandwidth::Bandwidth, bbr::BbrCongestionController, congestion_controller::Publisher,
        CongestionController, RttEstimator,
    },
    time::{timer, Clock, Timestamp},
};
use core::time::Duration;

pub type PacketInfo = <BbrCongestionController as CongestionController>::PacketInfo;

/// Upper bound on how far into the future a paced transmission may be scheduled.
///
/// A legitimate pacing interval is `send_quantum / pacing_rate`, at most a few tens of
/// milliseconds even on the slowest path (the BBR pacer floors the rate at
/// `send_quantum / (3 * rtt)`). One second is roughly two orders of magnitude beyond that, so
/// this clamp is inert during healthy operation and only engages if a degenerate
/// earliest-departure-time ever reaches a scheduler — at which point we cap the delay rather
/// than strand the context. This is a backstop; the root cause of a near-zero pacing rate is
/// prevented in BBR's pacer.
pub const MAX_TX_PACING_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct Controller {
    controller: BbrCongestionController,
}

impl Controller {
    #[inline]
    pub fn new(max_datagram_size: u16) -> Self {
        Self {
            controller: BbrCongestionController::new(max_datagram_size, Default::default()),
        }
    }

    #[inline]
    pub fn on_packet_sent(
        &mut self,
        time_sent: Timestamp,
        sent_bytes: u16,
        has_more_app_data: bool,
        rtt_estimator: &RttEstimator,
    ) -> PacketInfo {
        let sent_bytes = sent_bytes as usize;

        // The sender is application-limited whenever it has no more application data queued at
        // send time. We feed this signal honestly. A previous idle-gap heuristic here (only flag
        // app-limited when the inter-send gap exceeded one pacing interval) existed solely to
        // work around a bandwidth-estimator latch: `on_app_limited` re-armed the end-mark on
        // every send, so a per-context controller repeatedly flagged app-limited under spray
        // never closed the period and never left Startup. That latch is now fixed at its source
        // (the estimator pins the end-mark to the first send of each period), so the period
        // closes once the in-flight bytes drain and the controller exits Startup normally. The
        // heuristic is no longer needed and is removed.
        let app_limited = Some(!has_more_app_data);

        let publisher = &mut NoopPublisher;
        self.controller
            .on_packet_sent(time_sent, sent_bytes, app_limited, rtt_estimator, publisher)
    }

    #[inline]
    pub fn on_packet_ack(
        &mut self,
        newest_acked_time_sent: Timestamp,
        bytes_acked: usize,
        newest_acked_packet_info: PacketInfo,
        rtt_estimator: &RttEstimator,
        random_generator: &mut dyn random::Generator,
        ack_receive_time: Timestamp,
    ) {
        let publisher = &mut NoopPublisher;

        self.controller.on_rtt_update(
            newest_acked_time_sent,
            ack_receive_time,
            rtt_estimator,
            publisher,
        );

        self.controller.on_ack(
            newest_acked_time_sent,
            bytes_acked,
            newest_acked_packet_info,
            rtt_estimator,
            random_generator,
            ack_receive_time,
            publisher,
        )
    }

    #[inline]
    pub fn on_explicit_congestion(&mut self, ce_count: u64, now: Timestamp) {
        let publisher = &mut NoopPublisher;
        self.controller
            .on_explicit_congestion(ce_count, now, publisher);
    }

    #[inline]
    pub fn on_packet_lost(
        &mut self,
        bytes_lost: u32,
        packet_info: PacketInfo,
        new_loss_burst: bool,
        random_generator: &mut dyn random::Generator,
        now: Timestamp,
    ) {
        // BBRv2 ignores `persistent_congestion` entirely (it's `_persistent_congestion` in
        // BbrCongestionController::on_packet_lost). The only canonical consumer of that flag is
        // `RttEstimator::on_persistent_congestion`, which lives outside the CCA. So computing it
        // here would feed a value the controller discards — leave it false.
        let persistent_congestion = false;

        let publisher = &mut NoopPublisher;
        self.controller.on_packet_lost(
            bytes_lost,
            packet_info,
            persistent_congestion,
            new_loss_burst,
            random_generator,
            now,
            publisher,
        );
    }

    #[inline]
    pub fn on_packet_discarded(&mut self, sent_bytes: usize) {
        let publisher = &mut NoopPublisher;
        self.controller.on_packet_discarded(sent_bytes, publisher);
    }

    #[inline]
    pub fn is_congestion_limited(&self) -> bool {
        self.controller.is_congestion_limited()
    }

    #[inline]
    pub fn requires_fast_retransmission(&self) -> bool {
        self.controller.requires_fast_retransmission()
    }

    #[inline]
    pub fn congestion_window(&self) -> u32 {
        self.controller.congestion_window()
    }

    #[inline]
    pub fn bytes_in_flight(&self) -> u32 {
        self.controller.bytes_in_flight()
    }

    #[inline]
    pub fn send_quantum(&self) -> usize {
        self.controller.send_quantum().unwrap_or(usize::MAX)
    }

    /// The earliest time a paced packet may depart, clamped to at most
    /// [`MAX_TX_PACING_DELAY`] into the future.
    ///
    /// The clamp lives here, on the single accessor, rather than at each scheduling callsite.
    /// A degenerate (near-zero) pacing rate would otherwise produce a departure time days into
    /// the future; the BBR pacer now floors the rate at `send_quantum / (3 * rtt)` so this is
    /// already bounded, but applying the clamp centrally means every consumer — the tx_wheel,
    /// the sender load score, and any future caller — inherits the backstop without each
    /// having to remember to apply it. This was the gap behind the original report: the
    /// tx_wheel callsite clamped but the load-score path did not.
    ///
    /// The clock is queried lazily — only when there is actually a departure time to clamp —
    /// so callers that pass a bare `Timestamp` (which is itself a `Clock`) incur a no-op while
    /// callers that hold a real clock avoid an unnecessary `now` read when the CCA has no EDT.
    #[inline]
    pub fn earliest_departure_time<C: Clock + ?Sized>(&self, clock: &C) -> Option<Timestamp> {
        self.controller
            .earliest_departure_time()
            .map(|edt| edt.min(clock.get_time() + MAX_TX_PACING_DELAY))
    }

    #[inline]
    pub fn bandwidth(&self) -> Bandwidth {
        self.controller.pacing_rate()
    }

    /// True if the underlying BBR controller is still in Startup.
    #[cfg(test)]
    pub fn is_in_startup(&self) -> bool {
        self.controller.is_in_startup()
    }

    /// True if the underlying BBR controller is in an application-limited period.
    #[cfg(test)]
    pub fn is_app_limited(&self) -> bool {
        self.controller.is_app_limited()
    }
}

impl timer::Provider for Controller {
    #[inline]
    fn timers<Q: timer::Query>(&self, query: &mut Q) -> timer::Result {
        // No `now` is available here, so query the raw (unclamped) departure time. The pacer's
        // `send_quantum / (3 * rtt)` rate floor already bounds this; the additional
        // `MAX_TX_PACING_DELAY` clamp is applied by `earliest_departure_time` at the scheduling
        // callsites that do have a clock.
        if let Some(time) = self.controller.earliest_departure_time() {
            let mut timer = timer::Timer::default();
            timer.set(time);
            query.on_timer(&timer)?;
        }
        Ok(())
    }
}

struct NoopPublisher;

impl Publisher for NoopPublisher {
    #[inline]
    fn on_slow_start_exited(
        &mut self,
        _cause: event::builder::SlowStartExitCause,
        _congestion_window: u32,
    ) {
        // TODO
    }

    #[inline]
    fn on_delivery_rate_sampled(
        &mut self,
        _rate_sample: s2n_quic_core::recovery::bandwidth::RateSample,
    ) {
        // TODO
    }

    #[inline]
    fn on_pacing_rate_updated(
        &mut self,
        _pacing_rate: s2n_quic_core::recovery::bandwidth::Bandwidth,
        _burst_size: u32,
        _pacing_gain: num_rational::Ratio<u64>,
    ) {
        // TODO
    }

    #[inline]
    fn on_bbr_state_changed(&mut self, _state: event::builder::BbrState) {
        // TODO
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use s2n_quic_core::{
        packet::number::PacketNumberSpace,
        random,
        time::{Clock, NoopClock},
    };

    /// Models the spray regime that the removed idle-gap heuristic was meant to handle: a
    /// per-context controller receives work in bursts (whose body carries more queued app data,
    /// i.e. NOT app-limited) followed by an application-limited tail when the context drains.
    /// With the honest `app_limited = !has_more_app_data` signal restored and the estimator
    /// latch fixed (#398), the app-limited tail's period closes once its bubble drains, so the
    /// next burst's body is correctly seen as non-app-limited and BBR's full-pipe estimator
    /// advances and exits Startup.
    ///
    /// Before #398 the tail opened an app-limited period that never closed, poisoning every
    /// subsequent send (including the next burst's body) into being flagged app-limited, so the
    /// full-pipe estimator never ran and the controller was stuck in Startup forever. That latch
    /// is why the heuristic existed; with it fixed the heuristic is unnecessary. This test asserts
    /// the controller leaves Startup, which it cannot do if the latch is present.
    #[test]
    fn bursty_app_limited_sender_exits_startup() {
        let mtu = s2n_quic_core::path::MINIMUM_MAX_DATAGRAM_SIZE;
        let mut cca = Controller::new(mtu);
        let mut rng = random::testing::Generator::default();
        let mut rtt = RttEstimator::new(Duration::from_millis(10));
        let mut now = NoopClock.get_time();

        // Prime an RTT sample so the estimator has a real smoothed_rtt.
        let sample_rtt = |rtt: &mut RttEstimator, now| {
            rtt.update_rtt(
                Duration::ZERO,
                Duration::from_millis(10),
                now,
                true,
                PacketNumberSpace::ApplicationData,
            );
        };
        now += Duration::from_millis(10);
        sample_rtt(&mut rtt, now);

        // Send bursts of several packets. Within a burst, every packet but the last reports more
        // queued app data (not app-limited); the last reports none (app-limited tail). Each burst
        // is acked a round later, then the context idles briefly before the next burst.
        const BURST: usize = 8;
        'outer: for _ in 0..30 {
            let mut inflight = Vec::new();
            for i in 0..BURST {
                let has_more = i + 1 < BURST;
                let sent_at = now;
                let info = cca.on_packet_sent(sent_at, mtu, has_more, &rtt);
                inflight.push((sent_at, info));
            }
            // Ack the burst one round later.
            now += Duration::from_millis(10);
            sample_rtt(&mut rtt, now);
            for (sent_at, info) in inflight {
                cca.on_packet_ack(sent_at, mtu as usize, info, &rtt, &mut rng, now);
                if !cca.is_in_startup() {
                    break 'outer;
                }
            }
            // Idle gap between bursts (longer than a pacing interval).
            now += Duration::from_millis(50);
        }

        assert!(
            !cca.is_in_startup(),
            "controller stuck in Startup under bursty app-limited traffic — without the #398 \
             estimator latch fix the app-limited tail never clears and poisons later bursts, so \
             the full-pipe estimator never runs"
        );
    }
}
