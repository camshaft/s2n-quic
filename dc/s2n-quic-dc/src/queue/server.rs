// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Server-side queue state and dispatch.
//!
//! ## Architecture
//!
//! Server queue management is split into two layers:
//!
//! - [`ServerState`] — shared state stored on the path secret Entry. Owns the
//!   pinned page table and freed-slot accumulator. Created once per peer.
//!
//! - [`ServerView`] — lightweight cached view stored on each recv::Context.
//!   Caches raw pointers into the page table for O(1) dispatch without holding
//!   the RwLock on every packet.
//!
//! ## Slot lifecycle (server side)
//!
//! ```text
//! client creates stream    →  slot allocated, binding_id = session_binding_id
//! first server packet      →  bind_and_send_stream: bind + open + push (atomic)
//! data packets             →  send_stream / send_control
//! stream complete          →  ControlReceiver / StreamReceiver dropped
//!                          →  freed_state.record(queue_id, ...) → QueueFree to client
//! ```

use super::{
    freed::{FreedBatchTx, FreedInner},
    half::AutoWake,
    handle::{ControlReceiver, OnFree, StreamReceiver},
    page_table::{PageTable, SenderView},
    slot::BindState,
    Error,
};
use crate::{endpoint::msg, intrusive};
use s2n_quic_core::varint::VarInt;
use std::sync::Arc;

use crate::path::secret::map::Entry as PathSecretEntry;

// ── BindResult ────────────────────────────────────────────────────────────────

/// Outcome of `ServerView::bind_and_send_stream`.
pub enum BindResult {
    /// The slot already had a matching binding — packet pushed.
    /// `release_bytes` is the number of newly-buffered bytes the caller must
    /// return to the recv credit pool.
    Bound { waker: AutoWake, release_bytes: u64 },
    /// A new binding was created.  The caller must hand the receivers to the
    /// stream handshake task.
    NewBinding {
        waker: AutoWake,
        stream: StreamReceiver,
        control: ControlReceiver,
        /// Newly-buffered bytes from the bind frame's payload that exceed the
        /// unbacked initial window. Caller must release these to the recv pool.
        release_bytes: u64,
    },
}

// ── ServerState (shared, on path secret Entry) ───────────────────────────────

/// Shared server-side queue state for a single peer connection.
///
/// Stored on the path secret Entry. Contains the page table (slot storage) and
/// the freed-slot accumulator. Does not hold any channel references — those are
/// passed in at call sites by the recv::Context.
#[derive(Debug)]
pub struct ServerState {
    pub(crate) pages: PageTable,
    pub(crate) freed: FreedInner,
    pub(crate) max_queue_id: u64,
    /// Per-stream unbacked initial recv window — the bytes a peer may send
    /// before any pool-backed window grants flow. Configured at endpoint
    /// construction; passed into slot bind to seed `initial_window_remaining`.
    pub(crate) initial_recv_window: u64,
}

impl ServerState {
    pub fn new(max_queues: VarInt, initial_recv_window: u64) -> Self {
        Self {
            pages: PageTable::new(),
            freed: FreedInner::new(),
            max_queue_id: max_queues.as_u64().saturating_sub(1),
            initial_recv_window,
        }
    }

    /// Create a `ServerView` for use on a dispatch worker.
    pub fn view(self: &Arc<Self>) -> ServerView {
        ServerView {
            state: self.clone(),
            view: SenderView::new(),
        }
    }

    /// Push Reset into all allocated slots.
    ///
    /// Does NOT permanently close the slots — this is a transient peer-dead
    /// notification.  After cooldown expires, new bindings proceed normally.
    pub fn broadcast_reset(&self, error_code: VarInt, waker_sink: &mut impl FnMut(AutoWake)) {
        let mut view = SenderView::new();
        view.for_each_slot(&self.pages, |slot| {
            let (sw, cw) = slot.broadcast_reset(error_code);
            waker_sink(sw);
            waker_sink(cw);
        });
    }
}

// ── ServerView (per recv::Context) ───────────────────────────────────────────

/// Per-worker cached view for dispatching inbound packets.
///
/// Holds a single `Arc<ServerState>` (keeps page table + freed state alive)
/// plus a local `SenderView` pointer cache for O(1) dispatch.
pub struct ServerView {
    state: Arc<ServerState>,
    view: SenderView,
}

impl ServerView {
    /// Record a queue_id as freed without binding it.
    ///
    /// Used when the server rejects an init (e.g. acceptor not found) and needs
    /// to signal the client to reclaim the peer-side slot via QueueFree.
    pub fn record_freed(
        &self,
        queue_id: VarInt,
        path_entry: &Arc<PathSecretEntry>,
        endpoint_tx: &mut FreedBatchTx,
    ) {
        self.state.freed.record(queue_id, path_entry, endpoint_tx);
    }

    /// Hold the slot `queue_id` for the rejected init of `binding_id` until its reset leaves the
    /// priority router, so that the slot is recorded as freed only then. Returns false where the
    /// slot is held already, and for a slot past the peer's queues, which `bind_for_msg` rejects
    /// without a reset.
    pub fn hold_freed(&self, queue_id: VarInt, binding_id: VarInt) -> bool {
        if queue_id.as_u64() > self.state.max_queue_id {
            return false;
        }
        self.state.freed.hold(queue_id, binding_id)
    }

    /// End the hold of the slot `queue_id` for the rejected init of `binding_id`, where its reset
    /// doesn't reach the priority router, before the slot is freed.
    pub fn unhold_freed(&self, queue_id: VarInt, binding_id: VarInt) {
        self.state.freed.unhold(queue_id, binding_id)
    }

    /// Attempt to bind a slot and push the first stream entry.
    pub fn bind_and_send_stream(
        &mut self,
        queue_id: VarInt,
        binding_id: VarInt,
        entry: intrusive::Entry<msg::Stream>,
        path_entry: &Arc<PathSecretEntry>,
        endpoint_tx: &mut FreedBatchTx,
    ) -> Result<BindResult, Error<intrusive::Entry<msg::Stream>>> {
        let index = queue_id.as_u64() as usize;

        if queue_id.as_u64() > self.state.max_queue_id {
            return Err(Error::CapExceeded(entry));
        }

        if index >= self.view.total_slots() {
            self.view.grow_to_fit(index, &self.state.pages);
        }

        let Some(slot) = self.view.get(index, &self.state.pages) else {
            return Err(Error::Unallocated(entry));
        };

        let (state, release_bytes) =
            slot.bind_and_push_stream(binding_id, entry, self.state.initial_recv_window)?;
        match state {
            BindState::AlreadyBound(waker) => Ok(BindResult::Bound {
                waker,
                release_bytes,
            }),
            BindState::NewBinding(waker) => {
                let slot_ptr = slot.as_ptr();
                let on_free = OnFree::Server {
                    path_entry: path_entry.clone(),
                    endpoint_tx: endpoint_tx.clone(),
                };
                let stream = StreamReceiver::new(slot_ptr, on_free.clone());
                let control = ControlReceiver::new(slot_ptr, on_free);
                Ok(BindResult::NewBinding {
                    waker,
                    stream,
                    control,
                    release_bytes,
                })
            }
        }
    }

    /// Bind a slot for a QueueMsg init frame without pushing data.
    ///
    /// The actual data delivery happens through `push_msg` after binding.
    /// Returns `Bound` if the slot is already allocated with a matching binding.
    pub fn bind_for_msg(
        &mut self,
        queue_id: VarInt,
        binding_id: VarInt,
        path_entry: &Arc<PathSecretEntry>,
        endpoint_tx: &mut FreedBatchTx,
    ) -> Result<BindResult, Error<()>> {
        self.bind_for_init(queue_id, binding_id, path_entry, endpoint_tx)
            .map(|(result, _)| result)
    }

    /// [`Self::bind_for_msg`], which also returns the tombstone the slot held before a
    /// `NewBinding`, for [`Self::restore_tombstone`] to put back where the endpoint doesn't take
    /// the init that bound the slot; 0 for `Bound`.
    pub(crate) fn bind_for_init(
        &mut self,
        queue_id: VarInt,
        binding_id: VarInt,
        path_entry: &Arc<PathSecretEntry>,
        endpoint_tx: &mut FreedBatchTx,
    ) -> Result<(BindResult, u64), Error<()>> {
        let index = queue_id.as_u64() as usize;

        if queue_id.as_u64() > self.state.max_queue_id {
            return Err(Error::CapExceeded(()));
        }

        if index >= self.view.total_slots() {
            self.view.grow_to_fit(index, &self.state.pages);
        }

        let Some(slot) = self.view.get(index, &self.state.pages) else {
            return Err(Error::Unallocated(()));
        };

        // Check if already bound — use the same atomic check as push_stream
        let stored = slot.binding_id_raw();
        if stored & super::slot::UNALLOCATED_BIT == 0 {
            // Slot is allocated. If binding matches, it's already bound.
            if stored == binding_id.as_u64() {
                return Ok((
                    BindResult::Bound {
                        waker: AutoWake::default(),
                        release_bytes: 0,
                    },
                    0,
                ));
            }
            // Binding mismatch — stale or future
            if binding_id.as_u64() < stored {
                return Err(Error::StaleBinding(()));
            } else {
                return Err(Error::FutureBinding(()));
            }
        }

        // Fast path: reject stale/duplicate init frames from an old generation before
        // taking the slot locks. A recycled slot preserves its previous binding_id as a
        // tombstone (see `Slot::mark_unallocated`), and only a strictly greater binding
        // may re-allocate it. This `stored` read is lock-free and NOT atomic with the
        // allocation below, so it is only an optimization — `allocate_and_open`
        // re-validates the tombstone under the slot locks and returns
        // `AllocateOutcome::Stale` if a concurrent recycle bumped the tombstone in the
        // window between this read and the allocation. That in-lock check is what
        // actually guarantees a recycled slot can never be resurrected under a zombie
        // binding_id; this mirrors `bind_and_push_stream`.
        let tombstone = stored & !super::slot::UNALLOCATED_BIT;
        if binding_id.as_u64() <= tombstone {
            return Err(Error::StaleBinding(()));
        }

        // Slot is unallocated — bind it (tombstone re-checked under the lock).
        match slot.allocate_and_open_with_tombstone(binding_id, self.state.initial_recv_window) {
            Ok((super::slot::AllocateOutcome::Allocated, tombstone)) => {
                let slot_ptr = slot.as_ptr();
                let on_free = OnFree::Server {
                    path_entry: path_entry.clone(),
                    endpoint_tx: endpoint_tx.clone(),
                };
                let stream = StreamReceiver::new(slot_ptr, on_free.clone());
                let control = ControlReceiver::new(slot_ptr, on_free);
                Ok((
                    BindResult::NewBinding {
                        waker: AutoWake::default(),
                        stream,
                        control,
                        release_bytes: 0,
                    },
                    tombstone,
                ))
            }
            Ok((super::slot::AllocateOutcome::AlreadyBound, _)) => Ok((
                BindResult::Bound {
                    waker: AutoWake::default(),
                    release_bytes: 0,
                },
                0,
            )),
            // A concurrent recycle bumped the tombstone past `binding_id` after the
            // lock-free fast-path read passed — reject as stale.
            Ok((super::slot::AllocateOutcome::Stale, _)) => Err(Error::StaleBinding(())),
            Err(_) => Err(Error::SenderClosed),
        }
    }

    #[inline]
    pub fn send_stream(
        &mut self,
        queue_id: VarInt,
        binding_id: VarInt,
        entry: intrusive::Entry<msg::Stream>,
    ) -> Result<(AutoWake, u64), Error<intrusive::Entry<msg::Stream>>> {
        let index = queue_id.as_u64() as usize;
        let Some(slot) = self.view.get(index, &self.state.pages) else {
            return Err(Error::Unallocated(entry));
        };
        slot.push_stream(binding_id, entry)
    }

    /// Read-only diagnostic snapshot of the slot at `queue_id`, or `None` if no slot is mapped
    /// there. For logging why a delivery was rejected; see [`super::slot::Slot::diag`].
    #[inline]
    pub(crate) fn slot_diag(&mut self, queue_id: VarInt) -> Option<super::slot::SlotDiag> {
        let index = queue_id.as_u64() as usize;
        self.view
            .get(index, &self.state.pages)
            .map(|slot| slot.diag())
    }

    /// Puts back `tombstone`, which [`Self::bind_for_init`] returned as it bound the slot at
    /// `queue_id` for `binding_id`, once the endpoint tears that binding's stream down without
    /// taking its init. See [`super::slot::Slot::restore_tombstone`].
    pub(crate) fn restore_tombstone(
        &mut self,
        queue_id: VarInt,
        binding_id: VarInt,
        tombstone: u64,
    ) {
        let index = queue_id.as_u64() as usize;
        if let Some(slot) = self.view.get(index, &self.state.pages) {
            slot.restore_tombstone(binding_id, tombstone);
        }
    }

    /// Marks the slot at `queue_id` so that the last receiver's drop of its binding doesn't
    /// record the slot as freed, where the endpoint tears down a binding whose init it doesn't
    /// take. See [`super::slot::Slot::leave_free_unrecorded`].
    pub(crate) fn leave_free_unrecorded(&mut self, queue_id: VarInt) {
        let index = queue_id.as_u64() as usize;
        if let Some(slot) = self.view.get(index, &self.state.pages) {
            slot.leave_free_unrecorded();
        }
    }

    /// The shared state this view reads, for a teardown that outlives this view and puts a
    /// tombstone back through a view of its own.
    pub(crate) fn state(&self) -> &Arc<ServerState> {
        &self.state
    }

    #[inline]
    pub fn send_control(
        &mut self,
        queue_id: VarInt,
        binding_id: VarInt,
        entry: intrusive::Entry<msg::Control>,
    ) -> Result<AutoWake, Error<intrusive::Entry<msg::Control>>> {
        let index = queue_id.as_u64() as usize;
        let Some(slot) = self.view.get(index, &self.state.pages) else {
            return Err(Error::Unallocated(entry));
        };
        slot.push_control(binding_id, entry)
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn send_msg<E>(
        &mut self,
        queue_id: VarInt,
        binding_id: VarInt,
        msg_id: u64,
        stream_offset: u64,
        peer_max_offset: u64,
        message_size: u32,
        chunk_size: u16,
        chunk_index: u32,
        payload_len: u32,
        is_fin: bool,
        is_wakeup: bool,
        blocked: bool,
        write_fn: impl FnOnce(*mut u8, u32) -> Result<(), E>,
    ) -> Result<(AutoWake, u64), super::MsgError<E>> {
        let index = queue_id.as_u64() as usize;
        let Some(slot) = self.view.get(index, &self.state.pages) else {
            return Err(super::MsgError::Queue(Error::Unallocated(())));
        };
        slot.push_msg(
            binding_id,
            msg_id,
            stream_offset,
            peer_max_offset,
            message_size,
            chunk_size,
            chunk_index,
            payload_len,
            is_fin,
            is_wakeup,
            blocked,
            write_fn,
        )
    }

    /// Broadcast-close all slots — called when the path secret entry is evicted.
    pub fn close(&mut self, waker_sink: &mut impl FnMut(AutoWake)) {
        self.view.for_each_slot(&self.state.pages, |slot| {
            let (sw, cw) = slot.broadcast_close();
            waker_sink(sw);
            waker_sink(cw);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{freed::freed_batch_channel, testing::*};
    use s2n_quic_core::varint::VarInt;

    fn v(n: u64) -> VarInt {
        VarInt::new(n).unwrap()
    }

    fn test_server(
        max_queues: u64,
    ) -> (
        ServerView,
        Arc<PathSecretEntry>,
        FreedBatchTx,
        super::super::freed::FreedBatchRx,
    ) {
        use s2n_quic_core::dc;
        let (tx, rx) = freed_batch_channel();
        let mut params = dc::testing::TEST_APPLICATION_PARAMS;
        params.max_queues = VarInt::new(max_queues).unwrap();
        let path_entry: Arc<PathSecretEntry> =
            PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
                .endpoint_type(s2n_quic_core::endpoint::Type::Server)
                .params(params)
                .build(None);
        let crate::path::secret::map::entry::QueueState::Server(ref state) =
            *path_entry.queue_state()
        else {
            panic!("expected Server queue state");
        };
        let view = state.view();
        (view, path_entry, tx, rx)
    }

    #[test]
    fn bind_and_send_new() {
        let (mut server, path_entry, mut tx, _rx) = test_server(10);
        let result =
            server.bind_and_send_stream(v(0), v(1), make_stream_entry(), &path_entry, &mut tx);
        assert!(matches!(result, Ok(BindResult::NewBinding { .. })));
    }

    #[test]
    fn bind_and_send_existing() {
        let (mut server, path_entry, mut tx, _rx) = test_server(10);
        let result =
            server.bind_and_send_stream(v(0), v(1), make_stream_entry(), &path_entry, &mut tx);
        let Ok(BindResult::NewBinding {
            stream, control, ..
        }) = result
        else {
            panic!("expected NewBinding");
        };

        // Second send with same binding
        let result =
            server.bind_and_send_stream(v(0), v(1), make_stream_entry(), &path_entry, &mut tx);
        assert!(matches!(result, Ok(BindResult::Bound { .. })));

        drop((stream, control));
    }

    #[test]
    fn stale_binding_rejected() {
        let (mut server, path_entry, mut tx, _rx) = test_server(10);
        let result =
            server.bind_and_send_stream(v(0), v(2), make_stream_entry(), &path_entry, &mut tx);
        let Ok(BindResult::NewBinding {
            stream, control, ..
        }) = result
        else {
            panic!("expected NewBinding");
        };

        // Stale binding (1 < current 2)
        let result =
            server.bind_and_send_stream(v(0), v(1), make_stream_entry(), &path_entry, &mut tx);
        assert!(matches!(result, Err(Error::StaleBinding(_))));

        drop((stream, control));
    }

    #[test]
    fn future_binding_rejected() {
        let (mut server, path_entry, mut tx, _rx) = test_server(10);
        let result =
            server.bind_and_send_stream(v(0), v(1), make_stream_entry(), &path_entry, &mut tx);
        let Ok(BindResult::NewBinding {
            stream, control, ..
        }) = result
        else {
            panic!("expected NewBinding");
        };

        // Future binding (5 > current 1)
        let result =
            server.bind_and_send_stream(v(0), v(5), make_stream_entry(), &path_entry, &mut tx);
        assert!(matches!(result, Err(Error::FutureBinding(_))));

        drop((stream, control));
    }

    #[test]
    fn cap_exceeded() {
        let (mut server, path_entry, mut tx, _rx) = test_server(5);
        let result =
            server.bind_and_send_stream(v(5), v(1), make_stream_entry(), &path_entry, &mut tx);
        assert!(matches!(result, Err(Error::CapExceeded(_))));
    }

    #[test]
    fn send_stream_after_bind() {
        let (mut server, path_entry, mut tx, _rx) = test_server(10);
        let result =
            server.bind_and_send_stream(v(0), v(1), make_stream_entry(), &path_entry, &mut tx);
        let Ok(BindResult::NewBinding {
            stream, control, ..
        }) = result
        else {
            panic!("expected NewBinding");
        };

        let result = server.send_stream(v(0), v(1), make_stream_entry());
        assert!(result.is_ok());

        drop((stream, control));
    }

    #[test]
    fn send_control_after_bind() {
        let (mut server, path_entry, mut tx, _rx) = test_server(10);
        let result =
            server.bind_and_send_stream(v(0), v(1), make_stream_entry(), &path_entry, &mut tx);
        let Ok(BindResult::NewBinding {
            stream, control, ..
        }) = result
        else {
            panic!("expected NewBinding");
        };

        let result = server.send_control(v(0), v(1), make_control_entry());
        assert!(result.is_ok());

        drop((stream, control));
    }

    #[test]
    fn send_to_unbound_slot() {
        let (mut server, _path_entry, _tx, _rx) = test_server(10);
        let result = server.send_stream(v(0), v(1), make_stream_entry());
        assert!(matches!(result, Err(Error::Unallocated(_))));
    }

    #[test]
    fn close_wakes_receivers() {
        let (mut server, path_entry, mut tx, _rx) = test_server(10);
        let result =
            server.bind_and_send_stream(v(0), v(1), make_stream_entry(), &path_entry, &mut tx);
        let Ok(BindResult::NewBinding {
            stream, control, ..
        }) = result
        else {
            panic!("expected NewBinding");
        };

        let mut waker_count = 0;
        server.close(&mut |_| waker_count += 1);
        // At least the two bound halves should produce wakers
        assert!(waker_count >= 2);

        drop((stream, control));
    }

    #[test]
    fn freed_batch_submitted_on_receiver_drop() {
        use crate::{
            socket::channel::Budget,
            testing::{ext::*, sim},
        };

        sim(|| {
            let (mut server, path_entry, mut tx, mut rx) = test_server(10);

            async move {
                let mut budget = Budget::new(usize::MAX);

                let result = server.bind_and_send_stream(
                    v(0),
                    v(1),
                    make_stream_entry(),
                    &path_entry,
                    &mut tx,
                );
                let Ok(BindResult::NewBinding {
                    stream, control, ..
                }) = result
                else {
                    panic!("expected NewBinding");
                };

                // Drop both receivers — should trigger freed notification
                drop(stream);
                drop(control);

                // Batch should have been submitted
                let entry = crate::socket::channel::Receiver::<
                    crate::intrusive::Entry<crate::stream::endpoint::msg::Sender>,
                >::recv(&mut rx, &mut budget)
                .await;
                assert!(entry.is_some());
            }
            .primary()
            .spawn();
        });
    }

    #[test]
    fn bind_for_msg_rejects_stale_on_recycled() {
        let (mut server, path_entry, mut tx, _rx) = test_server(10);

        // Bind queue 0 with binding_id 5 via the msg-init path.
        let result = server.bind_for_msg(v(0), v(5), &path_entry, &mut tx);
        let Ok(BindResult::NewBinding {
            stream, control, ..
        }) = result
        else {
            panic!("expected NewBinding for fresh slot");
        };

        // Receivers drop → slot recycled, tombstone preserves binding 5.
        drop(stream);
        drop(control);

        // A stale/reordered init frame from the OLD binding (4 < tombstone 5)
        // arrives. It MUST be rejected as a stale binding, exactly as
        // bind_and_send_stream / bind_and_push_stream would do
        // (see slot.rs `bind_and_push_stale_on_recycled`). Otherwise the slot
        // is re-allocated under a zombie binding id.
        let result = server.bind_for_msg(v(0), v(4), &path_entry, &mut tx);
        assert!(
            matches!(result, Err(Error::StaleBinding(_))),
            "stale init on recycled slot must be rejected"
        );
    }

    #[test]
    fn bind_for_msg_rejects_equal_on_recycled() {
        let (mut server, path_entry, mut tx, _rx) = test_server(10);

        let result = server.bind_for_msg(v(0), v(5), &path_entry, &mut tx);
        let Ok(BindResult::NewBinding {
            stream, control, ..
        }) = result
        else {
            panic!("expected NewBinding");
        };
        drop(stream);
        drop(control);

        // A duplicate of the SAME binding that was just freed must not
        // resurrect the slot — equal-to-tombstone is stale.
        let result = server.bind_for_msg(v(0), v(5), &path_entry, &mut tx);
        assert!(
            matches!(result, Err(Error::StaleBinding(_))),
            "equal-to-tombstone init on recycled slot must be rejected"
        );
    }

    #[test]
    fn rebind_after_free() {
        let (mut server, path_entry, mut tx, _rx) = test_server(10);

        // First binding
        let result =
            server.bind_and_send_stream(v(0), v(1), make_stream_entry(), &path_entry, &mut tx);
        let Ok(BindResult::NewBinding {
            stream, control, ..
        }) = result
        else {
            panic!("expected NewBinding");
        };
        drop(stream);
        drop(control);

        // Re-bind with higher binding_id
        let result =
            server.bind_and_send_stream(v(0), v(2), make_stream_entry(), &path_entry, &mut tx);
        assert!(matches!(result, Ok(BindResult::NewBinding { .. })));
    }

    #[test]
    fn a_free_left_unrecorded_doesnt_record_the_slot() {
        let (mut server, path_entry, mut tx, _rx) = test_server(10);

        // Bindings 1 and 2 of slot 1 are marked before their receivers drop, and bindings 3
        // and 4 aren't, each pair in both orders of the drops.
        for (binding, marked, stream_last) in [
            (1, true, true),
            (2, true, false),
            (3, false, true),
            (4, false, false),
        ] {
            let result = server.bind_for_msg(v(1), v(binding), &path_entry, &mut tx);
            let Ok(BindResult::NewBinding {
                stream, control, ..
            }) = result
            else {
                panic!("expected NewBinding for binding {binding}");
            };
            if marked {
                server.leave_free_unrecorded(v(1));
            }
            if stream_last {
                drop(control);
                drop(stream);
            } else {
                drop(stream);
                drop(control);
            }
            let mut freed = crate::bitset::HierarchicalBitSet::new(1);
            if marked {
                assert!(
                    server.state.freed.take(&mut freed).is_none(),
                    "binding {binding}: a marked end doesn't record slot 1 as freed"
                );
            } else {
                assert!(
                    server.state.freed.take(&mut freed).is_some() && freed.contains(1),
                    "binding {binding}: an unmarked end records slot 1 as freed"
                );
            }
        }
    }
}
