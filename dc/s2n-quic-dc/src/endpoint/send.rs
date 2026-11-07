// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-peer send context: crypto state, congestion control, and inflight tracking.
//!
//! Each send socket maintains a map from credentials ID to a Context. The Context owns
//! the sealer for encryption, the CCA for pacing/windowing, and the inflight map for
//! tracking sent packets. Packet assembly (packing multiple frames into a segment,
//! encrypting, and registering in the inflight map) happens here.
//!
//! Contexts participate in three independent timing wheels via separate intrusive links:
//!
//! - **Transmission wheel**: fires at CCA pacing intervals to assemble and send packets.
//! - **PTO wheel**: fires at RTT-multiples for probe timeout / tail loss recovery.
//! - **Idle wheel**: fires after prolonged silence to reclaim resources.

use crate::{
    congestion,
    counter::QueueGauge,
    credentials::{self, Credentials},
    endpoint::{
        combinator::HasId,
        frame::{self, Frame, Priority},
        id::{LocalSendSocketId, LocalSenderId},
    },
    intrusive::{self, Queue},
    msg::segment,
    path::secret::map::Entry as PathSecretEntry,
    socket::channel::{ByteCost, UnboundedSender},
    stream::endpoint::{inflight, msg},
    time::precision,
    tracing::*,
};
use core::time::Duration;
use rustc_hash::FxHashMap;
use s2n_quic_core::{
    frame::ack::EcnCounts,
    packet::number::{PacketNumber, PacketNumberSpace},
    path::INITIAL_PTO_BACKOFF,
    random,
    recovery::RttEstimator,
    time::Timestamp,
    varint::VarInt,
};
use s2n_quic_platform::features::Gso;
use std::{cell::RefCell, rc::Rc, sync::Arc};

#[cfg(test)]
mod tests;

/// Pending frame queue with an integrated wire-cost counter.
///
/// This struct ensures that `byte_cost` always mirrors the true accumulated
/// [`ByteCost`] of every frame in the queue. All mutations (push/pop) go through
/// this type, so callers cannot accidentally desync the counter.
pub(crate) struct PendingFrames {
    queue: Queue<Frame>,
    /// Accumulated wire cost of all frames currently in the queue.
    ///
    /// Wire cost = payload bytes + header metadata bytes (type tag + routing
    /// varints + optional payload-length varint) for every frame.
    byte_cost: usize,
    gauge: QueueGauge,
}

impl PendingFrames {
    #[inline]
    pub fn new(gauge: QueueGauge) -> Self {
        Self {
            queue: Queue::new(),
            byte_cost: 0,
            gauge,
        }
    }

    /// Push a frame onto the back of the queue, updating the cost counter.
    #[inline]
    pub fn push_back(&mut self, frame: intrusive::Entry<Frame>) {
        debug_assert!(
            !matches!(frame.header, frame::Header::Ack { .. }),
            "Ack frames must use pending_acks, not the priority queues"
        );
        self.byte_cost += frame.byte_cost() as usize;
        self.gauge.enqueue(1);
        self.queue.push_back(frame);
    }

    /// Push a frame onto the front of the queue, updating the cost counter.
    ///
    /// Only call this with a frame that was just removed via [`pop_front`] — for
    /// example when a frame does not fit in the current segment and must be
    /// returned for the next assembly round. Calling this with a frame that was
    /// *not* previously popped will double-count its wire cost.
    #[inline]
    pub fn push_front(&mut self, frame: intrusive::Entry<Frame>) {
        debug_assert!(
            !matches!(frame.header, frame::Header::Ack { .. }),
            "Ack frames must use pending_acks, not the priority queues"
        );
        self.byte_cost += frame.byte_cost() as usize;
        self.gauge.enqueue(1);
        self.queue.push_front(frame);
    }

    /// Remove the next frame from the front of the queue, updating the cost counter.
    #[inline]
    pub fn pop_front(&mut self) -> Option<intrusive::Entry<Frame>> {
        let frame = self.queue.pop_front()?;
        let cost = frame.byte_cost() as usize;
        debug_assert!(
            self.byte_cost >= cost,
            "byte_cost underflow: counter={} frame_cost={}",
            self.byte_cost,
            cost
        );
        self.byte_cost = self.byte_cost.saturating_sub(cost);
        self.gauge.dequeue();
        Some(frame)
    }

    /// Returns the number of frames in the queue.
    #[inline]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Returns `true` if the queue contains no frames.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Returns the accumulated wire cost of all frames currently in the queue.
    #[inline]
    pub fn byte_cost(&self) -> usize {
        self.byte_cost
    }

    /// Append frames from a pre-split queue in O(1).
    ///
    /// `byte_cost` is the pre-computed wire cost for all frames in `queue`, including
    /// per-packet overhead. Callers should use the values returned by
    /// `FrameBatch::into_split` directly.
    #[inline]
    pub fn append_queue(&mut self, mut queue: Queue<Frame>, byte_cost: u64) {
        let count = queue.len() as u64;
        self.byte_cost += byte_cost as usize;
        self.gauge.enqueue(count);
        self.queue.append(&mut queue);
    }
}

/// Pending ACK submissions with integrated queue gauge.
pub(crate) struct PendingAcks {
    queue: Queue<msg::Sender>,
    gauge: QueueGauge,
}

impl PendingAcks {
    pub fn new(gauge: QueueGauge) -> Self {
        Self {
            queue: Queue::new(),
            gauge,
        }
    }

    #[inline]
    pub fn push_back(&mut self, entry: intrusive::Entry<msg::Sender>) {
        self.gauge.enqueue(1);
        self.queue.push_back(entry);
    }

    #[inline]
    pub fn push_front(&mut self, entry: intrusive::Entry<msg::Sender>) {
        self.gauge.enqueue(1);
        self.queue.push_front(entry);
    }

    #[inline]
    pub fn pop_front(&mut self) -> Option<intrusive::Entry<msg::Sender>> {
        let entry = self.queue.pop_front()?;
        self.gauge.dequeue();
        Some(entry)
    }

    #[inline]
    pub fn take(&mut self) -> Queue<msg::Sender> {
        let v = core::mem::take(&mut self.queue);
        self.gauge.dequeue_n(v.len() as _);
        v
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

/// Tracks RTT sampling for ACK-only (read-heavy) sends.
///
/// When there is no data in the inflight map, RTT samples cannot be obtained from
/// normal ACK processing because no ack-eliciting packets are outstanding. This
/// tracker records send times so the RTT estimator can be updated when the peer
/// acknowledges one of those packets.
///
/// Two slots mirror the design in `s2n_quic_core::ack::transmission::Set`:
///
/// - **`stable`** — the *oldest* ack-eliciting ACK-only transmission that has not yet
///   been acknowledged. Set once and held fixed until the peer ACKs it or loss is
///   declared. Acts as a reliable fallback RTT sample if newer sends are dropped.
///
/// - **`latest`** — the *most recent* tracked ACK-only transmission. Updated on every
///   send (ack-eliciting or not) while a probe is in-flight, so the RTT sample is
///   as fresh as possible when the peer ACKs a range covering it.
///
/// **ACK loop prevention**: after a sample is consumed, the `sampled` flag is set to
/// prevent issuing another ack-eliciting probe until [`clear`] is called (which
/// happens when data enters the inflight map). This ensures that in a truly idle
/// connection, the two sides do not ping-pong ack-eliciting packets indefinitely,
/// which would reset the idle timer and prevent the connection from timing out.
///
/// The assembler gates `is_ack_eliciting` on `!is_pending()`. Once `stable` is set
/// (a probe is in-flight), subsequent ACK-only sends are not ack-eliciting but still
/// call [`on_non_eliciting_sent`] to keep `latest` fresh. When the probe is
/// acknowledged, `sampled = true` suppresses further probing until `clear()`.
///
/// The tracker is also cleared whenever a data-carrying packet is inserted into
/// the inflight map, because those packets will produce RTT samples on their own.
#[derive(Debug)]
pub(crate) struct AckRttTracker {
    /// Oldest pending ack-eliciting ACK-only transmission.
    /// Set once; not replaced until acknowledged or declared lost.
    stable: Option<(VarInt, Timestamp)>,
    /// Most recently tracked ACK-only transmission (ack-eliciting or not).
    /// Updated while a probe is in-flight to keep the sample as fresh as possible.
    latest: Option<(VarInt, Timestamp)>,
    /// Set to `true` after a sample has been successfully consumed. Prevents
    /// re-probing until [`clear`] is called, breaking any potential ACK loop.
    ///
    /// Starts `true` so that a brand-new context doesn't probe on its first ACK
    /// (the first data send will produce an RTT sample naturally). Reset to `false`
    /// by [`clear`] when data enters inflight.
    sampled: bool,
}

impl Default for AckRttTracker {
    fn default() -> Self {
        Self {
            stable: None,
            latest: None,
            sampled: true,
        }
    }
}

impl AckRttTracker {
    /// Returns `true` when an ack-eliciting ACK-only probe is in-flight (`stable`
    /// is set) **or** when we have just consumed a sample and are in the cooldown
    /// period (`sampled` is set).
    ///
    /// The assembler gates `make_ack_eliciting` on `!is_pending()`, so at most one
    /// ack-eliciting probe is outstanding at a time and re-probing is suppressed
    /// until new data flows through the inflight map.
    #[inline]
    pub fn is_pending(&self) -> bool {
        self.stable.is_some() || self.sampled
    }

    /// Called just before an ack-eliciting ACK-only packet is sent.
    ///
    /// Updates `latest` and sets `stable` if not already set (so `stable` always
    /// refers to the oldest unacknowledged ack-eliciting transmission). Clears the
    /// `sampled` flag to begin a new probe cycle.
    #[inline]
    pub fn on_sent(&mut self, packet_number: VarInt, time_sent: Timestamp) {
        self.sampled = false;
        self.latest = Some((packet_number, time_sent));
        if self.stable.is_none() {
            self.stable = Some((packet_number, time_sent));
        }
    }

    /// Called for non-ack-eliciting ACK-only sends when a probe is already in-flight.
    ///
    /// Keeps `latest` pointing at the most recently sent PN so that if the peer
    /// ACKs a range covering it (e.g. as part of a future combined ACK), we return
    /// the freshest possible RTT sample. No-op when no probe is in-flight (`stable`
    /// is `None`), since there is nothing to compare against.
    #[inline]
    pub fn on_non_eliciting_sent(&mut self, packet_number: VarInt, time_sent: Timestamp) {
        if self.stable.is_some() {
            self.latest = Some((packet_number, time_sent));
        }
    }

    /// Suppress further probing until [`clear`] is called.
    ///
    /// Called when inflight drains to empty after an ACK that already produced an
    /// RTT sample via the normal data path. Prevents the immediately following
    /// ACK-only send from redundantly probing.
    #[inline]
    pub fn suppress(&mut self) {
        self.sampled = true;
    }

    /// Clear all slots and reset the `sampled` flag.
    ///
    /// Called when a data-carrying packet is inserted into the inflight map.
    /// In that case the tracker is no longer needed because RTT samples will
    /// arrive via the normal inflight ACK path. Clearing `sampled` allows the
    /// tracker to probe again the next time inflight becomes empty.
    #[inline]
    pub fn clear(&mut self) {
        self.stable = None;
        self.latest = None;
        self.sampled = false;
    }

    /// Check whether a single ACK range covers either the `latest` or `stable` PN.
    ///
    /// `start` and `end` are the inclusive VarInt bounds of the acknowledged range.
    /// Loss detection is handled separately by [`on_ack_done`], which must be called
    /// after all ranges in an ACK frame have been processed.
    ///
    /// **When `latest` is covered:** returns its `time_sent`, clears both slots, and
    /// sets `sampled = true`. This gives the freshest possible RTT sample.
    ///
    /// **When `stable` is covered (but `latest` is not):** returns `stable`'s
    /// `time_sent`, advances `stable ← latest`, and sets `sampled = true`. This
    /// handles the case where the most recent send was dropped but an older one was
    /// acknowledged.
    #[inline]
    pub fn check_range(&mut self, start: VarInt, end: VarInt) -> Option<Timestamp> {
        // Check latest first — fresher sample is preferable.
        if let Some((pn, time_sent)) = self.latest {
            if pn >= start && pn <= end {
                // Latest ACKed → clear both, mark sampled, return the fresh sample.
                self.stable = None;
                self.latest = None;
                self.sampled = true;
                return Some(time_sent);
            }
        }

        // Fall back to stable.
        if let Some((pn, time_sent)) = self.stable {
            if pn >= start && pn <= end {
                // Stable ACKed → advance stable to latest so the next ACK
                // can produce a fresher sample; mark sampled.
                self.stable = self.latest.take();
                self.sampled = true;
                return Some(time_sent);
            }
        }

        None
    }

    /// Declare loss for any tracked slots that were not covered by any range in the
    /// current ACK frame.
    ///
    /// Must be called **once** after all ACK ranges have been processed with
    /// [`check_range`]. Using `largest_acknowledged` (the overall largest PN in the
    /// ACK frame) as the loss signal: if a slot's PN is strictly less than
    /// `largest_acknowledged` and was not acknowledged by any range, it is
    /// considered lost.
    ///
    /// When `latest` is lost it is cleared. When `stable` is lost it is advanced to
    /// `latest` (which may also be `None`). The `sampled` flag is deliberately not
    /// set on loss — the packet was never acknowledged so we should be free to probe
    /// again on the next send.
    #[inline]
    pub fn on_ack_done(&mut self, largest_acknowledged: VarInt) {
        if let Some((pn, _)) = self.latest {
            if largest_acknowledged > pn {
                self.latest = None;
            }
        }
        if let Some((pn, _)) = self.stable {
            if largest_acknowledged > pn {
                // Advance stable to whatever latest holds (may be None).
                self.stable = self.latest.take();
            }
        }
    }
}

#[must_use = "WheelInterest must be dispatched; ignoring it silently skips wheel scheduling"]
#[derive(Clone, Copy, Debug)]
pub struct WheelInterest {
    pub immediate: bool,
    pub transmission: bool,
    pub pto: bool,
    pub idle_timeout: bool,
}

impl WheelInterest {
    pub const NONE: Self = Self {
        immediate: false,
        transmission: false,
        pto: false,
        idle_timeout: false,
    };
}

pub trait WheelRoutable {
    type Output;
    fn split(self) -> ((Rc<RefCell<Context>>, WheelInterest), Self::Output);
}

impl WheelRoutable for (Rc<RefCell<Context>>, WheelInterest) {
    type Output = ();
    #[inline]
    fn split(self) -> ((Rc<RefCell<Context>>, WheelInterest), ()) {
        (self, ())
    }
}

impl<T> WheelRoutable for (Rc<RefCell<Context>>, WheelInterest, T) {
    type Output = T;
    #[inline]
    fn split(self) -> ((Rc<RefCell<Context>>, WheelInterest), T) {
        ((self.0, self.1), self.2)
    }
}

/// A `Receiver<I::Output>` that takes `WheelRoutable` items from an inner receiver,
/// dispatches each context into the appropriate timing wheel senders, and forwards
/// the remaining output downstream.
pub struct WheelRouter<I, R, ImmW, TxW, PtoW, IdleW> {
    inner: R,
    immediate: ImmW,
    tx_wheel: TxW,
    pto_wheel: PtoW,
    idle_wheel: IdleW,
    _item: core::marker::PhantomData<fn() -> I>,
}

impl<I, R, ImmW, TxW, PtoW, IdleW> WheelRouter<I, R, ImmW, TxW, PtoW, IdleW> {
    pub fn new(
        inner: R,
        immediate: ImmW,
        tx_wheel: TxW,
        pto_wheel: PtoW,
        idle_wheel: IdleW,
    ) -> Self {
        Self {
            inner,
            immediate,
            tx_wheel,
            pto_wheel,
            idle_wheel,
            _item: core::marker::PhantomData,
        }
    }
}

impl<I, R, ImmW, TxW, PtoW, IdleW> crate::socket::channel::Receiver<I::Output>
    for WheelRouter<I, R, ImmW, TxW, PtoW, IdleW>
where
    I: WheelRoutable,
    R: crate::socket::channel::Receiver<I>,
    ImmW: UnboundedSender<Rc<RefCell<Context>>>,
    TxW: UnboundedSender<Rc<RefCell<Context>>>,
    PtoW: UnboundedSender<Rc<RefCell<Context>>>,
    IdleW: UnboundedSender<Rc<RefCell<Context>>>,
{
    fn poll_recv(
        &mut self,
        cx: &mut core::task::Context<'_>,
        budget: &mut crate::socket::channel::Budget,
    ) -> core::task::Poll<Option<I::Output>> {
        let item = core::task::ready!(self.inner.poll_recv(cx, budget));
        let Some(item) = item else {
            return core::task::Poll::Ready(None);
        };

        let ((context, interest), output) = item.split();

        if interest.idle_timeout {
            let _ = self.idle_wheel.send(context.clone());
        }
        if interest.pto {
            let _ = self.pto_wheel.send(context.clone());
        }
        if interest.immediate {
            let _ = self.immediate.send(context.clone());
        }
        if interest.transmission {
            let _ = self.tx_wheel.send(context);
        }

        core::task::Poll::Ready(Some(output))
    }

    fn on_consumed(&mut self, bytes: u64) {
        self.inner.on_consumed(bytes);
    }
}

/// PTO probe state for a send context.
///
/// The assembler checks this on every assembly round:
/// - `Idle`: no probe pending; perform normal immediate + pending drain.
/// - `Requested`: a probe must be sent. If `pending` data is present it serves as the
///   ack-eliciting packet (CWND is bypassed per RFC 9002 §6.2.4). Otherwise the assembler
///   retransmits frames from the oldest non-shell inflight entry under a new packet number,
///   linking the two via `inflight::Packet::probed_to`. After an ack-eliciting packet is
///   successfully encoded, the assembler calls `on_transmit()` to transition back to `Idle`.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProbeState {
    /// No probe pending.
    #[default]
    Idle,
    /// Two probe segments remain. Set when a PTO fires — we always emit 2
    /// ack-eliciting packets so the PN-threshold (2) can detect genuine loss.
    ProbeTwice,
    /// One probe segment remains (the first was already transmitted).
    ProbeOnce,
}

impl ProbeState {
    s2n_quic_core::state::is!(
        /// Returns `true` when at least one probe segment is pending.
        is_requested, ProbeTwice | ProbeOnce
    );
    s2n_quic_core::state::is!(
        /// Returns `true` when exactly one probe segment remains.
        is_probe_once, ProbeOnce
    );

    s2n_quic_core::state::event! {
        /// Transition to ProbeTwice when a PTO fires. No-op if already ProbeTwice.
        request(Idle | ProbeOnce | ProbeTwice => ProbeTwice);
        /// Transition after the assembler transmits an ack-eliciting probe segment.
        on_transmit(ProbeTwice => ProbeOnce, ProbeOnce => Idle);
        /// Transition when all inflight data is ACKed before the probe fires.
        on_all_acked(ProbeTwice => Idle, ProbeOnce => Idle);
    }

    #[cfg(test)]
    pub fn dot_test() {
        insta::assert_snapshot!(Self::dot());
    }
}

/// Per-peer send state, one per (credentials_id, send_socket) pair.
///
/// Holds crypto material, congestion control, inflight tracking, and the pending frame
/// queues. Frames are pushed in by the Dispatcher, then `assemble()` is called when the
/// local wheel fires to pack them into encrypted packets.
///
/// Data frames are held in a priority-indexed array of queues, all subject to CWND gating.
/// ACKs are handled separately via `pending_acks` (direct path, bypasses CWND).
pub(crate) struct Context {
    pub path_secret_entry: Arc<PathSecretEntry>,
    pub sealer: crate::crypto::awslc::seal::Application,
    pub credentials: Credentials,
    /// Resolved destination address for this sender (cached at context creation).
    pub peer_addr: std::net::SocketAddr,
    /// Next packet number to assign
    pub next_packet_number: VarInt,
    pub cca: congestion::Controller,
    pub rtt_estimator: RttEstimator,
    pub inflight: inflight::Map,
    pub pto: Pto,
    /// Per-priority frame queues.  Index 0 (`Priority::Ack`) bypasses CWND; indices 1–N
    /// are subject to congestion-window gating and are drained highest-priority first.
    pub queues: [PendingFrames; Priority::LEVELS],
    /// Pending direct ACK submissions from recv dispatch workers.
    pub pending_acks: PendingAcks,
    /// Pending QueueFree emission token. At most one per peer at any time
    /// (enforced by FreedInner::in_flight). Consumed by the assembler's
    /// QueueFree phase.
    pub pending_freed: Option<crate::intrusive::Entry<crate::stream::endpoint::msg::Sender>>,
    /// Index of this socket in the path secret entry's `sender_load_scores` array.
    ///
    /// Used by `publish_sender_load_score` to write the correct slot so the
    /// load-balancer pick-two logic has up-to-date per-socket load information.
    pub sender_idx: LocalSenderId,
    /// Intrusive links for the immediate transmission queue (ACKs, probes)
    pub tx_immediate: crate::intrusive::Links,
    /// Intrusive links for the wheel→assembler ready queue.
    ///
    /// Set once the context has expired off the `tx_wheel` and is queued, ready-now,
    /// for paced assembly. Kept distinct from `tx_wheel` so that "waiting in the wheel
    /// for a future EDT" and "already drained, queued for assembly" are independent,
    /// unambiguous states rather than two meanings of one shared link.
    pub tx_assembly: crate::intrusive::Links,
    /// Intrusive links and target time for the transmission pacing wheel
    pub tx_wheel: WheelLinks,
    /// Intrusive links and target time for the PTO (probe timeout) wheel
    pub pto_wheel: WheelLinks,
    /// Intrusive links and target time for the idle timeout wheel
    pub idle_wheel: WheelLinks,
    /// Last-seen ECN counts from peer ACK frames; used to compute deltas for the CCA.
    pub peer_ecn_counts: EcnCounts,
    pub created_at: precision::Timestamp,
    /// Last time this send context observed peer activity (ACK receipt).
    pub last_peer_activity: precision::Timestamp,
    /// Set when the context is invalidated (removed from cache). Prevents wheels
    /// from rearming after expiry.
    pub invalidated: bool,
    /// The next packet number of this context when its interface pair was marked lost, or
    /// `None` while the pair delivers. An acknowledgement of that packet or a later one
    /// clears the mark.
    pub lost_from: Option<VarInt>,
    /// When this context began to wait for an acknowledgement of a packet it sent, or `None`
    /// while the peer has acknowledged every packet it sent.
    pub ack_wait_since: Option<precision::Timestamp>,
    /// When [`Self::mark_path_lost`] marked this context's interface pair lost, or `None`
    /// while the pair delivers or was marked lost without a time.
    pub lost_at: Option<precision::Timestamp>,
    /// When this context sent an ack-eliciting packet that the inflight map doesn't hold, such
    /// as an ACK-only probe or a Ping, with no acknowledgement from the peer since, or `None`.
    /// [`Self::has_stopped_delivering`] reads it where the inflight map is empty.
    pub ack_only_probe_sent: Option<precision::Timestamp>,
    /// Whether this context sent an ACK-only packet that isn't ack-eliciting since the peer
    /// last acknowledged one of its packets: it sends acknowledgements alone.
    pub ack_only_since_peer_ack: bool,
    /// When this context last sent an ACK-only packet while its inflight map was empty, or
    /// `None` where it hasn't sent one. [`Self::check_acknowledgement_only_delivery`] reads it.
    pub ack_only_sent: Option<precision::Timestamp>,
    /// Whether the endpoint's send sockets are on more than one interface, so that another
    /// interface pair can carry this context's traffic once its pair stops delivering. Only
    /// then does a context that sends acknowledgements alone probe a quiet peer, and mark its
    /// pair lost once the probe waits too long: an endpoint with one interface has no other
    /// pair to move the traffic to, and a lost pair goes on probing the peer.
    pub spreads_over_interfaces: bool,
    /// RTT sampler for ACK-only (read-heavy) sends.
    ///
    /// When `inflight` is empty we have no ack-eliciting packets in flight and
    /// can no longer get RTT samples through the normal ACK path. This tracker
    /// periodically makes an ACK-only packet ack-eliciting so that the peer
    /// responds and we can measure the round-trip time.
    pub rtt_tracker: AckRttTracker,
}

impl crate::socket::channel::ByteCost for Context {
    #[inline]
    fn byte_cost(&self) -> u64 {
        self.queues.iter().map(|q| q.byte_cost() as u64).sum()
    }
}

#[derive(Debug)]
pub enum ContextError {
    PeerDataAddrsEmpty,
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PeerDataAddrsEmpty => write!(f, "peer data addrs list is empty"),
        }
    }
}

impl Context {
    /// Dump this send context's congestion/flow state for a `QueueDbg` frame passing through a
    /// send-pipeline hop (see the `queue-dbg` feature). `phase` names the hop ("assemble", "probe",
    /// "acked", "lost"); `pn` is the relevant packet number for that hop (the one being assigned,
    /// acked, or lost). Stamped with the shared `dump_id` plus the `cred_id`/`binding_id`
    /// correlation key so the line joins the end-to-end trace. The body is gated by
    /// [`crate::endpoint::dbg::on_enabled`], so it is inert in a production build.
    ///
    /// `now` is the time of this hop. `enqueued_at` is when the marker entered the pipeline
    /// (always set for `QueueDbg` frames) and `time_sent` is when the packet carrying it was last
    /// put on the wire (`None` until the frame is assembled). From these two the dump derives:
    ///
    /// * `sojourn_us` — `now - enqueued_at`: total time since the marker was submitted. At the
    ///   `assemble`/`probe` hop this is how long it waited in the transmit queue; at the `acked`
    ///   hop it is the full end-to-end submit-to-ack latency.
    /// * `since_sent_us` — `now - time_sent`: wire latency of the acked/lost transmission (the
    ///   round-trip for the `acked` hop). `None` at the assemble hops, where nothing is on the
    ///   wire yet.
    pub(crate) fn dump_queue_dbg(
        &self,
        dump_id: VarInt,
        queue_pair: crate::packet::datagram::QueuePair,
        binding_id: VarInt,
        phase: &'static str,
        pn: VarInt,
        now: precision::Timestamp,
        enqueued_at: Option<precision::Timestamp>,
        time_sent: Option<precision::Timestamp>,
    ) {
        crate::endpoint::dbg::on_enabled(|| {
            use crate::endpoint::frame::Priority;
            let q = &self.queues;
            let range = self.inflight.get_range();
            ::tracing::info!(
                dump_id = dump_id.as_u64(),
                phase,
                %self.credentials,
                sender_idx = %self.sender_idx,
                peer = %self.peer_addr,
                binding_id = binding_id.as_u64(),
                dest_queue_id = queue_pair.dest_queue_id.as_u64(),
                source_queue_id = queue_pair.source_queue_id.as_u64(),
                pn = pn.as_u64(),
                // ── sojourn timing ──
                // How long the marker has been in the pipeline, and (on ack/loss) the wire latency
                // of the transmission that just resolved. See the method docs.
                sojourn_us = ?enqueued_at.map(|e| now.duration_since(e).as_micros() as u64),
                since_sent_us = ?time_sent.map(|t| now.duration_since(t).as_micros() as u64),
                // ── why can't it transmit? the Phase-3 gates ──
                is_congestion_limited = self.cca.is_congestion_limited(),
                requires_fast_retransmission = self.cca.requires_fast_retransmission(),
                can_send_pending_frames = self.can_send_pending_frames(),
                cwnd = self.cca.congestion_window(),
                bytes_in_flight = self.cca.bytes_in_flight(),
                send_quantum = self.cca.send_quantum(),
                pacing_rate_bps = self.cca.bandwidth().as_bytes_per_second(),
                // ── pacer departure timing (pacer-runaway diagnosis) ──
                // The raw CCA earliest-departure-time (unclamped) vs. when the tx wheel is actually
                // armed to fire. A raw EDT far in the future is the pacer-rate-collapse signature.
                cca_earliest_departure_time = ?self.cca.earliest_departure_time_raw(),
                tx_wheel_target = ?self.tx_wheel.target_time,
                // ── scheduling: which wheels is it on? ──
                is_tx_scheduled = self.is_tx_scheduled(),
                is_queued_for_assembly = self.is_queued_for_assembly(),
                is_immediate_scheduled = self.is_immediate_scheduled(),
                is_pto_scheduled = self.is_pto_scheduled(),
                // ── pending work per priority ──
                pending_reset = q[Priority::QueueReset.as_index()].len(),
                pending_control = q[Priority::QueueControl.as_index()].len(),
                pending_data = q[Priority::QueueData.as_index()].len(),
                pending_init = q[Priority::QueueInit.as_index()].len(),
                has_pending_acks = self.has_pending_acks(),
                pending_freed = self.pending_freed.is_some(),
                // ── inflight + pto + rtt ──
                has_inflight = self.inflight.has_inflight(),
                inflight_range_start = range.start().as_u64(),
                inflight_range_end = range.end().as_u64(),
                inflight_sum_bytes = self.inflight.sum_sent_bytes(),
                pto_backoff = self.pto.backoff,
                pto_is_armed = self.pto.is_armed(),
                probe_requested = self.pto.probe_state.is_requested(),
                probe_state = ?self.pto.probe_state,
                smoothed_rtt_us = self.rtt_estimator.smoothed_rtt().as_micros(),
                next_packet_number = self.next_packet_number.as_u64(),
                "QueueDbg send-pipeline dump"
            );
        });
    }

    pub fn new(
        entry: &Arc<PathSecretEntry>,
        inflight_gauge: QueueGauge,
        ack_gauge: QueueGauge,
        pending_gauge: QueueGauge,
        sender_idx: LocalSenderId,
        clock: &impl precision::Clock,
    ) -> Result<Self, ContextError> {
        Self::new_paired(
            entry,
            inflight_gauge,
            ack_gauge,
            pending_gauge,
            sender_idx,
            None,
            clock,
        )
    }

    /// Builds a context whose peer address is the one `place` pairs with the sender's interface,
    /// where the endpoint lists interfaces and the peer tags its addresses. [`Self::new`] builds
    /// one without a place, which keeps the address the sender took before.
    pub fn new_paired(
        entry: &Arc<PathSecretEntry>,
        inflight_gauge: QueueGauge,
        ack_gauge: QueueGauge,
        pending_gauge: QueueGauge,
        sender_idx: LocalSenderId,
        place: Option<crate::socket::interface::SendPlace>,
        clock: &impl precision::Clock,
    ) -> Result<Self, ContextError> {
        let (sealer, credentials) = entry.reusable_sealer();
        let cca = congestion::Controller::new(entry.max_datagram_size());
        let rtt_estimator = RttEstimator::new(Duration::from_millis(2));
        let inflight = inflight::Map::new(inflight_gauge);

        let addrs = entry.peer_data_addrs();
        if addrs.is_empty() {
            return Err(ContextError::PeerDataAddrsEmpty);
        }
        // Where the endpoint and the peer both list interfaces, the sender sends to the peer's
        // interface that its own is paired with; otherwise it keeps the address it took before.
        let peer_addr = crate::socket::interface::paired_peer_addr(
            addrs,
            entry.peer_data_interfaces(),
            usize::from(sender_idx),
            place,
        );

        ::tracing::debug!(%credentials, %peer_addr, %sender_idx, "deriving sealer for credentials");

        // A new context of this sender slot starts out delivering, whatever an earlier context
        // of the slot recorded.
        let _ = entry.set_sender_lost(sender_idx, false);

        Ok(Self {
            path_secret_entry: entry.clone(),
            sealer,
            credentials,
            peer_addr,
            next_packet_number: VarInt::ZERO,
            cca,
            rtt_estimator,
            inflight,
            pto: Pto::default(),
            queues: core::array::from_fn(|_| PendingFrames::new(pending_gauge.clone())),
            pending_acks: PendingAcks::new(ack_gauge),
            pending_freed: None,
            sender_idx,
            tx_immediate: crate::intrusive::Links::new(),
            tx_assembly: crate::intrusive::Links::new(),
            tx_wheel: WheelLinks::new(),
            pto_wheel: WheelLinks::new(),
            idle_wheel: WheelLinks::new(),
            peer_ecn_counts: EcnCounts::default(),
            created_at: clock.now(),
            last_peer_activity: clock.now(),
            invalidated: false,
            lost_from: None,
            ack_wait_since: None,
            lost_at: None,
            ack_only_probe_sent: None,
            ack_only_since_peer_ack: false,
            ack_only_sent: None,
            spreads_over_interfaces: place.is_some_and(|place| place.count > 1),
            rtt_tracker: AckRttTracker::default(),
        })
    }

    /// Append all frames from a batch and return wheel interest indicating which wheels
    /// need this context inserted.
    ///
    /// Each priority level is appended in O(1) via an intrusive list splice.
    pub fn push_batch<Clk: precision::Clock + ?Sized>(
        &mut self,
        batch: super::combinator::FrameBatch,
        clock: &Clk,
    ) -> WheelInterest {
        let (queues, byte_costs) = batch.into_queues();
        for (queue, (slot, cost)) in queues
            .into_iter()
            .zip(self.queues.iter_mut().zip(byte_costs))
        {
            if !queue.is_empty() {
                slot.append_queue(queue, cost);
            }
        }
        // Refresh the load score immediately so pick-two sees the updated backlog.
        let now: s2n_quic_core::time::Timestamp = clock.now().into();
        self.publish_sender_load_score(now);
        let interest = self.wheel_interest(clock);
        self.invariants();
        interest
    }

    /// Process an inbound ACK against this context's inflight state.
    ///
    /// The first range (`ack_range..=largest_acknowledged`) and ECN counts come from
    /// the header. Additional gap/range pairs are in `extra_ranges` (often empty).
    /// Acknowledged frames go to `completed`, retransmittable lost frames to `lost`,
    /// and cancelled/expired frames to `cancelled`.
    pub fn process_ack<Clk, Rand>(
        &mut self,
        largest_acknowledged: VarInt,
        ack_range: VarInt,
        extra_ranges: &[u8],
        ecn_counts: EcnCounts,
        ack_delay: Duration,
        counters: &super::counters::Send,
        completed: &mut impl UnboundedSender<intrusive::Entry<Frame>>,
        lost: &mut impl UnboundedSender<intrusive::Entry<Frame>>,
        cancelled: &mut impl UnboundedSender<intrusive::Entry<Frame>>,
        clock: &Clk,
        random: &mut Rand,
        deferred: &mut Vec<PacketNumber>,
    ) -> WheelInterest
    where
        Clk: s2n_quic_core::time::Clock + precision::Clock + ?Sized,
        Rand: random::Generator,
    {
        self.on_peer_acknowledgement(clock.now(), largest_acknowledged);

        super::ack::process_ack(
            largest_acknowledged,
            ack_range,
            extra_ranges,
            ecn_counts,
            ack_delay,
            self,
            counters,
            completed,
            lost,
            cancelled,
            clock,
            random,
            deferred,
        );

        let interest = self.wheel_interest(clock);
        self.invariants();
        interest
    }

    /// Returns true if this send context has not received any peer activity
    /// within the idle timeout window.
    ///
    /// Checks both the send-specific `last_peer_activity` (ACK receipt) and the
    /// shared path entry's `last_activity` (any inbound packet, including gossip
    /// heartbeats). Either signal is sufficient to keep the context alive.
    #[inline]
    pub fn is_peer_idle(&self, now: precision::Timestamp) -> bool {
        let timeout = self.path_secret_entry.idle_timeout().as_nanos() as u64;
        let send_elapsed = now.nanos_since(self.last_peer_activity);
        if send_elapsed < timeout {
            return false;
        }
        let path_elapsed = now.nanos_since(self.path_secret_entry.last_activity());
        path_elapsed >= timeout
    }

    /// Compute wheel interest after a state change
    pub(crate) fn wheel_interest<Clk>(&mut self, clock: &Clk) -> WheelInterest
    where
        Clk: precision::Clock + ?Sized,
    {
        if self.invalidated {
            return WheelInterest::NONE;
        }

        if !self.inflight.has_inflight() {
            self.ack_wait_since = None;
        } else if self.ack_wait_since.is_none() {
            self.ack_wait_since = Some(clock.now());
        }

        // A lost pair keeps its probe timeout armed while no packet waits for an
        // acknowledgement, so that it is probed until one shows that it delivers again.
        if !self.inflight.has_inflight() && !self.is_path_lost() {
            self.pto.on_ack_received(false);
            self.pto_wheel.target_time = None;
        }

        debug_assert!(
            !self.pto.probe_state.is_requested()
                || self.inflight.has_inflight()
                || self.has_pending()
                || self.is_path_lost(),
            "probe_state is Requested but nothing to probe with"
        );

        let needs_urgent = self.has_pending_acks()
            || self.pending_freed.is_some()
            || self.pto.probe_state.is_requested();

        // Urgent work (ACK to send or PTO probe) always routes to the immediate
        // queue to bypass EDT pacing. This avoids putting ACKs in the "slow lane"
        // behind the tx_wheel, regardless of whether the context is already
        // tx-scheduled.
        let immediate = needs_urgent && !self.is_immediate_scheduled();

        // Only insert into the tx_wheel when the context is neither already
        // waiting in the wheel nor already queued for assembly. Both links must
        // be clear: an intrusive node can live in only one list at a time, and a
        // double-insert would trip the list's unlinked assertion / corrupt it.
        let transmission = if !self.is_tx_scheduled()
            && !self.is_queued_for_assembly()
            && self.has_pending_data()
            && self.can_send_pending_frames()
        {
            // Non-urgent pending data follows CCA pacing via the tx_wheel. The
            // earliest-departure-time is clamped to MAX_TX_PACING_DELAY inside the accessor,
            // so a degenerate pacing rate cannot park the wheel days out.
            let target = self
                .cca
                .earliest_departure_time(clock)
                .map(precision::Timestamp::from);
            // If target time is `None` then the wheel will schedule it immediately
            self.tx_wheel.target_time = target;
            true
        } else {
            false
        };

        let pto =
            if !self.is_pto_scheduled() && (self.inflight.has_inflight() || self.is_path_lost()) {
                if let Some(target) = self.pto.next_target(clock, &self.rtt_estimator) {
                    self.pto_wheel.target_time = Some(target);
                    true
                } else {
                    false
                }
            } else {
                false
            };

        let idle_timeout = if !self.is_idle_scheduled() {
            let timeout = self.path_secret_entry.idle_timeout();
            let target = clock.now() + timeout;
            self.idle_wheel.target_time = Some(target);
            true
        } else {
            false
        };

        let interest = WheelInterest {
            immediate,
            transmission,
            pto,
            idle_timeout,
        };

        self.invariants();
        interest
    }

    /// Pop the next pending frame, draining from highest priority first.
    #[inline]
    pub fn pop_pending(&mut self) -> Option<intrusive::Entry<Frame>> {
        for queue in &mut self.queues {
            if let Some(frame) = queue.pop_front() {
                return Some(frame);
            }
        }
        None
    }

    /// Push a frame back to the front of whichever priority queue it belongs to.
    ///
    /// Only call this with a frame just removed via [`pop_pending`].
    #[inline]
    pub fn push_front_frame(&mut self, frame: intrusive::Entry<Frame>) {
        self.queues[frame.priority().as_index()].push_front(frame);
    }

    /// Push a frame to the back of whichever priority queue it belongs to.
    #[inline]
    pub fn push_back_frame(&mut self, frame: intrusive::Entry<Frame>) {
        self.queues[frame.priority().as_index()].push_back(frame);
    }

    /// Publish the sender load score to the path secret entry.
    ///
    /// The score is a composite estimate of how busy this sender is:
    ///
    ///   score = base + queued_bytes / pacing_rate
    ///
    /// where `base = max(now, earliest_departure_time) + congestion_penalty`.
    ///
    /// Using `earliest_departure_time` as the floor means BBR pacing information is
    /// incorporated directly — a sender that is pacing-gated looks more loaded than one
    /// that is idle.  The congestion penalty (one smoothed RTT when cwnd-limited) steers
    /// new batches toward senders that still have room in their congestion window.
    ///
    /// Call this whenever the pending queue or CCA state changes (on send, on ACK, and
    /// on enqueue).
    #[inline]
    pub fn publish_sender_load_score(&self, now: s2n_quic_core::time::Timestamp) {
        use crate::socket::channel::ByteCost as _;
        let total_cost = self.byte_cost() as usize;

        // Use earliest_departure_time as the base if it is in the future so that
        // pacing-limited senders appear more loaded than idle ones. The accessor clamps the EDT
        // to MAX_TX_PACING_DELAY, so a degenerate pacing rate cannot inflate the load score to a
        // far-future timestamp and effectively remove this sender from routing.
        let base = self
            .cca
            .earliest_departure_time(&now)
            .map(|edt| edt.max(now))
            .unwrap_or(now);

        // Add a penalty equal to one smoothed RTT when cwnd-limited.  This makes
        // congested senders look more expensive, steering new work to peers with
        // available congestion window.
        let congestion_penalty = if self.cca.is_congestion_limited() {
            self.rtt_estimator.smoothed_rtt()
        } else {
            Duration::ZERO
        };

        self.path_secret_entry.update_sender_load_score(
            self.sender_idx,
            base + congestion_penalty,
            total_cost,
            self.cca.bandwidth(),
        );
    }

    #[inline]
    pub fn has_pending(&self) -> bool {
        self.queues.iter().any(|q| !q.is_empty())
    }

    #[cfg_attr(not(test), expect(dead_code))]
    #[inline]
    pub fn pending_count(&self) -> usize {
        self.queues.iter().map(|q| q.len()).sum()
    }

    #[inline]
    pub fn has_pending_acks(&self) -> bool {
        !self.pending_acks.is_empty()
    }

    #[inline]
    pub fn has_pending_data(&self) -> bool {
        self.queues.iter().any(|q| !q.is_empty())
    }

    /// True when a frame carrying stream payload (`QueueData`/`QueueMsg`, including the
    /// `QueueInit` variant) is queued. Unlike [`has_pending_data`](Self::has_pending_data),
    /// this excludes the control/reset priorities so callers can reason specifically about
    /// bulk data — e.g. GSO pad-mode batching, which should pad full data packets but leave
    /// control/reset traffic in tight mode.
    #[inline]
    pub fn has_pending_stream_data(&self) -> bool {
        !self.queues[Priority::QueueData.as_index()].is_empty()
            || !self.queues[Priority::QueueInit.as_index()].is_empty()
    }

    #[inline]
    pub fn can_send_pending_frames(&self) -> bool {
        self.cca.requires_fast_retransmission() || !self.cca.is_congestion_limited()
    }

    #[inline]
    pub fn is_tx_scheduled(&self) -> bool {
        self.tx_wheel.is_scheduled()
    }

    #[inline]
    pub fn is_immediate_scheduled(&self) -> bool {
        self.tx_immediate.is_linked()
    }

    /// True while the context is queued in the wheel→assembler ready channel
    /// (drained off the wheel, awaiting assembly). Distinct from
    /// [`is_tx_scheduled`](Self::is_tx_scheduled), which means "still waiting in
    /// the wheel for a future EDT".
    #[inline]
    pub fn is_queued_for_assembly(&self) -> bool {
        self.tx_assembly.is_linked()
    }

    #[inline]
    pub fn is_pto_scheduled(&self) -> bool {
        self.pto_wheel.is_scheduled()
    }

    #[inline]
    pub fn is_idle_scheduled(&self) -> bool {
        self.idle_wheel.is_scheduled()
    }

    #[inline]
    pub fn path_info(&self, gso: &Gso) -> PathInfo {
        let mtu = self.path_secret_entry.max_datagram_size();
        let send_quantum_segments = (self.cca.send_quantum() as u64).div_ceil(mtu as u64) as usize;
        let max_segments = gso
            .max_segments()
            .min(segment::MAX_COUNT)
            .min(send_quantum_segments);
        PathInfo {
            max_datagram_size: mtu,
            max_segments,
        }
    }

    /// Called when the PTO wheel fires for this context.
    ///
    /// Marks the context's interface pair lost where it stopped delivering, as
    /// [`Self::has_stopped_delivering`] decides, so that the routers stop choosing it and its
    /// frames move to the pairs that still deliver. While the pair is lost, it caps the probe
    /// backoff, as `Pto::cap_lost_backoff` says, so that the first probe after the pair
    /// delivers again doesn't wait about as long as the outage lasted.
    ///
    /// Transitions the probe state from `Idle` to `Requested` if a real probe
    /// should be sent, then computes and returns the updated `WheelInterest` for
    /// rescheduling. The caller only needs to dispatch the returned interest.
    pub fn on_pto_timeout<Clk: precision::Clock + ?Sized>(&mut self, clock: &Clk) -> WheelInterest {
        let probe = self.pto.on_timeout();
        let now = clock.now();
        if !self.is_path_lost() && self.has_stopped_delivering(now) {
            self.mark_path_lost(now);
        }
        if self.is_path_lost() {
            self.pto.cap_lost_backoff(&self.rtt_estimator);
        }
        if probe && (self.inflight.has_inflight() || self.has_pending() || self.is_path_lost()) {
            // The only failure case is `NoOp` — the state is already `Requested`
            // because a previous probe hasn't been consumed by the assembler yet.
            // That's harmless: the assembler will send the probe on its next run.
            let _ = self.pto.probe_state.request();
        }
        let interest = self.wheel_interest(clock);
        self.invariants();
        interest
    }

    /// Verify structural invariants of the context.
    ///
    /// Runs assertions guarded by `cfg!(debug_assertions)` — in release builds this
    /// compiles away to nothing. Call this after any mutation that could violate these
    /// invariants:
    /// - PTO target should be `None` when there is no inflight data (no need to probe), except
    ///   while the context's interface pair is lost, when the PTO keeps probing it.
    /// - Every inflight packet must either have a `probed_to` link (shell) or contain
    ///   non-empty, all-ack-eliciting frames (no stale ACK frames stored).
    /// - The sum of `sent_bytes` in the inflight map must equal `cca.bytes_in_flight()`.
    #[inline]
    pub fn invariants(&self) {
        if cfg!(debug_assertions) {
            if self.invalidated {
                return;
            }
            let has_pending_data = self.queues.iter().any(|q| !q.is_empty());
            let has_pending_data_predicate = self.has_pending_data();
            let has_pending_predicate = self.has_pending();
            assert_eq!(
                has_pending_data_predicate, has_pending_data,
                "has_pending_data predicate drifted from queue contents"
            );
            assert_eq!(
                has_pending_predicate, has_pending_data,
                "has_pending predicate drifted from queue contents"
            );

            // NOTE: the assembler only piggybacks data on the immediate path when the
            // CCA pacing EDT has already elapsed (`now >= tx_wheel.target_time`).
            // It remains possible for tx_wheel to be scheduled with no pending data
            // if the data was sent when the EDT elapsed; the wheel will fire and find
            // nothing to do.  A stricter invariant is therefore not enforceable here.

            if self.pto_wheel.is_scheduled() && self.pto_wheel.target_time.is_some() {
                assert!(
                    self.inflight.has_inflight() || self.is_path_lost(),
                    "pto wheel scheduled with target_time but no inflight packets"
                );
            }

            if self.idle_wheel.is_scheduled() {
                assert!(
                    self.idle_wheel.target_time.is_some(),
                    "idle wheel scheduled without target_time"
                );
            }

            if self.pto.probe_state.is_requested() {
                assert!(
                    self.inflight.has_inflight() || self.has_pending() || self.is_path_lost(),
                    "probe_state is Requested but there is no inflight or pending work"
                );
            }

            if !self.inflight.has_inflight() && !self.is_path_lost() {
                assert!(
                    !self.pto.is_armed(),
                    "PTO is armed but there is no inflight data to probe"
                );
            }

            if let Some(max_inflight_pn) = self.inflight.max_packet_number() {
                assert!(
                    self.next_packet_number.as_u64() > max_inflight_pn.as_u64(),
                    "next_packet_number ({}) must be > max inflight pn ({})",
                    self.next_packet_number.as_u64(),
                    max_inflight_pn.as_u64()
                );
            }

            let sender_slots = self.path_secret_entry.socket_sender_count();
            if sender_slots > 0 {
                assert!(
                    usize::from(self.sender_idx) < sender_slots,
                    "sender_idx ({}) out of range for sender_load_scores_len ({sender_slots})",
                    self.sender_idx
                );
            }

            let map_bytes: u32 = self.inflight.sum_sent_bytes();
            let cca_bytes = self.cca.bytes_in_flight();
            assert_eq!(
                map_bytes, cca_bytes,
                "inflight map bytes ({map_bytes}) != CCA bytes_in_flight ({cca_bytes})"
            );
        }
        self.inflight.invariants();
    }

    /// Reap orphaned probe shells once nothing is genuinely in flight.
    ///
    /// When `bytes_in_flight` reaches zero, any entries still in the inflight map are
    /// zero-byte shells (PTO-probe tombstones whose live tail is already gone). They can
    /// never resolve to a completion or carry a probe, so they only keep `has_inflight()`
    /// true and the PTO armed forever. This wraps the `bytes_in_flight == 0` precondition
    /// together with the clear so every drain site reaps them consistently — call it
    /// anywhere an operation may have just dropped the last bytes in flight.
    #[inline]
    pub fn reap_shells_if_drained(&mut self) {
        if self.cca.bytes_in_flight() == 0 {
            self.inflight.clear_orphaned_shells();
        }
    }

    /// True while this context's interface pair is marked lost.
    #[inline]
    pub fn is_path_lost(&self) -> bool {
        self.lost_from.is_some()
    }

    /// Marks this context's interface pair lost, or delivering again, and records the mark on
    /// the path secret entry, which the routers read.
    ///
    /// A lost mark keeps the next packet number this context sends: an acknowledgement of
    /// that packet or a later one shows that the pair delivers again.
    ///
    /// It clears the mark's time, which [`Self::mark_path_lost`] sets again after this call.
    ///
    /// A lost mark also records on the path secret entry when the pair stopped delivering:
    /// one smoothed RTT after the later of its last acknowledgement and the time it began to
    /// wait for one. The routers then prefer the senders the peer acknowledged after that
    /// time, since a sender on the same down interface may not be marked lost yet.
    pub fn set_path_lost(&mut self, lost: bool) {
        self.lost_from = lost.then_some(self.next_packet_number);
        self.lost_at = None;
        if lost {
            let waiting_since = self
                .ack_wait_start()
                .map_or(self.last_peer_activity, |since| {
                    since.max(self.last_peer_activity)
                });
            let stopped_at = waiting_since + self.rtt_estimator.smoothed_rtt();
            self.path_secret_entry.set_senders_lost_since(stopped_at);
        }
        let _ = self
            .path_secret_entry
            .set_sender_lost(self.sender_idx, lost);
    }

    /// Marks this context's interface pair lost at `now`, as [`Self::set_path_lost`] does, and
    /// records when, so that the idle wheel can tell whether the peer sent a packet since.
    pub fn mark_path_lost(&mut self, now: precision::Timestamp) {
        self.set_path_lost(true);
        self.lost_at = Some(now);
    }

    /// Clears the lost mark once the peer acknowledges a packet sent after the mark was set.
    #[inline]
    pub fn on_largest_acknowledged(&mut self, largest_acknowledged: VarInt) {
        if matches!(self.lost_from, Some(lost_from) if largest_acknowledged >= lost_from) {
            self.set_path_lost(false);
        }
    }

    /// Records an acknowledgement from the peer at `now`: the peer was active, and no packet
    /// of this context outside the inflight map waits for an acknowledgement any longer.
    #[inline]
    pub fn on_peer_acknowledged(&mut self, now: precision::Timestamp) {
        self.last_peer_activity = now;
        self.ack_only_probe_sent = None;
        self.ack_only_since_peer_ack = false;
    }

    /// Records an acknowledgement from the peer received at `now`: the peer's activity, as
    /// [`Self::on_peer_acknowledged`] records it, this sender's delivery on the path secret
    /// entry, and the end of a lost mark that the acknowledgement covers.
    pub fn on_peer_acknowledgement(
        &mut self,
        now: precision::Timestamp,
        largest_acknowledged: VarInt,
    ) {
        self.on_peer_acknowledged(now);
        self.path_secret_entry
            .record_sender_delivered(self.sender_idx, now);
        self.on_largest_acknowledged(largest_acknowledged);
    }

    /// True when this context has packets that wait for an acknowledgement and the peer
    /// hasn't acknowledged any of them for a persistent congestion period of its RTT estimate
    /// (RFC 9002, section 7.6.1): `(smoothed_rtt + max(4 * rttvar, 1ms) + max_ack_delay) * 3`.
    /// Where the inflight map is empty, the packet that waits is the ack-eliciting packet that
    /// [`Self::ack_only_probe_sent`] records, such as the probe of a context that sends
    /// acknowledgements alone. Only a context of an endpoint whose send sockets are on more
    /// than one interface ([`Self::spreads_over_interfaces`]) stops delivering while its
    /// inflight map is empty.
    ///
    /// The period counts from the later of the last acknowledgement and the time the context
    /// began to wait for one. The PTO wheel checks it each time it fires for this
    /// context, at least once a base probe timeout period, so a pair is marked lost within
    /// about one probe timeout of the period's end. A context whose inflight map is empty has
    /// no probe timeout armed, so [`Self::check_acknowledgement_only_delivery`] checks it each
    /// time the assembler assembles packets for the context.
    pub fn has_stopped_delivering(&self, now: precision::Timestamp) -> bool {
        let Some(waiting_since) = self.ack_wait_start() else {
            return false;
        };
        let elapsed = now
            .nanos_since(waiting_since)
            .min(now.nanos_since(self.last_peer_activity));
        let threshold = self.rtt_estimator.persistent_congestion_threshold();
        elapsed >= threshold.as_nanos() as u64
    }

    /// When this context began to wait for an acknowledgement, or `None` where no packet of it
    /// waits for one: [`Self::ack_wait_since`] while the inflight map holds a packet, and
    /// otherwise the time [`Self::ack_only_probe_sent`] records, such as when a context that
    /// sends acknowledgements alone sent its probe, where the context spreads over interfaces
    /// ([`Self::spreads_over_interfaces`]). [`Self::has_stopped_delivering`] counts its wait
    /// from it, and [`Self::set_path_lost`] the time a lost pair stopped delivering.
    #[inline]
    fn ack_wait_start(&self) -> Option<precision::Timestamp> {
        if self.inflight.has_inflight() {
            self.ack_wait_since
        } else if self.spreads_over_interfaces {
            self.ack_only_probe_sent
        } else {
            None
        }
    }

    /// True when this context sends acknowledgements alone and the peer hasn't acknowledged
    /// one of its packets for a persistent congestion period: its inflight map is empty, no
    /// ack-eliciting packet of it waits for an acknowledgement, and it sent an ACK-only packet
    /// that isn't ack-eliciting since the peer's last acknowledgement. The assembler then makes
    /// its next ACK-only packet ack-eliciting, though the RTT tracker suppresses probes, and a
    /// probe that the peer doesn't acknowledge shows that the interface pair stopped
    /// delivering (RFC 9000, section 13.2.4). The context probes again only once it sent such
    /// a packet since the peer acknowledged its probe, so two idle endpoints stop probing
    /// each other. A context of an endpoint with one interface doesn't probe
    /// ([`Self::spreads_over_interfaces`]).
    pub fn needs_delivery_probe(&self, now: precision::Timestamp) -> bool {
        let threshold = self.rtt_estimator.persistent_congestion_threshold();
        self.spreads_over_interfaces
            && !self.inflight.has_inflight()
            && self.ack_only_probe_sent.is_none()
            && self.ack_only_since_peer_ack
            && now.nanos_since(self.last_peer_activity) >= threshold.as_nanos() as u64
    }

    /// Marks the interface pair lost at `now`, as [`Self::mark_path_lost`] does, where this
    /// context sends acknowledgements alone and its last ack-eliciting packet waited for a
    /// persistent congestion period without an acknowledgement while the context kept sending
    /// ACK-only packets. The assembler calls it each time it assembles packets for the context:
    /// a context whose inflight map is empty has no probe timeout armed to check
    /// [`Self::has_stopped_delivering`].
    ///
    /// Where the context didn't send an ACK-only packet for a persistent congestion period
    /// before `now`, that wait doesn't show that the pair stopped delivering: the probe may
    /// have been lost in an earlier outage, and the context didn't send anything since that
    /// would show that the pair delivers again. The context then forgets the probe and doesn't
    /// mark the pair lost, so that it probes again as [`Self::needs_delivery_probe`] says.
    pub fn check_acknowledgement_only_delivery(&mut self, now: precision::Timestamp) {
        if self.inflight.has_inflight() || self.is_path_lost() || self.ack_only_probe_sent.is_none()
        {
            return;
        }
        let threshold = self.rtt_estimator.persistent_congestion_threshold();
        let quiet = match self.ack_only_sent {
            Some(sent) => now.nanos_since(sent) >= threshold.as_nanos() as u64,
            None => true,
        };
        if quiet {
            self.ack_only_probe_sent = None;
            return;
        }
        if self.has_stopped_delivering(now) {
            self.mark_path_lost(now);
        }
    }

    /// True when this context's pair is lost, it holds frames to move, and another sender of
    /// the path secret entry still delivers.
    #[inline]
    pub fn should_move_frames(&self) -> bool {
        self.is_path_lost()
            && (self.inflight.has_inflight() || self.has_pending_data())
            && self.path_secret_entry.has_delivering_sender()
    }

    /// Moves every inflight and pending frame of this context to `output` as pending, so that
    /// the send contexts of the pairs that still deliver send them.
    ///
    /// Unlike [`Self::drain_frames`], it leaves the pending acknowledgements and the freed
    /// batch token in place: the context stays in use, and its probes go on. Returns the
    /// number of frames moved.
    pub fn move_frames(
        &mut self,
        output: &mut impl UnboundedSender<intrusive::Entry<Frame>>,
    ) -> usize {
        let mut moved = 0usize;
        let mut discarded_bytes = 0usize;
        let range = self.inflight.get_range();
        for (_pn, packet) in self.inflight.remove_range(range) {
            if let Some(tx_info) = &packet.transmission_info {
                discarded_bytes += tx_info.sent_bytes as usize;
            }
            for mut frame in packet.frames {
                frame.status = frame::TransmissionStatus::Pending;
                let _ = output.send(frame);
                moved += 1;
            }
        }
        if discarded_bytes > 0 {
            self.cca.on_packet_discarded(discarded_bytes);
        }
        for queue in &mut self.queues {
            while let Some(mut frame) = queue.pop_front() {
                frame.status = frame::TransmissionStatus::Pending;
                let _ = output.send(frame);
                moved += 1;
            }
        }
        self.ack_wait_since = None;
        self.invariants();
        moved
    }

    /// Drains all inflight and pending frames from this context.
    ///
    /// Returns `(frames_drained, inflight_bytes_discarded, pending_acks)`.
    /// Callers must route `pending_acks` back through ack_completions so recv
    /// contexts can recover their ack_state.
    pub fn drain_frames(
        &mut self,
        reason: Option<frame::FailureReason>,
        output: &mut impl UnboundedSender<intrusive::Entry<Frame>>,
    ) -> (
        usize,
        usize,
        intrusive::Queue<crate::stream::endpoint::msg::Sender>,
    ) {
        let mut drained = 0usize;
        let mut discarded_bytes = 0usize;
        let range = self.inflight.get_range();
        for (_pn, packet) in self.inflight.remove_range(range) {
            if let Some(tx_info) = &packet.transmission_info {
                discarded_bytes += tx_info.sent_bytes as usize;
            }
            for mut frame in packet.frames {
                frame.status = reason.map_or(frame::TransmissionStatus::Pending, |reason| {
                    frame::TransmissionStatus::Failed(reason)
                });
                let _ = output.send(frame);
                drained += 1;
            }
        }
        if discarded_bytes > 0 {
            self.cca.on_packet_discarded(discarded_bytes);
        }
        for queue in &mut self.queues {
            while let Some(mut frame) = queue.pop_front() {
                frame.status = reason.map_or(frame::TransmissionStatus::Pending, |reason| {
                    frame::TransmissionStatus::Failed(reason)
                });
                let _ = output.send(frame);
                drained += 1;
            }
        }
        let pending_acks = self.pending_acks.take();
        // Clear pending_freed token — IDs remain in FreedInner.freed (take() hasn't
        // been called yet). Clear in_flight so future record() calls can resubmit.
        if self.pending_freed.take().is_some() {
            if let crate::path::secret::map::entry::QueueState::Server(ref state) =
                *self.path_secret_entry.queue_state()
            {
                state.freed.clear_in_flight();
            }
        }
        (drained, discarded_bytes, pending_acks)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PathInfo {
    pub max_datagram_size: u16,
    pub max_segments: usize,
}

// ── Wheel Links ───────────────────────────────────────────────────────────

pub(crate) use crate::time::wheel::WheelLinks;

// ── Wheel Adapters ────────────────────────────────────────────────────────
//
// Each adapter tells the intrusive list infrastructure how to reach the Links
// field for its wheel, and tells the timing wheel how to read/write target_time.
// The pointer type is Rc<RefCell<Context>> for all three.

crate::rc_adapter!(
    pub(crate) struct TxImmediateAdapter {
        tx_immediate: RefCell<Context>,
    }
);
crate::rc_adapter!(
    pub(crate) struct TxAssemblyAdapter {
        tx_assembly: RefCell<Context>,
    }
);
crate::context_wheel_adapter!(TxWheelAdapter, Context, tx_wheel);
crate::context_wheel_adapter!(PtoWheelAdapter, Context, pto_wheel);
crate::context_wheel_adapter!(IdleWheelAdapter, Context, idle_wheel);

// ── Immediate Sender ─────────────────────────────────────────────────────

impl HasId<LocalSenderId> for Rc<RefCell<Context>> {
    #[inline]
    fn id(&self) -> LocalSenderId {
        self.borrow().sender_idx
    }
}

/// Routes contexts to per-socket immediate channels based on sender_idx.
pub(crate) type ImmediateSender<T> =
    super::combinator::MappedSender<Rc<RefCell<Context>>, LocalSenderId, LocalSendSocketId, T>;

/// PTO (Probe Timeout) state for tail loss recovery.
///
/// When all inflight packets may be lost (no ACKs arriving), PTO fires to send a probe.
/// This ensures the peer generates an ACK, which either confirms delivery or triggers
/// loss detection.
///
/// ## Constant-period wheel arming
///
/// The intrusive timing wheel does not support updating existing entries, so we always
/// arm at one base PTO period (1× `pto_period(INITIAL_PTO_BACKOFF)`) and track the
/// effective backoff as a fire count. The wheel fires cheaply at the base rate;
/// the handler decrements the counter and only sends a real probe when it reaches zero.
///
/// On ACK: reset `firings_remaining` to 0 so the very next wheel firing is a probe
/// (equivalent to resetting backoff to 1×). After at most one base period, we know
/// whether more inflight data needs probing.
///
/// `arm_base` advances by one base period on every arm so successive firings are evenly
/// spaced, even when `last_sent_time` stops advancing. Reset to `None` on packet-sent
/// or ACK so the next arm re-anchors to the freshest send timestamp.
pub(crate) struct Pto {
    /// Number of base-period wheel firings to consume before sending the next probe.
    ///
    /// Set to `backoff - 1` after a probe fires (so the next `backoff` firings elapse
    /// before probing again). Reset to 0 on ACK.
    pub firings_remaining: u32,
    /// Current effective backoff multiplier (doubles after each probe, capped at 16×).
    pub backoff: u32,
    /// Rolling base for `next_target` computations.
    ///
    /// `next_target` sets `target = arm_base + base_period` then advances `arm_base`
    /// to that value so consecutive arms are evenly spaced. Reset to `None` on
    /// packet-sent and on ACK so the next arm re-anchors to `last_sent_time`.
    pub arm_base: Option<precision::Timestamp>,
    pub last_sent_time: Option<precision::Timestamp>,
    pub needs_update: bool,
    /// PTO probe state: set to `Requested` by the PTO handler via `on_pto_timeout`;
    /// cleared by the assembler after the probe segment is encoded.
    pub probe_state: ProbeState,
}

impl Default for Pto {
    fn default() -> Self {
        Self {
            firings_remaining: 0,
            backoff: INITIAL_PTO_BACKOFF,
            arm_base: None,
            last_sent_time: None,
            needs_update: false,
            probe_state: ProbeState::Idle,
        }
    }
}

impl Pto {
    /// Maximum PTO backoff multiplier. At the default base period (~170ms), 32× gives
    /// ~6s between probes — aggressive enough for loss recovery while avoiding
    /// excessive probing of unresponsive peers before the idle wheel fires.
    const MAX_BACKOFF: u32 = 32;

    pub fn on_packet_sent(&mut self, now: precision::Timestamp) {
        self.last_sent_time = Some(now);
        // Reset arm_base so the next update_target re-anchors to this send time.
        self.arm_base = None;
        self.needs_update = true;
    }

    pub fn on_ack_received(&mut self, has_remaining_inflight: bool) {
        self.backoff = INITIAL_PTO_BACKOFF;
        self.firings_remaining = 0;
        // Reset arm_base so the next arm is relative to the freshest last_sent_time.
        self.arm_base = None;

        if !has_remaining_inflight {
            let _ = self.probe_state.on_all_acked();
        }

        self.needs_update = has_remaining_inflight;
    }

    /// Called when the PTO wheel fires for this context.
    ///
    /// Returns `true` if a probe should be sent now, `false` if this was a
    /// countdown firing (or a needs-update re-sync) that simply re-arms the wheel.
    pub fn on_timeout(&mut self) -> bool {
        if self.needs_update {
            // A packet was sent since the last arm; re-sync the arm base to the new
            // last_sent_time rather than firing a spurious probe.
            self.needs_update = false;
            self.arm_base = None;
            return false;
        }

        if self.firings_remaining > 0 {
            self.firings_remaining -= 1;
            return false;
        }

        // Time to probe: double backoff for next round.
        self.backoff = self.backoff.saturating_mul(2).min(Self::MAX_BACKOFF);
        self.firings_remaining = self.backoff - 1;
        true
    }

    /// The period of one wheel firing: one probe timeout period at the initial backoff, and
    /// at least 2 ms.
    fn base_period(rtt_estimator: &RttEstimator) -> Duration {
        rtt_estimator
            .pto_period(INITIAL_PTO_BACKOFF, PacketNumberSpace::Initial)
            .max(Duration::from_millis(2))
    }

    /// Caps the backoff of a lost pair's probes, so that the pair is probed at least once a
    /// persistent congestion period of `rtt_estimator` (RFC 9002, section 7.6.1), rounded up to
    /// whole wheel firings.
    ///
    /// A lost pair's probes go unacknowledged, so without the cap its backoff doubles up to
    /// `Self::MAX_BACKOFF`, and the first probe after the pair delivers again can wait about
    /// as long as the outage lasted.
    pub fn cap_lost_backoff(&mut self, rtt_estimator: &RttEstimator) {
        let base_period = Self::base_period(rtt_estimator).as_nanos();
        let threshold = rtt_estimator.persistent_congestion_threshold().as_nanos();
        let periods = threshold.div_ceil(base_period);
        let max_backoff = periods.clamp(1, u128::from(Self::MAX_BACKOFF)) as u32;
        self.backoff = self.backoff.min(max_backoff);
        self.firings_remaining = self.firings_remaining.min(self.backoff - 1);
    }

    /// Returns `true` if the PTO needs to fire (has remaining countdown or pending state).
    pub fn is_armed(&self) -> bool {
        self.needs_update || self.arm_base.is_some()
    }

    /// Compute the next wheel arm target.
    ///
    /// Always uses one base period (1× `pto_period(INITIAL_PTO_BACKOFF)`) regardless
    /// of the current backoff level. `arm_base` advances by one period each call so
    /// consecutive firings are evenly spaced.
    pub fn next_target<Clk: precision::Clock + ?Sized>(
        &mut self,
        clock: &Clk,
        rtt_estimator: &RttEstimator,
    ) -> Option<precision::Timestamp> {
        let base_period = Self::base_period(rtt_estimator);

        // Anchor to arm_base if available, otherwise to last_sent_time, otherwise now.
        let base = self
            .arm_base
            .unwrap_or_else(|| self.last_sent_time.unwrap_or_else(|| clock.now()));
        let next = base + base_period;
        // Advance arm_base so the next call steps forward by another period.
        self.arm_base = Some(next);
        Some(next)
    }
}

/// Runs `f` on `ctx`, and records a change of the context's lost mark that `f` makes: a pair
/// marked lost, or a lost pair that delivers again. It counts the change on `send_counters`,
/// and publishes its event through `path_secret_map` where one is set.
///
/// [`Cache::track_path_lost`] runs its calls through it. The assembler, which doesn't hold the
/// cache, runs its lost mark of a pair that sends acknowledgements alone through it.
pub(crate) fn track_path_lost<R>(
    ctx: &mut Context,
    send_counters: &super::counters::Send,
    path_secret_map: Option<&crate::path::secret::Map>,
    f: impl FnOnce(&mut Context) -> R,
) -> R {
    let was_lost = ctx.is_path_lost();
    let result = f(ctx);
    match (was_lost, ctx.is_path_lost()) {
        (false, true) => {
            send_counters.on_interface_pair_lost();
            if let Some(map) = path_secret_map {
                map.on_interface_pair_lost(ctx.path_secret_entry.peer());
            }
        }
        (true, false) => {
            send_counters.on_interface_pair_delivering();
            if let Some(map) = path_secret_map {
                map.on_interface_pair_delivering(ctx.path_secret_entry.peer());
            }
        }
        _ => {}
    }
    result
}

/// Per-socket cache of send contexts, keyed by credentials ID.
///
/// Each send socket has its own Cache. A single peer always routes through the same
/// send socket (via credential hashing), so there's exactly one Context per peer per socket.
pub(crate) struct Cache {
    contexts: FxHashMap<credentials::Id, Rc<RefCell<Context>>>,
    inflight_gauge: QueueGauge,
    ack_gauge: QueueGauge,
    pending_gauge: QueueGauge,
    sender_idx: LocalSenderId,
    send_counters: Rc<super::counters::Send>,
    /// The place of this cache's send socket among the endpoint's interfaces, which pairs its
    /// contexts with the peer's interfaces, or `None` where the endpoint doesn't list any.
    place: Option<crate::socket::interface::SendPlace>,
    /// The path secret map that publishes the lost-pair events of this cache's contexts, or
    /// `None` where the cache counts them on its send counters alone.
    path_secret_map: Option<crate::path::secret::Map>,
}

impl Cache {
    pub fn new(counter_registry: &crate::counter::Registry, sender_idx: LocalSenderId) -> Self {
        let variant = format!("send.{sender_idx}");
        let send_counters = super::counters::Send::new(counter_registry, sender_idx);
        Self {
            contexts: FxHashMap::default(),
            inflight_gauge: counter_registry
                .register_queue_gauge_nominal("send.inflight", &variant),
            ack_gauge: counter_registry.register_queue_gauge_nominal("send.ack", &variant),
            pending_gauge: counter_registry.register_queue_gauge_nominal("send.pending", &variant),
            sender_idx,
            send_counters,
            place: None,
            path_secret_map: None,
        }
    }

    /// Sets the place of this cache's send socket among the endpoint's interfaces, so that each
    /// context it builds sends to the peer's interface that the socket's own is paired with.
    pub fn with_place(mut self, place: Option<crate::socket::interface::SendPlace>) -> Self {
        self.place = place;
        self
    }

    #[cfg(test)]
    pub fn context_count(&self) -> usize {
        self.contexts.len()
    }

    pub fn get_or_insert(
        &mut self,
        entry: &Arc<PathSecretEntry>,
        clock: &impl precision::Clock,
    ) -> Result<Rc<RefCell<Context>>, ContextError> {
        use std::collections::hash_map::Entry as MapEntry;

        let id = *entry.id();

        match self.contexts.entry(id) {
            MapEntry::Occupied(e) => Ok(e.get().clone()),
            MapEntry::Vacant(e) => {
                let ctx = Context::new_paired(
                    entry,
                    self.inflight_gauge.clone(),
                    self.ack_gauge.clone(),
                    self.pending_gauge.clone(),
                    self.sender_idx,
                    self.place,
                    clock,
                )?;
                self.send_counters.on_context_created();
                Ok(e.insert(Rc::new(RefCell::new(ctx))).clone())
            }
        }
    }

    pub fn get(&self, id: &credentials::Id) -> Option<Rc<RefCell<Context>>> {
        self.contexts.get(id).cloned()
    }

    #[inline]
    pub fn send_counters(&self) -> &Rc<super::counters::Send> {
        &self.send_counters
    }

    /// Sets the path secret map that publishes the lost-pair events of this cache's contexts.
    /// A cache without one counts the events on its send counters alone.
    pub fn with_path_secret_map(mut self, map: crate::path::secret::Map) -> Self {
        self.path_secret_map = Some(map);
        self
    }

    /// Runs `f` on `ctx`, and records a change of the context's lost mark that `f` makes, as
    /// the function `track_path_lost` of this module does, with this cache's send counters
    /// and path secret map.
    ///
    /// The probe timeout task and the ACK processor run their calls through it. The clear in
    /// [`Self::invalidate`] isn't recorded, since the context leaves the cache there.
    pub fn track_path_lost<R>(&self, ctx: &mut Context, f: impl FnOnce(&mut Context) -> R) -> R {
        track_path_lost(ctx, &self.send_counters, self.path_secret_map.as_ref(), f)
    }

    /// Moves a lost pair's frames to `output`, as [`Context::move_frames`] does, and records
    /// the frames it moves. Returns the number of frames moved.
    pub fn move_frames(
        &self,
        ctx: &mut Context,
        output: &mut impl UnboundedSender<intrusive::Entry<Frame>>,
    ) -> usize {
        let moved = ctx.move_frames(output);
        if moved > 0 {
            self.send_counters
                .on_interface_pair_frames_moved(moved as u64);
            if let Some(map) = &self.path_secret_map {
                map.on_interface_pair_frames_moved(ctx.path_secret_entry.peer(), moved);
            }
        }
        moved
    }

    /// True where the idle wheel doesn't mark the peer of `entry` dead: this cache's pair is
    /// lost and another pair of the peer still delivers. Records each skip.
    pub fn skips_dead_mark(&self, path_lost: bool, entry: &PathSecretEntry) -> bool {
        let skips = path_lost && entry.has_delivering_sender();
        if skips {
            self.send_counters.on_dead_mark_skipped();
            if let Some(map) = &self.path_secret_map {
                map.on_dead_mark_skipped(entry.peer());
            }
        }
        skips
    }

    pub fn invalidate(
        &mut self,
        id: &credentials::Id,
        reason: frame::FailureReason,
        cancelled: &mut impl UnboundedSender<intrusive::Entry<Frame>>,
    ) -> Option<(
        usize,
        usize,
        intrusive::Queue<crate::stream::endpoint::msg::Sender>,
    )> {
        let Some(ctx) = self.contexts.remove(id) else {
            trace!(%id, sender_idx = %self.sender_idx, "invalidate: no context found");
            return None;
        };
        self.send_counters.on_context_removed();
        let mut ctx = ctx.borrow_mut();
        ctx.invalidated = true;
        // The routers read the lost mark from the path secret entry, and once the context leaves
        // the cache, none of its probes can clear the mark.
        if ctx.is_path_lost() {
            ctx.set_path_lost(false);
        }
        debug!(%id, sender_idx = %self.sender_idx, "invalidating send context");
        Some(ctx.drain_frames(Some(reason), cancelled))
    }

    pub fn invalidate_stale_key(
        &mut self,
        id: &credentials::Id,
        sender_idx: LocalSenderId,
        rejected_key_id: VarInt,
        retransmit: &mut impl UnboundedSender<intrusive::Entry<Frame>>,
    ) -> Option<(
        usize,
        usize,
        intrusive::Queue<crate::stream::endpoint::msg::Sender>,
    )> {
        assert_eq!(
            self.sender_idx, sender_idx,
            "invalidate_stale_key called for wrong sender cache"
        );
        if self.sender_idx != sender_idx {
            return None;
        }

        let Some(ctx) = self.contexts.get(id) else {
            trace!(
                %id,
                sender_idx = %self.sender_idx,
                stale_sender_id = %sender_idx,
                "invalidate_stale_key: no context found"
            );
            return None;
        };

        // If the send context's key_id is already past the rejected one, this
        // invalidation is stale (e.g. a delayed/replayed control packet) — ignore it.
        if ctx.borrow().credentials.key_id > rejected_key_id {
            trace!(
                %id,
                sender_idx = %self.sender_idx,
                rejected_key_id = rejected_key_id.as_u64(),
                current_key_id = ctx.borrow().credentials.key_id.as_u64(),
                "invalidate_stale_key: context already advanced past rejected key_id"
            );
            return None;
        }

        let ctx = self.contexts.remove(id).unwrap();
        self.send_counters.on_context_removed();
        let mut ctx = ctx.borrow_mut();
        ctx.invalidated = true;
        // Clear the lost mark, as `invalidate` does.
        if ctx.is_path_lost() {
            ctx.set_path_lost(false);
        }
        debug!(
            %id,
            sender_idx = %self.sender_idx,
            stale_sender_id = %sender_idx,
            rejected_key_id = rejected_key_id.as_u64(),
            "invalidating stale/replay send context for retransmission"
        );
        Some(ctx.drain_frames(None, retransmit))
    }
}
