// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The device registry — the application-facing factory that mints self-scheduling devices.
//!
//! [`DeviceRegistry::new`] spawns one long-lived **registrar task** that owns the execution backend
//! and drives the per-device machinery. The application then registers each device with
//! [`DeviceRegistry::register_device`] (lazily, as devices are opened — the setup site often does not
//! know them yet) and holds the returned `Arc<Device>`; there is no device table. The result is a
//! `Send + Sync` [`DeviceRegistry`] that clones freely across threads.
//!
//! # Each device schedules itself
//!
//! There is **no global scheduler** arbitrating across devices. A [`Device`] owns its own submission
//! channel, dispatch task (routing its ops to its own execution lanes), and credit distributor — so
//! submitting is a method *on the `Arc<Device>`* (`device.read(..)`), the op it builds carries that
//! same `Arc<Device>`, and it finishes itself in place. The registry exists only to (1) own the
//! backend + registrar and (2) build a device's `!Send` machinery on the worker after the
//! application created the `Arc<Device>` on some other thread.
//!
//! # Threading model (mirrors the network endpoint)
//!
//! The registry is a **`Send + Sync`** object, cheaply cloned (it holds only `Arc`s and a clone-able
//! `Send` registration channel). Any thread can register devices and any thread can submit against an
//! `Arc<Device>` concurrently; the `!Send` per-worker machinery (lane senders, dispatch tasks,
//! distributors) lives *behind* the registration + per-device submission channels on the worker tasks
//! the runtime spawns.
//!
//! Completions need no central task: an [`IoOp`](crate::fs::op::IoOp) carries its own `Arc<Device>`
//! and `Send + Sync` completion sender, so the backend worker that finishes an op completes it in
//! place ([`combinator::complete`](crate::fs::combinator::complete)).
//!
//! The module is split by concern: [`alloc`] (the reusable slot-bearing credit-acquire context),
//! [`dispatch`] (a device's submission-routing task), and [`registrar`] (lazy device registration /
//! per-device task spawning). The submit API itself lives on [`Device`].

pub(crate) mod alloc;
mod dispatch;
mod registrar;

use crate::{
    counter::Registry,
    fs::{
        backend::Backend,
        config::DeviceConfig,
        device::Device,
        op::IoOp,
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
/// [`materialize`](crate::fs::materialize::materialize) spray.
#[derive(Clone, Debug)]
pub struct BlockRef {
    /// The device this block lives on, carried by `Arc` so a `materialize` stream resolves it once
    /// and the op carries it straight through. Obtain the handle from
    /// [`DeviceRegistry::register_device`].
    pub device: Arc<Device>,
    /// The file this block lives in, carried as an [`Fd`](crate::fs::op::Fd) so the read keeps it
    /// open until the op completes (no UAF if the caller drops its `File` mid-spray).
    pub fd: crate::fs::op::Fd,
    pub offset: u64,
    pub len: u32,
    /// Bytes to trim from the head of the read result (for a range starting mid-block).
    pub head_trim: u32,
    /// Bytes to trim from the tail of the read result (for a range ending mid-block).
    pub tail_trim: u32,
}

impl BlockRef {
    /// A whole-block read with no trimming.
    pub fn whole(device: Arc<Device>, fd: crate::fs::op::Fd, offset: u64, len: u32) -> Self {
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

/// Shared registry state behind the handle — **all fields are `Send + Sync`** (an `Arc`-based `Send`
/// registration channel, a clock, a registry, and an atomic counter), so the handle is freely shared
/// across threads. The `!Send` backend, lane senders, dispatch state, and distributors live inside
/// the spawned registrar task, never here. There is **no device table**: a device is its
/// `Arc<Device>`, held by the application and carried by every op.
pub(crate) struct RegistryInner {
    /// `Send` registration channel: `register_device` sends each new device's execution work here;
    /// the registrar task (on a worker) builds its lanes, dispatch task, and distributors.
    registration: sync_chan::Sender<registrar::Registration>,
    pub(crate) clock: crate::time::DefaultClock,
    /// Registry kept so [`DeviceRegistry::register_device`] can register a device's credit gauges and
    /// its per-device nominal counters.
    registry: Registry,
    /// Monotonic source of each device's crate-internal registration index (dense from 0). Stamped
    /// onto the `Device` so internal per-device indexers (materialize's acquire slots) can index an
    /// array directly. Not application-visible.
    next_device_id: AtomicUsize,
}

/// The device registry. A `Send + Sync` factory: build it once, register devices from any thread, and
/// submit against the returned `Arc<Device>`s concurrently. Owns the spawned registrar task (which in
/// turn owns the backend and every device's dispatch + distributor + lane tasks).
#[derive(Clone)]
pub struct DeviceRegistry {
    inner: Arc<RegistryInner>,
}

impl DeviceRegistry {
    /// Build the registry: spawn a single registrar task that **owns** `backend` and drives every
    /// device's execution. `spawner` drives the registrar; `registry` receives each device's per-pool
    /// credit gauges and per-device nominal counters as devices register; `clock` is the precision
    /// clock the **credit distributors** read for their refill/liveness timing. (The per-device
    /// dispatch EDT load balancer uses the process `DefaultClock` independently — it needs only
    /// relative timestamps to score lane occupancy, not this clock's epoch.) After construction the
    /// application registers each device with [`register_device`] and holds the returned `Arc<Device>`.
    ///
    /// [`register_device`]: Self::register_device
    pub fn new<B, S, Clk>(backend: B, spawner: &mut S, registry: &Registry, clock: Clk) -> Self
    where
        B: Backend + 'static,
        S: Spawner,
        Clk: precision::Clock + Clone + Send + 'static,
    {
        // Spawn the registrar task: it owns the backend and the growable set of per-device tasks
        // (dispatch + distributors), adding them each time `register_device` sends a device's work.
        // Spawned once, up front; lazily-registered devices reach it over the (Send) registration
        // channel.
        let (registration_tx, registration_rx) = sync_chan::new::<registrar::Registration>();
        spawner.spawn_named(
            "fs.registrar",
            registrar::registrar(backend, registration_rx, clock),
        );

        Self {
            inner: Arc::new(RegistryInner {
                registration: registration_tx,
                clock: crate::time::DefaultClock::default(),
                registry: registry.clone(),
                next_device_id: AtomicUsize::new(0),
            }),
        }
    }

    /// Build a device under `label` from `cfg` — registering its credit gauges, creating its own
    /// submission channel, and handing the registrar the work to build its lanes + dispatch task +
    /// distributor on the worker — and return the shared [`Arc<Device>`]. That `Arc<Device>` is the
    /// **only** device handle: call reads/writes on it directly (`device.read(..)`) and pass it to
    /// [`BlockRef`]s. The application owns it; the registry keeps no table. Safe to call from any
    /// thread at any time after construction (the setup site often does not know its devices yet, so
    /// they register lazily as opened).
    ///
    /// `label` is diagnostics only — it prefixes the device's credit gauges (`fs.credit.{label}.*`)
    /// and appears in logs. Everything that crosses to the registrar is `Send` (the submission
    /// receiver, the lane count, the `Arc<Pool>`s) — never a `!Send` future or lane. Returns
    /// `BrokenPipe` only if the registry has been torn down (registration channel closed).
    pub fn register_device(
        &self,
        label: impl Into<Arc<str>>,
        cfg: &DeviceConfig,
    ) -> std::io::Result<Arc<Device>> {
        let inner = &self.inner;
        let label = label.into();
        let index = inner.next_device_id.fetch_add(1, Ordering::Relaxed);

        // The device owns the sender; the registrar gets the receiver to drive the dispatch task.
        let (submission_tx, submission_rx) = sync_chan::new::<IoOp>();
        let device = Arc::new(Device::new(
            label.clone(),
            index,
            &inner.registry,
            cfg,
            submission_tx,
            inner.clock.clone(),
        ));

        // Register each pool's gauges and collect them for the registrar (one distributor per pool).
        let mut pools = Vec::new();
        for (pool_idx, pool) in device.pools.all().enumerate() {
            pool.register_gauges(&inner.registry, &format!("fs.credit.{label}.pool{pool_idx}"));
            pools.push(pool.clone());
        }

        let reg = registrar::Registration {
            submission_rx,
            lane_count: cfg.lane_count.max(1),
            pools,
        };
        if inner.registration.send_entry(Entry::new(reg)).is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "io scheduler: registration channel closed (registry torn down)",
            ));
        }

        Ok(device)
    }
}

/// Compile-time guarantee that the registry handle is a process-wide, thread-shareable object. If a
/// future change reintroduces an `Rc`/`RefCell`/`Cell` into the shared state, this fails to compile.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DeviceRegistry>();
};
