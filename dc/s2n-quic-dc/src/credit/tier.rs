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

    /// Detach the entire wait list into the distributor's task-local mirror, which shares this
    /// tier's list identity.
    ///
    /// The distributor calls this only to refill an *empty* mirror: it moves all currently-parked
    /// waiters out from under the tier mutex in one O(1) splice, then grants them off-lock across
    /// however many passes are needed. Unserved waiters remain in the mirror — they are never
    /// prepended back — so under a sustained backlog the tier mutex is taken only on refill.
    /// Because the returned list shares the tier's id, slots moving between the two keep the debug
    /// ownership stamp valid (and a slot lives in exactly one of the two lists at any time).
    ///
    /// Safe: holding `&mut self` (only reachable through the tier `MutexGuard`) is what authorizes
    /// the move; `List::detach` itself has no preconditions.
    #[inline]
    pub(crate) fn detach(&mut self) -> List<SlotAdapter> {
        self.list.detach()
    }
}
