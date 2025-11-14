// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::clock::{self, SleepHandle};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use pin_project_lite::pin_project;
use s2n_quic_core::time::Timestamp;
use std::fmt::Debug;

#[derive(Clone, Debug)]
pub struct Clock<C> {
    inner: C,
}

impl<C: clock::Clock + Clone> Clock<C> {
    pub fn new(inner: C) -> Self {
        Self { inner }
    }
}

impl<C: clock::Clock + Clone + Unpin> clock::Clock for Clock<C> {
    #[inline]
    fn sleep(&self, amount: Duration) -> (SleepHandle, Timestamp) {
        let now = self.inner.get_time();
        let clock = self.clone();
        let target = now + amount;
        let sleep = Sleep {
            clock,
            target: Some(target),
        };
        let sleep = Box::pin(sleep);
        (sleep, target)
    }
}

impl<C: clock::Clock> s2n_quic_core::time::Clock for Clock<C> {
    fn get_time(&self) -> Timestamp {
        self.inner.get_time()
    }
}

pin_project!(
    #[derive(Debug)]
    pub struct Sleep<C: clock::Clock> {
        #[pin]
        clock: C,
        target: Option<Timestamp>,
    }
);

impl<C: clock::Clock + Debug + Unpin> clock::Sleep for Sleep<C> {
    fn update(self: Pin<&mut Self>, target: Timestamp) {
        *self.project().target = Some(target);
    }
}

impl<C: clock::Clock + Debug> clock::Clock for Sleep<C> {
    fn sleep(&self, amount: Duration) -> (SleepHandle, Timestamp) {
        self.clock.sleep(amount)
    }
}

impl<C: clock::Clock> s2n_quic_core::time::Clock for Sleep<C> {
    fn get_time(&self) -> Timestamp {
        self.clock.get_time()
    }
}

impl<C: clock::Clock + Unpin + 'static> Future for Sleep<C> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let now = this.clock.get_time();
        match this.target {
            None => Poll::Ready(()),
            Some(target) if now >= target => {
                this.target = None;
                Poll::Ready(())
            }
            _ => Poll::Pending,
        }
    }
}
