// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    credentials::{self, Credentials},
    endpoint::{
        id::{LocalSenderId, RecvDispatchWorkerId, RemoteSenderId},
        msg,
    },
    intrusive,
    path::{
        self,
        secret::map::{entry::QueueState, Entry as PathSecretEntry},
    },
    queue,
    socket::interface::{paired_ack_sender, paired_peer_interface, SendPlace},
    stream::endpoint::ack::state as ack_state,
    tracing::*,
};
use rustc_hash::FxHashMap;
use s2n_quic_core::{frame::ack::EcnCounts, inet::SocketAddress, varint::VarInt};
use std::{cell::RefCell, collections::hash_map, fmt, rc::Rc, sync::Arc};

// ── QueueView ─────────────────────────────────────────────────────────────

pub(crate) enum QueueView {
    Client(queue::ClientDispatch),
    Server(queue::ServerView),
}

impl QueueView {
    pub fn send_stream(
        &mut self,
        queue_id: VarInt,
        binding_id: VarInt,
        entry: intrusive::Entry<msg::Stream>,
    ) -> Result<(queue::AutoWake, u64), queue::Error<intrusive::Entry<msg::Stream>>> {
        match self {
            Self::Client(d) => d.send_stream(queue_id, binding_id, entry),
            Self::Server(d) => d.send_stream(queue_id, binding_id, entry),
        }
    }

    pub fn send_control(
        &mut self,
        queue_id: VarInt,
        binding_id: VarInt,
        entry: intrusive::Entry<msg::Control>,
    ) -> Result<queue::AutoWake, queue::Error<intrusive::Entry<msg::Control>>> {
        match self {
            Self::Client(d) => d.send_control(queue_id, binding_id, entry),
            Self::Server(d) => d.send_control(queue_id, binding_id, entry),
        }
    }

    /// Read-only diagnostic snapshot of the slot at `queue_id`. For logging why a delivery was
    /// rejected; see [`queue::slot::Slot::diag`].
    pub fn slot_diag(&mut self, queue_id: VarInt) -> Option<queue::slot::SlotDiag> {
        match self {
            Self::Client(d) => d.slot_diag(queue_id),
            Self::Server(d) => d.slot_diag(queue_id),
        }
    }

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
    ) -> Result<(queue::AutoWake, u64), queue::MsgError<E>> {
        match self {
            Self::Client(d) => d.send_msg(
                queue_id,
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
            ),
            Self::Server(d) => d.send_msg(
                queue_id,
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
            ),
        }
    }

    pub fn as_server_mut(&mut self) -> Option<&mut queue::ServerView> {
        match self {
            Self::Server(v) => Some(v),
            Self::Client(_) => None,
        }
    }

    pub fn as_client_mut(&mut self) -> Option<&mut queue::ClientDispatch> {
        match self {
            Self::Client(d) => Some(d),
            Self::Server(_) => None,
        }
    }
}

/// Errors returned by [`Cache::get_or_insert`].
pub(crate) enum CacheError {
    /// Path-secret lookup failed; `control_out` has been populated with an
    /// `UnknownPathSecret` control packet ready to be forwarded to the sender.
    PathSecretNotFound,
    /// The decrypt callback returned `None` (authentication / decryption failed).
    DecryptFailed,
    /// `post_authentication` failed: the key-id has already been seen (definite replay)
    /// or falls outside the receiver's replay window (possible replay / too old).
    ReplayDetected,
}

pub(crate) mod ack_ranges;
pub(crate) mod dedup;

/// ACK transmission state machine.
///
/// ```text
/// Idle → Scheduled (ack-eliciting packet received)
/// Scheduled → Flushed (submission sent to send worker)
/// Flushed → Idle (completion returned, no new data)
/// Flushed → FlushedStale (ack-eliciting packet received while completion is in flight)
/// FlushedStale → Scheduled (completion returned, needs re-flush)
/// ```
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AckState {
    /// No ACK pending and none in flight.
    #[default]
    Idle,
    /// ACK pending — new ack-eliciting packets have arrived and need acknowledgment.
    Scheduled,
    /// ACK submission is in the send pipeline. New packets update the shared state
    /// but don't produce another submission until the completion returns.
    Flushed,
    /// ACK completion is in flight and new ack-eliciting data arrived.
    FlushedStale,
}

impl AckState {
    s2n_quic_core::state::is!(is_scheduled, Scheduled);
    s2n_quic_core::state::is!(is_flushed, Flushed);
    s2n_quic_core::state::is!(is_flushed_stale, FlushedStale);

    s2n_quic_core::state::event! {
        /// An ack-eliciting packet was received.
        on_ack_eliciting(
            Idle | Scheduled => Scheduled,
            Flushed | FlushedStale => FlushedStale,
        );
        /// The ACK submission was sent to the send worker.
        on_flush(Scheduled => Flushed);
        /// ACK flush completion returned.
        ///
        /// If no new packets arrived while in flight, transition back to idle.
        /// If packets arrived (FlushedStale), transition to scheduled so the
        /// completion path can re-encode and resubmit.
        on_flush_complete(
            Flushed => Idle,
            FlushedStale => Scheduled,
        );
        /// Scheduled but nothing to encode — reset to idle.
        on_empty(Scheduled => Idle);
    }
}

pub(crate) struct AckBurstAdapter;

impl crate::intrusive::Adapter for AckBurstAdapter {
    type Value = RefCell<Context>;
    type Target = RefCell<Context>;
    type Pointer = Rc<RefCell<Context>>;

    unsafe fn links(value: *mut Self::Value) -> *mut intrusive::Links {
        core::ptr::addr_of_mut!((*(*value).as_ptr()).ack_burst)
    }

    unsafe fn target(value: *mut Self::Value) -> *mut Self::Target {
        value
    }

    fn as_ptr(ptr: &Self::Pointer) -> *const Self::Value {
        Rc::as_ptr(ptr)
    }

    fn into_raw(ptr: Self::Pointer) -> *mut Self::Value {
        Rc::into_raw(ptr) as *mut Self::Value
    }

    unsafe fn from_raw(ptr: *mut Self::Value) -> Self::Pointer {
        Rc::from_raw(ptr)
    }
}

/// Cached crypto state and ACK tracking for a peer.
///
/// Keyed by (credentials.id, source_sender_id) because ACK spaces and
/// deduplication windows are per-sender, not per-peer.
pub(crate) struct Context {
    pub path_entry: Arc<PathSecretEntry>,
    /// The remote peer's sender_id (from the packet routing info).
    /// Echoed back as `dest_sender_id` in outgoing ACK frames so the peer
    /// can route the ACK to its loss detection context.
    pub remote_sender_id: RemoteSenderId,
    // TODO: Support key rotation by maintaining multiple openers indexed by key_id.
    // Currently we only track the latest key, which means packets with old key_ids
    // after rotation will fail to decrypt. Need to maintain a small cache of recent
    // openers (e.g., HashMap<VarInt, Opener>) to handle in-flight packets during rotation.
    pub opener: crate::crypto::awslc::open::Application,
    /// The key_id this opener corresponds to
    pub current_key_id: VarInt,
    /// Sliding window for packet number deduplication.
    pub dedup_filter: dedup::StreamFilter,
    /// Lightweight ACK range tracker for the direct ACK path.
    pub ack_ranges: ack_ranges::AckRanges,
    /// Which local sender_id outgoing ACKs for this peer route through.
    pub local_sender_id: LocalSenderId,
    /// The place of each of the endpoint's send sockets among its interfaces, by sender, as
    /// the cache holds them, or `None` where the endpoint doesn't list interfaces. They order
    /// the move of the acknowledgements off `local_sender_id` while it doesn't deliver.
    pub send_places: Option<Arc<[SendPlace]>>,
    /// The senders of the interface for the endpoint's highest-priority traffic, as the cache
    /// holds them: the acknowledgements move to one of them, ahead of other interfaces, once no
    /// sender of the local interface of `local_sender_id` delivers.
    pub priority_senders: Option<Arc<[LocalSenderId]>>,
    /// Accumulated ECN counts for received packets, reported back to the sender
    /// in each ACK frame so the sender can validate ECN support and detect congestion.
    pub ecn_counts: EcnCounts,
    pub idle_wheel: crate::time::wheel::WheelLinks,
    pub created_at: crate::time::precision::Timestamp,
    pub ack_state: AckState,
    /// Timestamp when ack_state entered Flushed (submission sent to send pipeline).
    /// Used to measure how long the recv context is blocked waiting for ACK completion.
    pub flushed_at: Option<crate::time::precision::Timestamp>,
    /// Map from binding_id to allocated queue_id for this sender.
    /// Shared with queue handles so they can remove entries when closed.
    /// Cached queue dispatch view (client or server depending on role).
    pub queue_view: QueueView,
    /// Intrusive links for recv-worker pending-ACK burst queue membership.
    pub ack_burst: intrusive::Links,
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("path_secret_id", self.path_entry.id())
            .field("current_key_id", &self.current_key_id)
            .field("remote_sender_id", &self.remote_sender_id)
            .field("local_sender_id", &self.local_sender_id)
            .field("ack_state", &self.ack_state)
            .field("ack_ranges", &self.ack_ranges)
            .field("dedup_filter", &self.dedup_filter)
            .finish()
    }
}

impl Context {
    #[inline]
    pub fn key(&self) -> Key {
        Key {
            id: *self.path_entry.id(),
            remote_sender_id: self.remote_sender_id,
        }
    }

    #[inline]
    pub fn invariants(&self) {
        if cfg!(debug_assertions) {
            if self.ack_ranges.is_empty() {
                assert!(
                    self.ack_ranges.largest_recv_time().is_none(),
                    "largest_recv_time present while ack_ranges is empty"
                );
            } else {
                assert!(
                    self.ack_ranges.largest_recv_time().is_some(),
                    "ack_ranges has packets but largest_recv_time is missing"
                );
            }

            if self.ack_state.is_scheduled()
                || self.ack_state.is_flushed()
                || self.ack_state.is_flushed_stale()
            {
                assert!(
                    !self.ack_ranges.is_empty(),
                    "ack_state indicates pending/inflight ACK but no ranges are present"
                );
                assert!(
                    self.ack_ranges.largest_recv_time().is_some(),
                    "ack_state indicates pending/inflight ACK but largest_recv_time is missing"
                );
            }
        }
    }

    pub fn new(
        path_entry: Arc<PathSecretEntry>,
        remote_sender_id: RemoteSenderId,
        local_sender_id: LocalSenderId,
        opener: crate::crypto::awslc::open::Application,
        key_id: VarInt,
        now: crate::time::precision::Timestamp,
        queue_view: QueueView,
    ) -> Self {
        let idle_timeout = path_entry.idle_timeout();
        let mut idle_wheel = crate::time::wheel::WheelLinks::new();
        idle_wheel.target_time = Some(now + idle_timeout);

        Self {
            path_entry,
            remote_sender_id,
            opener,
            current_key_id: key_id,
            dedup_filter: Default::default(),
            ack_ranges: Default::default(),
            local_sender_id,
            send_places: None,
            priority_senders: None,
            ecn_counts: Default::default(),
            idle_wheel,
            created_at: now,
            ack_state: AckState::Idle,
            flushed_at: None,
            queue_view,
            ack_burst: intrusive::Links::new(),
        }
    }

    /// Sets the place of each of the endpoint's send sockets and the senders of the interface
    /// for its highest-priority traffic, which order the move of the acknowledgements off the
    /// context's sender while that sender doesn't deliver.
    pub fn with_ack_senders(
        mut self,
        send_places: Option<Arc<[SendPlace]>>,
        priority_senders: Option<Arc<[LocalSenderId]>>,
    ) -> Self {
        self.send_places = send_places;
        self.priority_senders = priority_senders;
        self
    }

    /// Returns the sender that the context's acknowledgements go to: its sender while that
    /// sender delivers, and otherwise the sender that `routing::delivering_ack_sender_id` picks
    /// by the senders' interfaces. A sender delivers as `PathSecretEntry::delivering_sender`
    /// reads it: while the routers prefer the senders the peer acknowledged since the path's
    /// lost pairs stopped delivering, a sender delivers only where the peer acknowledged it
    /// since then, and otherwise while its pair isn't marked lost.
    fn delivering_ack_sender(&self) -> LocalSenderId {
        let entry = &*self.path_entry;
        let places = self.send_places.as_deref();
        let place = |sender: LocalSenderId| places?.get(sender.as_usize()).copied();
        let peer_interface = |sender: LocalSenderId| {
            paired_peer_interface(
                entry.peer_data_addrs(),
                entry.peer_data_interfaces(),
                sender.as_usize(),
                place(sender),
            )
        };
        let prefer_delivered = entry.prefers_senders_delivering_since_lost();
        let delivers = |sender: LocalSenderId| {
            if prefer_delivered {
                entry.delivers_since_lost(sender)
            } else {
                !entry.is_sender_lost(sender)
            }
        };
        super::routing::delivering_ack_sender_id(
            self.local_sender_id,
            entry.socket_sender_count(),
            self.priority_senders.as_deref(),
            |sender| place(sender).map(|at| at.rank),
            peer_interface,
            delivers,
        )
    }

    /// Encode the current ACK state and produce a direct submission for the send worker.
    ///
    /// Only produces a submission when ack_state is Scheduled (new packets arrived
    /// since the last submission). Transitions to Flushed after submitting to enforce
    /// at-most-one-in-flight. When the completion returns, the recv worker checks
    /// whether ack_state went back to Scheduled (new packets arrived) and re-submits.
    ///
    /// Returns `None` if there are no ranges or an ACK is already in flight.
    pub fn encode_and_flush(
        &mut self,
        recv_worker_id: RecvDispatchWorkerId,
    ) -> Option<ack_state::Submission> {
        if !self.ack_state.is_scheduled() {
            return None;
        }

        let mtu = self.path_entry.max_datagram_size() as usize;
        let max_extra_len = mtu.saturating_sub(ack_ranges::PACKET_OVERHEAD);
        let Some(encoded) = self.ack_ranges.encode_ack(max_extra_len) else {
            let transition = self.ack_state.on_empty();
            debug_assert!(
                transition.is_ok(),
                "on_empty transition failed from Scheduled"
            );
            self.invariants();
            return None;
        };
        let Some(largest_recv_time) = self.ack_ranges.largest_recv_time() else {
            let transition = self.ack_state.on_empty();
            debug_assert!(
                transition.is_ok(),
                "on_empty transition failed from Scheduled"
            );
            self.invariants();
            return None;
        };

        let transition = self.ack_state.on_flush();
        debug_assert!(
            transition.is_ok(),
            "on_flush transition failed from Scheduled"
        );
        self.flushed_at = Some(largest_recv_time.into());
        self.invariants();

        Some(ack_state::Submission {
            largest_acknowledged: encoded.largest_acknowledged,
            ack_range: encoded.ack_range,
            extra_ranges: encoded.extra_ranges,
            ecn_counts: self.ecn_counts,
            largest_recv_time: largest_recv_time.into(),
            path_secret_entry: self.path_entry.clone(),
            // The acknowledgements go to this context's sender while its interface pair
            // delivers, and otherwise to a delivering sender, in the order of their interfaces.
            local_sender_id: self.delivering_ack_sender(),
            remote_sender_id: self.remote_sender_id,
            recv_worker_id,
        })
    }

    pub fn on_ack_completion(
        &mut self,
        recv_worker_id: RecvDispatchWorkerId,
    ) -> (Option<ack_state::Submission>, u64) {
        if !(self.ack_state.is_flushed() || self.ack_state.is_flushed_stale()) {
            warn!(
                ?self.ack_state,
                "ack completion observed while context is not in a flushed state"
            );
            self.invariants();
            return (None, 0);
        }
        let transition = self.ack_state.on_flush_complete();
        debug_assert!(
            transition.is_ok(),
            "on_flush_complete transition failed from Flushed/FlushedStale"
        );
        self.flushed_at = None;
        let ranges_culled = self.ack_ranges.on_completion();
        let submission = self.encode_and_flush(recv_worker_id);
        self.invariants();
        (submission, ranges_culled)
    }
}

/// Key for sender state lookup — keyed by peer identity (stable) + sender_id,
/// NOT by full Credentials (which includes the per-packet key_id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Key {
    pub id: credentials::Id,
    pub remote_sender_id: RemoteSenderId,
}

impl core::hash::Hash for Key {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.id.to_hash() ^ self.remote_sender_id.as_varint().as_u64());
    }
}

/// Per-worker sender state cache.
pub(crate) struct Cache {
    pub senders: FxHashMap<Key, Rc<RefCell<Context>>>,
    pub worker_id: RecvDispatchWorkerId,
    /// The senders of the interface that carries the endpoint's highest-priority traffic, which
    /// a peer's acknowledgements go to, where the endpoint has one.
    pub priority_senders: Option<Arc<[LocalSenderId]>>,
    /// The place of each of the endpoint's send sockets among its interfaces, by sender, or
    /// `None` where the endpoint doesn't list interfaces. It picks the sender of the
    /// acknowledgements of each new context, ahead of `priority_senders`.
    send_places: Option<Arc<[SendPlace]>>,
}

impl Cache {
    pub fn new(worker_id: RecvDispatchWorkerId) -> Self {
        Self {
            senders: FxHashMap::default(),
            worker_id,
            priority_senders: None,
            send_places: None,
        }
    }

    /// Sets the senders of the interface for the highest-priority traffic, which a peer's
    /// acknowledgements go to.
    pub fn with_priority_senders(mut self, priority_senders: Option<Vec<LocalSenderId>>) -> Self {
        self.priority_senders = priority_senders.map(Arc::from);
        self
    }

    /// Sets the place of each of the endpoint's send sockets, so that each new context sends its
    /// acknowledgements back along the interface pair the peer's packets came on.
    pub fn with_send_places(mut self, send_places: Option<Arc<[SendPlace]>>) -> Self {
        self.send_places = send_places;
        self
    }

    /// Look up an existing [`Context`] by `(credentials.id, remote_sender_id)`, or derive
    /// a new one on cache miss.
    ///
    /// `decrypt` is invoked with the opener (cached on hit, freshly derived on miss).  If
    /// it returns `None` the whole call fails with [`CacheError::DecryptFailed`] and **no**
    /// new entry is inserted into the cache.
    ///
    /// On a cache miss, after a successful decrypt,
    /// [`post_authentication`](crate::path::secret::receiver::State::post_authentication)
    /// is called via `path_secret_map`.  This records the `key_id` as seen in the
    /// receiver's replay window, preventing a replayed initial packet from poisoning
    /// the cache with a stale path-secret entry.  If `post_authentication` detects a
    /// replay the call fails with [`CacheError::ReplayDetected`] without inserting the
    /// context.
    ///
    /// On a cache hit `post_authentication` is **not** called because many packets
    /// legitimately share the same `key_id`; per-packet replay protection is handled
    /// separately by the `dedup_filter` inside the returned [`Context`].
    #[inline]
    pub fn get_or_insert<Clk, Route, F, R>(
        &mut self,
        credentials: &Credentials,
        remote_sender_id: RemoteSenderId,
        path_secret_map: &path::secret::map::Map,
        clock: &Clk,
        peer_addr: SocketAddress,
        control_out: &mut Vec<u8>,
        route: &Route,
        decrypt: F,
    ) -> Result<(R, Rc<RefCell<Context>>, bool), CacheError>
    where
        Clk: crate::time::precision::Clock + ?Sized,
        Route: super::routing::SenderRoute,
        F: FnOnce(
            &crate::crypto::awslc::open::Application,
            &mut QueueView,
            &Arc<PathSecretEntry>,
        ) -> Option<R>,
    {
        let key = Key {
            id: credentials.id,
            remote_sender_id,
        };

        match self.senders.entry(key) {
            hash_map::Entry::Occupied(mut entry) => {
                let ctx = entry.get().clone();
                let cached_key_id = ctx.borrow().current_key_id;

                if credentials.key_id == cached_key_id {
                    ctx.borrow().invariants();
                    let mut borrow = ctx.borrow_mut();
                    let ctx_ref = &mut *borrow;
                    let path_entry = ctx_ref.path_entry.clone();
                    let r = decrypt(&ctx_ref.opener, &mut ctx_ref.queue_view, &path_entry)
                        .ok_or(CacheError::DecryptFailed)?;
                    drop(borrow);
                    return Ok((r, ctx, true));
                }

                if credentials.key_id < cached_key_id {
                    // The incoming key_id is older than what we already have —
                    // this is a stale or replayed packet. Send a control error
                    // and reject.
                    path_secret_map
                        .check_dedup(
                            &ctx.borrow().path_entry,
                            credentials,
                            Some(remote_sender_id.as_varint()),
                            control_out,
                        )
                        .map_err(|_| CacheError::ReplayDetected)?;

                    // If check_dedup somehow accepted it (shouldn't happen for
                    // an older key_id), still reject since we can't decrypt with
                    // the wrong opener.
                    return Err(CacheError::ReplayDetected);
                }

                // key_id > cached: the peer advanced (e.g. stale-key recovery).
                // Derive a fresh opener and decrypt before touching the cache.
                let (opener, path_entry) = path_secret_map
                    .opener_for_credentials(
                        credentials,
                        Some(remote_sender_id.as_varint()),
                        crate::path::secret::map::store::ControlResponse::ReturnBuffer {
                            out: control_out,
                        },
                    )
                    .ok_or(CacheError::PathSecretNotFound)?;

                let r = {
                    let mut borrow = ctx.borrow_mut();
                    let ctx_ref = &mut *borrow;
                    decrypt(&opener, &mut ctx_ref.queue_view, &ctx_ref.path_entry)
                        .ok_or(CacheError::DecryptFailed)?
                };

                path_secret_map
                    .check_dedup(
                        &path_entry,
                        credentials,
                        Some(remote_sender_id.as_varint()),
                        control_out,
                    )
                    .map_err(|_| CacheError::ReplayDetected)?;

                // Packet is authentic — replace the entry with a fresh context.
                // Key advancement means the peer abandoned its old sender context
                // (e.g., idle timeout recreation), so the old PN space is dead.
                // Transfer queue_view since it's independent of PN space.
                debug!(
                    %credentials,
                    %remote_sender_id,
                    cached_key_id = cached_key_id.as_u64(),
                    new_key_id = credentials.key_id.as_u64(),
                    "recv cache key_id advanced — replacing entry"
                );

                let dest_sender_id = ack_sender_id(
                    self.send_places.as_deref(),
                    self.priority_senders.as_deref(),
                    &path_entry,
                    peer_addr,
                    remote_sender_id,
                    route,
                );

                // Take queue_view from old context — it was already used by decrypt
                // and is independent of the PN space being replaced.
                let queue_view = {
                    let mut old = ctx.borrow_mut();
                    let old_ref = &mut *old;
                    // Replace with a dummy Client view; the old ctx is about to be dropped.
                    let state = old_ref.path_entry.queue_state();
                    let replacement = match state {
                        QueueState::Client(s) => {
                            QueueView::Client(queue::ClientDispatch::new(s.clone()))
                        }
                        QueueState::Server(s) => QueueView::Server(s.view()),
                    };
                    core::mem::replace(&mut old_ref.queue_view, replacement)
                };

                let new_ctx = Rc::new(RefCell::new(
                    Context::new(
                        path_entry,
                        remote_sender_id,
                        dest_sender_id,
                        opener,
                        credentials.key_id,
                        clock.now(),
                        queue_view,
                    )
                    .with_ack_senders(self.send_places.clone(), self.priority_senders.clone()),
                ));
                new_ctx.borrow().invariants();
                entry.insert(new_ctx.clone());

                Ok((r, new_ctx, false))
            }
            hash_map::Entry::Vacant(entry) => {
                debug!(%credentials, %peer_addr, sender_id = %remote_sender_id, recv_worker_id = %self.worker_id, "deriving opener for credentials");

                let (opener, path_entry) = path_secret_map
                    .opener_for_credentials(
                        credentials,
                        Some(remote_sender_id.as_varint()),
                        crate::path::secret::map::store::ControlResponse::ReturnBuffer {
                            out: control_out,
                        },
                    )
                    .ok_or(CacheError::PathSecretNotFound)?;

                let mut queue_view = match path_entry.queue_state() {
                    QueueState::Client(state) => {
                        QueueView::Client(queue::ClientDispatch::new(state.clone()))
                    }
                    QueueState::Server(state) => QueueView::Server(state.view()),
                };

                let r = decrypt(&opener, &mut queue_view, &path_entry)
                    .ok_or(CacheError::DecryptFailed)?;

                // Record the key_id as seen in the receiver's replay window.  This prevents
                // a replayed initial packet from establishing a poisoned cache entry.
                // Only called on miss: the key_id is registered once when the session is
                // first established; subsequent packets (same key_id, different packet
                // numbers) are deduplicated by the Context's `dedup_filter`.
                path_secret_map
                    .check_dedup(
                        &path_entry,
                        credentials,
                        Some(remote_sender_id.as_varint()),
                        control_out,
                    )
                    .map_err(|_| CacheError::ReplayDetected)?;

                let dest_sender_id = ack_sender_id(
                    self.send_places.as_deref(),
                    self.priority_senders.as_deref(),
                    &path_entry,
                    peer_addr,
                    remote_sender_id,
                    route,
                );

                let ctx = Rc::new(RefCell::new(
                    Context::new(
                        path_entry,
                        remote_sender_id,
                        dest_sender_id,
                        opener,
                        credentials.key_id,
                        clock.now(),
                        queue_view,
                    )
                    .with_ack_senders(self.send_places.clone(), self.priority_senders.clone()),
                ));
                ctx.borrow().invariants();
                entry.insert(ctx.clone());
                Ok((r, ctx, false))
            }
        }
    }

    pub fn remove(&mut self, key: &Key) {
        self.senders.remove(key);
    }

    pub fn invalidate_by_id(&mut self, id: &credentials::Id) {
        let before = self.senders.len();
        self.senders.retain(|key, _| key.id != *id);
        let removed = before - self.senders.len();
        debug!(%id, removed, worker_id = %self.worker_id, "invalidating recv contexts");
    }
}

/// Returns the sender that sends the acknowledgements of the peer's sender `remote_sender_id`,
/// whose packets come from `peer_addr`.
///
/// Where the endpoint lists interfaces and the peer tags its data addresses, the sender is on
/// the interface paired with the one the packets came from, so that the acknowledgements go
/// back along the pair the packets came on and don't cross another interface, which can be
/// lost while that pair delivers. Where the peer has more interfaces than the endpoint and no
/// local interface sends to the one the packets came from, the sender is on the local interface
/// the packets arrived at. Otherwise it is the sender that `routing::ack_sender_id` picks from
/// the one `route` chooses: a sender of the interface for the endpoint's highest-priority
/// traffic, where `priority_senders` lists that interface's senders.
fn ack_sender_id<Route: super::routing::SenderRoute>(
    send_places: Option<&[SendPlace]>,
    priority_senders: Option<&[LocalSenderId]>,
    path_entry: &PathSecretEntry,
    peer_addr: SocketAddress,
    remote_sender_id: RemoteSenderId,
    route: &Route,
) -> LocalSenderId {
    send_places
        .and_then(|places| {
            paired_ack_sender(
                path_entry.peer_data_addrs(),
                path_entry.peer_data_interfaces(),
                places,
                peer_addr,
                remote_sender_id.as_varint().as_u64() as usize,
            )
        })
        .and_then(|sender| VarInt::new(sender as u64).ok())
        .map(LocalSenderId::new)
        .unwrap_or_else(|| {
            super::routing::ack_sender_id(
                route.sender_id_for_ack(remote_sender_id),
                remote_sender_id,
                priority_senders,
            )
        })
}

crate::context_wheel_adapter!(IdleWheelAdapter, Context, idle_wheel);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::{
        id::Id,
        routing::{ModuloRoute, SenderRoute},
    };
    use s2n_quic_core::endpoint::Type;
    use std::net::SocketAddr;

    /// A map of path secrets that holds one entry, for `peer`, and that entry's credential id.
    fn map_with_peer(peer: SocketAddr) -> (path::secret::map::Map, credentials::Id) {
        let map = path::secret::map::testing::new(16);
        map.test_stop_cleaner();
        map.test_insert(peer, Type::Server);
        let id = *map
            .get_raw(peer)
            .expect("the map holds the peer's entry")
            .id();
        (map, id)
    }

    /// Passes a packet of `key_id` from the peer's sender `remote_sender_id` through `cache`,
    /// and returns the context that the cache then holds for that sender, and whether the
    /// cache held that context before the packet.
    fn receive(
        cache: &mut Cache,
        map: &path::secret::map::Map,
        peer: SocketAddr,
        id: credentials::Id,
        key_id: u8,
        remote_sender_id: RemoteSenderId,
        route: &ModuloRoute,
    ) -> (Rc<RefCell<Context>>, bool) {
        let credentials = Credentials {
            id,
            key_id: VarInt::from_u8(key_id),
        };
        let clock = crate::time::testing::Clock::new(core::time::Duration::ZERO);
        let mut control_out = Vec::new();
        let ((), ctx, hit) = cache
            .get_or_insert(
                &credentials,
                remote_sender_id,
                map,
                &clock,
                peer.into(),
                &mut control_out,
                route,
                |_opener, _queue_view, _entry| Some(()),
            )
            .unwrap_or_else(|_| panic!("the cache takes the packet"));
        (ctx, hit)
    }

    /// Passes a packet of key id 0 and then one of key id 1 from the peer's sender 6 through a
    /// cache with `priority_senders`, checks that the second packet replaces the context, and
    /// returns the sender that each context sends the peer's acknowledgements to.
    fn ack_senders_across_a_key_id_advance(
        priority_senders: Option<Vec<LocalSenderId>>,
    ) -> (LocalSenderId, LocalSenderId) {
        let peer: SocketAddr = "127.0.0.1:4433".parse().unwrap();
        let (map, id) = map_with_peer(peer);
        // Four senders: the route sends the acknowledgements of the peer's sender 6 to sender 2.
        let route = ModuloRoute::new(4);
        let remote_sender_id = RemoteSenderId::new(VarInt::from_u8(6));
        assert_eq!(
            route.sender_id_for_ack(remote_sender_id),
            LocalSenderId::from_index(2)
        );
        let mut cache =
            Cache::new(RecvDispatchWorkerId::new(0)).with_priority_senders(priority_senders);

        let (derived, hit) = receive(&mut cache, &map, peer, id, 0, remote_sender_id, &route);
        assert!(!hit, "the first packet derives a context");

        let (replaced, hit) = receive(&mut cache, &map, peer, id, 1, remote_sender_id, &route);
        assert!(!hit, "a packet of a newer key id replaces the context");
        assert!(!Rc::ptr_eq(&derived, &replaced));
        assert_eq!(replaced.borrow().current_key_id, VarInt::from_u8(1));
        let key = Key {
            id,
            remote_sender_id,
        };
        assert!(Rc::ptr_eq(&cache.senders[&key], &replaced));

        let derived_sender = derived.borrow().local_sender_id;
        let replaced_sender = replaced.borrow().local_sender_id;
        (derived_sender, replaced_sender)
    }

    #[test]
    fn a_context_replaced_at_a_key_id_advance_acknowledges_on_the_highest_priority_interface() {
        // Senders 1 and 3 are on the interface for the highest-priority traffic. The route's
        // sender 2 isn't, so the acknowledgements of the peer's sender 6 go to the priority
        // sender at position 6 % 2 = 0, sender 1.
        let priority = vec![LocalSenderId::from_index(1), LocalSenderId::from_index(3)];
        let (derived, replaced) = ack_senders_across_a_key_id_advance(Some(priority));
        assert_eq!(derived, LocalSenderId::from_index(1));
        assert_eq!(replaced, LocalSenderId::from_index(1));
    }

    #[test]
    fn a_context_replaced_at_a_key_id_advance_keeps_the_routed_sender_without_priority_senders() {
        let (derived, replaced) = ack_senders_across_a_key_id_advance(None);
        assert_eq!(derived, LocalSenderId::from_index(2));
        assert_eq!(replaced, LocalSenderId::from_index(2));
    }

    /// Schedules `ctx` to acknowledge the packets it holds, and returns the sender that the
    /// acknowledgements it then submits go to.
    fn submission_sender(ctx: &mut Context) -> LocalSenderId {
        ctx.ack_state = AckState::Scheduled;
        ctx.encode_and_flush(RecvDispatchWorkerId::new(0))
            .expect("the context holds a packet to acknowledge")
            .local_sender_id
    }

    #[test]
    fn acknowledgements_leave_a_sender_whose_pair_is_lost() {
        let peer: SocketAddr = "127.0.0.1:4433".parse().unwrap();
        let map = path::secret::map::testing::new(16);
        map.test_stop_cleaner();
        // The peer's entry keeps a slot for each of four senders.
        map.set_socket_sender_count(4);
        map.test_insert(peer, Type::Server);
        let id = *map
            .get_raw(peer)
            .expect("the map holds the peer's entry")
            .id();
        // The route sends the acknowledgements of the peer's sender 6 to sender 2.
        let route = ModuloRoute::new(4);
        let remote_sender_id = RemoteSenderId::new(VarInt::from_u8(6));
        let mut cache = Cache::new(RecvDispatchWorkerId::new(0));
        let (ctx, _hit) = receive(&mut cache, &map, peer, id, 0, remote_sender_id, &route);
        let mut ctx = ctx.borrow_mut();
        let sender = LocalSenderId::from_index;
        assert_eq!(ctx.local_sender_id, sender(2));
        assert_eq!(ctx.path_entry.socket_sender_count(), 4);
        let now = unsafe {
            s2n_quic_core::time::Timestamp::from_duration(core::time::Duration::from_millis(1))
        };
        ctx.ack_ranges.on_packet_received(VarInt::from_u8(0), now);

        assert_eq!(submission_sender(&mut ctx), sender(2));
        // While the pair of sender 2 is marked lost, the acknowledgements go to the next sender
        // whose pair delivers, past each sender whose pair is marked lost too.
        assert!(ctx.path_entry.set_sender_lost(sender(2), true));
        assert_eq!(submission_sender(&mut ctx), sender(3));
        assert!(ctx.path_entry.set_sender_lost(sender(3), true));
        assert_eq!(submission_sender(&mut ctx), sender(0));
        // Once the pair of sender 2 delivers again, the acknowledgements go back to it, and the
        // context keeps its sender throughout.
        assert!(ctx.path_entry.set_sender_lost(sender(2), false));
        assert_eq!(submission_sender(&mut ctx), sender(2));
        assert_eq!(ctx.local_sender_id, sender(2));
    }

    #[test]
    fn acknowledgements_leave_a_lost_sender_for_a_sender_of_the_same_peer_interface() {
        let local: SocketAddr = "10.0.1.1:4433".parse().unwrap();
        let peer: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        // The peer's data addresses: two on its interface 1 and two on its interface 2. The
        // endpoint doesn't list interfaces, so its sender `n` sends to the address at `n`.
        let peer_data_addrs: [SocketAddr; 4] = [
            "10.0.0.1:4000".parse().unwrap(),
            "10.0.0.1:4001".parse().unwrap(),
            "10.0.0.2:4000".parse().unwrap(),
            "10.0.0.2:4001".parse().unwrap(),
        ];
        let peer_data_interfaces: [Option<u16>; 4] = [Some(1), Some(1), Some(2), Some(2)];

        let map = path::secret::map::testing::new(16);
        map.test_stop_cleaner();
        // The peer's entry keeps a slot for each of four senders.
        map.set_socket_sender_count(4);
        let peer_map = path::secret::map::testing::new(16);
        peer_map.test_stop_cleaner();
        let id = map
            .test_insert_pair(
                PathSecretEntry::builder(peer)
                    .local(local)
                    .peer_data_addrs(peer_data_addrs)
                    .peer_data_interfaces(peer_data_interfaces),
                &peer_map,
                PathSecretEntry::builder(local).local(peer),
            )
            .local;

        // The route sends the acknowledgements of the peer's sender 5 to sender 1, which sends
        // to the peer's interface 1.
        let route = ModuloRoute::new(4);
        let remote_sender_id = RemoteSenderId::new(VarInt::from_u8(5));
        let mut cache = Cache::new(RecvDispatchWorkerId::new(0));
        let source = peer_data_addrs[1];
        let (ctx, _hit) = receive(&mut cache, &map, source, id, 0, remote_sender_id, &route);
        let mut ctx = ctx.borrow_mut();
        let sender = LocalSenderId::from_index;
        assert_eq!(ctx.local_sender_id, sender(1));
        assert_eq!(ctx.path_entry.socket_sender_count(), 4);
        let now = unsafe {
            s2n_quic_core::time::Timestamp::from_duration(core::time::Duration::from_millis(1))
        };
        ctx.ack_ranges.on_packet_received(VarInt::from_u8(0), now);
        assert_eq!(submission_sender(&mut ctx), sender(1));

        // While the pair of sender 1 is marked lost, the acknowledgements go to sender 0, which
        // sends to the same peer interface, ahead of sender 2, the next sender by slot.
        assert!(ctx.path_entry.set_sender_lost(sender(1), true));
        assert_eq!(submission_sender(&mut ctx), sender(0));
        // Once no sender to that peer interface delivers, they go to the next sender by slot.
        assert!(ctx.path_entry.set_sender_lost(sender(0), true));
        assert_eq!(submission_sender(&mut ctx), sender(2));
        // While the routers prefer the senders the peer acknowledged since the pairs were lost,
        // sender 3 alone delivers.
        let at = |nanos| crate::time::precision::Timestamp { nanos };
        ctx.path_entry.set_senders_lost_since(at(200));
        ctx.path_entry.set_senders_preferred_until(at(1_000));
        ctx.path_entry.record_sender_delivered(sender(3), at(300));
        assert!(ctx.path_entry.prefers_senders_delivering_since_lost());
        assert_eq!(submission_sender(&mut ctx), sender(3));
        assert_eq!(ctx.local_sender_id, sender(1));
    }

    #[test]
    fn acknowledgements_leave_a_lost_local_interface_for_the_flagged_interface() {
        let local: SocketAddr = "10.0.1.1:4433".parse().unwrap();
        let peer: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        // The peer's data addresses, one on each of its interfaces 1 to 3.
        let peer_data_addrs: [SocketAddr; 3] = [
            "10.0.0.1:4000".parse().unwrap(),
            "10.0.0.2:4000".parse().unwrap(),
            "10.0.0.3:4000".parse().unwrap(),
        ];
        let peer_data_interfaces: [Option<u16>; 3] = [Some(1), Some(2), Some(3)];

        let map = path::secret::map::testing::new(16);
        map.test_stop_cleaner();
        // The peer's entry keeps a slot for each of six senders.
        map.set_socket_sender_count(6);
        let peer_map = path::secret::map::testing::new(16);
        peer_map.test_stop_cleaner();
        let id = map
            .test_insert_pair(
                PathSecretEntry::builder(peer)
                    .local(local)
                    .peer_data_addrs(peer_data_addrs)
                    .peer_data_interfaces(peer_data_interfaces),
                &peer_map,
                PathSecretEntry::builder(local).local(peer),
            )
            .local;

        // Six send sockets on three interfaces: sockets 0 and 3 on the first, 1 and 4 on the
        // second, and 2 and 5 on the third, which carries the highest-priority traffic.
        let places: Arc<[SendPlace]> = (0..6)
            .map(|socket| SendPlace {
                rank: socket % 3,
                count: 3,
                index: socket / 3,
            })
            .collect();
        let priority = vec![LocalSenderId::from_index(2), LocalSenderId::from_index(5)];

        // The peer's sender 6 sends from its interface 1, the pair of the first interface, so
        // the acknowledgements go to sender 0.
        let source = peer_data_addrs[0];
        let route = ModuloRoute::new(6);
        let remote_sender_id = RemoteSenderId::new(VarInt::from_u8(6));
        let mut cache = Cache::new(RecvDispatchWorkerId::new(0))
            .with_priority_senders(Some(priority))
            .with_send_places(Some(places.clone()));
        let (ctx, _hit) = receive(&mut cache, &map, source, id, 0, remote_sender_id, &route);
        let mut ctx = ctx.borrow_mut();
        let sender = LocalSenderId::from_index;
        assert_eq!(ctx.local_sender_id, sender(0));
        assert_eq!(ctx.send_places, Some(places));
        assert_eq!(
            ctx.priority_senders.as_deref(),
            Some(&[sender(2), sender(5)][..])
        );
        let now = unsafe {
            s2n_quic_core::time::Timestamp::from_duration(core::time::Duration::from_millis(1))
        };
        ctx.ack_ranges.on_packet_received(VarInt::from_u8(0), now);
        assert_eq!(submission_sender(&mut ctx), sender(0));

        // While the pair of sender 0 is marked lost, the acknowledgements go to sender 3, of
        // the same interface and pair, ahead of senders 1 and 2.
        assert!(ctx.path_entry.set_sender_lost(sender(0), true));
        assert_eq!(submission_sender(&mut ctx), sender(3));
        // Once no sender of the first interface delivers, they go to sender 2, of the interface
        // for the highest-priority traffic, ahead of sender 1, the next sender by slot.
        assert!(ctx.path_entry.set_sender_lost(sender(3), true));
        assert_eq!(submission_sender(&mut ctx), sender(2));
    }

    #[test]
    fn a_context_replaced_at_a_key_id_advance_acknowledges_on_the_paired_interface() {
        let local: SocketAddr = "10.0.1.1:4433".parse().unwrap();
        let peer: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        // The peer's data addresses: two on its interface 1 and two on its interface 2.
        let peer_data_addrs: [SocketAddr; 4] = [
            "10.0.0.1:4000".parse().unwrap(),
            "10.0.0.1:4001".parse().unwrap(),
            "10.0.0.2:4000".parse().unwrap(),
            "10.0.0.2:4001".parse().unwrap(),
        ];
        let peer_data_interfaces: [Option<u16>; 4] = [Some(1), Some(1), Some(2), Some(2)];

        let map = path::secret::map::testing::new(16);
        map.test_stop_cleaner();
        let peer_map = path::secret::map::testing::new(16);
        peer_map.test_stop_cleaner();
        let id = map
            .test_insert_pair(
                PathSecretEntry::builder(peer)
                    .local(local)
                    .peer_data_addrs(peer_data_addrs)
                    .peer_data_interfaces(peer_data_interfaces),
                &peer_map,
                PathSecretEntry::builder(local).local(peer),
            )
            .local;
        let entry = map.get_by_id(&id).expect("the map holds the peer's entry");

        // Four send sockets on two interfaces: sockets 0 and 2 on the first, 1 and 3 on the
        // second.
        let place = |rank, index| SendPlace {
            rank,
            count: 2,
            index,
        };
        let places: Arc<[SendPlace]> =
            vec![place(0, 0), place(1, 0), place(0, 1), place(1, 1)].into();
        // The first interface carries the highest-priority traffic.
        let priority = vec![LocalSenderId::from_index(0), LocalSenderId::from_index(2)];

        // The peer's sender 6 sends from the first address of its interface 2. Four senders
        // route that sender's acknowledgements to sender 2, a sender of the first interface,
        // which the flagged-interface rule keeps. The paired sender is on the second interface:
        // the first of senders 1 and 3, as the address is the first of its interface.
        let source = peer_data_addrs[2];
        let route = ModuloRoute::new(4);
        let remote_sender_id = RemoteSenderId::new(VarInt::from_u8(6));
        let paired = paired_ack_sender(
            entry.peer_data_addrs(),
            entry.peer_data_interfaces(),
            &places,
            source.into(),
            6,
        )
        .map(LocalSenderId::from_index)
        .expect("a packet from a tagged address is paired");
        assert_eq!(paired, LocalSenderId::from_index(1));
        let routed = route.sender_id_for_ack(remote_sender_id);
        assert_ne!(paired, routed);
        assert!(priority.contains(&routed));

        let mut cache = Cache::new(RecvDispatchWorkerId::new(0))
            .with_priority_senders(Some(priority))
            .with_send_places(Some(places.clone()));

        let (derived, hit) = receive(&mut cache, &map, source, id, 0, remote_sender_id, &route);
        assert!(!hit, "the first packet derives a context");
        assert_eq!(derived.borrow().local_sender_id, paired);

        // The peer's sender keeps its socket across a key id advance, so the packet of the new
        // key comes from the same address.
        let (replaced, hit) = receive(&mut cache, &map, source, id, 1, remote_sender_id, &route);
        assert!(!hit, "a packet of a newer key id replaces the context");
        assert!(!Rc::ptr_eq(&derived, &replaced));
        assert_eq!(replaced.borrow().current_key_id, VarInt::from_u8(1));
        let key = Key {
            id,
            remote_sender_id,
        };
        assert!(Rc::ptr_eq(&cache.senders[&key], &replaced));
        assert_eq!(replaced.borrow().local_sender_id, paired);
        // The new context keeps the cache's send places and priority senders, which order the
        // move of its acknowledgements off a sender marked lost.
        assert_eq!(replaced.borrow().send_places, Some(places));
        assert_eq!(
            replaced.borrow().priority_senders.as_deref(),
            Some(&[LocalSenderId::from_index(0), LocalSenderId::from_index(2)][..])
        );
    }
}
