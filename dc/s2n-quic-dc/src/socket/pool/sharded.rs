// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{descriptor, Pool};
use std::{
    fmt,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
};

pub struct Sharded {
    pools: Arc<[Pool]>,
    mask: u16,
    index: AtomicU16,
}

impl fmt::Debug for Sharded {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sharded")
            .field("pools", &self.pools)
            .finish()
    }
}

impl Clone for Sharded {
    fn clone(&self) -> Self {
        Self {
            pools: self.pools.clone(),
            mask: self.mask,
            index: AtomicU16::new(self.index.fetch_add(1, Ordering::Relaxed)),
        }
    }
}

impl Sharded {
    pub fn new(pools: Arc<[Pool]>) -> Self {
        assert!(!pools.is_empty());
        assert!(pools.len().is_power_of_two());
        assert!(pools.len() <= u16::MAX as usize);
        let mask = (pools.len() - 1) as u16;
        Self {
            pools,
            mask,
            index: AtomicU16::new(0),
        }
    }

    pub fn alloc(&self) -> Option<descriptor::Unfilled> {
        self.pool().alloc()
    }

    pub fn alloc_or_grow(&self) -> descriptor::Unfilled {
        self.pool().alloc_or_grow()
    }

    fn pool(&self) -> &Pool {
        let index = self.index.fetch_add(1, Ordering::Relaxed) & self.mask;
        unsafe { self.pools.get_unchecked(index as usize) }
    }
}
