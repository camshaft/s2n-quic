// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A persistent task waker for the credit distributor.
//!
//! Unlike [`atomic_waker::AtomicWaker`], this never *clears* the stored waker on `wake`, so the
//! distributor does not have to re-arm every poll. It is registered by the single distributor and
//! woken by any number of producers via `wake_by_ref`.
//!
//! The stored waker is updated only when it actually changes (`will_wake`), which is the common
//! steady state on tokio/bach (the task's waker is stable across polls). We do not *assume*
//! stability, though: the `Future` contract permits a fresh waker on any poll (and loom's executor
//! exercises exactly that), so `register` always refreshes a changed waker rather than asserting.
//!
//! Backed by `crate::sync::RwLock`, so under the `loom` feature the register/wake race is modeled.

use crate::sync::RwLock;
use std::task::Waker;

pub(crate) struct TaskWaker(RwLock<Option<Waker>>);

impl TaskWaker {
    #[inline]
    pub(crate) fn new() -> Self {
        Self(RwLock::new(None))
    }

    /// Register (or refresh) the distributor's waker. Called only by the single distributor. The
    /// common case — an unchanged waker — takes only the read lock; the write lock is taken just on
    /// first registration or an actual change.
    #[inline]
    pub(crate) fn register(&self, waker: &Waker) {
        if let Some(existing) = self.0.read().unwrap().as_ref() {
            if existing.will_wake(waker) {
                return;
            }
        }
        *self.0.write().unwrap() = Some(waker.clone());
    }

    /// Wake the distributor. Never clears the stored waker.
    #[inline]
    pub(crate) fn wake(&self) {
        if let Some(waker) = self.0.read().unwrap().as_ref() {
            waker.wake_by_ref();
        }
    }
}
