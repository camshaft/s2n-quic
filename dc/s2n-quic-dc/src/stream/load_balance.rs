// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicUsize, Ordering};

/// Implements "pick 2" load balancing for selecting from a pool of resources.
///
/// This strategy selects two candidates and chooses the one with fewer references,
/// providing good load distribution with minimal overhead.
pub struct PickTwo {
    next_idx: AtomicUsize,
}

impl PickTwo {
    pub fn new() -> Self {
        Self {
            next_idx: AtomicUsize::new(0),
        }
    }

    /// Selects an index from the pool using "pick 2" load balancing.
    ///
    /// If the pool has only one item, returns index 0. Otherwise, selects two
    /// candidates and returns the index of the one with lower weight, indicating lower load.
    ///
    /// The `weight_fn` closure is called on each candidate to determine its current load.
    /// The `random_fn` closure is called to generate a random index in the range [0, upper_bound).
    pub fn select<T>(
        &self,
        pool: &[T],
        weight_fn: impl Fn(&T) -> usize,
        random_fn: impl Fn(usize) -> usize,
    ) -> usize {
        if pool.len() == 1 {
            return 0;
        }

        let idx1 = self.next_idx.fetch_add(1, Ordering::Relaxed) % pool.len();
        let mut idx2 = random_fn(pool.len() - 1);

        // shift the second index up so they're non-overlapping
        if idx2 >= idx1 {
            idx2 += 1;
        }

        // Choose the item with lower weight
        let weight1 = weight_fn(&pool[idx1]);
        let weight2 = weight_fn(&pool[idx2]);

        if weight1 <= weight2 {
            idx1
        } else {
            idx2
        }
    }
}

impl Default for PickTwo {
    fn default() -> Self {
        Self::new()
    }
}
