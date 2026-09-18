// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{fmt, ops, time::Duration};

pub trait Clock: s2n_quic_core::time::Clock + Send + Sync + 'static {
    type Timer: Timer;

    fn now(&self) -> Timestamp;

    /// A coarse "now" for callers that poll on a fixed cadence and explicitly accept a possibly
    /// stale reading (e.g. busy-poll timer-expiry checks, which are already sweep-granular).
    ///
    /// The default is a genuine fresh [`now`](Self::now) read — implementations that maintain a
    /// cheaper cached clock override this. Callers that need an exact current time must use
    /// [`now`](Self::now); this exists so a fresh-read caller never silently gets a stale value.
    fn coarse_now(&self) -> Timestamp {
        self.now()
    }

    /// Creates a new timer from this clock.
    fn timer(&self) -> Self::Timer;
}

pub trait Timer: Send + 'static {
    fn now(&self) -> Timestamp;

    /// Coarse counterpart to [`now`](Self::now); see [`Clock::coarse_now`]. Defaults to a fresh read.
    fn coarse_now(&self) -> Timestamp {
        self.now()
    }

    fn sleep_until(&mut self, target: Timestamp) -> impl core::future::Future<Output = ()> + Send;

    /// Poll to see if the timer has expired.
    fn poll_ready(&mut self, cx: &mut core::task::Context) -> core::task::Poll<()>;

    /// Update the timer target.
    fn update(&mut self, target: Timestamp);

    /// Cancel the timer.
    fn cancel(&mut self);

    /// Check if the timer is armed (has a target).
    fn is_armed(&self) -> bool;
}

#[derive(Clone, Copy, PartialEq, PartialOrd, Eq, Ord)]
pub struct Timestamp {
    pub(crate) nanos: u64,
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.nanos / 1_000_000_000;
        let nanos = self.nanos % 1_000_000_000;
        write!(f, "{secs}.{nanos:09}")
    }
}

impl Timestamp {
    pub fn duration_since(&self, other: Timestamp) -> std::time::Duration {
        let nanos = self.nanos.saturating_sub(other.nanos);
        std::time::Duration::from_nanos(nanos)
    }

    pub fn nanos_since(&self, other: Timestamp) -> u64 {
        self.nanos.saturating_sub(other.nanos)
    }
}

impl ops::Add<Duration> for Timestamp {
    type Output = Timestamp;

    fn add(self, rhs: Duration) -> Self::Output {
        let nanos = self.nanos.saturating_add(rhs.as_nanos() as _);
        Timestamp { nanos }
    }
}

impl ops::Sub<Duration> for Timestamp {
    type Output = Timestamp;

    fn sub(self, rhs: Duration) -> Self::Output {
        let nanos = self.nanos.saturating_sub(rhs.as_nanos() as _);
        Timestamp { nanos }
    }
}

impl ops::Sub for Timestamp {
    type Output = Duration;

    fn sub(self, rhs: Self) -> Self::Output {
        let nanos = self.nanos.saturating_sub(rhs.nanos);
        std::time::Duration::from_nanos(nanos)
    }
}

impl From<s2n_quic_core::time::Timestamp> for Timestamp {
    fn from(value: s2n_quic_core::time::Timestamp) -> Self {
        let nanos = unsafe { value.as_duration().as_nanos() as u64 };
        Timestamp { nanos }
    }
}

impl From<Timestamp> for s2n_quic_core::time::Timestamp {
    fn from(value: Timestamp) -> Self {
        let duration = std::time::Duration::from_nanos(value.nanos);
        unsafe { s2n_quic_core::time::Timestamp::from_duration(duration) }
    }
}

impl s2n_quic_core::time::Clock for Timestamp {
    fn get_time(&self) -> s2n_quic_core::time::Timestamp {
        (*self).into()
    }
}

impl Timer for crate::time::Timer {
    fn now(&self) -> Timestamp {
        use s2n_quic_core::time::Clock;
        let nanos = unsafe { self.get_time().as_duration().as_nanos() as u64 };
        Timestamp { nanos }
    }

    async fn sleep_until(&mut self, target: Timestamp) {
        let target = std::time::Duration::from_nanos(target.nanos);
        let target = unsafe { s2n_quic_core::time::Timestamp::from_duration(target) };
        self.sleep(target).await;
    }

    fn poll_ready(&mut self, cx: &mut core::task::Context) -> core::task::Poll<()> {
        <Self as s2n_quic_core::time::clock::Timer>::poll_ready(self, cx)
    }

    fn update(&mut self, target: Timestamp) {
        let core_target = std::time::Duration::from_nanos(target.nanos);
        let core_target = unsafe { s2n_quic_core::time::Timestamp::from_duration(core_target) };
        <Self as s2n_quic_core::time::clock::Timer>::update(self, core_target);
    }

    fn cancel(&mut self) {
        self.cancel();
    }

    fn is_armed(&self) -> bool {
        use s2n_quic_core::time::timer::Provider;
        Provider::is_armed(self)
    }
}
