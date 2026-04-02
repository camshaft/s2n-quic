// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Forward progress watchdog for the send state machine.
//!
//! Tracks the minimum unacked stream offset and arms a timer when data is
//! in flight. If the minimum unacked offset does not advance within the
//! configured timeout, the stream is considered stuck (e.g. due to a
//! retransmission amplification loop) and should be terminated.
//!
//! The timer is NOT armed when:
//! - The stream is flow-control blocked (waiting for MAX_DATA)
//! - There are no unacked ranges with in-flight payload packets
//! - The stream is in a terminal state

use core::time::Duration;
use s2n_quic_core::{
    ensure,
    time::{timer, Clock, Timer, Timestamp},
    varint::VarInt,
};

#[derive(Debug)]
pub struct Progress {
    /// The minimum unacked offset at the time the timer was last armed/reset
    min_offset: VarInt,
    /// Timer that fires when no forward progress has been made
    timer: Timer,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            min_offset: VarInt::ZERO,
            timer: Timer::default(),
        }
    }
}

impl Progress {
    /// Called when ACK processing advances the minimum unacked offset.
    ///
    /// If the offset has advanced, the timer is reset. This means the stream
    /// is making forward progress and we don't need to intervene.
    #[inline]
    pub fn on_progress(&mut self, min_unacked: VarInt) {
        if min_unacked > self.min_offset {
            self.min_offset = min_unacked;
            // Cancel the timer — it will be re-armed in `update` if needed
            self.timer.cancel();
        }
    }

    /// Arms or re-arms the progress timer if data is in flight and we're not
    /// flow-blocked.
    ///
    /// Should be called from `before_sleep` after all other processing.
    #[inline]
    pub fn update(
        &mut self,
        clock: &impl Clock,
        idle_timeout: Duration,
        has_inflight_payload: bool,
        is_flow_blocked: bool,
    ) {
        // Don't track progress if nothing is in flight or we're flow-blocked
        if !has_inflight_payload || is_flow_blocked {
            self.timer.cancel();
            return;
        }

        // If the timer is already armed, leave it alone — we're waiting for
        // the current deadline to expire
        ensure!(!self.timer.is_armed());

        // Arm the timer with the idle timeout as the deadline
        // Make the timeout longer than the idle timer to avoid conflating the two errors
        let target = clock.get_time() + idle_timeout * 2;
        self.timer.set(target);
    }

    /// Checks if the progress timer has expired.
    ///
    /// Returns `true` if the stream has been stuck for too long and should
    /// be terminated.
    #[inline]
    pub fn on_timeout(&mut self, now: Timestamp) -> bool {
        self.timer.poll_expiration(now).is_ready()
    }

    /// Cancels the progress timer (e.g. on error or success).
    #[inline]
    pub fn cancel(&mut self) {
        self.timer.cancel();
    }
}

impl timer::Provider for Progress {
    #[inline]
    fn timers<Q: timer::Query>(&self, query: &mut Q) -> timer::Result {
        self.timer.timers(query)?;
        Ok(())
    }
}
