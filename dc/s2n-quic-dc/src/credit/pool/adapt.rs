// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The adaptive capacity of a credit pool.
//!
//! An adaptive pool starts at its configured capacity and changes it from the device's answers. A
//! would-block answer halves the capacity, and a run of clean completions raises it by one
//! operation's cost. A *clean completion* is the completion of an operation that the device took
//! without a would-block answer and that returns its credit while a submitter waits for credit
//! from the same pool. A *run* ends once the costs of its clean completions add up to the capacity
//! at its start; a would-block answer or a raise starts a new run.
//!
//! [`Adapt`] holds the arithmetic alone. The pool keeps it under a mutex, and the distributor
//! applies each change of the capacity to the pool's free credit.

use std::collections::{btree_map::Entry, BTreeMap};

/// The capacity of one adaptive pool, with the state its changes read.
#[derive(Debug)]
pub(crate) struct Adapt {
    /// The current capacity.
    capacity: u64,
    /// The upper bound the configuration sets, where it sets one.
    max_capacity: Option<u64>,
    /// Advanced by each decrease. An operation keeps the epoch in which its credit was granted,
    /// so that a would-block answer to an operation granted before the last decrease is skipped.
    epoch: u64,
    /// The capacity at the start of the current run.
    run_start: u64,
    /// The summed cost of the current run's clean completions.
    run_sum: u64,
    /// Each cost among the operations waiting for the pool's credit, with how many wait at it.
    waiting: BTreeMap<u64, usize>,
}

impl Adapt {
    //= rendered/0011-storage/02-admission.md#DC-0011-045
    //# The device MUST start each pool's capacity at the capacity the device's configuration sets
    //# for that pool.
    pub(crate) fn new(capacity: u64, max_capacity: Option<u64>) -> Self {
        Self {
            capacity,
            max_capacity,
            epoch: 0,
            run_start: capacity,
            run_sum: 0,
            waiting: BTreeMap::new(),
        }
    }

    /// The current capacity.
    #[inline]
    pub(crate) fn capacity(&self) -> u64 {
        self.capacity
    }

    /// The number of decreases so far.
    #[inline]
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    fn start_run(&mut self) {
        self.run_start = self.capacity;
        self.run_sum = 0;
    }

    fn largest_waiting(&self) -> u64 {
        self.waiting.keys().next_back().copied().unwrap_or(0)
    }

    /// An operation of `cost` begins to wait for the pool's credit. Returns the raised capacity
    /// where the cost is higher than the capacity.
    //= rendered/0011-storage/02-admission.md#DC-0011-048
    //# When an operation whose cost is higher than its pool's capacity begins to wait for the
    //# pool's credit, the device MUST raise the pool's capacity to that operation's cost.
    pub(crate) fn on_wait(&mut self, cost: u64) -> Option<u64> {
        *self.waiting.entry(cost).or_insert(0) += 1;
        if cost <= self.capacity {
            return None;
        }
        self.capacity = cost;
        self.start_run();
        Some(cost)
    }

    /// An operation of `cost` stops waiting, because it was granted or is gone.
    pub(crate) fn on_wait_end(&mut self, cost: u64) {
        if let Entry::Occupied(mut entry) = self.waiting.entry(cost) {
            *entry.get_mut() -= 1;
            if *entry.get() == 0 {
                entry.remove();
            }
        }
    }

    /// The device answered would-block to an operation that the pool granted in `grant_epoch`.
    /// Returns the lowered capacity where the answer lowers it.
    //= rendered/0011-storage/02-admission.md#DC-0011-046
    //# On a would-block answer to an operation, the device MUST lower the capacity of the pool the
    //# operation drew from to half its value, rounded up, or to the largest cost among the
    //# operations waiting for that pool's credit where that cost is higher.
    //= rendered/0011-storage/02-admission.md#DC-0011-047
    //# The device MUST NOT lower a pool's capacity for a would-block answer to an operation that
    //# the pool granted before its last decrease.
    pub(crate) fn on_would_block(&mut self, grant_epoch: u64) -> Option<u64> {
        // Every would-block answer starts a new run, whether or not it lowers the capacity.
        self.start_run();
        if grant_epoch < self.epoch {
            return None;
        }
        let half = self.capacity - self.capacity / 2;
        let lowered = half.max(self.largest_waiting());
        if lowered >= self.capacity {
            return None;
        }
        self.capacity = lowered;
        self.epoch += 1;
        self.start_run();
        Some(lowered)
    }

    /// An operation of `cost` completed without a would-block answer and returned its credit.
    /// Returns the raised capacity where its completion ends a run that raises it.
    //= rendered/0011-storage/02-admission.md#DC-0011-049
    //# After each run of clean completions from a pool, the device MUST raise that pool's capacity
    //# by the cost of the operation whose completion ends the run, up to the upper bound the
    //# device's configuration sets for the pool where it sets one.
    pub(crate) fn on_clean_completion(&mut self, cost: u64) -> Option<u64> {
        // A completion is clean only while a submitter waits for the pool's credit.
        if self.waiting.is_empty() {
            return None;
        }
        self.run_sum = self.run_sum.saturating_add(cost);
        if self.run_sum < self.run_start {
            return None;
        }
        let mut raised = self.capacity.saturating_add(cost);
        if let Some(max) = self.max_capacity {
            raised = raised.min(max);
        }
        let changed = raised > self.capacity;
        self.capacity = self.capacity.max(raised);
        self.start_run();
        changed.then_some(self.capacity)
    }

    /// The number of operations waiting for the pool's credit.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn debug_waiting(&self) -> usize {
        self.waiting.values().sum()
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::Adapt;
    use crate::{
        credit::{Config, Counters, Pool, Priority},
        sync::Ordering,
    };

    //= rendered/0011-storage/02-admission.md#DC-0011-045
    //= type=test
    //# The device MUST start each pool's capacity at the capacity the device's configuration sets
    //# for that pool.
    #[test]
    fn starts_at_the_configured_capacity() {
        let adapt = Adapt::new(64, None);
        assert_eq!(adapt.capacity(), 64);
        assert_eq!(adapt.epoch(), 0);
    }

    //= rendered/0011-storage/02-admission.md#DC-0011-046
    //= type=test
    //# On a would-block answer to an operation, the device MUST lower the capacity of the pool the
    //# operation drew from to half its value, rounded up, or to the largest cost among the
    //# operations waiting for that pool's credit where that cost is higher.
    #[test]
    fn a_would_block_halves_the_capacity_rounded_up() {
        let mut adapt = Adapt::new(7, None);
        assert_eq!(adapt.on_would_block(0), Some(4));
        assert_eq!(adapt.capacity(), 4);
        assert_eq!(adapt.epoch(), 1);
    }

    //= rendered/0011-storage/02-admission.md#DC-0011-046
    //= type=test
    //# On a would-block answer to an operation, the device MUST lower the capacity of the pool the
    //# operation drew from to half its value, rounded up, or to the largest cost among the
    //# operations waiting for that pool's credit where that cost is higher.
    #[test]
    fn a_decrease_stops_at_the_largest_waiting_cost() {
        let mut adapt = Adapt::new(64, None);
        assert_eq!(adapt.on_wait(40), None);
        assert_eq!(adapt.on_would_block(0), Some(40));
        assert_eq!(adapt.capacity(), 40);

        let mut adapt = Adapt::new(64, None);
        assert_eq!(adapt.on_wait(64), None);
        assert_eq!(adapt.on_would_block(0), None);
        assert_eq!(adapt.capacity(), 64);
        assert_eq!(adapt.epoch(), 0);
    }

    //= rendered/0011-storage/02-admission.md#DC-0011-047
    //= type=test
    //# The device MUST NOT lower a pool's capacity for a would-block answer to an operation that
    //# the pool granted before its last decrease.
    #[test]
    fn a_would_block_granted_before_the_last_decrease_is_skipped() {
        let mut adapt = Adapt::new(64, None);
        assert_eq!(adapt.on_would_block(0), Some(32));
        assert_eq!(adapt.on_would_block(0), None);
        assert_eq!(adapt.capacity(), 32);
        assert_eq!(adapt.on_would_block(1), Some(16));
        assert_eq!(adapt.epoch(), 2);
    }

    //= rendered/0011-storage/02-admission.md#DC-0011-048
    //= type=test
    //# When an operation whose cost is higher than its pool's capacity begins to wait for the
    //# pool's credit, the device MUST raise the pool's capacity to that operation's cost.
    #[test]
    fn a_waiter_above_the_capacity_raises_it_to_its_cost() {
        let mut adapt = Adapt::new(64, None);
        assert_eq!(adapt.on_would_block(0), Some(32));
        assert_eq!(adapt.on_wait(48), Some(48));
        adapt.on_wait_end(48);
        assert_eq!(adapt.capacity(), 48);
    }

    //= rendered/0011-storage/02-admission.md#DC-0011-049
    //= type=test
    //# After each run of clean completions from a pool, the device MUST raise that pool's capacity
    //# by the cost of the operation whose completion ends the run, up to the upper bound the
    //# device's configuration sets for the pool where it sets one.
    #[test]
    fn a_run_of_clean_completions_raises_by_the_last_cost() {
        let mut adapt = Adapt::new(8, None);
        assert_eq!(adapt.on_wait(4), None);
        assert_eq!(adapt.on_clean_completion(4), None);
        assert_eq!(adapt.on_clean_completion(4), Some(12));
        // The raise starts a new run, which ends once its costs add up to 12.
        assert_eq!(adapt.on_clean_completion(8), None);
        assert_eq!(adapt.on_clean_completion(4), Some(16));
    }

    //= rendered/0011-storage/02-admission.md#DC-0011-049
    //= type=test
    //# After each run of clean completions from a pool, the device MUST raise that pool's capacity
    //# by the cost of the operation whose completion ends the run, up to the upper bound the
    //# device's configuration sets for the pool where it sets one.
    #[test]
    fn a_raise_stops_at_the_upper_bound() {
        let mut adapt = Adapt::new(8, Some(10));
        assert_eq!(adapt.on_wait(4), None);
        assert_eq!(adapt.on_clean_completion(8), Some(10));
        assert_eq!(adapt.on_clean_completion(10), None);
        assert_eq!(adapt.capacity(), 10);
    }

    #[test]
    fn a_completion_without_a_waiter_isnt_counted() {
        let mut adapt = Adapt::new(8, None);
        assert_eq!(adapt.on_clean_completion(8), None);
        assert_eq!(adapt.on_wait(1), None);
        // The completion above didn't join the run, so this one doesn't end it.
        assert_eq!(adapt.on_clean_completion(4), None);
        assert_eq!(adapt.capacity(), 8);
    }

    #[test]
    fn a_would_block_starts_a_new_run() {
        let mut adapt = Adapt::new(8, None);
        assert_eq!(adapt.on_wait(1), None);
        assert_eq!(adapt.on_would_block(0), Some(4));
        assert_eq!(adapt.on_clean_completion(2), None);
        // A stale answer doesn't lower the capacity, and it still starts a new run.
        assert_eq!(adapt.on_would_block(0), None);
        assert_eq!(adapt.on_clean_completion(2), None);
        assert_eq!(adapt.on_clean_completion(2), Some(6));
    }

    #[test]
    fn a_waiter_that_stops_waiting_doesnt_hold_a_decrease() {
        let mut adapt = Adapt::new(64, None);
        assert_eq!(adapt.on_wait(64), None);
        adapt.on_wait_end(64);
        assert_eq!(adapt.on_would_block(0), Some(32));
    }

    fn config(capacity: u64) -> Config {
        Config {
            capacity,
            max_single_acquire: [capacity; Priority::LEVELS],
            min_grant_slice: [capacity; Priority::LEVELS],
            refill: None,
        }
    }

    fn adaptive(capacity: u64, max_capacity: Option<u64>) -> Pool {
        Pool::with_counters_adaptive(config(capacity), Counters::default(), max_capacity)
    }

    //= rendered/0011-storage/08-observability.md#DC-0011-053
    //= type=test
    //# The device MUST report the current capacity of each of its pools.
    #[test]
    fn an_adaptive_pool_starts_at_its_configured_capacity() {
        let pool = adaptive(64, None);
        assert_eq!(pool.capacity(), 64);
        assert_eq!(pool.decrease_epoch(), 0);
        assert_eq!(pool.debug_free_total(), 64);
    }

    //= rendered/0011-storage/02-admission.md#DC-0011-051
    //= type=test
    //# The device MUST change a pool's free credit by the same amount as each change of the pool's
    //# capacity.
    //= rendered/0011-storage/02-admission.md#DC-0011-052
    //= type=test
    //# A decrease of a pool's capacity MUST NOT take credit from an operation that holds it.
    #[test]
    fn a_decrease_takes_its_difference_from_the_free_credit() {
        let pool = adaptive(64, None);
        // An operation holds 16 credits.
        pool.available.fetch_sub(16, Ordering::Relaxed);
        pool.on_would_block(pool.decrease_epoch());
        assert_eq!(pool.decrease_epoch(), 1);
        assert_eq!(pool.apply_capacity_target(), 0);
        assert_eq!(pool.capacity(), 32);
        assert_eq!(pool.debug_available(), 16);
        // The holder returns its whole 16, and the pool conserves the lowered capacity.
        pool.release(16);
        assert_eq!(pool.debug_free_total(), 32);
    }

    //= rendered/0011-storage/02-admission.md#DC-0011-051
    //= type=test
    //# The device MUST change a pool's free credit by the same amount as each change of the pool's
    //# capacity.
    #[test]
    fn a_raise_adds_its_difference_to_the_free_credit() {
        let pool = adaptive(64, None);
        assert!(pool.adapt_with(|adapt| adapt.on_wait(80).is_some()));
        assert_eq!(pool.apply_capacity_target(), 16);
        assert_eq!(pool.capacity(), 80);
        // Applied once: the next pass doesn't add it again.
        assert_eq!(pool.apply_capacity_target(), 0);
        pool.end_wait(80);
    }

    //= rendered/0011-storage/02-admission.md#DC-0011-049
    //= type=test
    //# After each run of clean completions from a pool, the device MUST raise that pool's capacity
    //# by the cost of the operation whose completion ends the run, up to the upper bound the
    //# device's configuration sets for the pool where it sets one.
    #[test]
    fn a_pool_raises_after_a_run_of_clean_completions() {
        let pool = adaptive(8, None);
        assert!(!pool.adapt_with(|adapt| adapt.on_wait(4).is_some()));
        pool.on_clean_completion(4);
        pool.on_clean_completion(4);
        assert_eq!(pool.apply_capacity_target(), 4);
        assert_eq!(pool.capacity(), 12);
        pool.end_wait(4);
    }

    #[test]
    fn a_fixed_pool_keeps_its_capacity() {
        let pool = Pool::new(config(64));
        pool.on_would_block(0);
        assert!(!pool.adapt_with(|adapt| adapt.on_wait(80).is_some()));
        pool.on_clean_completion(64);
        assert_eq!(pool.apply_capacity_target(), 0);
        assert_eq!(pool.capacity(), 64);
        assert_eq!(pool.decrease_epoch(), 0);
    }
}
