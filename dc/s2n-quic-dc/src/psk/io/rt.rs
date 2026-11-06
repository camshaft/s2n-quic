// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The clock, timers and tasks of the PSK handshake path: tokio's, or the bach simulation's where
//! one is active, so that the crate's bach harness runs the PSK client and servers.

use core::{future::Future, ops::Add, time::Duration};

/// A point in time on tokio's clock, or on the bach simulation's clock where one was active when
/// it was read.
#[derive(Clone, Copy, Debug)]
pub(super) enum Instant {
    Tokio(tokio::time::Instant),
    #[cfg(any(test, feature = "testing"))]
    Bach(bach::time::Instant),
}

impl Instant {
    /// Reads the bach simulation's clock where a simulation is active, and tokio's otherwise.
    pub(super) fn now() -> Self {
        #[cfg(any(test, feature = "testing"))]
        if bach::is_active() {
            return Self::Bach(bach::time::Instant::now());
        }
        Self::Tokio(tokio::time::Instant::now())
    }

    /// Returns the time since this instant, on the clock it was read on.
    pub(super) fn elapsed(&self) -> Duration {
        match self {
            Self::Tokio(instant) => instant.elapsed(),
            #[cfg(any(test, feature = "testing"))]
            Self::Bach(instant) => instant.elapsed(),
        }
    }
}

impl Add<Duration> for Instant {
    type Output = Self;

    fn add(self, duration: Duration) -> Self {
        match self {
            Self::Tokio(instant) => Self::Tokio(instant + duration),
            #[cfg(any(test, feature = "testing"))]
            Self::Bach(instant) => Self::Bach(instant + duration),
        }
    }
}

/// The error of a [`timeout_at`] whose deadline came first.
#[derive(Debug)]
pub(super) struct Elapsed;

/// Waits for `future` until `deadline`, on the clock `deadline` was read on.
pub(super) async fn timeout_at<F: Future>(
    deadline: Instant,
    future: F,
) -> Result<F::Output, Elapsed> {
    match deadline {
        Instant::Tokio(deadline) => tokio::time::timeout_at(deadline, future)
            .await
            .map_err(|_| Elapsed),
        #[cfg(any(test, feature = "testing"))]
        Instant::Bach(deadline) => bach::time::timeout_at(deadline, future)
            .await
            .map_err(|_| Elapsed),
    }
}

/// Sleeps for `duration`, on the bach simulation's clock where a simulation is active, and on
/// tokio's otherwise.
pub(super) async fn sleep(duration: Duration) {
    #[cfg(any(test, feature = "testing"))]
    if bach::is_active() {
        bach::time::sleep(duration).await;
        return;
    }
    tokio::time::sleep(duration).await;
}

/// Spawns `future` as a task of the bach simulation where one is active, and of tokio's runtime
/// otherwise.
pub(super) fn spawn<F>(future: F)
where
    F: 'static + Send + Future<Output = ()>,
{
    #[cfg(any(test, feature = "testing"))]
    if bach::is_active() {
        bach::spawn(future);
        return;
    }
    tokio::spawn(future);
}
