// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tracks the receiver-side MAX_DATA flow control state.
//!
//! The receiver advertises a `max_data` value to the sender, indicating how much
//! data it is willing to accept. As the application reads data, the window advances
//! and the new value is queued for transmission.
//!
//! When the sender is blocked on flow control (indicated by a DATA_BLOCKED frame),
//! the receiver retransmits the MAX_DATA frame with exponential backoff until the
//! sender acknowledges receipt (via `next_expected_control_packet` advancing past
//! the packet carrying the MAX_DATA).
//!
//! ```text
//!    ┌──────┐  on_new_value   ┌────────┐  on_transmit  ┌──────────┐  on_ack   ┌──────┐
//!    │ Idle │ ──────────────> │ Queued │ ────────────> │ Inflight │ ───────> │ Idle │
//!    └──────┘                 └────────┘               └──────────┘          └──────┘
//!                                  ^                        │
//!                                  │     on_timeout         │
//!                                  └────────────────────────┘
//! ```

use core::time::Duration;
use s2n_quic_core::{
    ensure, frame,
    state::{event, is},
    time::{timer, Clock, Timer},
    varint::VarInt,
};

/// Initial retransmit timeout for MAX_DATA when the sender is flow-blocked.
///
/// This is deliberately short since we know the sender is stalled and waiting
/// for this update. We use exponential backoff to avoid flooding.
const INITIAL_RETRANSMIT_TIMEOUT: Duration = Duration::from_millis(100);

/// Maximum retransmit timeout to cap exponential backoff.
const MAX_RETRANSMIT_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    Queued,
    Inflight,
}

impl State {
    is!(is_idle, Idle);
    is!(is_queued, Queued);
    is!(is_inflight, Inflight);

    event! {
        on_queued(Idle | Inflight => Queued);
        on_blocked_received(Idle => Queued);
        on_transmit(Queued => Inflight);
        on_ack(Inflight => Idle);
        on_timeout(Inflight => Queued);
    }
}

#[derive(Debug)]
pub struct MaxData {
    /// The value that was last transmitted to the peer
    transmitted_value: VarInt,
    /// The current max_data value to advertise to the peer
    pending_value: VarInt,
    /// The window size to add to the current offset when advancing
    window: VarInt,
    /// The state of the max_data transmission
    state: State,
    /// The control packet number that carried the last MAX_DATA transmission.
    /// Used to determine when the sender has acknowledged receipt.
    inflight_pn: Option<VarInt>,
    /// Timer for retransmitting MAX_DATA when inflight
    retransmit_timer: Timer,
    /// Current retransmit timeout (exponential backoff)
    retransmit_timeout: Duration,
}

impl MaxData {
    #[inline]
    pub fn new(initial_max_data: VarInt, window: VarInt) -> Self {
        Self {
            transmitted_value: initial_max_data,
            pending_value: initial_max_data,
            window,
            state: State::default(),
            inflight_pn: None,
            retransmit_timer: Timer::default(),
            retransmit_timeout: INITIAL_RETRANSMIT_TIMEOUT,
        }
    }

    /// Returns the current max_data value to advertise
    #[inline]
    pub fn frame(&self) -> frame::MaxData {
        frame::MaxData {
            maximum_data: self.pending_value,
        }
    }

    /// Returns the current pending max_data value
    #[inline]
    pub fn value(&self) -> VarInt {
        self.pending_value
    }

    /// Returns `true` if there is a new max_data value queued for transmission
    #[inline]
    pub fn is_queued(&self) -> bool {
        self.state.is_queued()
    }

    /// Called after reading from the buffer to potentially advance the max_data window.
    ///
    /// Updates the pending max_data value but does NOT independently trigger a
    /// control packet transmission. The updated value will be piggybacked on the
    /// next ACK control packet. This prevents max_data window advancement from
    /// inflating the control packet rate during bulk transfers.
    ///
    /// If the sender becomes flow-blocked, it will send a DATA_BLOCKED frame,
    /// which triggers an immediate dedicated max_data transmission via
    /// [`on_data_blocked`].
    ///
    /// Returns `true` if the pending value was updated.
    #[inline]
    pub fn on_read(&mut self, current_offset: VarInt, final_offset: Option<VarInt>) -> bool {
        // TODO instead of fixed windows we should measure how fast the application is reading
        // from the stream and dynamically scale it up if there's more demand.
        let new_max_data = current_offset.saturating_add(self.window);

        let new_max_data = final_offset.unwrap_or(VarInt::MAX).min(new_max_data);

        // only increase, never decrease
        ensure!(new_max_data > self.pending_value, false);

        self.pending_value = new_max_data;

        // Reset backoff when we have a genuinely new value
        self.retransmit_timeout = INITIAL_RETRANSMIT_TIMEOUT;

        true
    }

    /// Called when we receive a DATA_BLOCKED frame from the peer.
    ///
    /// If our max_data is higher than what the peer sees, we queue a retransmission.
    #[inline]
    pub fn on_data_blocked(&mut self, peer_limit: VarInt) {
        if peer_limit < self.pending_value {
            // The sender is behind — they haven't received our latest MAX_DATA.
            // If we're inflight, the packet may have been lost, so re-queue.
            if self.state.is_inflight() {
                let _ = self.state.on_timeout();
            } else {
                let _ = self.state.on_blocked_received();
            }
        }
    }

    /// Checks whether the given packet fits within the current max_data limit.
    #[inline]
    pub fn ensure_packet(&self, stream_offset: VarInt, payload_len: u64) -> bool {
        self.pending_value
            .as_u64()
            .checked_sub(payload_len)
            .and_then(|v| v.checked_sub(stream_offset.as_u64()))
            .is_some()
    }

    /// Called after the max_data frame has been transmitted in a control packet.
    ///
    /// The `packet_number` is the control packet number that carried the MAX_DATA,
    /// used to determine when the sender acknowledges it.
    #[inline]
    pub fn on_transmit(&mut self, packet_number: VarInt) {
        self.transmitted_value = self.pending_value;

        ensure!(self.state.on_transmit().is_ok());

        self.inflight_pn = Some(packet_number);
    }

    /// Called when the sender acknowledges receipt of a control packet.
    ///
    /// If the acknowledged packet number is >= the packet that carried our
    /// MAX_DATA, we know the sender has received the update.
    #[inline]
    pub fn on_largest_delivered_packet(&mut self, largest_delivered: VarInt) {
        let Some(inflight_pn) = self.inflight_pn else {
            return;
        };

        if largest_delivered >= inflight_pn {
            // The sender has received the packet containing our MAX_DATA
            self.inflight_pn = None;
            self.retransmit_timer.cancel();
            self.retransmit_timeout = INITIAL_RETRANSMIT_TIMEOUT;
            let _ = self.state.on_ack();
        }
    }

    /// Called on timeout to check if we need to retransmit MAX_DATA.
    #[inline]
    pub fn on_timeout<Clk: Clock + ?Sized>(&mut self, clock: &Clk) {
        ensure!(self.state.is_inflight());

        let now = clock.get_time();
        if self.retransmit_timer.poll_expiration(now).is_ready() {
            // Retransmit timer expired — re-queue the MAX_DATA
            let _ = self.state.on_timeout();
            self.inflight_pn = None;

            // Exponential backoff
            self.retransmit_timeout = (self.retransmit_timeout * 2).min(MAX_RETRANSMIT_TIMEOUT);
        }
    }

    /// Arms the retransmit timer after transitioning to Inflight.
    ///
    /// Called from recv State after on_packet_sent so we have access to a clock.
    #[inline]
    pub fn arm_retransmit_timer<Clk: Clock + ?Sized>(&mut self, clock: &Clk) {
        if self.state.is_inflight() && !self.retransmit_timer.is_armed() {
            self.retransmit_timer
                .set(clock.get_time() + self.retransmit_timeout);
        }
    }
}

impl timer::Provider for MaxData {
    #[inline]
    fn timers<Q: timer::Query>(&self, query: &mut Q) -> timer::Result {
        self.retransmit_timer.timers(query)?;
        Ok(())
    }
}
