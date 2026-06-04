// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::slot::{Slot, SlotAdapter, SlotPtr};
use crate::intrusive::List;
use std::ptr::NonNull;

pub(crate) struct Tier {
    list: List<SlotAdapter>,
}

impl Tier {
    #[inline]
    pub(crate) fn new() -> Self {
        Self { list: List::new() }
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.list.len()
    }

    /// Link a slot into the back of this tier's wait list.
    ///
    /// # Safety
    ///
    /// The slot must have been prepared for parking and the caller must
    /// hold the tier mutex.
    #[inline]
    pub(crate) unsafe fn push(&mut self, ptr: NonNull<Slot>) {
        self.list.push_back(SlotPtr::new(ptr));
    }

    /// Pop the first slot from the list.
    ///
    /// Returns the owning `SlotPtr`. The caller must either:
    /// - Call `.take()` to suppress the drop (normal grant path), or
    /// - Let it drop (pool shutdown path — writes sentinel and wakes).
    #[inline]
    pub(crate) fn pop_front(&mut self) -> Option<SlotPtr> {
        self.list.pop_front()
    }
}
