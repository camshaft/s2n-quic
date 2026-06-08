// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

mod config;
mod pool;
mod slot;
mod tier;
mod waker;

// The deterministic suite uses the production sync types and `std`'s `Wake` trait; under loom we
// run only the `pool::loom_tests` models, so skip it there.
#[cfg(all(test, not(feature = "loom")))]
mod tests;

pub use config::Config;
pub use pool::{Distributor, Pool, Priority};
pub use slot::{AbandonResult, DeadSlot, DeadSlotQueue, GrantResult, Slot};
