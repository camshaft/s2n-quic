// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

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
