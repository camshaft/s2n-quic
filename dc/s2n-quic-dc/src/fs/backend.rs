// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The backend seam — channel-wired, not a lowest-common-denominator per-op trait.
//!
//! A backend is a *constructor* of execution lanes. Given a way to spawn tasks, the scheduler
//! counters, and the lane count, it returns one lane submit handle per lane and spawns whatever
//! internal tasks it wants (bach per-op timer tasks, a bounded blocking thread pool, an io_uring
//! ring loop). The submission dispatch task routes each admitted [`IoOp`] to a lane; the backend
//! drains it, executes the op however it likes (reading the rich `IoOp` fields directly, taking the
//! buffer by value), stamps `status`, and **completes the op in place** via
//! [`combinator::complete`](crate::fs::combinator::complete) — releasing its credit to its own
//! `Arc<Device>` pool and notifying its submitter, on the worker's own thread. There is no
//! completion sink, no reaper, no cross-thread completion bridge.
//!
//! This mirrors how `endpoint::tasks::send_worker` is handed `batch_rx` and finishes each frame
//! itself — the *channel* is the abstraction, so io_uring's out-of-order completion and the blocking
//! pool's thread hand-off both fit without a common syscall signature.
//!
//! The seam is fully **monomorphized**: a backend declares its concrete lane sender via
//! [`Backend::Lane`], so there is no `Box<dyn>` or vtable indirection per lane.

/// Deterministic in-memory backend for bach simulation tests.
#[cfg(any(test, feature = "testing"))]
pub mod bach;
/// A runtime for blocking futures (one dedicated OS thread per spawn) + the `block_on` driver. The
/// syscall and io_uring backends spawn their lane threads on it; an application constructs one and
/// hands it to the backend (see [`syscall::SyscallBackend::new`] / [`uring::UringBackend::new`]).
pub mod blocking;
/// In-flight slab keyed by `user_data` (used by the io_uring backend; platform-agnostic + unit-tested
/// on all platforms even though only `uring` consumes it — hence `dead_code` off-Linux).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod inflight;
pub mod syscall;
/// io_uring backend — Linux only.
#[cfg(target_os = "linux")]
pub mod uring;

use crate::{
    counter::Registry,
    fs::{counters::Counters, op::IoOp},
    intrusive::Entry,
    runtime::Spawner,
    sched::UnboundedSender,
    sync::Arc,
};

/// Context handed to a backend when it builds its lanes. The spawner is passed separately to
/// [`Backend::spawn_lanes`] (the `Spawner` trait is not object-safe, so it cannot live in a struct).
pub struct LaneSetup {
    /// How many lanes to create.
    pub lane_count: usize,
    /// The scheduler counters. A lane completes each finished op in place via
    /// [`combinator::complete`](crate::fs::combinator::complete), which records its disposition
    /// here, so every lane holds an `Arc<Counters>`.
    pub counters: Arc<Counters>,
    /// Registry for the backend's own counters/task topology.
    pub registry: Registry,
}

/// A storage IO execution backend.
pub trait Backend {
    /// The concrete sender type the submission dispatcher uses to hand an op to one lane. Knowing the
    /// concrete type (rather than `Box<dyn>`) lets the dispatch loop monomorphize — no vtable, no
    /// per-lane heap box.
    type Lane: UnboundedSender<Entry<IoOp>> + 'static;

    /// Build `setup.lane_count` lanes, spawning each lane's execution task on `spawner`, and return
    /// the submit handles in lane order. `LocalRingId(i)` routes to `handles[i]`.
    fn spawn_lanes<S: Spawner>(&self, setup: LaneSetup, spawner: &mut S) -> Vec<Self::Lane>;
}
