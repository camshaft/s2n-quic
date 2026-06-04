// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Total byte budget for the pool.
    pub capacity: u64,
    /// Minimum bytes to grant per woken stream. Prevents fragmenting credits
    /// into units too small for meaningful work.
    pub min_grant: u64,
    /// Maximum bytes a single CAS fast-path acquisition can take.
    pub max_single_acquire: u64,
}

impl Default for Config {
    #[inline]
    fn default() -> Self {
        Self {
            capacity: 256 * 1024 * 1024,
            min_grant: 64 * 1024,
            max_single_acquire: 4 * 1024 * 1024,
        }
    }
}

impl Config {
    #[inline]
    pub(crate) fn normalized(self) -> Self {
        Self {
            capacity: self.capacity.min(i64::MAX as u64),
            min_grant: self.min_grant.max(1),
            max_single_acquire: self.max_single_acquire.max(1).min(i64::MAX as u64),
        }
    }

    #[inline]
    pub(crate) fn clamp_request(&self, n: u64) -> u64 {
        n.min(self.max_single_acquire)
    }
}
