// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::intrusive::{Entry, Queue};
use std::task::Waker;

pub(crate) const PRIORITY_LEVELS: usize = 5;

pub(crate) struct WaiterQueue {
    pub(crate) queue: Queue<WaiterEntry>,
}

pub(crate) struct WaiterEntry {
    pub(crate) waker: Waker,
    pub(crate) requested: u64,
}

impl WaiterQueue {
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            queue: Queue::new(),
        }
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    #[inline]
    pub(crate) fn front_requested(&self) -> Option<u64> {
        self.queue.front().map(|waiter| waiter.requested)
    }

    #[inline]
    pub(crate) fn push(&mut self, waiter: WaiterEntry) {
        self.queue.push_back(Entry::new(waiter));
    }

    #[inline]
    pub(crate) fn pop(&mut self) -> Option<WaiterEntry> {
        self.queue.pop_front().map(|waiter| waiter.into_inner())
    }
}
