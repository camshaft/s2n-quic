// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The application-facing scheduler.
//!
//! [`Scheduler::new`] builds the device table, spawns one credit distributor per device pool, a
//! submission **dispatch task** that routes admitted ops to the backend's execution lanes, and a
//! completion dispatcher, then hands back a `Send + Sync` [`SubmitHandle`].
//!
//! # Threading model (mirrors the network endpoint)
//!
//! The scheduler is a **single, process-wide, `Send + Sync`** object, cheaply cloned (it holds only
//! `Arc`s and a clone-able `Send` submission channel) — exactly like `stream::Client` over an
//! `Arc<Endpoint>`. Any thread can hold a [`SubmitHandle`] and submit concurrently; the `!Send`
//! per-worker machinery (lane senders, dispatch task, completion dispatcher, distributors) lives
//! *behind* the submission channel on the worker tasks the runtime spawns.
//!
//! The module is split by concern: [`handle`] (the `SubmitHandle` + submit primitive), [`alloc`] (the
//! reusable slot-bearing credit-acquire context), and [`dispatch`] (the submission routing task).

pub(crate) mod alloc;
mod dispatch;
mod handle;

pub use handle::SubmitHandle;

use crate::{
    counter::Registry,
    fs::{
        backend::{Backend, LaneSetup},
        combinator::{completion_channel, completion_dispatcher},
        config::Config,
        counters::Counters,
        device::{Device, DeviceId, DeviceTable},
    },
    runtime::Spawner,
    sched::{Distributor, Pool, ReceiverExt as _},
    socket::channel::intrusive::sync as sync_chan,
    sync::Arc,
    time::precision,
};
use core::sync::atomic::AtomicUsize;

/// A reference to one block of a logical object: which device/fd it lives on, its byte range, and
/// optional head/tail trim for edge blocks of a sub-range read. The unit of a
/// [`materialize`](SubmitHandle::materialize) spray.
#[derive(Clone, Copy, Debug)]
pub struct BlockRef {
    pub device: DeviceId,
    pub fd: i32,
    pub offset: u64,
    pub len: u32,
    /// Bytes to trim from the head of the read result (for a range starting mid-block).
    pub head_trim: u32,
    /// Bytes to trim from the tail of the read result (for a range ending mid-block).
    pub tail_trim: u32,
}

impl BlockRef {
    /// A whole-block read with no trimming.
    pub fn whole(device: DeviceId, fd: i32, offset: u64, len: u32) -> Self {
        Self {
            device,
            fd,
            offset,
            len,
            head_trim: 0,
            tail_trim: 0,
        }
    }
}

/// Shared scheduler state behind the handle — **all fields are `Send + Sync`** (an `Arc`-based
/// submission channel, an `Arc` device table, an atomic round-robin counter, a clock, and counters),
/// so the handle is freely shared across threads. The `!Send` lane senders and dispatch state live
/// inside the spawned dispatch task, never here.
pub(crate) struct SchedulerInner {
    pub(crate) devices: Arc<DeviceTable>,
    /// `Send` submission channel: every thread's `submit` pushes the built `IoOp` here; the dispatch
    /// task (on a worker) drains it and routes to the backend lanes.
    pub(crate) submission: sync_chan::Sender<crate::fs::op::IoOp>,
    /// Round-robins the lane assignment hint across submitters without a lock.
    pub(crate) round_robin: AtomicUsize,
    pub(crate) lane_count: usize,
    pub(crate) clock: crate::time::DefaultClock,
    pub(crate) counters: Arc<Counters>,
}

/// The IO scheduler. A single, process-wide, `Send + Sync` object: build it once, clone
/// [`SubmitHandle`]s freely across threads (cheap — just `Arc`s). Owns the spawned distributor,
/// dispatch, lane, and completion tasks.
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<SchedulerInner>,
}

impl Scheduler {
    /// Build the scheduler: device table, per-pool distributors, backend lanes, a submission dispatch
    /// task, and a completion dispatcher. `spawner` drives all internal tasks; `registry` receives the
    /// scheduler's counters and the per-pool credit gauges; `clock` is the precision clock used by the
    /// distributors and the backend.
    pub fn new<B, S, Clk>(
        config: &Config,
        backend: &B,
        spawner: &mut S,
        registry: &Registry,
        clock: Clk,
    ) -> Self
    where
        B: Backend,
        S: Spawner,
        Clk: precision::Clock + Clone,
    {
        let counters = Counters::register(registry);

        // 1. Build devices and count their pools — one credit distributor (and one waker-drain slot)
        //    per pool.
        let devices: Vec<Device> = config.devices.iter().map(Device::new).collect();
        let pool_count: usize = devices
            .iter()
            .map(|d| d.pools.all().count())
            .sum::<usize>()
            .max(1);

        // Offload distributor grant wakes onto a single drain task rather than firing them inline on
        // the distributor task (the endpoint's `waker::Sink`/`Drain` pattern): each distributor pushes
        // its per-poll waker batch into its own slot, and the drain task fires them in bulk. One drain
        // task is plenty here — the scheduler's wake volume is far below the endpoint's line-rate
        // dispatch — so `num_drains = 1`.
        let (mut waker_sinks, mut waker_drains) = crate::endpoint::waker::new(pool_count, 1);
        let drain = waker_drains
            .pop()
            .expect("waker::new yields one drain for num_drains=1");
        spawner.spawn_named(
            "fs.waker_drain",
            crate::endpoint::tasks::waker_drain(drain)
                .drain_budgeted(Some(crate::fs::combinator::DRAIN_BUDGET)),
        );

        // 2. Register each pool's gauges and spawn its distributor, handing it a waker sink.
        for (device_idx, device) in devices.iter().enumerate() {
            for (pool_idx, pool) in device.pools.all().enumerate() {
                pool.register_gauges(
                    registry,
                    &format!("fs.credit.dev{device_idx}.pool{pool_idx}"),
                );
                let sink = waker_sinks.pop().expect("one waker sink per pool");
                spawn_distributor(pool.clone(), spawner, sink, clock.clone());
            }
        }
        debug_assert!(
            waker_sinks.is_empty(),
            "every waker sink handed to a distributor"
        );
        let devices = Arc::new(DeviceTable::new(devices));

        // 2. Wire the completion path: backend lanes (and the dispatch task, for undeliverable ops)
        //    push into the sink; the dispatcher drains it, releases credit, and routes each op back
        //    to its submitter.
        let (sink, completion_rx) = completion_channel();
        spawner.spawn_named(
            "fs.completion_dispatcher",
            completion_dispatcher(completion_rx, devices.clone(), counters.clone()),
        );

        // 3. Build the backend's execution lanes.
        let lane_count = config.ring_count.max(1);
        let lanes = backend.spawn_lanes(
            LaneSetup {
                devices: devices.clone(),
                lane_count,
                completion: sink.clone(),
                registry: registry.clone(),
            },
            spawner,
        );

        // 4. Spawn the submission dispatch task: it drains the Send submission channel and routes each
        //    admitted op to a backend lane. This task owns the !Send lane senders.
        let (submission_tx, submission_rx) = sync_chan::new::<crate::fs::op::IoOp>();
        spawner.spawn_named(
            "fs.dispatch",
            dispatch::dispatch_loop(
                submission_rx,
                lanes,
                sink,
                counters.clone(),
                crate::time::DefaultClock::default(),
            ),
        );

        Self {
            inner: Arc::new(SchedulerInner {
                devices,
                submission: submission_tx,
                round_robin: AtomicUsize::new(0),
                lane_count,
                clock: crate::time::DefaultClock::default(),
                counters,
            }),
        }
    }

    /// A handle for submitting work. Cheap to clone and `Send + Sync`.
    pub fn handle(&self) -> SubmitHandle {
        SubmitHandle {
            inner: self.inner.clone(),
        }
    }

    /// The device table, for tests/introspection (conservation checks).
    #[cfg(test)]
    pub(crate) fn devices(&self) -> &Arc<DeviceTable> {
        &self.inner.devices
    }
}

/// Compile-time guarantee that the scheduler handle is a process-wide, thread-shareable object. If a
/// future change reintroduces an `Rc`/`RefCell`/`Cell` into the shared state, this fails to compile.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Scheduler>();
    assert_send_sync::<SubmitHandle>();
};

/// Spawn a credit distributor for `pool`, offloading its grant wakers to `waker_sink` (drained by the
/// scheduler's single waker-drain task) rather than firing them inline.
fn spawn_distributor<S, Clk>(
    pool: Arc<Pool>,
    spawner: &mut S,
    waker_sink: crate::endpoint::waker::Sink,
    clock: Clk,
) where
    S: Spawner,
    Clk: precision::Clock,
{
    let dist = Distributor::new(pool);
    let budget = crate::sched::Budget::new(1 << 20);
    spawner.spawn_named(
        "fs.credit_distributor",
        dist.distribute(budget, waker_sink, clock),
    );
}
