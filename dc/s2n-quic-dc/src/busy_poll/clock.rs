// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::clock::precision::{self, Clock, Timestamp};
use core::task::Poll;
use std::{fmt::Debug, future::poll_fn};

#[derive(Clone, Debug)]
pub struct Timer<C: Clock> {
    clock: C,
    drift: u64,
}

impl<C: precision::Clock> Timer<C> {
    pub fn new(clock: C) -> Self {
        Self { clock, drift: 0 }
    }
}

impl<C: precision::Clock> precision::Clock for Timer<C> {
    fn now(&self) -> Timestamp {
        self.clock.now()
    }
}

impl<C: precision::Clock + Send + Sync> precision::Timer for Timer<C> {
    async fn sleep_until(&mut self, target: Timestamp) {
        poll_fn(|_cx| {
            let now = self.clock.now();
            if let Some(diff) = now.nanos.checked_sub(target.nanos) {
                // eprintln!("diff: {}", diff);
                self.drift = (self.drift * 7 + diff) / 8;
                Poll::Ready(())
            } else if let Some(diff) = (now.nanos + self.drift).checked_sub(target.nanos) {
                let diff = diff.saturating_sub(self.drift);
                // eprintln!("diff: {}", diff);
                self.drift = (self.drift * 7 + diff) / 8;
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }
}
