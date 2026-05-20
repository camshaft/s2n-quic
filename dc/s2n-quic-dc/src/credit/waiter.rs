// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{collections::VecDeque, task::Waker};

pub(crate) const PRIORITY_LEVELS: usize = 5;

pub(crate) struct WaiterQueue {
    /// One FIFO per priority level. Index 0 = highest priority.
    pub(crate) tiers: [VecDeque<WaiterEntry>; PRIORITY_LEVELS],
    pub(crate) total: usize,
}

pub(crate) struct WaiterEntry {
    pub(crate) waker: Waker,
    pub(crate) requested: u64,
}

impl WaiterQueue {
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            tiers: std::array::from_fn(|_| VecDeque::new()),
            total: 0,
        }
    }

    #[inline]
    pub(crate) fn push(&mut self, priority: usize, waiter: WaiterEntry) {
        let idx = priority.min(PRIORITY_LEVELS - 1);
        self.tiers[idx].push_back(waiter);
        self.total += 1;
    }
}
