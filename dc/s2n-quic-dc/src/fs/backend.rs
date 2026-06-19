// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The backend seam — channel-wired, not a lowest-common-denominator per-op trait.
//!
//! A backend is a *constructor* of execution lanes. Given a way to spawn tasks, a completion sink,
//! the device table, and a clock, it returns one [`LaneSubmit`] per lane and spawns whatever
//! internal tasks it wants (a mock timer loop, a bounded blocking thread pool, an io_uring CQ
//! reaper). The dispatcher routes each admitted [`IoOp`] to a lane's `LaneSubmit`; the backend
//! drains it, executes the op however it likes (reading the rich `IoOp` fields directly, taking the
//! buffer by value), stamps `status`, and pushes the completed op into the completion sink.
//!
//! This mirrors how `endpoint::tasks::send_worker` is handed `batch_rx` + a completion sender and
//! builds its own internal pipeline — the *channel* is the abstraction, so io_uring's out-of-order
//! completion and the blocking pool's thread hand-off both fit without a common syscall signature.

pub mod mock;
pub mod syscall;
/// io_uring backend — Linux only.
#[cfg(target_os = "linux")]
pub mod uring;

use crate::{
    fs::{device::DeviceTable, op::IoOp, SpawnHandle},
    sched::UnboundedSender,
    sync::Arc,
};

/// A handle the dispatcher uses to submit ops to one execution lane. Boxed unbounded-sender of
/// `Entry<IoOp>`; the lane's own task drains it (the credit pool has already bounded total admitted
/// work upstream, so the lane needs no separate cap in v1).
pub type LaneSubmit = Box<dyn UnboundedSender<crate::intrusive::Entry<IoOp>>>;

/// A cloneable sink the backend pushes completed ops into. Completions feed the completion
/// dispatcher, which releases credit and routes each op back to its submitter.
pub trait CompletionSink {
    /// Push one completed op toward the completion dispatcher.
    fn send(&self, op: crate::intrusive::Entry<IoOp>);
    /// Clone into a fresh boxed sink (one per lane).
    fn boxed_clone(&self) -> Box<dyn CompletionSink>;
}

/// Context handed to a backend when it builds its lanes.
pub struct LaneSetup {
    /// The device table (fds, cost models) — backends that touch real files resolve fds here.
    pub devices: Arc<DeviceTable>,
    /// How many lanes to create.
    pub lane_count: usize,
    /// Sink for completed ops (the backend takes `boxed_clone`s, one per lane).
    pub completion: Box<dyn CompletionSink>,
    /// Cloneable spawner for the lanes' internal tasks.
    pub spawn: SpawnHandle,
}

/// A storage IO execution backend.
pub trait Backend {
    /// Build `setup.lane_count` lanes, spawning each lane's execution task, and return the submit
    /// handles in lane order. `LocalRingId(i)` routes to `handles[i]`.
    fn spawn_lanes(&self, setup: LaneSetup) -> Vec<LaneSubmit>;
}
