// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Frame/packet flight recorder, backed by [`backbeat`].
//!
//! Every frame the endpoint observes is recorded as a compact, **per-kind** [`backbeat`] event:
//! one event type per wire frame kind ([`QueueDataFrame`], [`QueueMsgFrame`], [`QueueMaxDataFrame`],
//! [`QueueResetFrame`], [`QueueFreeFrame`], [`AckFrame`], [`QueueDataBlockedFrame`], [`QueueDbgFrame`],
//! [`QueueControlFrame`], [`PingFrame`]), each carrying that kind's *real, correctly-named* fields.
//! Coarser per-packet signals are recorded as [`PacketRecord`] events. All are captured into the
//! process-wide [`backbeat::global`] recorder — the hot path is a CPU-sharded ring push with no
//! allocation.
//!
//! Per-kind events (rather than one union with a "primary offset") keep the dump honest: a
//! `QueueFreeFrame` exposes `free_request_id`/`smallest_queue_id`, an `AckFrame` exposes
//! `largest_acknowledged`/`is_ack_eliciting`, a `QueueDataFrame` exposes `offset`/`largest_offset`/
//! `is_fin` — no field ever holds a value its name doesn't describe. backbeat's sparse-wide Parquet
//! makes each event type its own set of columns at zero cost.
//!
//! Two axes are *fields*, not types:
//!
//! * **[`Lifecycle`]** — where in the frame's life it was observed: [`AppSend`](Lifecycle::AppSend)
//!   into the send pipeline, [`Outbound`](Lifecycle::Outbound) at assembly, [`Inbound`](Lifecycle::Inbound)
//!   /[`InboundFastPath`](Lifecycle::InboundFastPath) at decode, [`AckCompleted`](Lifecycle::AckCompleted)
//!   /[`AckLost`](Lifecycle::AckLost)/[`AckCancelled`](Lifecycle::AckCancelled)/
//!   [`SendCancelled`](Lifecycle::SendCancelled) at completion, [`RxDropped`](Lifecycle::RxDropped)
//!   on rejection, and [`AppRecv`](Lifecycle::AppRecv) at delivery. Tracking one frame across its
//!   whole life is `… WHERE lifecycle IN (…)`, not a join across types.
//! * **`packet_number` + `sender_id`** — present at wire phases, [`NO_PACKET_NUMBER`] at the
//!   app-layer phases (`AppSend`/`AppRecv`) that have no PN yet. A packet number is only unique
//!   within `(cred_id, sender_id)`, so the packet join key is `(cred_id, sender_id, packet_number)`.
//!
//! Every frame event carries the flow key `(cred_id, binding_id)` where it has one, so an
//! `AppSend`/`AppRecv` joins to the wire sightings of the same stream, and a frame joins to its
//! packet through `(cred_id, sender_id, packet_number)`.
//!
//! Interleaved are coarser per-packet [`PacketRecord`]s — received off the wire (the earliest
//! sighting of a PN, before decrypt/dedup), assembled/sent, acked, lost, or dropped pre-decode —
//! carrying the packet-only signals a frame can't: wire byte count, frame count, probe shell→pn
//! linkage, and pre-decode drop reasons.
//!
//! Unlike the per-`dump_id` [`QueueDbg`](super::frame::Header::QueueDbg) diagnostic — which traces
//! *one* stuck frame end-to-end — this records *all* frames, so the dump shows the surrounding
//! history. Each `QueueDbg` we receive [triggers](trigger) an asynchronous dump of the rings to disk.
//!
//! The whole facility is gated behind [`super::dbg::ENABLED`]: in plain release builds every record
//! entry point folds to nothing — the event is never constructed and the [`backbeat::global`]
//! recorder (and its dumper thread) is never built. It is active under `test`, the `testing`
//! feature, or the `queue-dbg` feature — the same builds that already speak `QueueDbg`.
//!
//! Dumps are self-describing `.bb` files written by `backbeat::global`'s background dumper, named by
//! UTC timestamp under `BACKBEAT_PATH` (default `${TMPDIR}/backbeat.<pid>.bb`). Read them with the
//! `backbeat` CLI: `backbeat inspect <file>.bb`, or `backbeat convert <file>.bb -o out.parquet`
//! (query with DuckDB) / `-o out.json` (load in Chrome/Perfetto). The recorder honours backbeat's
//! `BACKBEAT_*` environment overrides (`BACKBEAT_PATH`, `BACKBEAT_BYTES`, `BACKBEAT_THROTTLE_MS`,
//! `BACKBEAT_MAX_DUMPS`, `BACKBEAT_SIGNAL`, …).

use super::frame::Header;
use crate::credentials::Id as CredId;
// `Event`/`EventEnum` name both a trait and a derive macro in backbeat (it re-exports both under the
// same name); importing them plainly brings the derive macros — and their `#[event(...)]` helper
// attribute — into scope, and also the traits (so `QueueDataFrame::ID` resolves).
use backbeat::{Event, EventEnum};
use s2n_quic_core::varint::VarInt;
use zerocopy::{Immutable, IntoBytes};

// Register the dc stream-correlation views (Tier-2 domain overlay) so every `.bb` dump carries the
// `stream_timeline` / `stream_by_dump` / `flow_frames` / `flow_packets` DuckDB macros — `backbeat
// convert` appends them after its generated per-event views. Keyed on the flow key
// `(cred_id, binding_id)`; see the SQL file for the join contract.
backbeat::register_views!(include_str!("frame_trace.views.sql"));

/// Stored in a `packet_number` / `sender_id` / `linked_pn` field when no packet number is in scope
/// (the app-layer `AppSend`/`AppRecv` phases, or a packet with no probed-from shell PN). Part B of
/// the design maps this to SQL `NULL` at the view layer.
const NO_PACKET_NUMBER: u64 = u64::MAX;

/// The endpoint bit in byte 0 of a `credentials::Id` (mirrors the private `Id::ENDPOINT_BIT`). It
/// flips between the server and client view of the same id, so it is masked off before storing the
/// id in a record — see [`canonicalize_cred`].
const CRED_ENDPOINT_BIT: u8 = 0x80;

/// Where in a frame's life it was observed. A *field* on every frame event (not a type), so one
/// frame can be tracked across its whole lifecycle with a single column filter.
///
/// Discriminants are stable dump values, carried over from the original `Direction` enum (append
/// new variants, never renumber). `backbeat` embeds the value→label map in the schema (via
/// [`EventEnum`]), so the CLI renders these as names.
#[derive(EventEnum, IntoBytes, Immutable, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Lifecycle {
    /// Received in a multi-frame packet (slow path decode).
    Inbound = 0,
    /// Received as a single-QueueMsg fast-path packet.
    InboundFastPath = 1,
    /// Emitted at packet assembly (on the wire).
    Outbound = 2,
    /// Acknowledged by the peer.
    AckCompleted = 3,
    /// Declared lost (will be retransmitted).
    AckLost = 4,
    /// Cancelled post-send (sender gone) or TTL-exhausted, discovered during loss detection.
    AckCancelled = 5,
    /// Submitted by the application into the send pipeline, before aggregation/credit/pacing/
    /// assembly — the first sighting of an app-originated frame. No packet number yet
    /// ([`NO_PACKET_NUMBER`]); pairs with [`Outbound`](Lifecycle::Outbound) to expose submit→wire
    /// latency.
    AppSend = 6,
    /// Reassembled stream bytes delivered to the application by the reader — the last sighting of
    /// received data. No packet number ([`NO_PACKET_NUMBER`]); pairs with [`Inbound`](Lifecycle::Inbound)
    /// to expose wire→consume latency. Only [`QueueDataFrame`] carries this (it is a data delivery),
    /// with the delivered byte count in [`QueueDataFrame::payload_len`].
    AppRecv = 7,
    /// Cancelled at assembly before ever reaching the wire — the emitting handle was already gone.
    /// Distinct from [`AckCancelled`](Lifecycle::AckCancelled), a *post-send* cancellation.
    SendCancelled = 8,
    /// Rejected after decode on the receive path (unallocated/stale/future binding, half-closed,
    /// cap-exceeded, window violation). Carries a [`DropReason`].
    RxDropped = 9,
}

/// Why a frame or packet was dropped. `None` (0) is the resting value for records that did not
/// record a drop. Receive-side post-decode reasons mirror `crate::queue::Error`; pre-decode reasons
/// have no frame to attach to and only appear on a [`PacketRecord`].
///
/// Discriminants are stable dump values — append, never renumber.
#[derive(EventEnum, IntoBytes, Immutable, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum DropReason {
    /// Not a drop (resting value).
    None = 0,
    // ── post-decode, per-frame (Lifecycle::RxDropped) ──
    /// Destination queue id is not allocated.
    Unallocated = 1,
    /// Receiver half is already closed.
    HalfClosed = 2,
    /// Binding id is older than the slot's active binding (replicated/stale read).
    StaleBinding = 3,
    /// Binding id is ahead of the allocated range.
    FutureBinding = 4,
    /// Sender half is closed.
    SenderClosed = 5,
    /// Queue id exceeds the server cap.
    CapExceeded = 6,
    /// Data extends beyond the advertised receive window.
    WindowViolation = 7,
    /// Message-table insert rejected (msg_id gap beyond `MAX_PENDING_MESSAGES`, or bad geometry).
    MsgInsertRejected = 8,
    // ── pre-decode, per-packet (PacketRecord) ──
    /// AEAD authentication / decrypt failed.
    DecryptFailed = 9,
    /// Replay detected (key id stale or outside the receiver's window).
    ReplayDetected = 10,
    /// Packet number already seen (dedup).
    Duplicate = 11,
    /// No path secret maps to the packet's credentials.
    PathSecretNotFound = 12,
}

/// Mirror of `crate::packet::datagram::ResetTarget` as a dump enum (the wire type isn't `IntoBytes`).
/// Stored on [`QueueResetFrame::reset_target`].
#[derive(EventEnum, IntoBytes, Immutable, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ResetTarget {
    Both = 0,
    Stream = 1,
    Control = 2,
}

impl From<crate::packet::datagram::ResetTarget> for ResetTarget {
    fn from(t: crate::packet::datagram::ResetTarget) -> Self {
        use crate::packet::datagram::ResetTarget as W;
        match t {
            W::Both => ResetTarget::Both,
            W::Stream => ResetTarget::Stream,
            W::Control => ResetTarget::Control,
        }
    }
}

/// A packet's place in its lifecycle. Stored in [`PacketRecord::event`].
///
/// Discriminants are stable dump values — append, never renumber.
#[derive(EventEnum, IntoBytes, Immutable, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PacketEvent {
    /// Received off the wire — recorded the instant the packet number is known, *before* decrypt,
    /// dedup, and frame dispatch. The earliest sighting of an inbound packet, so a gap in the
    /// received-PN sequence here pins loss to the wire (vs. loss after processing).
    RxArrived = 0,
    /// Dropped before its frames could be decoded: see [`PacketRecord::reason`].
    RxDropped = 1,
    /// Assembled and sent on the wire. [`PacketRecord::frame_count`]/[`PacketRecord::sent_bytes`]
    /// describe the segment; [`PacketRecord::linked_pn`] is the probed-from shell PN for a PTO probe.
    Sent = 2,
    /// Acknowledged by the peer.
    Acked = 3,
    /// Declared lost by loss detection.
    Lost = 4,
}

// ──────────────────────────────────────────────────────────────────────────────────────────────
// Per-kind frame events.
//
// Every frame event begins with the same identity prefix — `packet_number`, `sender_id`, then any
// kind-specific 8-byte fields, then `cred_id` — and ends with `lifecycle` + `reason` + kind-specific
// `u8`/`bool` fields + explicit `_pad`. The 8-byte fields lead so the layout is naturally aligned
// with no implicit gap (which `zerocopy::IntoBytes` rejects); the trailing `_pad` fills the struct
// out to its 8-byte alignment. `packet_number`/`sender_id` are [`NO_PACKET_NUMBER`] for the app
// phases. Each routed kind carries the flow key `(cred_id, binding_id)`.
// ──────────────────────────────────────────────────────────────────────────────────────────────

/// `QueueData` — stream data. Also the type used for the synthesised [`Lifecycle::AppRecv`] delivery
/// and reader-side [`Lifecycle::RxDropped`] sightings (a delivery *is* a QueueData crossing).
#[derive(Event, IntoBytes, Immutable, Debug)]
#[event(namespace = "s2n_quic_dc::frame::queue_data")]
#[repr(C)]
pub struct QueueDataFrame {
    #[event(key, sentinel = u64::MAX)]
    pub packet_number: u64,
    #[event(key, sentinel = u64::MAX)]
    pub sender_id: u64,
    pub source_queue_id: u64,
    pub dest_queue_id: u64,
    #[event(key)]
    pub binding_id: u64,
    /// Stream offset of this frame's data (consumed offset for [`Lifecycle::AppRecv`]).
    #[event(unit = "bytes")]
    pub offset: u64,
    /// Writer's high-water mark — the largest offset it currently wants to send.
    #[event(unit = "bytes")]
    pub largest_offset: u64,
    #[event(key)]
    pub cred_id: [u8; 16],
    /// Bytes delivered to the application for [`Lifecycle::AppRecv`], else 0.
    #[event(unit = "bytes")]
    pub payload_len: u32,
    pub lifecycle: Lifecycle,
    pub reason: DropReason,
    pub is_fin: bool,
    pub blocked: bool,
    /// This frame carries init fields (it can create the binding).
    pub is_init: bool,
    pub _pad: [u8; 7],
}

/// `QueueMsg` — pre-allocated message data (whole-message reassembly).
#[derive(Event, IntoBytes, Immutable, Debug)]
#[event(namespace = "s2n_quic_dc::frame::queue_msg")]
#[repr(C)]
pub struct QueueMsgFrame {
    #[event(key, sentinel = u64::MAX)]
    pub packet_number: u64,
    #[event(key, sentinel = u64::MAX)]
    pub sender_id: u64,
    pub source_queue_id: u64,
    pub dest_queue_id: u64,
    #[event(key)]
    pub binding_id: u64,
    /// Message-local data offset.
    #[event(unit = "bytes")]
    pub stream_offset: u64,
    /// Writer's high-water mark.
    #[event(unit = "bytes")]
    pub largest_offset: u64,
    #[event(key)]
    pub msg_id: u64,
    #[event(unit = "bytes")]
    pub message_size: u64,
    #[event(unit = "bytes")]
    pub chunk_size: u64,
    pub chunk_index: u64,
    #[event(key)]
    pub cred_id: [u8; 16],
    pub lifecycle: Lifecycle,
    pub reason: DropReason,
    pub is_fin: bool,
    pub is_wakeup: bool,
    pub blocked: bool,
    pub is_init: bool,
    pub _pad: [u8; 2],
}

/// `QueueMaxData` — inline window update (MAX_DATA value carried in the header).
#[derive(Event, IntoBytes, Immutable, Debug)]
#[event(namespace = "s2n_quic_dc::frame::queue_max_data")]
#[repr(C)]
pub struct QueueMaxDataFrame {
    #[event(key, sentinel = u64::MAX)]
    pub packet_number: u64,
    #[event(key, sentinel = u64::MAX)]
    pub sender_id: u64,
    pub source_queue_id: u64,
    pub dest_queue_id: u64,
    #[event(key)]
    pub binding_id: u64,
    /// The advertised receive-window maximum.
    #[event(unit = "bytes")]
    pub maximum_data: u64,
    #[event(key)]
    pub cred_id: [u8; 16],
    pub lifecycle: Lifecycle,
    pub reason: DropReason,
    pub _pad: [u8; 6],
}

/// `QueueDataBlocked` — standalone flow-control-blocked signal (writer→reader).
#[derive(Event, IntoBytes, Immutable, Debug)]
#[event(namespace = "s2n_quic_dc::frame::queue_data_blocked")]
#[repr(C)]
pub struct QueueDataBlockedFrame {
    #[event(key, sentinel = u64::MAX)]
    pub packet_number: u64,
    #[event(key, sentinel = u64::MAX)]
    pub sender_id: u64,
    pub source_queue_id: u64,
    pub dest_queue_id: u64,
    #[event(key)]
    pub binding_id: u64,
    /// Offset the writer is blocked at / wants to reach.
    #[event(unit = "bytes")]
    pub desired_offset: u64,
    #[event(key)]
    pub cred_id: [u8; 16],
    pub lifecycle: Lifecycle,
    pub reason: DropReason,
    pub _pad: [u8; 6],
}

/// `QueueReset` — reset a flow.
#[derive(Event, IntoBytes, Immutable, Debug)]
#[event(namespace = "s2n_quic_dc::frame::queue_reset")]
#[repr(C)]
pub struct QueueResetFrame {
    #[event(key, sentinel = u64::MAX)]
    pub packet_number: u64,
    #[event(key, sentinel = u64::MAX)]
    pub sender_id: u64,
    pub source_queue_id: u64,
    pub dest_queue_id: u64,
    #[event(key)]
    pub binding_id: u64,
    pub error_code: u64,
    #[event(key)]
    pub cred_id: [u8; 16],
    pub lifecycle: Lifecycle,
    pub reason: DropReason,
    /// Which halves the reset targets.
    pub reset_target: ResetTarget,
    /// This reset carries init fields (it can create the binding).
    pub is_init: bool,
    pub _pad: [u8; 4],
}

/// `QueueFree` — server→client slot-credit return. Not routed by queue/binding; carries its own
/// `free_request_id` (dedup stamp) and `smallest_queue_id`.
#[derive(Event, IntoBytes, Immutable, Debug)]
#[event(namespace = "s2n_quic_dc::frame::queue_free")]
#[repr(C)]
pub struct QueueFreeFrame {
    #[event(key, sentinel = u64::MAX)]
    pub packet_number: u64,
    #[event(key, sentinel = u64::MAX)]
    pub sender_id: u64,
    /// Monotonic stamp for receiver-side dedup of replayed/duplicate frees.
    #[event(key)]
    pub free_request_id: u64,
    pub smallest_queue_id: u64,
    #[event(key)]
    pub cred_id: [u8; 16],
    pub lifecycle: Lifecycle,
    pub reason: DropReason,
    pub _pad: [u8; 6],
}

/// `Ack` — acknowledgement. Routed by `dest_sender_id`, not queue/binding.
#[derive(Event, IntoBytes, Immutable, Debug)]
#[event(namespace = "s2n_quic_dc::frame::ack")]
#[repr(C)]
pub struct AckFrame {
    #[event(key, sentinel = u64::MAX)]
    pub packet_number: u64,
    #[event(key, sentinel = u64::MAX)]
    pub sender_id: u64,
    /// The sender whose packets this ACK acknowledges.
    #[event(key)]
    pub dest_sender_id: u64,
    pub largest_acknowledged: u64,
    #[event(key)]
    pub cred_id: [u8; 16],
    pub lifecycle: Lifecycle,
    pub reason: DropReason,
    pub is_ack_eliciting: bool,
    pub _pad: [u8; 5],
}

/// `QueueDbg` — stuck-stream diagnostic marker. Carries the `dump_id`.
#[derive(Event, IntoBytes, Immutable, Debug)]
#[event(namespace = "s2n_quic_dc::frame::queue_dbg")]
#[repr(C)]
pub struct QueueDbgFrame {
    #[event(key, sentinel = u64::MAX)]
    pub packet_number: u64,
    #[event(key, sentinel = u64::MAX)]
    pub sender_id: u64,
    pub source_queue_id: u64,
    pub dest_queue_id: u64,
    #[event(key)]
    pub binding_id: u64,
    /// The diagnostic episode id (shared across every hop the marker touches).
    #[event(key)]
    pub dump_id: u64,
    #[event(key)]
    pub cred_id: [u8; 16],
    pub lifecycle: Lifecycle,
    pub reason: DropReason,
    pub _pad: [u8; 6],
}

/// `QueueControl` — generic flow-control escape hatch (opaque payload). Routing only.
#[derive(Event, IntoBytes, Immutable, Debug)]
#[event(namespace = "s2n_quic_dc::frame::queue_control")]
#[repr(C)]
pub struct QueueControlFrame {
    #[event(key, sentinel = u64::MAX)]
    pub packet_number: u64,
    #[event(key, sentinel = u64::MAX)]
    pub sender_id: u64,
    pub source_queue_id: u64,
    pub dest_queue_id: u64,
    #[event(key)]
    pub binding_id: u64,
    #[event(key)]
    pub cred_id: [u8; 16],
    pub lifecycle: Lifecycle,
    pub reason: DropReason,
    pub _pad: [u8; 6],
}

/// `Ping` — payload-less PTO-probe filler. No routing identity beyond the path credential.
#[derive(Event, IntoBytes, Immutable, Debug)]
#[event(namespace = "s2n_quic_dc::frame::ping")]
#[repr(C)]
pub struct PingFrame {
    #[event(key, sentinel = u64::MAX)]
    pub packet_number: u64,
    #[event(key, sentinel = u64::MAX)]
    pub sender_id: u64,
    #[event(key)]
    pub cred_id: [u8; 16],
    pub lifecycle: Lifecycle,
    pub reason: DropReason,
    pub _pad: [u8; 6],
}

/// A compact per-packet record.
///
/// Captures the packet-only signals a frame record can't: the wire byte count, how many frames
/// shared the packet, a probe's shell→pn linkage, and the pre-decode drop reasons that have no
/// decoded frame to attach to. Frames join to their packet through `(cred_id, sender_id, packet_number)`.
#[derive(Event, IntoBytes, Immutable, Debug)]
#[event(namespace = "s2n_quic_dc::packet")]
#[repr(C)]
pub struct PacketRecord {
    #[event(key, sentinel = u64::MAX)]
    pub packet_number: u64,
    #[event(key, sentinel = u64::MAX)]
    pub sender_id: u64,
    /// For [`PacketEvent::Sent`] probes, the shell PN this packet was probed from; else
    /// [`NO_PACKET_NUMBER`] (mapped to SQL NULL). Same sender space as `packet_number`.
    #[event(sentinel = u64::MAX)]
    pub linked_pn: u64,
    #[event(key)]
    pub cred_id: [u8; 16],
    #[event(unit = "bytes")]
    pub sent_bytes: u32,
    pub frame_count: u16,
    pub event: PacketEvent,
    pub reason: DropReason,
}

/// Canonicalizes a credential id by masking off the endpoint bit (MSB of byte 0). That bit encodes
/// which side a `credentials::Id` is viewed from — the server and client hold the same logical id
/// with this bit flipped (see `Id::for_endpoint`/`for_peer`). Clearing it makes a record from either
/// endpoint share one id, so a single query correlates both ends of a stream.
#[inline]
fn canonicalize_cred(mut cred_id: [u8; 16]) -> [u8; 16] {
    cred_id[0] &= !CRED_ENDPOINT_BIT;
    cred_id
}

// ──────────────────────────────────────────────────────────────────────────────────────────────
// Recording API. Each entry point is gated by `super::dbg::on_enabled` (a `false` const in plain
// release builds, so the whole body — including event construction — is stripped). Call sites pass
// plain values; no closures or imports needed.
// ──────────────────────────────────────────────────────────────────────────────────────────────

/// Records a wire frame (one with a packet number) at lifecycle `lifecycle`. The frame kind and all
/// its fields are pulled from `header`; `pn`/`sender_id` are the packet number and the sender that
/// assigned it; `reason` tags a [`Lifecycle::RxDropped`] record ([`DropReason::None`] otherwise).
#[inline]
pub(crate) fn wire(
    lifecycle: Lifecycle,
    header: &Header,
    pn: VarInt,
    sender_id: VarInt,
    reason: DropReason,
    cred_id: CredId,
) {
    super::dbg::on_enabled(|| {
        emit_header(
            lifecycle,
            header,
            pn.as_u64(),
            sender_id.as_u64(),
            reason,
            cred_id,
        )
    });
}

/// Records an [`Lifecycle::AppSend`] sighting — a frame submitted into the send pipeline before a
/// packet number is assigned. The kind and fields come from `header`; `packet_number`/`sender_id`
/// are [`NO_PACKET_NUMBER`].
#[inline]
pub(crate) fn app_send(header: &Header, cred_id: CredId) {
    super::dbg::on_enabled(|| {
        emit_header(
            Lifecycle::AppSend,
            header,
            NO_PACKET_NUMBER,
            NO_PACKET_NUMBER,
            DropReason::None,
            cred_id,
        )
    });
}

/// Records an [`Lifecycle::AppRecv`] delivery — reassembled stream bytes handed to the application.
/// There is no wire `Header` at this layer, so it is synthesised as a [`QueueDataFrame`] from the
/// reader's routing identity: `offset` is the consumed stream offset, `delivered` the bytes handed
/// over. No packet number.
#[inline]
pub(crate) fn app_recv(
    source_queue_id: VarInt,
    dest_queue_id: VarInt,
    binding_id: VarInt,
    offset: u64,
    delivered: u32,
    cred_id: CredId,
) {
    super::dbg::on_enabled(|| {
        backbeat::global::record(&QueueDataFrame {
            packet_number: NO_PACKET_NUMBER,
            sender_id: NO_PACKET_NUMBER,
            source_queue_id: source_queue_id.as_u64(),
            dest_queue_id: dest_queue_id.as_u64(),
            binding_id: binding_id.as_u64(),
            offset,
            largest_offset: 0,
            cred_id: canonicalize_cred(*cred_id),
            payload_len: delivered,
            lifecycle: Lifecycle::AppRecv,
            reason: DropReason::None,
            is_fin: false,
            blocked: false,
            is_init: false,
            _pad: [0; 7],
        })
    });
}

/// Records a reader-detected drop ([`Lifecycle::RxDropped`] with no wire `Header`), e.g. a
/// receive-window violation. Synthesised as a [`QueueDataFrame`] from the reader's routing identity.
#[inline]
pub(crate) fn reader_drop(
    source_queue_id: VarInt,
    dest_queue_id: VarInt,
    binding_id: VarInt,
    offset: u64,
    reason: DropReason,
    cred_id: CredId,
) {
    super::dbg::on_enabled(|| {
        backbeat::global::record(&QueueDataFrame {
            packet_number: NO_PACKET_NUMBER,
            sender_id: NO_PACKET_NUMBER,
            source_queue_id: source_queue_id.as_u64(),
            dest_queue_id: dest_queue_id.as_u64(),
            binding_id: binding_id.as_u64(),
            offset,
            largest_offset: 0,
            cred_id: canonicalize_cred(*cred_id),
            payload_len: 0,
            lifecycle: Lifecycle::RxDropped,
            reason,
            is_fin: false,
            blocked: false,
            is_init: false,
            _pad: [0; 7],
        })
    });
}

/// Records a per-packet [`PacketRecord`]. `linked_pn` is the probed-from shell PN for a
/// [`PacketEvent::Sent`] probe (else `None`); `sent_bytes`/`frame_count` describe a `Sent` segment
/// (else 0); `reason` carries the pre-decode drop reason for [`PacketEvent::RxDropped`].
#[inline]
#[allow(clippy::too_many_arguments)]
pub(crate) fn packet(
    event: PacketEvent,
    pn: VarInt,
    sender_id: VarInt,
    cred_id: CredId,
    sent_bytes: u32,
    frame_count: u16,
    linked_pn: Option<VarInt>,
    reason: DropReason,
) {
    super::dbg::on_enabled(|| {
        backbeat::global::record(&PacketRecord {
            packet_number: pn.as_u64(),
            sender_id: sender_id.as_u64(),
            linked_pn: linked_pn.map_or(NO_PACKET_NUMBER, |p| p.as_u64()),
            cred_id: canonicalize_cred(*cred_id),
            sent_bytes,
            frame_count,
            event,
            reason,
        })
    });
}

/// Requests an asynchronous dump of the recorder's rings to disk. A no-op when disabled.
#[inline]
pub(crate) fn trigger() {
    super::dbg::on_enabled(backbeat::global::trigger);
}

/// The single authoritative `Header` → per-kind-event dispatch. Builds the event for `header`'s kind
/// with its real fields and records it. Caller has already checked the diagnostic is enabled.
#[inline]
fn emit_header(
    lifecycle: Lifecycle,
    header: &Header,
    packet_number: u64,
    sender_id: u64,
    reason: DropReason,
    cred_id: CredId,
) {
    let cred = canonicalize_cred(*cred_id);
    match *header {
        Header::QueueData {
            queue_pair,
            binding_id,
            offset,
            largest_offset,
            is_fin,
            blocked,
            dest_acceptor_id,
            ..
        } => backbeat::global::record(&QueueDataFrame {
            packet_number,
            sender_id,
            source_queue_id: queue_pair.source_queue_id.as_u64(),
            dest_queue_id: queue_pair.dest_queue_id.as_u64(),
            binding_id: binding_id.as_u64(),
            offset: offset.as_u64(),
            largest_offset: largest_offset.as_u64(),
            cred_id: cred,
            payload_len: 0,
            lifecycle,
            reason,
            is_fin,
            blocked,
            is_init: dest_acceptor_id.is_some(),
            _pad: [0; 7],
        }),
        Header::QueueMsg {
            queue_pair,
            binding_id,
            msg_id,
            stream_offset,
            largest_offset,
            message_size,
            chunk_size,
            chunk_index,
            is_fin,
            is_wakeup,
            blocked,
            dest_acceptor_id,
            ..
        } => backbeat::global::record(&QueueMsgFrame {
            packet_number,
            sender_id,
            source_queue_id: queue_pair.source_queue_id.as_u64(),
            dest_queue_id: queue_pair.dest_queue_id.as_u64(),
            binding_id: binding_id.as_u64(),
            stream_offset: stream_offset.as_u64(),
            largest_offset: largest_offset.as_u64(),
            msg_id: msg_id.as_u64(),
            message_size: message_size.as_u64(),
            chunk_size: chunk_size.as_u64(),
            chunk_index: chunk_index.as_u64(),
            cred_id: cred,
            lifecycle,
            reason,
            is_fin,
            is_wakeup,
            blocked,
            is_init: dest_acceptor_id.is_some(),
            _pad: [0; 2],
        }),
        Header::QueueMaxData {
            queue_pair,
            binding_id,
            maximum_data,
        } => backbeat::global::record(&QueueMaxDataFrame {
            packet_number,
            sender_id,
            source_queue_id: queue_pair.source_queue_id.as_u64(),
            dest_queue_id: queue_pair.dest_queue_id.as_u64(),
            binding_id: binding_id.as_u64(),
            maximum_data: maximum_data.as_u64(),
            cred_id: cred,
            lifecycle,
            reason,
            _pad: [0; 6],
        }),
        Header::QueueDataBlocked {
            queue_pair,
            binding_id,
            desired_offset,
        } => backbeat::global::record(&QueueDataBlockedFrame {
            packet_number,
            sender_id,
            source_queue_id: queue_pair.source_queue_id.as_u64(),
            dest_queue_id: queue_pair.dest_queue_id.as_u64(),
            binding_id: binding_id.as_u64(),
            desired_offset: desired_offset.as_u64(),
            cred_id: cred,
            lifecycle,
            reason,
            _pad: [0; 6],
        }),
        Header::QueueReset {
            queue_pair,
            binding_id,
            reset_target,
            error_code,
            init,
            ..
        } => backbeat::global::record(&QueueResetFrame {
            packet_number,
            sender_id,
            source_queue_id: queue_pair.source_queue_id.as_u64(),
            dest_queue_id: queue_pair.dest_queue_id.as_u64(),
            binding_id: binding_id.as_u64(),
            error_code: error_code.as_u64(),
            cred_id: cred,
            lifecycle,
            reason,
            reset_target: reset_target.into(),
            is_init: init.is_some(),
            _pad: [0; 4],
        }),
        Header::QueueFree {
            free_request_id,
            smallest_queue_id,
        } => backbeat::global::record(&QueueFreeFrame {
            packet_number,
            sender_id,
            free_request_id: free_request_id.as_u64(),
            smallest_queue_id: smallest_queue_id.as_u64(),
            cred_id: cred,
            lifecycle,
            reason,
            _pad: [0; 6],
        }),
        Header::Ack {
            dest_sender_id,
            largest_acknowledged,
            is_ack_eliciting,
            ..
        } => backbeat::global::record(&AckFrame {
            packet_number,
            sender_id,
            dest_sender_id: dest_sender_id.as_u64(),
            largest_acknowledged: largest_acknowledged.as_u64(),
            cred_id: cred,
            lifecycle,
            reason,
            is_ack_eliciting,
            _pad: [0; 5],
        }),
        Header::QueueDbg {
            dump_id,
            queue_pair,
            binding_id,
        } => backbeat::global::record(&QueueDbgFrame {
            packet_number,
            sender_id,
            source_queue_id: queue_pair.source_queue_id.as_u64(),
            dest_queue_id: queue_pair.dest_queue_id.as_u64(),
            binding_id: binding_id.as_u64(),
            dump_id: dump_id.as_u64(),
            cred_id: cred,
            lifecycle,
            reason,
            _pad: [0; 6],
        }),
        Header::QueueControl {
            queue_pair,
            binding_id,
        } => backbeat::global::record(&QueueControlFrame {
            packet_number,
            sender_id,
            source_queue_id: queue_pair.source_queue_id.as_u64(),
            dest_queue_id: queue_pair.dest_queue_id.as_u64(),
            binding_id: binding_id.as_u64(),
            cred_id: cred,
            lifecycle,
            reason,
            _pad: [0; 6],
        }),
        Header::Ping => backbeat::global::record(&PingFrame {
            packet_number,
            sender_id,
            cred_id: cred,
            lifecycle,
            reason,
            _pad: [0; 6],
        }),
    }
}

#[cfg(test)]
impl Lifecycle {
    pub(crate) fn as_u8(self) -> u8 {
        self as u8
    }
}

#[cfg(test)]
impl PacketEvent {
    pub(crate) fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Test-only: `(EventId, offset_of!(lifecycle))` for every frame event type, so the test reader can
/// pull the `lifecycle` discriminant out of any frame record by its `event_id`.
#[cfg(test)]
const FRAME_LIFECYCLE: &[(backbeat::EventId, usize)] = &[
    (
        QueueDataFrame::ID,
        core::mem::offset_of!(QueueDataFrame, lifecycle),
    ),
    (
        QueueMsgFrame::ID,
        core::mem::offset_of!(QueueMsgFrame, lifecycle),
    ),
    (
        QueueMaxDataFrame::ID,
        core::mem::offset_of!(QueueMaxDataFrame, lifecycle),
    ),
    (
        QueueDataBlockedFrame::ID,
        core::mem::offset_of!(QueueDataBlockedFrame, lifecycle),
    ),
    (
        QueueResetFrame::ID,
        core::mem::offset_of!(QueueResetFrame, lifecycle),
    ),
    (
        QueueFreeFrame::ID,
        core::mem::offset_of!(QueueFreeFrame, lifecycle),
    ),
    (AckFrame::ID, core::mem::offset_of!(AckFrame, lifecycle)),
    (
        QueueDbgFrame::ID,
        core::mem::offset_of!(QueueDbgFrame, lifecycle),
    ),
    (
        QueueControlFrame::ID,
        core::mem::offset_of!(QueueControlFrame, lifecycle),
    ),
    (PingFrame::ID, core::mem::offset_of!(PingFrame, lifecycle)),
];

/// Test-only snapshot of which kinds are currently resident in the global recorder: the set of
/// [`Lifecycle`] discriminants across all frame events, and the set of [`PacketEvent`] discriminants
/// across [`PacketRecord`]s. Lets an end-to-end test assert the trace points fire during a real
/// transfer without an on-disk dump.
#[cfg(test)]
pub(crate) fn resident_event_kinds() -> (
    std::collections::BTreeSet<u8>,
    std::collections::BTreeSet<u8>,
) {
    use backbeat::record::RecordView;

    let bytes = backbeat::global::recorder().dump(
        backbeat::registry::schemas(),
        core::iter::empty(),
        backbeat::registry::views(),
        "",
    );

    let mut lifecycles = std::collections::BTreeSet::new();
    let mut packet_events = std::collections::BTreeSet::new();

    let Ok(reader) = backbeat::wire::DumpReader::new(bytes) else {
        return (lifecycles, packet_events);
    };
    let Ok(shards) = reader.shards() else {
        return (lifecycles, packet_events);
    };
    for shard in shards {
        backbeat::ring::walk(
            &shard.region,
            shard.head as usize,
            shard.capacity as usize,
            |payload| {
                let Some(view) = RecordView::parse(&payload) else {
                    return false;
                };
                if view.event_id == PacketRecord::ID {
                    if let Some(&b) = view.fields.get(core::mem::offset_of!(PacketRecord, event)) {
                        packet_events.insert(b);
                    }
                } else if let Some(&(_, off)) =
                    FRAME_LIFECYCLE.iter().find(|(id, _)| *id == view.event_id)
                {
                    if let Some(&b) = view.fields.get(off) {
                        lifecycles.insert(b);
                    }
                }
                true
            },
        );
    }

    (lifecycles, packet_events)
}
