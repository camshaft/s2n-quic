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
//! - It grows where a worker could take a batch that the batch router doesn't have while the
//!   window refuses a frame: by the bytes of the last batch a worker's sender took, and at least
//!   by the minimum, once for each refusal. A [`WindowReader`] tells the window so under one
//!   paced interface, and [`InterfacePacedPickTwo`](super::InterfacePacedPickTwo) itself under
//!   several, only while no batch waits in it for a sender. A pacer that holds a batch until its
//!   interface may send doesn't ask for another, and the limit doesn't grow while a peer's
//!   batches wait for its busy senders, so the limit doesn't grow at the pace of the frames.
//! - It halves as a worker's sender takes a batch that waited in the router for a sender, beyond
//!   the batch that each pacer holds, and doesn't go below the minimum. It halves once for the
//!   bytes the window held then: the batches that release those bytes don't halve it again.
//!
//! The window admits any frame while it holds nothing, so that a frame larger than the limit
//! still goes, and every frame once the priority router holds its only clone, since nothing
//! would then release their bytes. It also admits a peer's frame while it holds none of that
//! peer's bytes, so that one peer's frames don't hold back another peer's: the window holds at
//! most its limit and one frame of each peer past it.

use super::FrameBatch;
use crate::{
    endpoint::frame::Frame,
    path::secret::map::Entry as PathSecretEntry,
    socket::channel::{Budget, ByteCost, Receiver, UnboundedSender},
};
use core::{
    cell::{Cell, RefCell},
    task::{self, Poll},
};
use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
};

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
    /// The bytes the window holds for each peer, where it holds any.
    peers: RefCell<HashMap<PeerKey, u64>>,
    /// The bytes past which the window refuses a frame, where it holds any.
    limit: Cell<u64>,
    /// The least that `limit` halves to, and grows by.
    minimum: u64,
    /// The bytes of the last batch that a worker's sender took.
    last_released: Cell<u64>,
    /// The bytes that the window held as `limit` last halved and that no worker's sender took
    /// since: `limit` doesn't halve again until they are gone.
    halving_held: Cell<u64>,
    /// The peers whose frames the window refused since it last admitted a frame of theirs, with
    /// `None` for bytes of no one peer.
    refused: RefCell<HashSet<Option<PeerKey>>>,
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

    /// Admits `bytes` of frames without send credit, as [`Self::admit_peer`] does for frames
    /// of no one peer.
    pub fn admit(&self, bytes: u64) -> bool {
        self.admit_peer(None, bytes)
    }

    /// Admits `bytes` of frames without send credit of `peer` and returns `true` where the
    /// window holds nothing, where it holds none of the bytes of `peer`, where the bytes it
    /// holds stay within its limit with them, or where no other clone of the window is left.
    /// Otherwise it refuses them and returns `false`.
    ///
    /// A refusal here doesn't record an event, since neither the bytes nor a peer's key tell
    /// the peer's address: [`Self::admit_frame`] records each frame it refuses, with its peer.
    pub fn admit_peer(&self, peer: Option<PeerKey>, bytes: u64) -> bool {
        let state = &self.state;
        if Rc::strong_count(&self.state) == 1 {
            return true;
        }
        let held = state.held.get();
        let mut peers = state.peers.borrow_mut();
        // A peer whose frames the window holds none of goes, so that another peer's frames
        // don't hold it back.
        let first_of_peer = peer.is_some_and(|peer| !peers.contains_key(&peer));
        if held != 0 && !first_of_peer && held.saturating_add(bytes) > state.limit.get() {
            state.refused.borrow_mut().insert(peer);
            return false;
        }
        // The window admits this peer's frames again. A refused frame of another peer, which
        // the priority router still holds back, still lets the limit grow.
        state.refused.borrow_mut().remove(&peer);
        state.held.set(held.saturating_add(bytes));
        if let Some(peer) = peer {
            let peer_held = peers.entry(peer).or_default();
            *peer_held = peer_held.saturating_add(bytes);
        }
        true
    }

    /// Admits `frame` where it borrows send credit, which the send credit pool bounds, and
    /// otherwise admits its bytes for its peer as [`Self::admit_peer`] does.
    ///
    /// Each refusal records a frame-refused event of the window's path secret map, where it
    /// has one, with the frame's peer and its bytes.
    pub fn admit_frame(&self, frame: &Frame) -> bool {
        if frame.flow_credits != 0 {
            return true;
        }
        let bytes = frame.byte_cost();
        if self.admit_peer(Some(PeerKey::of(&frame.path_secret_entry)), bytes) {
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

    /// Tells the window that a worker could take a batch and the batch router doesn't have one.
    ///
    /// Where the window refused a frame since it last admitted a frame of the same peer, the
    /// limit grows by the bytes of the last batch that a worker's sender took, and at least by
    /// the minimum, and the window wakes the priority router. It grows once for each refusal.
    /// Each growth records a limit-grew event of the window's path secret map, where it has
    /// one, with the bytes of that last batch.
    pub fn starved(&self) {
        let state = &self.state;
        if state.refused.take().is_empty() {
            return;
        }
        let released = state.last_released.get();
        let limit = state
            .limit
            .get()
            .saturating_add(released.max(state.minimum));
        state.limit.set(limit);
        if let Some(path_secret_map) = &state.path_secret_map {
            path_secret_map.on_creditless_window_limit_grew(released, state.held.get(), limit);
        }
        self.wake();
    }

    /// Releases `bytes` of the frames of `peer` of a batch, as [`Self::release`] does, with the
    /// bytes the window holds for `peer`.
    pub fn release_peer(&self, peer: Option<PeerKey>, bytes: u64, waited: bool) {
        if let Some(peer) = peer {
            let mut peers = self.state.peers.borrow_mut();
            if let Some(peer_held) = peers.get_mut(&peer) {
                *peer_held = peer_held.saturating_sub(bytes);
                if *peer_held == 0 {
                    peers.remove(&peer);
                }
            }
        }
        self.release(bytes, waited);
    }

    /// Releases `bytes` of a batch as a worker's sender takes the batch, and wakes the priority
    /// router.
    ///
    /// Where the batch waited in the router for a sender, the limit halves, and doesn't go below
    /// the minimum, unless bytes that the window held as the limit last halved are still in it.
    /// Each halving, and each one at which the minimum keeps the limit as it is, records a
    /// limit-halved event of the window's path secret map, where it has one.
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
            let previous_limit = state.limit.get();
            let limit = (previous_limit / 2).max(state.minimum);
            state.limit.set(limit);
            state.halving_held.set(held);
            if let Some(path_secret_map) = &state.path_secret_map {
                path_secret_map.on_creditless_window_limit_halved(held, previous_limit, limit);
            }
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

    /// Returns the bytes the window holds for `peer`.
    #[cfg(test)]
    pub fn peer_held(&self, peer: PeerKey) -> u64 {
        self.state.peers.borrow().get(&peer).copied().unwrap_or(0)
    }
}

impl Drop for CreditlessWindow {
    fn drop(&mut self) {
        // The window admits every frame once the priority router holds its only clone, so the
        // priority router polls again as a clone drops.
        self.wake();
    }
}

/// A peer of the frames without send credit, read from the address of its path secret entry,
/// by which the window counts the bytes it holds for each peer. A frame that the window holds
/// keeps its entry, and a worker's sender releases a batch's bytes in the call that hands the
/// batch on, so a key stays one peer's while the window holds bytes for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PeerKey(usize);

impl PeerKey {
    /// Returns the key of the peer of `entry`.
    pub fn of(entry: &Arc<PathSecretEntry>) -> Self {
        Self(Arc::as_ptr(entry) as usize)
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

    /// Returns the peer of the batch's frames without send credit, where it holds any.
    fn creditless_peer(&self) -> Option<PeerKey> {
        None
    }
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

    fn creditless_peer(&self) -> Option<PeerKey> {
        // A batch holds the frames of one path secret entry.
        self.queues
            .iter()
            .flat_map(|queue| queue.iter())
            .find(|frame| frame.flow_credits == 0)
            .map(|frame| PeerKey::of(&frame.path_secret_entry))
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

    fn creditless_peer(&self) -> Option<PeerKey> {
        (**self).creditless_peer()
    }
}

/// The receiver of the batch router's batches, which tells a [`CreditlessWindow`] where it
/// doesn't have a batch for a worker that could take one.
///
/// The router after it polls it only while a worker could take a batch, as `Paced` does while it
/// doesn't hold a batch. A poll that ends `Pending` while budget is left, and so not for want of
/// budget, starves that worker. [`InterfacePacedPickTwo`](super::InterfacePacedPickTwo) tells
/// its window itself instead, since a free sender of one interface can't take the batches that
/// wait in it for the senders of another.
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
        let peer = value.creditless_peer();
        let result = self.sender.send(value);
        // A router ends once a worker's receiver is gone, so the window releases the bytes
        // whether the worker took the batch or not.
        self.window.release_peer(peer, bytes, waited);
        result
    }
}
