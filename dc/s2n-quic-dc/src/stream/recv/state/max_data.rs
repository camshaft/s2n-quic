// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tracks the receiver-side MAX_DATA flow control state.
//!
//! The receiver advertises a `max_data` value to the sender, indicating how much
//! data it is willing to accept. As the application reads data, the window advances
//! and the new value is queued for transmission.
//!
//! ```text
//!    ┌──────┐  on_new_value   ┌────────┐  on_transmit  ┌──────┐
//!    │ Idle │ ──────────────> │ Queued │ ────────────> │ Idle │
//!    └──────┘                 └────────┘               └──────┘
//! ```

use s2n_quic_core::{
    ensure, frame,
    state::{event, is},
    varint::VarInt,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    Queued,
}

impl State {
    is!(is_idle, Idle);
    is!(is_queued, Queued);

    event! {
        on_queued(Idle => Queued);
        on_blocked_received(Idle => Queued);
        on_transmit(Queued => Idle);
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
}

impl MaxData {
    #[inline]
    pub fn new(initial_max_data: VarInt, window: VarInt) -> Self {
        Self {
            transmitted_value: initial_max_data,
            pending_value: initial_max_data,
            window,
            state: State::default(),
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
    /// Returns `true` if a new max_data value was queued.
    #[inline]
    pub fn on_read(&mut self, current_offset: VarInt, final_offset: Option<VarInt>) -> bool {
        // TODO instead of fixed windows we should measure how fast the application is reading
        // from the stream and dynamically scale it up if there's more demand.
        let new_max_data = current_offset.saturating_add(self.window);

        let new_max_data = final_offset.unwrap_or(VarInt::MAX).min(new_max_data);

        // only increase, never decrease
        ensure!(new_max_data > self.pending_value, false);

        // TODO make this smarter to avoid sending too many MAX_DATA frames
        // TODO set a timer here or queue immediately if we're approaching the limit
        self.pending_value = new_max_data;

        let _ = self.state.on_queued();

        true
    }

    /// Called when we receive a DATA_BLOCKED frame from the peer.
    ///
    /// If our max_data is higher than what the peer sees, we queue a retransmission.
    #[inline]
    pub fn on_data_blocked(&mut self, peer_limit: VarInt) {
        if peer_limit < self.pending_value {
            let _ = self.state.on_blocked_received();
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

    /// Called after the max_data frame has been transmitted
    #[inline]
    pub fn on_transmit(&mut self) {
        // TODO record transmit time
        self.transmitted_value = self.pending_value;
        let _ = self.state.on_transmit();
    }
}
