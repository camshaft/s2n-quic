// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Lazy device registration: building a device's execution (lanes + dispatch task + credit
//! distributor) onto the worker *after* construction, from an arbitrary application thread.
//!
//! # The problem
//!
//! [`DeviceRegistry::register_device`](super::DeviceRegistry::register_device) runs on whatever
//! thread the application calls it from, but a device's machinery is `!Send` and must run on the
//! worker: the execution **lanes** the backend builds (a bach lane is a `!Send` `unsync::Sender`),
//! the **dispatch task** that routes that device's ops to its lanes, and the **credit distributor**
//! cooperative future. We cannot hand any of these across threads, and we cannot durably hold a
//! [`Spawner`](crate::runtime::Spawner) to spawn them later — the busy-poll spawner is a transient
//! `!Send` borrow (`Spawner<'a>`), valid only during the setup closure.
//!
//! # The mechanism
//!
//! Build only the `Arc<Device>` on the application thread — what crosses the boundary is all `Send`:
//! the device's submission-channel **receiver**, its lane count, and its credit `Arc<Pool>`s, never a
//! future and never a `!Send` lane. [`DeviceRegistry::new`](super::DeviceRegistry::new) spawns one
//! long-lived **registrar task** that **owns the backend** and drives a *growable set* of per-device
//! futures. On each registration the registrar — running on the worker, where `!Send` is fine —
//! builds that device's lanes via the backend, then adds its dispatch future and its distributor
//! future(s) to the set it polls. This is the "pool of per-device tasks" with membership that grows
//! at runtime, and the generic [`Spawner`](crate::runtime::Spawner) only ever spawns the registrar
//! itself, up front.
//!
//! Grant-wakes fire **inline** via [`InlineWakerSink`] rather than through the endpoint's pre-sized
//! waker-drain offload: the drain's slot count is fixed at construction, incompatible with devices
//! appearing later, and the scheduler's wake volume is far below the endpoint's line-rate dispatch,
//! so a direct `waker.wake()` per grant is the right cost/complexity trade.

use crate::{
    fs::{
        backend::{Backend, LaneSetup},
        op::IoOp,
        scheduler::dispatch::dispatch_loop,
    },
    sched::{Distributor, Pool},
    socket::channel::{intrusive::sync as sync_chan, Budget},
    sync::Arc,
    time::precision,
};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use std::collections::VecDeque;

/// A [`WakerSink`](crate::credit::WakerSink) that invokes each granted waker inline. Correct for the
/// scheduler's low wake volume; sidesteps the fixed-slot waker-drain offload that cannot accommodate
/// devices registered after construction.
pub(super) struct InlineWakerSink;

impl crate::credit::WakerSink for InlineWakerSink {
    fn append_wakers(&mut self, batch: &mut VecDeque<Waker>) {
        for waker in batch.drain(..) {
            waker.wake();
        }
    }
}

/// One registered device's execution work, sent from `register_device` to the registrar task. Every
/// field is `Send` — the registrar builds the `!Send` lanes + dispatch + distributor futures on the
/// worker from these ingredients.
pub(super) struct Registration {
    /// The device's submission-channel receiver — the dispatch task drains it.
    pub(super) submission_rx: sync_chan::Receiver<IoOp>,
    /// How many execution lanes to build for this device.
    pub(super) lane_count: usize,
    /// The device's credit pool(s) — one distributor is spun up per pool.
    pub(super) pools: Vec<Arc<Pool>>,
}

/// The registrar task: own the backend and drive a growable set of per-device futures (one dispatch
/// task + one distributor per pool, per device), adding them each time a [`Registration`] arrives,
/// until the registration channel closes (registry teardown).
///
/// This is itself a single `!Send` future spawned once on the worker by `DeviceRegistry::new`. It
/// never resolves until the channel closes and every spawned future has ended; dropping the task
/// tears everything down (including the backend, which joins its lane threads).
pub(super) async fn registrar<B, Clk>(
    backend: B,
    rx: sync_chan::Receiver<Registration>,
    clock: Clk,
) where
    B: Backend,
    Clk: precision::Clock + Clone + 'static,
{
    RegistrarFuture {
        backend,
        rx,
        clock,
        tasks: Vec::new(),
        budget: Budget::new(1),
        rx_open: true,
    }
    .await
}

struct RegistrarFuture<B, Clk> {
    /// The execution backend, owned here so lanes are built on the worker (a bach lane is `!Send`).
    backend: B,
    rx: sync_chan::Receiver<Registration>,
    clock: Clk,
    /// One driven future per spawned task (a device's dispatch loop, or one of its distributors).
    /// Boxed + pinned so the heterogeneous set lives in one `Vec` we poll round-robin.
    tasks: Vec<Pin<Box<dyn Future<Output = ()>>>>,
    budget: Budget,
    rx_open: bool,
}

impl<B, Clk> Future for RegistrarFuture<B, Clk>
where
    B: Backend,
    Clk: precision::Clock + Clone + 'static,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // SAFETY: we never move any pinned task out of `self.tasks`; we only push new boxes and poll
        // in place. The backend, rx, clock, and budget are `Unpin` and freely accessed.
        let this = unsafe { self.get_unchecked_mut() };

        // 1. Drain any newly-registered devices and build their execution on this (worker) thread:
        //    the backend's lanes, a dispatch task routing the device's ops to those lanes, and one
        //    credit distributor per pool.
        if this.rx_open {
            this.budget.reset();
            loop {
                // Disambiguate: the `sync` receiver impls `Receiver` for both `Entry<T>` and
                // `Queue<T>`; we want the single-entry form.
                let polled =
                    crate::sched::Receiver::<crate::intrusive::Entry<Registration>>::poll_recv(
                        &mut this.rx,
                        cx,
                        &mut this.budget,
                    );
                match polled {
                    Poll::Ready(Some(entry)) => {
                        let Registration {
                            submission_rx,
                            lane_count,
                            pools,
                        } = entry.into_inner();

                        // Build the device's lanes via the backend (on the worker — `!Send` is fine
                        // here) and a dispatch task that pick-two-routes this device's ops across
                        // them.
                        let lanes = this.backend.spawn_lanes(LaneSetup { lane_count });
                        this.tasks.push(Box::pin(dispatch_loop(
                            submission_rx,
                            lanes,
                            crate::time::DefaultClock::default(),
                        )));

                        // One distributor per pool (a split device has two).
                        for pool in pools {
                            let budget = crate::sched::Budget::new(1 << 20);
                            let fut = Distributor::new(pool).distribute(
                                budget,
                                InlineWakerSink,
                                this.clock.clone(),
                            );
                            this.tasks.push(Box::pin(fut));
                        }
                        this.budget.reset();
                    }
                    // Registration channel closed (registry dropped): stop accepting new devices, but
                    // keep driving the tasks we already own until they end.
                    Poll::Ready(None) => {
                        this.rx_open = false;
                        break;
                    }
                    Poll::Pending => break,
                }
            }
        }

        // 2. Drive every spawned task. Under normal operation a dispatch loop ends when its device's
        //    submission channel closes, and a distributor never resolves; a resolved one is removed.
        //    When the registration channel is closed and no tasks remain, the registrar task ends.
        let mut i = 0;
        while i < this.tasks.len() {
            match this.tasks[i].as_mut().poll(cx) {
                Poll::Ready(()) => {
                    // Drop the finished task (explicit: `swap_remove` returns the boxed future, which
                    // is `#[must_use]`). Don't advance `i` — the swapped-in tail now occupies index
                    // `i` and must be polled this pass.
                    drop(this.tasks.swap_remove(i));
                }
                Poll::Pending => i += 1,
            }
        }

        if !this.rx_open && this.tasks.is_empty() {
            return Poll::Ready(());
        }
        Poll::Pending
    }
}
