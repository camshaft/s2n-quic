// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Manages ACK transmission state for the receiver.
//!
//! During active receive (`Recv`/`SizeKnown`), ACKs are queued immediately for
//! each new packet. Once we've received all data and are in a later state, ACKs
//! are rate-limited to avoid ACK storms while still ensuring the sender gets
//! delivery confirmation.

use crate::stream::recv::ack;
use core::time::Duration;
use s2n_quic_core::{
    ack::Ranges as AckRanges,
    ensure,
    frame::{self, ack::EcnCounts},
    inet::ExplicitCongestionNotification,
    state::{event, is},
    time::{timer, Clock, Timer},
    varint::VarInt,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    Queued,
    Throttled,
    Finished,
}

impl State {
    is!(is_idle, Idle);
    is!(is_queued, Queued);
    is!(is_throttled, Throttled);
    is!(is_finished, Finished);

    event! {
        on_packet_received(Idle => Queued);
        on_packet_received_throttled(Idle => Throttled);
        on_transmit(Queued | Throttled => Idle);
        on_stream_finished(Idle | Queued | Throttled => Finished);
        on_timeout(Throttled => Queued, Idle => Idle);
    }
}

/// Rate limit period for ACKs after the stream has received all data.
///
/// Once we're past `Recv`/`SizeKnown`, the sender is mostly probing to confirm
/// delivery. We throttle ACKs to avoid storms while still making progress.
const THROTTLE_PERIOD: Duration = Duration::from_secs(1);

/// Number of stream-space packets to accumulate before sending an ACK during
/// active data reception. This amortizes ACK overhead across multiple data
/// packets. Recovery/retransmission packets always ACK immediately.
///
/// A value of 2 gives a ~0.5 control-to-stream ratio for bulk transfers and
/// aligns with QUIC's default ACK frequency (RFC 9000 §13.2.1).
const ACK_PACKET_THRESHOLD: u16 = 2;

/// Maximum time to defer an ACK during active reception. If the packet
/// threshold hasn't been reached within this period, we send an ACK anyway to
/// avoid stalling congestion control.
const MAX_ACK_DELAY: Duration = Duration::from_millis(25);

#[derive(Debug, Default)]
pub struct Transmission {
    pub stream_ack: ack::Space,
    pub recovery_ack: ack::Space,
    ecn_counts: EcnCounts,
    state: State,
    /// Number of stream-space packets received since the last ACK was sent.
    packets_since_ack: u16,
    /// Timer for rate-limiting ACKs once the stream has received all data.
    /// When armed, we defer new-packet ACKs until it fires.
    throttle: Timer,
    /// Timer that caps how long we defer ACKs during active reception.
    max_ack_delay: Timer,
}

impl Transmission {
    /// Returns `true` if there is pending transmission interest
    #[inline]
    pub fn is_queued(&self) -> bool {
        self.state.is_queued()
    }

    /// Called when a new stream-space packet is received during active data
    /// reception.
    ///
    /// ACKs are deferred until [`ACK_PACKET_THRESHOLD`] packets accumulate,
    /// batching acknowledgements to reduce control packet overhead. A max-ack-delay
    /// timer ensures we don't wait forever.
    #[inline]
    pub fn on_new_packet_active(&mut self) {
        self.packets_since_ack += 1;

        if self.packets_since_ack >= ACK_PACKET_THRESHOLD {
            let _ = self.state.on_packet_received();
            // the threshold was reached; the delay timer is no longer needed
            self.max_ack_delay.cancel();
        }
        // If we're below the threshold and no delay timer is running, start one
        // so we don't wait indefinitely.
        // (The timer is checked via `on_timeout` → `on_timeout`)
    }

    /// Called when a recovery/retransmission-space packet is received during
    /// active data reception.
    ///
    /// Recovery packets signal loss, so we ACK immediately to give the sender
    /// timely feedback for loss recovery regardless of the packet counter.
    #[inline]
    pub fn on_recovery_packet_active(&mut self) {
        let _ = self.state.on_packet_received();
        self.max_ack_delay.cancel();
    }

    /// Arms the max-ack-delay timer if we have unacknowledged packets but
    /// haven't reached the threshold yet.
    #[inline]
    pub fn arm_max_ack_delay<Clk: Clock + ?Sized>(&mut self, clock: &Clk) {
        if self.packets_since_ack > 0 && !self.state.is_queued() && !self.max_ack_delay.is_armed() {
            self.max_ack_delay.set(clock.get_time() + MAX_ACK_DELAY);
        }
    }

    /// Called when we've received all data (buffered fin).
    ///
    /// We need to send an ACK to confirm we received everything so the sender
    /// can finish cleanly.
    #[inline]
    pub fn on_receive_all_data(&mut self) {
        let _ = self.state.on_packet_received();
    }

    /// Called when a local error occurs that needs to be transmitted to the peer.
    ///
    /// Queues an immediate transmission so the error gets sent out promptly.
    #[inline]
    pub fn on_error(&mut self) {
        let _ = self.state.on_packet_received();
    }

    /// Called when a new packet is received after the stream has received all data.
    ///
    /// The sender is likely probing to confirm delivery, so we rate-limit ACKs
    /// to avoid storms. If no throttle timer is active, we queue immediately
    /// and start the timer. Otherwise we defer until the timer fires.
    #[inline]
    pub fn on_new_packet_draining<Clk: Clock + ?Sized>(&mut self, clock: &Clk) {
        // Mark that we need to send a packet after the throttle timeout
        if self.throttle.is_armed() {
            let _ = self.state.on_packet_received_throttled();
            return;
        }

        // record that we have pending data to ACK
        ensure!(self.state.on_packet_received().is_ok());
        self.throttle.set(clock.get_time() + THROTTLE_PERIOD);
    }

    /// Called on timeout to check if the throttle timer or max-ack-delay timer
    /// has expired.
    #[inline]
    pub fn on_timeout<Clk: Clock + ?Sized>(&mut self, clock: &Clk) {
        let now = clock.get_time();
        if self.throttle.poll_expiration(now).is_ready() {
            let _ = self.state.on_timeout();
        }
        if self.max_ack_delay.poll_expiration(now).is_ready() {
            // The max-ack-delay fired before we hit the packet threshold.
            // Queue an ACK now so the sender gets timely feedback.
            if self.packets_since_ack > 0 {
                let _ = self.state.on_packet_received();
            }
        }
    }

    #[inline]
    pub fn increment_ecn(&mut self, ecn: ExplicitCongestionNotification) {
        self.ecn_counts.increment(ecn);
    }

    /// Returns the total number of ACK intervals across both spaces
    #[inline]
    pub fn interval_len(&self) -> usize {
        self.stream_ack.interval_len() + self.recovery_ack.interval_len()
    }

    /// Compute the encoding for both ACK spaces, given the initial frame size budget
    #[inline]
    pub fn encoding<Clk>(
        &mut self,
        max_data_encoding_size: VarInt,
        mtu: u16,
        clock: &Clk,
    ) -> (
        Option<frame::Ack<&AckRanges>>,
        Option<frame::Ack<&AckRanges>>,
        VarInt,
    )
    where
        Clk: Clock + ?Sized,
    {
        // compute the recovery ACKs first so we have enough space for those - if we run out,
        // the sender will convert the stream PNs anyway
        let (recovery_ack, encoding_size) =
            self.recovery_ack
                .encoding(max_data_encoding_size, None, mtu, clock);

        let (stream_ack, encoding_size) =
            self.stream_ack
                .encoding(encoding_size, Some(self.ecn_counts), mtu, clock);

        (stream_ack, recovery_ack, encoding_size)
    }

    /// Called after a packet has been transmitted
    #[inline]
    pub fn on_transmit(&mut self, packet_number: VarInt) {
        self.stream_ack.on_transmit(packet_number);
        self.recovery_ack.on_transmit(packet_number);
        self.packets_since_ack = 0;
        let _ = self.state.on_transmit();
    }

    /// Notify that a control packet from the peer was delivered
    #[inline]
    pub fn on_largest_delivered_packet(&mut self, largest_delivered_control_packet: VarInt) {
        self.stream_ack
            .on_largest_delivered_packet(largest_delivered_control_packet);
        self.recovery_ack
            .on_largest_delivered_packet(largest_delivered_control_packet);
    }

    /// Clear all ACK state
    #[inline]
    pub fn clear(&mut self) {
        self.stream_ack.clear();
        self.recovery_ack.clear();
        self.throttle.cancel();
        let _ = self.state.on_stream_finished();
    }
}

impl timer::Provider for Transmission {
    #[inline]
    fn timers<Q: timer::Query>(&self, query: &mut Q) -> timer::Result {
        self.throttle.timers(query)?;
        self.max_ack_delay.timers(query)?;
        Ok(())
    }
}
