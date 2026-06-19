// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Lazy device registration: spawning a credit distributor onto the scheduler worker *after*
//! construction, from an arbitrary application thread.
//!
//! # The problem
//!
//! [`Scheduler::register_device`](super::Scheduler::register_device) runs on whatever thread the
//! application calls it from, but a device's credit distributor is a `!Send` cooperative future
//! that must run on the scheduler's worker (interleaved with the other distributors, the dispatch
//! task, and the waker-drain). We cannot hand the distributor future across threads, and we cannot
//! durably hold a [`Spawner`](crate::runtime::Spawner) to spawn it later — the busy-poll spawner is
//! a transient `!Send` borrow (`Spawner<'a>`), valid only during the setup closure.
//!
//! # The mechanism
//!
//! Build only the device on the application thread — the device (an `Arc<Device>`, which is `Send`)
//! is all that crosses the boundary, never a future. [`Scheduler::new`](super::Scheduler::new)
//! spawns one long-lived **registrar task** that owns and drives a *growable set* of distributor
//! futures. Registration sends the new `Arc<Device>` over a `Send` channel; the registrar receives
//! it, constructs the distributor future **on the worker** (where `!Send` is fine), and adds it to
//! the set it polls. This is the "pool of distributor tasks, one per device" with membership that
//! grows at runtime — no per-distributor `spawn` call needed, and the generic `Spawner` only ever
//! spawns the registrar itself, up front.
//!
//! Grant-wakes fire **inline** via [`InlineWakerSink`] rather than through the endpoint's
//! pre-sized waker-drain offload: the drain's slot count is fixed at construction, incompatible
//! with devices appearing later, and the scheduler's wake volume is far below the endpoint's
//! line-rate dispatch, so a direct `waker.wake()` per grant is the right cost/complexity trade.

use crate::{
    credit::{Distributor, Pool},
    socket::channel::{intrusive::sync as sync_chan, Budget},
    sync::Arc,
    time::precision,
};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    task::Waker,
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

/// One registered device's distributor work, sent from `register_device` to the registrar task. We
/// ship the `Arc<Pool>` set (Send) and let the registrar build the `!Send` distributor future on the
/// worker. One message per pool (a split device sends two).
pub(super) struct Registration {
    pub(super) pool: Arc<Pool>,
}

/// The registrar task: drive a growable set of distributor futures, adding one each time a
/// [`Registration`] arrives, until the registration channel closes (scheduler teardown).
///
/// This is itself a single `!Send` future spawned once on the worker by `Scheduler::new`. It never
/// resolves until the channel closes and every distributor has ended; dropping the task tears
/// everything down.
pub(super) async fn registrar<Clk>(rx: sync_chan::Receiver<Registration>, clock: Clk)
where
    Clk: precision::Clock + Clone + 'static,
{
    RegistrarFuture {
        rx,
        clock,
        distributors: Vec::new(),
        budget: Budget::new(1),
        rx_open: true,
    }
    .await
}

struct RegistrarFuture<Clk> {
    rx: sync_chan::Receiver<Registration>,
    clock: Clk,
    /// One driven distributor future per registered pool. Boxed + pinned so the heterogeneous set
    /// lives in one `Vec` we poll round-robin.
    distributors: Vec<Pin<Box<dyn Future<Output = ()>>>>,
    budget: Budget,
    rx_open: bool,
}

impl<Clk> Future for RegistrarFuture<Clk>
where
    Clk: precision::Clock + Clone + 'static,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // SAFETY: we never move any pinned distributor out of `self.distributors`; we only push new
        // boxes and poll in place. `self` is `Unpin`-free only through the boxed futures, which we
        // keep pinned.
        let this = unsafe { self.get_unchecked_mut() };

        // 1. Drain any newly-registered pools and spin up their distributors on this (worker) thread.
        if this.rx_open {
            this.budget.reset();
            loop {
                // Disambiguate: the `sync` receiver impls `Receiver` for both `Entry<T>` and
                // `Queue<T>`; we want the single-entry form.
                let polled = crate::sched::Receiver::<crate::intrusive::Entry<Registration>>::poll_recv(
                    &mut this.rx,
                    cx,
                    &mut this.budget,
                );
                match polled {
                    Poll::Ready(Some(entry)) => {
                        let Registration { pool } = entry.into_inner();
                        let budget = crate::sched::Budget::new(1 << 20);
                        let fut = Distributor::new(pool).distribute(
                            budget,
                            InlineWakerSink,
                            this.clock.clone(),
                        );
                        this.distributors.push(Box::pin(fut));
                        this.budget.reset();
                    }
                    // Registration channel closed (scheduler dropped): stop accepting new devices,
                    // but keep driving the distributors we already own until they end.
                    Poll::Ready(None) => {
                        this.rx_open = false;
                        break;
                    }
                    Poll::Pending => break,
                }
            }
        }

        // 2. Drive every distributor. They never resolve under normal operation; a resolved one is
        //    removed. When the registration channel is closed and no distributors remain, the
        //    registrar task ends.
        let mut i = 0;
        while i < this.distributors.len() {
            match this.distributors[i].as_mut().poll(cx) {
                Poll::Ready(()) => {
                    // Drop the finished distributor future (explicit: `swap_remove` returns the boxed
                    // future, which is `#[must_use]`). Don't advance `i` — the swapped-in tail now
                    // occupies index `i` and must be polled this pass.
                    drop(this.distributors.swap_remove(i));
                }
                Poll::Pending => i += 1,
            }
        }

        if !this.rx_open && this.distributors.is_empty() {
            return Poll::Ready(());
        }
        Poll::Pending
    }
}
