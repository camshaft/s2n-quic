// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Storage IO scheduler built on the generic scheduling core ([`crate::sched`]).
//!
//! A QUIC send endpoint and a storage IO scheduler are the same problem: N concurrent streams share
//! one limited resource, and work must be scheduled fairly and efficiently with priorities and
//! allocation that scales with the number of active streams. This module reuses the endpoint's
//! generic machinery — the priority-tiered demand-elastic [`credit::Pool`](crate::credit::Pool),
//! the channel combinator pipeline, the token-bucket pacer, and the completion-notification channel
//! — and adds storage-specific stages on top, mirroring how the QUIC-specific stages were built on
//! the generic channel.
//!
//! # Why this exists
//!
//! Storage IO done via `tokio::spawn_blocking` (the `core-fs-direct` `IoPool` pattern) deadlocks:
//! each pool spawns blocking threads with no global bound and no backpressure on thread spawn, so
//! under load the pool exhausts and in-flight ops wait on ops that can never be scheduled. The
//! scheduler fixes this structurally: submission is gated by [credit](crate::credit) *before* any
//! thread/ring slot is consumed, a blocked submitter parks cooperatively (a waker, not a thread),
//! and execution is a fixed bounded resource that never dynamically spawns. It also adds the
//! fairness and priority the semaphore model lacks — latency-sensitive reads protected from
//! background writes.
//!
//! # Layers
//!
//! * **Stream** — a caller-defined opaque handle and fairness participant. Submits [`op::IoOp`]s.
//! * **Device** ([`device::Device`]) — the limited resource **and its own scheduler**: it owns the
//!   credit pool(s), cost model, submission channel, dispatch task, and execution lanes. A read/write
//!   is a method on the `Arc<Device>`; there is no global scheduler arbitrating across devices.
//! * **Device registry** ([`scheduler::DeviceRegistry`]) — the factory that owns the backend and
//!   mints self-scheduling devices via [`register_device`](scheduler::DeviceRegistry::register_device).
//! * **Execution lane** ([`device::LocalRingId`]) — one of a device's worker rings / blocking-pool
//!   slots. Lane count is a per-device knob, independent of the device's queue depth.

pub mod backend;
pub mod combinator;
pub mod config;
pub(crate) mod dbg;
pub mod device;
pub mod direct;
pub mod materialize;
pub mod op;
pub mod scheduler;
pub(crate) mod trace;

#[cfg(test)]
mod tests;

pub mod counters;

pub use config::{CostModel, DeviceConfig, OpWeights, PoolMode, DEFAULT_LANE_COUNT};
pub use device::{Device, LocalRingId};
pub use op::{IoBuf, IoKind, IoOp, IoStatus};
pub use materialize::{materialize, materialize_direct, Block, MaterializeStream};
pub use scheduler::{BlockRef, DeviceRegistry};
