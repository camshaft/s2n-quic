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
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

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
    /// How many bound slots still have a receiver the app holds: counted up when a binding
    /// opens a slot, and down when the second of its receivers is dropped. While it isn't
    /// zero, a stream on this path may wait on the peer, so the send idle wheel probes the
    /// path while it is quiet (`send_idle_wheel_drain`).
    open_slots: AtomicUsize,
    /// The resets this side owes the peer for the streams it reset when it marked the peer dead
    /// (`path::secret::map::Entry::mark_dead_owing_resets`).
    pub(crate) owed_resets: super::OwedResets,
}

impl ServerState {
    pub fn new(max_queues: VarInt, initial_recv_window: u64) -> Self {
        Self {
            pages: PageTable::new(),
            freed: FreedInner::new(),
            max_queue_id: max_queues.as_u64().saturating_sub(1),
            initial_recv_window,
            open_slots: AtomicUsize::new(0),
            owed_resets: Default::default(),
        }
    }

    /// Create a `ServerView` for use on a dispatch worker.
    pub fn view(self: &Arc<Self>) -> ServerView {
        ServerView {
            state: self.clone(),
            view: SenderView::new(),
        }
    }

    /// Returns `true` while a bound slot still has a receiver the app holds.
    pub(crate) fn has_open_slot(&self) -> bool {
        self.open_slots.load(Ordering::Relaxed) > 0
    }

    /// Counts a slot down once the second of its receivers is dropped.
    pub(crate) fn on_slot_freed(&self) {
        let previous = self.open_slots.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(previous > 0, "a slot was freed more often than it was opened");
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

    /// Push Reset into the slot of each of `streams` that still holds the stream's binding, as
    /// [`Self::broadcast_reset`] does for every allocated slot.
    ///
    /// `streams` are the streams that [`Self::open_streams`] read as the dead mark recorded the
    /// resets it owes the peer (`Entry::mark_dead_owing_resets`). A slot that read doesn't name,
    /// one bound after it or one whose writer hadn't recorded the peer's queue yet, isn't reset,
    /// and neither is a slot freed or bound again since: each stream the dead mark resets on this
    /// side is owed a reset.
    pub fn reset_streams(
        &self,
        streams: &[super::OwedReset],
        error_code: VarInt,
        waker_sink: &mut impl FnMut(AutoWake),
    ) {
        let mut view = SenderView::new();
        for stream in streams {
            let index = stream.queue_pair.source_queue_id.as_u64() as usize;
            let Some(slot) = view.get(index, &self.pages) else {
                continue;
            };
            let (sw, cw) = slot.reset_binding(stream.binding_id, error_code);
            waker_sink(sw);
            waker_sink(cw);
        }
    }

    /// The stream of each bound slot whose writer recorded the peer's queue, as a reset of it
    /// that this side owes the peer routes. The dead mark resets these streams alone
    /// ([`Self::reset_streams`]).
    pub fn open_streams(&self) -> Vec<super::OwedReset> {
        let mut streams = Vec::new();
        let mut view = SenderView::new();
        view.for_each_slot(&self.pages, |slot| streams.extend(slot.owed_reset()));
        streams
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
                self.state.open_slots.fetch_add(1, Ordering::Relaxed);
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
                return Ok(BindResult::Bound {
                    waker: AutoWake::default(),
                    release_bytes: 0,
                });
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
        match slot.allocate_and_open(binding_id, self.state.initial_recv_window) {
            Ok(super::slot::AllocateOutcome::Allocated) => {
                self.state.open_slots.fetch_add(1, Ordering::Relaxed);
                let slot_ptr = slot.as_ptr();
                let on_free = OnFree::Server {
                    path_entry: path_entry.clone(),
                    endpoint_tx: endpoint_tx.clone(),
                };
                let stream = StreamReceiver::new(slot_ptr, on_free.clone());
                let control = ControlReceiver::new(slot_ptr, on_free);
                Ok(BindResult::NewBinding {
                    waker: AutoWake::default(),
                    stream,
                    control,
                    release_bytes: 0,
                })
            }
            Ok(super::slot::AllocateOutcome::AlreadyBound) => Ok(BindResult::Bound {
                waker: AutoWake::default(),
                release_bytes: 0,
            }),
            // A concurrent recycle bumped the tombstone past `binding_id` after the
            // lock-free fast-path read passed — reject as stale.
            Ok(super::slot::AllocateOutcome::Stale) => Err(Error::StaleBinding(())),
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
    fn open_slot_counts_until_both_receivers_drop() {
        let (mut server, path_entry, mut tx, _rx) = test_server(10);
        assert!(!server.state.has_open_slot());

        // A slot bound by a QueueMsg init.
        let result = server.bind_for_msg(v(0), v(5), &path_entry, &mut tx);
        let Ok(BindResult::NewBinding {
            stream, control, ..
        }) = result
        else {
            panic!("expected NewBinding");
        };
        assert!(server.state.has_open_slot());
        drop(stream);
        assert!(
            server.state.has_open_slot(),
            "the control receiver still holds the slot"
        );
        drop(control);
        assert!(!server.state.has_open_slot());

        // A slot bound with its first stream entry.
        let result =
            server.bind_and_send_stream(v(1), v(1), make_stream_entry(), &path_entry, &mut tx);
        let Ok(BindResult::NewBinding {
            stream, control, ..
        }) = result
        else {
            panic!("expected NewBinding");
        };
        assert!(server.state.has_open_slot());
        drop(control);
        assert!(
            server.state.has_open_slot(),
            "the stream receiver still holds the slot"
        );
        drop(stream);
        assert!(!server.state.has_open_slot());
    }

    /// `open_streams` names the stream of each bound slot whose writer recorded the peer's
    /// queue, as a reset of it that the server owes the client routes, and none of a freed slot.
    #[test]
    fn open_streams_names_each_bound_stream_whose_peer_queue_is_recorded() {
        let (mut server, path_entry, mut tx, _rx) = test_server(10);
        let result = server.bind_for_msg(v(2), v(5), &path_entry, &mut tx);
        let Ok(BindResult::NewBinding {
            stream, control, ..
        }) = result
        else {
            panic!("expected NewBinding");
        };
        assert!(
            server.state.open_streams().is_empty(),
            "the client's queue isn't recorded yet"
        );

        // As the stream's writer does when it is made.
        control.set_peer_queue_id(v(8));
        assert_eq!(
            server.state.open_streams(),
            vec![crate::queue::OwedReset {
                queue_pair: crate::packet::datagram::QueuePair {
                    source_queue_id: v(2),
                    dest_queue_id: v(8),
                },
                binding_id: v(5),
            }]
        );

        drop((stream, control));
        assert!(
            server.state.open_streams().is_empty(),
            "a freed slot doesn't hold a stream"
        );
    }

    /// `reset_streams` resets the slot of each stream it names while the slot holds that
    /// stream's binding, and no other slot: one it doesn't name, as one bound after the dead mark
    /// read the open streams, and one bound again since the binding it names.
    #[test]
    fn reset_streams_resets_each_named_stream_and_no_other() {
        let (mut server, path_entry, mut tx, _rx) = test_server(10);
        let mut receivers = Vec::new();
        for (queue_id, binding_id) in [(0, 1), (1, 1), (2, 3)] {
            let Ok(BindResult::NewBinding {
                stream, control, ..
            }) = server.bind_for_msg(v(queue_id), v(binding_id), &path_entry, &mut tx)
            else {
                panic!("expected NewBinding");
            };
            receivers.push((stream, control));
        }
        let named = |queue_id, binding_id| crate::queue::OwedReset {
            queue_pair: crate::packet::datagram::QueuePair {
                source_queue_id: v(queue_id),
                dest_queue_id: v(8),
            },
            binding_id: v(binding_id),
        };

        // Slot 2 holds binding 3: the stream named with binding 2 is one it held before.
        server
            .state
            .reset_streams(&[named(0, 1), named(2, 2)], v(42), &mut |_| {});

        let mut depth = |queue_id| {
            let diag = server.slot_diag(v(queue_id)).expect("the slot exists");
            (diag.stream_depth, diag.control_depth)
        };
        assert_eq!(depth(0), (1, 1), "a named stream is reset");
        assert_eq!(depth(1), (0, 0), "a stream it doesn't name isn't reset");
        assert_eq!(depth(2), (0, 0), "a slot bound again since isn't reset");
        drop(receivers);
    }
}
