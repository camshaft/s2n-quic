// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tokio-based clock implementation with microsecond-accurate timing
//!
//! This module provides a clock implementation that supports microsecond-accurate
//! sleep operations, which is essential for the packet pacer in s2n-quic-dc.
//!
//! ## Timer Behavior
//!
//! The timer uses a hybrid approach to achieve microsecond accuracy while remaining
//! cooperative with the async executor:
//!
//! - **For delays > 1ms**: Uses tokio's native timer to sleep until 1ms before the
//!   target time, then switches to spinning for the final microsecond-level precision.
//!
//! - **For delays ≤ 1ms**: Uses a cooperative spin-loop that repeatedly wakes and yields
//!   the task to the executor. This allows microsecond-level precision without blocking
//!   the executor thread.
//!
//! ## Performance Implications
//!
//! The cooperative spin-loop approach for short delays will:
//! - Increase CPU usage compared to coarser-grained timers
//! - Wake the task frequently (on every executor poll cycle)
//! - Provide much better precision than tokio's millisecond-floored timer
//!
//! This trade-off is necessary for the packet pacer, which requires microsecond-accurate
//! timing for token bucket refills and timing wheel granularity.
//!
//! ## Example
//!
//! ```no_run
//! use s2n_quic_dc::clock::{tokio::Clock, Timer};
//! use std::time::Duration;
//! use s2n_quic_core::time::{clock::Timer as _, Clock as _};
//!
//! # async fn example() {
//! let clock = Clock::default();
//! let mut timer = Timer::new(&clock);
//!
//! // Sleep for 100 microseconds with high precision
//! let start = clock.get_time();
//! timer.update(start + Duration::from_micros(100));
//! timer.ready().await;
//! # }
//! ```

use tokio::time::{self, sleep_until, Instant};

impl_microsecond_clock!();

#[cfg(test)]
mod tests {
    use crate::clock::{tokio::Clock, Timer};
    use core::time::Duration;
    use s2n_quic_core::time::{clock::Timer as _, Clock as _};

    #[tokio::test]
    async fn clock_test() {
        let clock = Clock::default();
        let mut timer = Timer::new(&clock);
        timer.ready().await;
        timer.update(clock.get_time() + Duration::from_secs(1));
        timer.ready().await;
    }

    #[tokio::test]
    async fn microsecond_precision_test() {
        let clock = Clock::default();
        let mut timer = Timer::new(&clock);
        
        // Test microsecond-level sleep
        let start = clock.get_time();
        timer.update(start + Duration::from_micros(100));
        timer.ready().await;
        let elapsed = clock.get_time() - start;
        
        // Should be close to 100 microseconds (allow some tolerance for scheduling)
        let elapsed_micros = unsafe { elapsed.as_duration() }.as_micros();
        assert!(elapsed_micros >= 100, "elapsed {} < 100 microseconds", elapsed_micros);
        assert!(elapsed_micros < 1000, "elapsed {} >= 1000 microseconds (too much delay)", elapsed_micros);
    }
}
