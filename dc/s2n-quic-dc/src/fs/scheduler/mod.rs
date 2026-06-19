// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The application-facing scheduler.
//!
//! [`Scheduler::new`] builds the backend's execution lanes, spawns the submission **dispatch task**
//! that routes admitted ops to those lanes, and spawns a long-lived **registrar task** that drives
//! the per-device credit distributors. The application then registers each device with
//! [`Scheduler::register_device`] (lazily, as devices are opened — the setup site often does not
//! know them yet) and holds the returned `Arc<Device>`; there is no device table. The result is a
//! `Send + Sync` [`Scheduler`] whose [`SubmitHandle`]s clone freely across threads.
//!
//! # Threading model (mirrors the network endpoint)
//!
//! The scheduler is a **single, process-wide, `Send + Sync`** object, cheaply cloned (it holds only
//! `Arc`s and clone-able `Send` channels) — exactly like `stream::Client` over an `Arc<Endpoint>`.
//! Any thread can hold a [`SubmitHandle`] and submit concurrently; the `!Send` per-worker machinery
//! (lane senders, dispatch task, registrar + distributors) lives *behind* the submission and
//! registration channels on the worker tasks the runtime spawns.
//!
//! Completions need no central task: an [`IoOp`](crate::fs::op::IoOp) carries its own `Arc<Device>`
//! and `Send + Sync` completion sender, so the backend worker that finishes an op completes it in
//! place ([`combinator::complete`](crate::fs::combinator::complete)).
//!
//! The module is split by concern: [`handle`] (the `SubmitHandle` + submit primitive), [`alloc`] (the
//! reusable slot-bearing credit-acquire context), [`dispatch`] (the submission routing task), and
//! [`registrar`] (lazy device registration / distributor spawning).

pub(crate) mod alloc;
mod dispatch;
mod handle;
mod registrar;

pub use handle::SubmitHandle;

use crate::{
    counter::Registry,
    fs::{
        backend::{Backend, LaneSetup},
        config::{Config, DeviceConfig},
        counters::Counters,
        device::Device,
    },
    intrusive::Entry,
    runtime::Spawner,
    socket::channel::intrusive::sync as sync_chan,
    sync::Arc,
    time::precision,
};
use core::sync::atomic::{AtomicUsize, Ordering};

/// A reference to one block of a logical object: which device/fd it lives on, its byte range, and
/// optional head/tail trim for edge blocks of a sub-range read. The unit of a
/// [`materialize`](SubmitHandle::materialize) spray.
#[derive(Clone, Debug)]
pub struct BlockRef {
    /// The device this block lives on, carried by `Arc` so a `materialize` stream resolves it once
    /// and the op carries it straight through. Obtain the handle from
    /// [`Scheduler::register_device`].
    pub device: Arc<Device>,
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
    pub fn whole(device: Arc<Device>, fd: i32, offset: u64, len: u32) -> Self {
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

/// Shared scheduler state behind the handle — **all fields are `Send + Sync`** (`Arc`-based Send
/// submission + registration channels, a clock, and counters), so the handle is freely shared across
/// threads. The `!Send` lane senders, dispatch state, and distributors live inside the spawned
/// worker tasks, never here. There is **no device table**: a device is its `Arc<Device>`, held by
/// the application and carried by every op.
pub(crate) struct SchedulerInner {
    /// `Send` submission channel: every thread's `submit` pushes the built `IoOp` here; the dispatch
    /// task (on a worker) drains it and routes to the backend lanes.
    pub(crate) submission: sync_chan::Sender<crate::fs::op::IoOp>,
    /// `Send` registration channel: `register_device` sends each new pool's distributor work here;
    /// the registrar task (on a worker) spins up the distributor.
    registration: sync_chan::Sender<registrar::Registration>,
    pub(crate) lane_count: usize,
    pub(crate) clock: crate::time::DefaultClock,
    pub(crate) counters: Arc<Counters>,
    /// Registry kept so [`Scheduler::register_device`] can register a device's credit gauges.
    registry: Registry,
    /// Monotonic source of each device's crate-internal registration index (dense from 0). Stamped
    /// onto the `Device` so internal per-device indexers (materialize's acquire slots) can index an
    /// array directly. Not application-visible.
    next_device_id: AtomicUsize,
    /// This scheduler's unique origin stamp, copied onto every device it registers so the submit
    /// chokepoint can `debug_assert` a handle is only used with its own scheduler's devices. Zero-cost
    /// in release builds.
    pub(crate) origin: crate::fs::device::SchedulerId,
}

/// The IO scheduler. A single, process-wide, `Send + Sync` object: build it once, clone
/// [`SubmitHandle`]s freely across threads (cheap — just `Arc`s). Owns the spawned registrar,
/// dispatch, lane, and distributor tasks.
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<SchedulerInner>,
}

impl Scheduler {
    /// Build the scheduler: backend lanes, a submission dispatch task, and a registrar task. Each
    /// `spawner` drives all internal tasks; `registry` receives the scheduler's counters and (as
    /// devices register) their per-pool credit gauges; `clock` is the precision clock used by the
    /// distributors and the dispatch EDT. After construction the application registers each device
    /// with [`register_device`] and holds the returned `Arc<Device>`.
    ///
    /// [`register_device`]: Self::register_device
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
        Clk: precision::Clock + Clone + Send + 'static,
    {
        let counters = Counters::register(registry);

        // 1. Spawn the registrar task: it owns and drives the growable set of per-device credit
        //    distributors, adding one each time `register_device` sends a pool. Spawned once, up
        //    front; lazily-registered devices reach it over the (Send) registration channel.
        let (registration_tx, registration_rx) = sync_chan::new::<registrar::Registration>();
        spawner.spawn_named(
            "fs.registrar",
            registrar::registrar(registration_rx, clock.clone()),
        );

        // 2. Build the backend's execution lanes. A lane completes each finished op in place (it
        //    holds the counters), so there is no completion sink to wire.
        let lane_count = config.ring_count.max(1);
        let lanes = backend.spawn_lanes(
            LaneSetup {
                lane_count,
                counters: counters.clone(),
                registry: registry.clone(),
            },
            spawner,
        );

        // 3. Spawn the submission dispatch task: it drains the Send submission channel and routes
        //    each admitted op to a backend lane. This task owns the !Send lane senders.
        let (submission_tx, submission_rx) = sync_chan::new::<crate::fs::op::IoOp>();
        spawner.spawn_named(
            "fs.dispatch",
            dispatch::dispatch_loop(
                submission_rx,
                lanes,
                counters.clone(),
                crate::time::DefaultClock::default(),
            ),
        );

        Self {
            inner: Arc::new(SchedulerInner {
                submission: submission_tx,
                registration: registration_tx,
                lane_count,
                clock: crate::time::DefaultClock::default(),
                counters,
                registry: registry.clone(),
                next_device_id: AtomicUsize::new(0),
                origin: crate::fs::device::SchedulerId::next(),
            }),
        }
    }

    /// Build a device under `label` from `cfg`, register its credit gauges, spawn its distributor
    /// onto the scheduler worker, and return the shared [`Arc<Device>`] — the **only** device handle:
    /// pass it (by `&`) to [`SubmitHandle`] reads/writes and to [`BlockRef`]s. The application owns
    /// it; the scheduler keeps no table. Safe to call from any thread at any time after construction
    /// (the setup site often does not know its devices yet, so they register lazily as opened).
    ///
    /// `label` is diagnostics only — it prefixes the device's credit gauges (`fs.credit.{label}.*`)
    /// and appears in logs. The distributor is spawned via the registrar task (the device's
    /// `Arc<Pool>`s — which are `Send` — cross the channel, not a `!Send` future). Returns
    /// `BrokenPipe` only if the scheduler has been torn down (registration channel closed).
    pub fn register_device(
        &self,
        label: impl Into<Arc<str>>,
        cfg: &DeviceConfig,
    ) -> std::io::Result<Arc<Device>> {
        let inner = &self.inner;
        let label = label.into();
        let index = inner.next_device_id.fetch_add(1, Ordering::Relaxed);
        let device = Arc::new(Device::new(label.clone(), index, inner.origin, cfg));

        // Register each pool's gauges and hand its distributor work to the registrar.
        for (pool_idx, pool) in device.pools.all().enumerate() {
            pool.register_gauges(&inner.registry, &format!("fs.credit.{label}.pool{pool_idx}"));
            let reg = registrar::Registration { pool: pool.clone() };
            if inner.registration.send_entry(Entry::new(reg)).is_err() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "io scheduler: registration channel closed (scheduler torn down)",
                ));
            }
        }

        Ok(device)
    }

    /// A handle for submitting work. Cheap to clone and `Send + Sync`.
    pub fn handle(&self) -> SubmitHandle {
        SubmitHandle {
            inner: self.inner.clone(),
        }
    }
}

/// Compile-time guarantee that the scheduler handle is a process-wide, thread-shareable object. If a
/// future change reintroduces an `Rc`/`RefCell`/`Cell` into the shared state, this fails to compile.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Scheduler>();
    assert_send_sync::<SubmitHandle>();
};
