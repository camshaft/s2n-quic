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
//! and the priority router moves such a frame only once the window admits its bytes. A round of
//! the window starts at each pass of the priority router over its staged frames, whether or not
//! the pass moves a frame. The window's limit comes from how fast the workers take the batches:
//!
//! - It starts at a minimum, which `frame_dispatch` sets to the batch router's batch target.
//! - It grows by the bytes of each batch that a worker takes in the round in which the batch
//!   router read the batch, where the window refused a frame in that round.
//! - It halves, once a round, as a worker takes a batch that the batch router read in an
//!   earlier round, and doesn't go below the minimum.
//!
//! The window admits any frame while it holds nothing, so that a frame larger than the limit
//! still goes, and every frame once the priority router holds its only clone, since nothing
//! would then release their bytes.

use super::FrameBatch;
use crate::{
    endpoint::frame::Frame,
    socket::channel::{ByteCost, UnboundedSender},
};
use core::{cell::Cell, task};
use std::rc::Rc;

/// The bytes of the frames without send credit that the priority router moved into the frame
/// lanes and that no worker's sender took yet, with the limit that the priority router checks
/// before it moves another such frame.
///
/// Its clones share one count: the priority router holds one, and the batch router holds the
/// others, for the round in which it reads each batch and in each [`WindowSender`].
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
    /// The least that `limit` halves to.
    minimum: u64,
    /// The round of the window, which starts at each pass of the priority router over its
    /// staged frames, whether or not the pass moves a frame.
    round: Cell<u64>,
    /// The round in which `limit` last halved.
    halved_in: Cell<Option<u64>>,
    /// Whether the window refused a frame in this round.
    refused: Cell<bool>,
    /// The priority router's waker, which it registers as the window refuses a frame.
    waker: Cell<Option<task::Waker>>,
    /// The path secret map whose subscriber records the window's events, where the window has
    /// one.
    path_secret_map: Option<crate::path::secret::Map>,
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

    /// Returns a window like [`Self::new`] that records each frame it refuses and each change
    /// of its limit as an event of `path_secret_map`'s subscriber.
    pub fn with_path_secret_map(minimum: u64, path_secret_map: crate::path::secret::Map) -> Self {
        Self {
            state: Rc::new(State {
                limit: Cell::new(minimum),
                minimum,
                path_secret_map: Some(path_secret_map),
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

    /// Starts a round of the window. The priority router calls it at each pass over its staged
    /// frames, whether or not the pass moves a frame.
    pub fn start_round(&self) {
        let state = &self.state;
        state.round.set(state.round.get() + 1);
        state.refused.set(false);
    }

    /// Returns the round of the window.
    pub fn round(&self) -> u64 {
        self.state.round.get()
    }

    /// Admits `bytes` of frames without send credit and returns `true` where the window holds
    /// nothing, where the bytes it holds stay within its limit with them, or where no other
    /// clone of the window is left. Otherwise it refuses them and returns `false`.
    ///
    /// A refusal here doesn't record an event, since the bytes don't tell whose frames they
    /// are: [`Self::admit_frame`] records each frame it refuses, with its peer.
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
        state.held.set(held.saturating_add(bytes));
        true
    }

    /// Admits `frame` where it borrows send credit, which the send credit pool bounds, and
    /// otherwise admits its bytes as [`Self::admit`] does.
    ///
    /// Each refusal records a frame-refused event of the window's path secret map, where it
    /// has one, with the frame's peer and its bytes.
    pub fn admit_frame(&self, frame: &Frame) -> bool {
        if frame.flow_credits != 0 {
            return true;
        }
        let bytes = frame.byte_cost();
        if self.admit(bytes) {
            return true;
        }
        // A refusal leaves the bytes the window holds and its limit as they were.
        let state = &self.state;
        if let Some(path_secret_map) = &state.path_secret_map {
            path_secret_map.on_creditless_window_frame_refused(
                frame.path_secret_entry.peer(),
                bytes,
                state.held.get(),
                state.limit.get(),
            );
        }
        false
    }

    /// Releases `bytes` of a batch that the batch router read in round `read_in`, as a worker's
    /// sender takes the batch, and wakes the priority router.
    ///
    /// The limit halves, once a round, where the batch was read in an earlier round, and grows
    /// by `bytes` where the batch was read in this round and the window refused a frame in it.
    /// Each halving, and each round in which the minimum keeps the limit from halving, records
    /// a limit-halved event, and each growth a limit-grew event, of the window's path secret
    /// map, where it has one.
    pub fn release(&self, bytes: u64, read_in: u64) {
        if bytes == 0 {
            return;
        }
        let state = &self.state;
        let held = state.held.get().saturating_sub(bytes);
        state.held.set(held);
        let round = state.round.get();
        if read_in < round {
            if state.halved_in.get() != Some(round) {
                state.halved_in.set(Some(round));
                let previous_limit = state.limit.get();
                let limit = (previous_limit / 2).max(state.minimum);
                state.limit.set(limit);
                if let Some(path_secret_map) = &state.path_secret_map {
                    path_secret_map.on_creditless_window_limit_halved(held, previous_limit, limit);
                }
            }
        } else if state.refused.get() {
            let limit = state.limit.get().saturating_add(bytes);
            state.limit.set(limit);
            if let Some(path_secret_map) = &state.path_secret_map {
                path_secret_map.on_creditless_window_limit_grew(bytes, held, limit);
            }
        }
        self.wake();
    }

    /// Registers `waker`, which the window wakes as a worker's sender releases bytes or a clone
    /// of the window drops.
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

/// A batch's bytes of frames without send credit, and the round of the window in which the
/// batch router read the batch.
pub trait CreditlessCost {
    /// Returns the bytes of the batch's frames without send credit.
    fn creditless_bytes(&self) -> u64;

    /// Returns the round of the window in which the batch router read the batch.
    fn window_round(&self) -> u64;
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

    fn window_round(&self) -> u64 {
        self.window_round
    }
}

impl<T: CreditlessCost> CreditlessCost for crate::intrusive::Entry<T> {
    fn creditless_bytes(&self) -> u64 {
        (**self).creditless_bytes()
    }

    fn window_round(&self) -> u64 {
        (**self).window_round()
    }
}

impl FrameBatch {
    /// Sets the round of the window in which the batch router read the batch.
    pub fn set_window_round(&mut self, round: u64) {
        self.window_round = round;
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
        let read_in = value.window_round();
        let result = self.sender.send(value);
        // A router ends once a worker's receiver is gone, so the window releases the bytes
        // whether the worker took the batch or not.
        self.window.release(bytes, read_in);
        result
    }
}
