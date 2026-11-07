// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A window that bounds the bytes of the frames without send credit between the priority router
//! of `frame_dispatch` and the workers' senders.
//!
//! The send credit pool bounds each frame that borrows send credit. A frame that doesn't, such
//! as a control frame, an acknowledgement or a reset, has `flow_credits` 0, and nothing bounded
//! those frames before this window: the frame lanes don't push back, and the batch router holds
//! any number of a waiting peer's batches, as
//! [`InterfacePacedPickTwo`](super::InterfacePacedPickTwo) does while each sender that delivers
//! to the peer is busy.
//!
//! A [`CreditlessWindow`] holds the bytes of those frames from the moment the priority router
//! moves them into their lane until a [`WindowSender`] hands their batch to a worker's sender,
//! and the priority router moves such a frame only once the window admits its bytes. The
//! window's limit comes from how fast the workers take the batches:
//!
//! - It starts at a minimum, which `frame_dispatch` sets to the batch router's batch target.
//! - It grows where a [`WindowReader`] finds that the batch router doesn't have a batch for a
//!   worker that could take one while the window refuses a frame: by the bytes of the last batch
//!   a worker's sender took, and at least by the minimum, once for each refusal. A pacer that
//!   holds a batch until its interface may send doesn't ask for another, so the limit stops
//!   growing once the batch router has a batch each time a pacer asks for one, however fast the
//!   frames come.
//! - It halves as a worker's sender takes a batch that waited in the router for a sender, beyond
//!   the batch that each pacer holds, and doesn't go below the minimum. It halves once for the
//!   bytes the window held then: the batches that release those bytes don't halve it again.
//!
//! The window admits any frame while it holds nothing, so that a frame larger than the limit
//! still goes, and every frame once the priority router holds its only clone, since nothing
//! would then release their bytes.

use super::FrameBatch;
use crate::{
    endpoint::frame::Frame,
    socket::channel::{Budget, ByteCost, Receiver, UnboundedSender},
};
use core::{
    cell::Cell,
    task::{self, Poll},
};
use std::rc::Rc;

/// The bytes of the frames without send credit that the priority router moved into the frame
/// lanes and that no worker's sender took yet, with the limit that the priority router checks
/// before it moves another such frame.
///
/// Its clones share one count: the priority router holds one, and the batch router holds the
/// others, in its [`WindowReader`] and in each [`WindowSender`].
#[derive(Clone, Default)]
pub struct CreditlessWindow {
    state: Rc<State>,
}

#[derive(Default)]
struct State {
    /// The bytes the window holds.
    held: Cell<u64>,
    /// The bytes past which the window refuses a frame, where it holds any.
    limit: Cell<u64>,
    /// The least that `limit` halves to, and grows by.
    minimum: u64,
    /// The bytes of the last batch that a worker's sender took.
    last_released: Cell<u64>,
    /// The bytes that the window held as `limit` last halved and that no worker's sender took
    /// since: `limit` doesn't halve again until they are gone.
    halving_held: Cell<u64>,
    /// Whether the window refused a frame since it last admitted one.
    refused: Cell<bool>,
    /// The priority router's waker, which it registers as the window refuses a frame.
    waker: Cell<Option<task::Waker>>,
}

impl CreditlessWindow {
    /// Returns a window whose limit starts at `minimum` and doesn't halve below it.
    pub fn new(minimum: u64) -> Self {
        Self {
            state: Rc::new(State {
                limit: Cell::new(minimum),
                minimum,
                ..Default::default()
            }),
        }
    }

    /// Returns a window whose limit starts at `limit`, with a minimum of 0.
    #[cfg(test)]
    pub fn with_limit(limit: u64) -> Self {
        Self {
            state: Rc::new(State {
                limit: Cell::new(limit),
                ..Default::default()
            }),
        }
    }

    /// Admits `bytes` of frames without send credit and returns `true` where the window holds
    /// nothing, where the bytes it holds stay within its limit with them, or where no other
    /// clone of the window is left. Otherwise it refuses them and returns `false`.
    pub fn admit(&self, bytes: u64) -> bool {
        let state = &self.state;
        if Rc::strong_count(&self.state) == 1 {
            return true;
        }
        let held = state.held.get();
        if held != 0 && held.saturating_add(bytes) > state.limit.get() {
            state.refused.set(true);
            return false;
        }
        state.refused.set(false);
        state.held.set(held.saturating_add(bytes));
        true
    }

    /// Admits `frame` where it borrows send credit, which the send credit pool bounds, and
    /// otherwise admits its bytes as [`Self::admit`] does.
    pub fn admit_frame(&self, frame: &Frame) -> bool {
        frame.flow_credits != 0 || self.admit(frame.byte_cost())
    }

    /// Tells the window that a worker could take a batch and the batch router doesn't have one.
    ///
    /// Where the window refused a frame since it last admitted one, the limit grows by the bytes
    /// of the last batch that a worker's sender took, and at least by the minimum, and the
    /// window wakes the priority router. It grows once for each refusal.
    pub fn starved(&self) {
        let state = &self.state;
        if !state.refused.replace(false) {
            return;
        }
        let growth = state.last_released.get().max(state.minimum);
        state.limit.set(state.limit.get().saturating_add(growth));
        self.wake();
    }

    /// Releases `bytes` of a batch as a worker's sender takes the batch, and wakes the priority
    /// router.
    ///
    /// Where the batch waited in the router for a sender, the limit halves, and doesn't go below
    /// the minimum, unless bytes that the window held as the limit last halved are still in it.
    pub fn release(&self, bytes: u64, waited: bool) {
        if bytes == 0 {
            return;
        }
        let state = &self.state;
        let held = state.held.get().saturating_sub(bytes);
        state.held.set(held);
        state.last_released.set(bytes);
        let halving_held = state.halving_held.get();
        if halving_held != 0 {
            state.halving_held.set(halving_held.saturating_sub(bytes));
        } else if waited {
            state.limit.set((state.limit.get() / 2).max(state.minimum));
            state.halving_held.set(held);
        }
        self.wake();
    }

    /// Registers `waker`, which the window wakes as a worker's sender releases bytes, as its
    /// limit grows, or as a clone of the window drops.
    pub fn register(&self, waker: &task::Waker) {
        let waker = match self.state.waker.take() {
            Some(registered) if registered.will_wake(waker) => registered,
            _ => waker.clone(),
        };
        self.state.waker.set(Some(waker));
    }

    fn wake(&self) {
        if let Some(waker) = self.state.waker.take() {
            waker.wake();
        }
    }

    /// Returns the bytes the window holds.
    #[cfg(test)]
    pub fn held(&self) -> u64 {
        self.state.held.get()
    }

    /// Returns the window's limit.
    #[cfg(test)]
    pub fn limit(&self) -> u64 {
        self.state.limit.get()
    }
}

impl Drop for CreditlessWindow {
    fn drop(&mut self) {
        // The window admits every frame once the priority router holds its only clone, so the
        // priority router polls again as a clone drops.
        self.wake();
    }
}

/// A batch's bytes of frames without send credit, and whether the batch waited in the router
/// for a sender.
pub trait CreditlessCost {
    /// Returns the bytes of the batch's frames without send credit.
    fn creditless_bytes(&self) -> u64;

    /// Returns whether the batch waited in the router for a sender, beyond the batch that each
    /// pacer holds.
    fn waited(&self) -> bool;

    /// Marks the batch as one that waited in the router for a sender.
    fn set_waited(&mut self);
}

impl CreditlessCost for FrameBatch {
    fn creditless_bytes(&self) -> u64 {
        self.queues
            .iter()
            .flat_map(|queue| queue.iter())
            .filter(|frame| frame.flow_credits == 0)
            .map(|frame| frame.byte_cost())
            .sum()
    }

    fn waited(&self) -> bool {
        self.waited
    }

    fn set_waited(&mut self) {
        self.waited = true;
    }
}

impl<T: CreditlessCost> CreditlessCost for crate::intrusive::Entry<T> {
    fn creditless_bytes(&self) -> u64 {
        (**self).creditless_bytes()
    }

    fn waited(&self) -> bool {
        (**self).waited()
    }

    fn set_waited(&mut self) {
        (**self).set_waited();
    }
}

/// The receiver of the batch router's batches, which tells a [`CreditlessWindow`] where it
/// doesn't have a batch for a worker that could take one.
///
/// The router after it polls it only while a worker could take a batch: `Paced` while it
/// doesn't hold a batch, and [`InterfacePacedPickTwo`](super::InterfacePacedPickTwo) while the
/// pacer of an interface doesn't hold one. A poll that ends `Pending` while budget is left, and
/// so not for want of budget, starves that worker.
pub struct WindowReader<R> {
    inner: R,
    window: CreditlessWindow,
}

impl<R> WindowReader<R> {
    pub fn new(inner: R, window: CreditlessWindow) -> Self {
        Self { inner, window }
    }
}

impl<T, R> Receiver<T> for WindowReader<R>
where
    R: Receiver<T>,
{
    fn poll_recv(&mut self, cx: &mut task::Context<'_>, budget: &mut Budget) -> Poll<Option<T>> {
        let poll = self.inner.poll_recv(cx, budget);
        if poll.is_pending() && !budget.is_exhausted() {
            self.window.starved();
        }
        poll
    }

    fn on_consumed(&mut self, bytes: u64) {
        self.inner.on_consumed(bytes);
    }
}

/// A worker's sender that releases from a [`CreditlessWindow`] the bytes of each batch that it
/// hands on.
pub struct WindowSender<S> {
    sender: S,
    window: CreditlessWindow,
}

impl<S> WindowSender<S> {
    pub fn new(sender: S, window: CreditlessWindow) -> Self {
        Self { sender, window }
    }
}

impl<T, S> UnboundedSender<T> for WindowSender<S>
where
    T: CreditlessCost,
    S: UnboundedSender<T>,
{
    fn send(&mut self, value: T) -> Result<(), T> {
        let bytes = value.creditless_bytes();
        let waited = value.waited();
        let result = self.sender.send(value);
        // A router ends once a worker's receiver is gone, so the window releases the bytes
        // whether the worker took the batch or not.
        self.window.release(bytes, waited);
        result
    }
}
