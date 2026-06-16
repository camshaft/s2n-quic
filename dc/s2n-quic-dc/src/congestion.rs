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
    last_packet_sent_time: Option<Timestamp>,
}

impl Controller {
    #[inline]
    pub fn new(max_datagram_size: u16) -> Self {
        Self {
            controller: BbrCongestionController::new(max_datagram_size, Default::default()),
            last_packet_sent_time: None,
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

        // Only mark as app-limited if the sender has been idle for longer than one
        // pacing interval. This prevents brief inter-RPC gaps (microseconds) from being
        // classified as app-limited, which would cause BBR's pipe-filling estimator to
        // skip evaluation and keep the CCA stuck in Startup indefinitely.
        let is_idle = !has_more_app_data
            && self.last_packet_sent_time.is_some_and(|last| {
                let elapsed = time_sent.saturating_duration_since(last);
                let pacing_interval = self.send_quantum() as u64 / self.bandwidth();
                elapsed > pacing_interval
            });
        let app_limited = Some(is_idle);

        self.last_packet_sent_time = Some(time_sent);

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
