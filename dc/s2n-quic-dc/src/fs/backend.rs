// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The backend seam — channel-wired, not a lowest-common-denominator per-op trait.
//!
//! A backend is a *constructor* of execution lanes. Given a way to spawn tasks, a completion sink,
//! the device table, and a clock, it returns one lane submit handle per lane and spawns whatever
//! internal tasks it wants (bach per-op timer tasks, a bounded blocking thread pool, an io_uring CQ
//! reaper). The submission dispatch task routes each admitted [`IoOp`] to a lane; the backend drains
//! it, executes the op however it likes (reading the rich `IoOp` fields directly, taking the buffer
//! by value), stamps `status`, and pushes the completed op into the completion sink.
//!
//! This mirrors how `endpoint::tasks::send_worker` is handed `batch_rx` + a completion sender and
//! builds its own internal pipeline — the *channel* is the abstraction, so io_uring's out-of-order
//! completion and the blocking pool's thread hand-off both fit without a common syscall signature.
//!
//! The seam is fully **monomorphized**: a backend declares its concrete lane sender via
//! [`Backend::Lane`] and receives the concrete [`ChannelCompletionSink`], so there is no `Box<dyn>`
//! or vtable indirection per lane or per completion.

/// Deterministic in-memory backend for bach simulation tests.
#[cfg(any(test, feature = "testing"))]
pub mod bach;
/// Drives a `Receiver` on a dedicated blocking OS thread via a thread-park waker (the syscall pool).
mod blocking;
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
    fs::{combinator::CompletionSink, device::DeviceTable, op::IoOp},
    intrusive::Entry,
    runtime::Spawner,
    sched::UnboundedSender,
    sync::Arc,
};

/// Context handed to a backend when it builds its lanes. The spawner is passed separately to
/// [`Backend::spawn_lanes`] (the `Spawner` trait is not object-safe, so it cannot live in a struct).
pub struct LaneSetup {
    /// The device table (fds, cost models) — backends that touch real files resolve fds here.
    pub devices: Arc<DeviceTable>,
    /// How many lanes to create.
    pub lane_count: usize,
    /// The channel completed ops are pushed into; the backend clones it per lane (cheap `Rc` clone).
    pub completion: CompletionSink,
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
