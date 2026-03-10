// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Keep-alive mechanism for stream connections
//!
//! This module implements a keep-alive state machine that sends periodic probes to detect
//! broken connections before the idle timeout expires. The keep-alive timer fires at
//! intervals to allow the application to send probe packets, helping maintain the connection
//! and prevent premature idle timeout.
//!
//! # State Machine
//!
//! The keep-alive mechanism operates in three states:
//!
//! - **Disabled**: Keep-alive is not active. No timer is armed.
//! - **Cooldown**: Keep-alive is active but waiting for the timer to expire before queuing a probe.
//! - **Queued**: Timer has expired and a keep-alive probe should be sent.
//!
//! ## State Transitions
//!
//! ```text
//!                     set(enabled=true)
//!          Disabled ──────────────────────> Cooldown
//!              ^                                 │
//!              │                                 │ on_timeout()
//!              │                                 v
//!              │                              Queued
//!              │                                 │
//!              │                                 │ on_transmit()
//!              └─────────────────────────────────┘
//!                   set(enabled=false)
//! ```
//!
//! # Timer Behavior
//!
//! The keep-alive timer is set to expire at `idle_timeout_target - idle_timeout/4`. This ensures
//! that the keep-alive probe is sent before the connection idle timeout expires, giving the
//! connection a chance to demonstrate activity and reset the idle timer.
//!
//! After a timeout occurs and a probe is transmitted, the timer is re-armed at `now + 3/4 * idle_timeout`
//! to schedule the next keep-alive probe. In normal operation, this is unlikely to fire before the idle timeout
//! expires or the peer responds to the probe. However, for completeness it's armed anyway.

use s2n_quic_core::{
    ensure, ready,
    state::{event, is},
    time::{
        timer::{self, Provider},
        Clock, Timer, Timestamp,
    },
};
use std::{task::Poll, time::Duration};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum State {
    #[default]
    Disabled,
    Queued,
    Cooldown,
    Closed,
}

impl State {
    is!(is_disabled, Disabled);
    is!(is_queued, Queued);
    is!(is_cooldown, Cooldown);
    is!(is_closed, Closed);
    is!(is_armed, Queued | Cooldown);

    event! {
        on_enable(Disabled => Cooldown);
        on_disable(Queued | Cooldown => Disabled);
        on_transmit(Queued => Cooldown);
        on_idle_timer_update(Queued | Cooldown => Cooldown);
        on_timeout(Cooldown => Queued);
        on_fin_known(Disabled | Queued | Cooldown => Closed);
        on_shutdown(Disabled | Queued | Cooldown => Closed);
    }
}

#[derive(Debug, Default)]
pub struct KeepAlive {
    timer: Timer,
    state: State,
}

impl KeepAlive {
    /// Enable or disable the keep-alive mechanism
    pub fn set(&mut self, enabled: bool, idle_timeout_target: Timestamp, idle_timeout: Duration) {
        if enabled {
            if self.state.on_enable().is_ok() {
                self.arm_timer(idle_timeout_target, idle_timeout);
            }
        } else {
            let _ = self.state.on_disable();
            self.timer.cancel();
        }
    }

    /// Returns true if a keep-alive probe should be sent
    ///
    /// When this returns true, the caller should send a keep-alive probe packet
    /// and then call `on_transmit()` to acknowledge the transmission.
    pub fn is_queued(&self) -> bool {
        self.state.is_queued()
    }

    /// Poll the keep-alive timer for expiration
    ///
    /// This should be called on the returned timer expiration.
    pub fn on_timeout(&mut self, clock: &impl Clock, idle_timeout: Duration) -> Poll<()> {
        if !self.state.is_armed() {
            self.timer.cancel();
            return Poll::Pending;
        }

        let now = clock.get_time();
        ready!(self.timer.poll_expiration(now));
        let _ = self.state.on_timeout();

        self.timer.set(now + idle_timeout / 4 * 3);

        Poll::Ready(())
    }

    /// Notify that a keep-alive probe was transmitted
    ///
    /// This method should be called after a keep-alive probe is sent to transition
    /// the state machine from Queued back to Cooldown.
    pub fn on_transmit(&mut self) {
        let _ = self.state.on_transmit();
    }

    /// Update the keep-alive timer when the idle timeout target changes
    ///
    /// This method should be called whenever the idle timeout target is updated,
    /// such as when activity occurs on the connection and the idle timer is reset.
    ///
    /// The timer is only moved **earlier**, never later. Receiving peer activity
    /// (e.g., ACKs) resets the local idle timer but does **not** reset the peer's
    /// idle timer. If we allowed the keep-alive timer to move forward, the peer's
    /// idle timer could expire before we send the next probe.
    pub fn on_idle_timer_update(&mut self, idle_timeout_target: Timestamp, idle_timeout: Duration) {
        ensure!(self.state.is_armed());
        let timeout = idle_timeout_target - idle_timeout / 4;
        // Only move the timer earlier, never later. Moving it later risks the
        // peer's idle timer expiring before we send a keep-alive probe.
        if let Some(expiration) = self.timer.next_expiration() {
            ensure!(timeout < expiration);
        }
        self.timer.set(timeout);
        let _ = self.state.on_idle_timer_update();
    }

    /// Called when the final byte offset is known for the stream
    pub fn on_fin_known(&mut self) {
        let _ = self.state.on_fin_known();
    }

    /// Called when the connection is being shut down
    pub fn on_shutdown(&mut self) {
        let _ = self.state.on_shutdown();
    }

    fn arm_timer(&mut self, idle_timeout_target: Timestamp, idle_timeout: Duration) -> bool {
        let timeout = idle_timeout_target - idle_timeout / 4;
        if let Some(expiration) = self.timer.next_expiration() {
            if expiration == timeout {
                return false;
            }
        }
        self.timer.set(timeout);
        true
    }
}

impl timer::Provider for KeepAlive {
    fn timers<Q: timer::Query>(&self, query: &mut Q) -> timer::Result {
        ensure!(self.state.is_armed(), Ok(()));
        self.timer.timers(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use s2n_quic_core::time::{timer::Provider, Timestamp};
    use std::{task::Poll, time::Duration};

    fn clock() -> Timestamp {
        unsafe { Timestamp::from_duration(Duration::from_micros(1)) }
    }

    // Helper to create consistent test parameters
    fn test_params(clock: &impl Clock) -> (Timestamp, Duration) {
        let idle_timeout = Duration::from_secs(40);
        let idle_timeout_target = clock.get_time() + idle_timeout;
        (idle_timeout_target, idle_timeout)
    }

    #[test]
    fn enable_transitions_to_cooldown_and_arms_timer() {
        let clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);

        assert!(keep_alive.state.is_cooldown());
        assert!(keep_alive.timer.is_armed());
    }

    #[test]
    fn enable_calculates_timer_at_one_quarter_before_target() {
        let clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);

        // Timer should be at: idle_timeout_target - (idle_timeout / 4)
        // = 40 - 10 = 30 seconds
        let expected_timeout = idle_timeout_target - idle_timeout / 4;
        assert_eq!(keep_alive.timer.next_expiration(), Some(expected_timeout));
    }

    #[test]
    fn enable_when_already_enabled_is_noop() {
        let clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);
        let first_state = keep_alive.state.clone();
        let first_timer = keep_alive.timer.next_expiration();

        keep_alive.set(true, idle_timeout_target, idle_timeout);

        assert_eq!(keep_alive.state, first_state);
        assert_eq!(keep_alive.timer.next_expiration(), first_timer);
    }

    #[test]
    fn disable_from_queued_transitions_to_disabled_and_cancels_timer() {
        let mut clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        // Enable and wait for timeout to transition to Queued
        keep_alive.set(true, idle_timeout_target, idle_timeout);
        clock += idle_timeout;
        let _ = keep_alive.on_timeout(&clock, idle_timeout);
        assert!(keep_alive.state.is_queued());

        keep_alive.set(false, idle_timeout_target, idle_timeout);

        assert!(keep_alive.state.is_disabled());
        assert!(!keep_alive.timer.is_armed());
    }

    #[test]
    fn disable_from_cooldown_transitions_to_disabled_and_cancels_timer() {
        let clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);
        assert!(keep_alive.state.is_cooldown());

        keep_alive.set(false, idle_timeout_target, idle_timeout);

        assert!(keep_alive.state.is_disabled());
        assert!(!keep_alive.timer.is_armed());
    }

    #[test]
    fn disable_when_already_disabled_is_idempotent() {
        let clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(false, idle_timeout_target, idle_timeout);
        keep_alive.set(false, idle_timeout_target, idle_timeout);

        assert!(keep_alive.state.is_disabled());
    }

    #[test]
    fn on_timeout_before_expiration_returns_pending() {
        let mut clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);
        clock += Duration::from_secs(9);

        let result = keep_alive.on_timeout(&clock, idle_timeout);

        assert_eq!(result, Poll::Pending);
        assert!(keep_alive.state.is_cooldown());
    }

    #[test]
    fn on_timeout_after_expiration_transitions_cooldown_to_queued() {
        let mut clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);
        clock += idle_timeout;

        let result = keep_alive.on_timeout(&clock, idle_timeout);

        assert_eq!(result, Poll::Ready(()));
        assert!(keep_alive.state.is_queued());
    }

    #[test]
    fn is_queued_returns_true_after_timeout() {
        let mut clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);
        assert!(!keep_alive.is_queued());

        clock += idle_timeout;
        let _ = keep_alive.on_timeout(&clock, idle_timeout);

        assert!(keep_alive.is_queued());
    }

    #[test]
    fn on_transmit_transitions_queued_to_cooldown() {
        let mut clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);
        clock += idle_timeout;
        let _ = keep_alive.on_timeout(&clock, idle_timeout);
        assert!(keep_alive.state.is_queued());

        keep_alive.on_transmit();

        assert!(keep_alive.state.is_cooldown());
    }

    #[test]
    fn on_transmit_from_cooldown_is_noop() {
        let clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);
        assert!(keep_alive.state.is_cooldown());

        keep_alive.on_transmit();

        assert!(keep_alive.state.is_cooldown());
    }

    #[test]
    fn full_cycle_enable_timeout_transmit_timeout() {
        let mut clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        // Enable -> Cooldown
        keep_alive.set(true, idle_timeout_target, idle_timeout);
        assert!(keep_alive.state.is_cooldown());
        assert!(!keep_alive.is_queued());

        // Timeout -> Queued
        clock += idle_timeout;
        assert_eq!(keep_alive.on_timeout(&clock, idle_timeout), Poll::Ready(()));
        assert!(keep_alive.state.is_queued());
        assert!(keep_alive.is_queued());

        // Transmit -> Cooldown
        keep_alive.on_transmit();
        assert!(keep_alive.state.is_cooldown());
        assert!(!keep_alive.is_queued());

        // Another timeout -> Queued
        clock += idle_timeout;
        assert_eq!(
            keep_alive.on_timeout(&clock, idle_timeout),
            Poll::Ready(()),
            "{keep_alive:?}"
        );
        assert!(keep_alive.state.is_queued());
    }

    #[test]
    fn disable_and_reenable_resets_cleanly() {
        let mut clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        // First cycle
        keep_alive.set(true, idle_timeout_target, idle_timeout);
        clock += idle_timeout;
        let _ = keep_alive.on_timeout(&clock, idle_timeout);
        assert!(keep_alive.is_queued());

        // Disable
        keep_alive.set(false, idle_timeout_target, idle_timeout);
        assert!(keep_alive.state.is_disabled());
        assert!(!keep_alive.timer.is_armed());

        // Re-enable should work cleanly
        keep_alive.set(true, idle_timeout_target, idle_timeout);
        assert!(keep_alive.state.is_cooldown());
        assert!(keep_alive.timer.is_armed());
    }

    #[test]
    fn on_idle_timer_update_ignores_when_disabled() {
        let clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        // Should be no-op when disabled
        keep_alive.on_idle_timer_update(idle_timeout_target, idle_timeout);

        assert!(keep_alive.state.is_disabled());
        assert!(!keep_alive.timer.is_armed());
    }

    #[test]
    fn on_idle_timer_update_rearms_when_target_is_earlier() {
        let clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);
        let first_expiration = keep_alive.timer.next_expiration();

        // Update with an earlier target — timer should move earlier
        let earlier_target = idle_timeout_target - Duration::from_secs(10);
        keep_alive.on_idle_timer_update(earlier_target, idle_timeout);

        let new_expiration = keep_alive.timer.next_expiration();
        assert_ne!(first_expiration, new_expiration);
        assert!(new_expiration < first_expiration);
        assert!(keep_alive.state.is_cooldown());
    }

    #[test]
    fn on_idle_timer_update_ignores_later_target() {
        let clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);
        let first_expiration = keep_alive.timer.next_expiration();

        // Update with a later target — timer should NOT move later
        let later_target = clock + Duration::from_secs(60);
        keep_alive.on_idle_timer_update(later_target, idle_timeout);

        assert_eq!(keep_alive.timer.next_expiration(), first_expiration);
    }

    #[test]
    fn on_idle_timer_update_does_not_rearm_when_timeout_unchanged() {
        let clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);
        let first_expiration = keep_alive.timer.next_expiration();

        // Update with same values
        keep_alive.on_idle_timer_update(idle_timeout_target, idle_timeout);

        assert_eq!(keep_alive.timer.next_expiration(), first_expiration);
    }

    #[test]
    fn on_idle_timer_update_from_queued_transitions_to_cooldown() {
        let mut clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);
        clock += idle_timeout;
        let _ = keep_alive.on_timeout(&clock, idle_timeout);
        assert!(keep_alive.state.is_queued());

        // Update should transition to Cooldown
        let new_target = idle_timeout_target + Duration::from_secs(10);
        keep_alive.on_idle_timer_update(new_target, idle_timeout);

        assert!(keep_alive.state.is_cooldown(), "{:?}", keep_alive.state);
    }

    #[test]
    fn arm_timer_returns_true_when_setting_new_value() {
        let clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        let result = keep_alive.arm_timer(idle_timeout_target, idle_timeout);

        assert!(result);
        assert!(keep_alive.timer.is_armed());
    }

    #[test]
    fn arm_timer_returns_false_when_value_unchanged() {
        let clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        let first = keep_alive.arm_timer(idle_timeout_target, idle_timeout);
        let second = keep_alive.arm_timer(idle_timeout_target, idle_timeout);

        assert!(first);
        assert!(!second);
    }

    #[test]
    fn arm_timer_calculates_one_quarter_offset_correctly() {
        let clock = clock();
        let mut keep_alive = KeepAlive::default();
        let idle_timeout = Duration::from_secs(80);
        let target = clock + Duration::from_secs(200);

        keep_alive.arm_timer(target, idle_timeout);

        // Expected: 200 - (80 / 4) = 200 - 20 = 180
        let expected = clock + Duration::from_secs(180);
        assert_eq!(keep_alive.timer.next_expiration(), Some(expected));
    }

    #[test]
    fn multiple_timeout_cycles() {
        let mut clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);

        // First timeout cycle
        clock += idle_timeout;
        assert_eq!(keep_alive.on_timeout(&clock, idle_timeout), Poll::Ready(()));
        assert!(keep_alive.is_queued());
        keep_alive.on_transmit();
        assert!(keep_alive.state.is_cooldown());

        // Second timeout cycle
        clock += idle_timeout;
        assert_eq!(keep_alive.on_timeout(&clock, idle_timeout), Poll::Ready(()));
        assert!(keep_alive.is_queued());
        keep_alive.on_transmit();
        assert!(keep_alive.state.is_cooldown());

        // Third timeout cycle
        clock += idle_timeout;
        assert_eq!(keep_alive.on_timeout(&clock, idle_timeout), Poll::Ready(()));
        assert!(keep_alive.is_queued());
    }

    #[test]
    fn timer_expiration_at_exact_boundary() {
        let mut keep_alive = KeepAlive::default();
        let idle_timeout = Duration::from_secs(40);
        let mut clock = clock();
        let idle_timeout_target = clock + idle_timeout;

        keep_alive.set(true, idle_timeout_target, idle_timeout);
        clock += idle_timeout;

        // Use the expiration time as the clock
        assert_eq!(keep_alive.on_timeout(&clock, idle_timeout), Poll::Ready(()));
        assert!(keep_alive.is_queued());
    }

    #[test]
    fn very_short_duration() {
        let mut keep_alive = KeepAlive::default();
        let idle_timeout = Duration::from_nanos(1000);
        let mut clock = clock();
        let idle_timeout_target = clock + idle_timeout;

        keep_alive.set(true, idle_timeout_target, idle_timeout);

        clock += Duration::from_micros(1);
        let result = keep_alive.on_timeout(&clock, idle_timeout);

        assert_eq!(result, Poll::Ready(()));
    }

    #[test]
    fn rapid_idle_timer_updates() {
        let mut keep_alive = KeepAlive::default();
        let idle_timeout = Duration::from_secs(40);
        let base = clock();

        keep_alive.set(true, base + idle_timeout, idle_timeout);

        // Rapid updates with different targets
        for i in 1..=10 {
            let new_target = base + Duration::from_secs(40 + i);
            keep_alive.on_idle_timer_update(new_target, idle_timeout);
            assert!(keep_alive.state.is_cooldown());
        }
    }

    #[test]
    fn transmit_before_timeout_keeps_cooldown() {
        let mut clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);

        // Transmit before timeout (should be no-op)
        clock += Duration::from_secs(10);
        keep_alive.on_transmit();
        assert!(keep_alive.state.is_cooldown());

        // Timer should still expire later
        clock += idle_timeout;
        assert_eq!(keep_alive.on_timeout(&clock, idle_timeout), Poll::Ready(()));
    }

    #[test]
    fn timer_provider_delegates_to_internal_timer() {
        let clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);

        assert!(keep_alive.is_armed());
    }

    #[test]
    fn disable_mid_cycle_cancels_everything() {
        let mut clock = clock();
        let mut keep_alive = KeepAlive::default();
        let (idle_timeout_target, idle_timeout) = test_params(&clock);

        keep_alive.set(true, idle_timeout_target, idle_timeout);
        clock += Duration::from_secs(10);

        // Disable in the middle of waiting for timeout
        keep_alive.set(false, idle_timeout_target, idle_timeout);

        clock += idle_timeout;
        assert_eq!(keep_alive.on_timeout(&clock, idle_timeout), Poll::Pending);
        assert!(keep_alive.state.is_disabled());
    }
}
