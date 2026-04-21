// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::clock::precision::{self, Clock, Timestamp};
use core::task::Poll;
use std::{fmt::Debug, future::poll_fn};

#[derive(Clone, Debug)]
pub struct Timer<C: Clock> {
    clock: C,
}

impl<C: precision::Clock> Timer<C> {
    pub fn new(clock: C) -> Self {
        Self { clock }
    }
}

impl<C: precision::Clock + Send + Sync + Clone> precision::Clock for Timer<C> {
    type Timer = Self;

    fn now(&self) -> Timestamp {
        self.clock.now()
    }

    fn timer(&self) -> Self::Timer {
        self.clone()
    }
}

impl<C: precision::Clock + Send + Sync + Clone> precision::Timer for Timer<C> {
    fn now(&self) -> Timestamp {
        precision::Clock::now(self)
    }

    async fn sleep_until(&mut self, target: Timestamp) {
        let mut yielded = false;
        poll_fn(|_cx| {
            if core::mem::replace(&mut yielded, true) && self.clock.now().nanos >= target.nanos {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }
}
