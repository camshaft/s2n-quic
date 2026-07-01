// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Synchronization primitives, swappable for loom-instrumented versions under test.
//!
//! Everything in the crate that needs an atomic, an `Arc`, a `Mutex`, or an `UnsafeCell` for
//! concurrency-sensitive code should import it from here rather than `std`/`core`/`parking_lot`
//! directly. Under `cfg(all(feature = "loom", test))` these resolve to loom's instrumented types so
//! the loom model checker can explore interleavings; otherwise they resolve to the production types
//! (`parking_lot::Mutex` for uncontended speed, std atomics/`Arc`).
//!
//! Because this is a plain module re-export (not a global `--cfg loom`), the swap is crate-scoped
//! and never leaks into dependencies.
//!
//! `Mutex` deliberately exposes the [`lock`] free function rather than a `.lock()` method so callers
//! are identical across the parking_lot (infallible) and loom (`Result`) APIs.

pub mod free_list;
pub(crate) mod waiter;
pub mod wake;

pub use wake::AutoWake;

#[cfg(all(feature = "loom", test))]
#[allow(unused_imports)]
mod imp {
    pub use loom::sync::{
        atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        Mutex, MutexGuard, RwLock,
    };

    // `Arc` is deliberately NOT swapped for `loom::sync::Arc`. Loom's `Arc` shim is a minimal subset
    // of `std::sync::Arc` (no `Weak`/`downgrade`, no `From<&str>`, no `Arc<[T]>::from(Vec)`, no
    // arbitrary-self-type methods), so swapping it crate-wide breaks the large body of non-model code
    // that relies on those APIs (the `fs` scheduler, credit-pool gauges, etc.). None of the loom
    // models use `Arc` as a synchronization edge — every shared state they exercise lives behind the
    // loom-instrumented atomics/mutex above, and model threads are explicitly joined — so std `Arc`
    // is sound here and loses no coverage.
    pub use std::sync::Arc;

    #[inline(always)]
    pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap()
    }
}

#[cfg(not(all(feature = "loom", test)))]
#[allow(unused_imports)]
mod imp {
    pub use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    pub use parking_lot::{Mutex, MutexGuard};
    pub use std::sync::{Arc, RwLock};

    #[inline(always)]
    pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock()
    }
}

#[allow(unused_imports)]
pub(crate) use imp::*;
