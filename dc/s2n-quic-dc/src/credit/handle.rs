// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::Pool;
use crate::socket::channel;
use std::{
    sync::Arc,
    task::{Context, Poll, Waker},
};

pub struct Handle {
    pool: Arc<Pool>,
    held: u64,
}

impl Handle {
    #[inline]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool, held: 0 }
    }

    #[inline]
    pub fn poll_acquire(
        &mut self,
        cx: &mut Context<'_>,
        n: u64,
        last_epoch: u64,
        priority: usize,
    ) -> Poll<u64> {
        match self.pool.poll_acquire(cx, n, last_epoch, priority) {
            Poll::Ready(acquired) => {
                self.held = self.held.saturating_add(acquired);
                Poll::Ready(acquired)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    #[inline]
    pub fn try_acquire(&mut self, n: u64, last_epoch: u64) -> u64 {
        let acquired = self.pool.try_acquire(n, last_epoch);
        self.held = self.held.saturating_add(acquired);
        acquired
    }

    #[inline]
    pub fn release<W: channel::UnboundedSender<Waker>>(&mut self, n: u64, wake_sender: &mut W) {
        let released = n.min(self.held);
        self.held -= released;
        self.pool.release(released, wake_sender);
    }
}

impl Drop for Handle {
    #[inline]
    fn drop(&mut self) {
        if self.held > 0 {
            struct InlineWakeSender;
            impl channel::UnboundedSender<Waker> for InlineWakeSender {
                #[inline]
                fn send(&mut self, waker: Waker) -> Result<(), Waker> {
                    waker.wake();
                    Ok(())
                }
            }

            let mut sender = InlineWakeSender;
            self.pool.release(self.held, &mut sender);
        }
    }
}
