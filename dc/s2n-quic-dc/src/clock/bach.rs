// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use bach::time::{self, sleep_until, Instant};

fn root() -> Instant {
    use std::sync::OnceLock;
    static ROOT: OnceLock<Instant> = OnceLock::new();

    *ROOT.get_or_init(|| unsafe {
        // SAFETY: bach stores durations
        // TODO: add a `zero` method in bach
        core::mem::transmute(core::time::Duration::ZERO)
    })
}

impl_clock!();

impl crate::clock::precision::Timer for Clock {
    async fn sleep_until(&mut self, target: super::precision::Timestamp) {
        let target = self.0 + core::time::Duration::from_nanos(target.nanos);
        sleep_until(target).await;
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        clock::{bach::Clock, Timer},
        testing::{ext::*, sim},
    };
    use bach::time::Instant;
    use core::time::Duration;
    use s2n_quic_core::time::{clock::Timer as _, Clock as _};

    #[test]
    fn clock_test() {
        sim(|| {
            async {
                let clock = Clock::default();
                let mut timer = Timer::new(&clock);
                timer.ready().await;
                let before = clock.get_time();
                let wait = Duration::from_secs(1);
                let target = before + wait;
                timer.update(target);
                timer.ready().await;
                assert_eq!(before + wait, clock.get_time());
            }
            .primary()
            .spawn();
        });
    }
}
