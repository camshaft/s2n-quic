// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Total byte budget (active_reserve + available = capacity at init)
    pub capacity: u64,
    /// Fraction reserved for active streams
    pub active_reserve_fraction: f64,
    /// Max bytes a single acquire call can request (burst cap)
    pub max_single_acquire: u64,
}

impl Default for Config {
    #[inline]
    fn default() -> Self {
        Self {
            capacity: 1024 * 1024,
            active_reserve_fraction: 0.25,
            max_single_acquire: 64 * 1024,
        }
    }
}

impl Config {
    #[inline]
    pub(crate) fn normalized(self) -> Self {
        let capacity = self.capacity.min(i64::MAX as u64);
        let max_single_acquire = self.max_single_acquire.min(capacity);
        let active_reserve_fraction = self.active_reserve_fraction.clamp(0.0, 1.0);

        Self {
            capacity,
            active_reserve_fraction,
            max_single_acquire,
        }
    }

    #[inline]
    pub(crate) fn clamp_request(&self, n: u64) -> u64 {
        n.min(self.max_single_acquire)
    }

    #[inline]
    pub(crate) fn active_reserve_target(&self) -> i64 {
        ((self.capacity as f64) * self.active_reserve_fraction) as i64
    }
}
