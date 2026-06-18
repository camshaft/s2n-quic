// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Global frame flight recorder.
//!
//! Every frame the endpoint observes is recorded as a compact fixed-size [`FrameRecord`] into a
//! single process-wide [`BumpRing`]. The hot path is just an atomic `fetch_add` plus a memcpy (see
//! [`crate::sync::bump_ring`]). Frames are captured across the whole lifecycle:
//!
//! * **App edges** — [`Direction::AppSend`] when the application submits a frame into the send
//!   pipeline (before aggregation/credit/pacing/assembly), and [`Direction::AppRecv`] when
//!   reassembled stream bytes are delivered to the application. These bracket the transport so the
//!   submit→wire and wire→consume latencies (where lost-wakeup stalls hide) become visible.
//! * **Wire** — [`Direction::Inbound`]/[`Direction::InboundFastPath`] at decode, [`Direction::Outbound`]
//!   at assembly.
//! * **Completion** — [`Direction::AckCompleted`], [`Direction::AckLost`], [`Direction::AckCancelled`].
//! * **Drops** — [`Direction::SendCancelled`] (cancelled at assembly before the wire) and
//!   [`Direction::RxDropped`] (rejected after decode, e.g. stale/future binding), each tagged with a
//!   [`DropReason`] so a frame that silently dies between app and wire is no longer a blind spot.
//!
//! Interleaved with the per-frame records are coarser per-packet [`PacketRecord`]s (a distinct,
//! smaller record discriminated by length and its own [`PACKET_RECORD_MAGIC`]). They capture each
//! packet's lifecycle — received off the wire (the earliest possible sighting of a packet number,
//! before decrypt/dedup, so loss *before* processing is immediately distinguishable from loss on
//! the wire), assembled/sent, acked, lost, or dropped pre-decode (decrypt/replay/dedup). Because
//! every frame record carries its `packet_number`, frames can be grouped into their packet; the
//! packet records add the packet-only signals a frame can't carry: wire byte count, frame count,
//! probe shell→pn linkage, and the pre-decode drop reasons that have no frame to attach to.
//!
//! Unlike the per-`dump_id` [`QueueDbg`](super::frame::Header::QueueDbg) diagnostic — which traces
//! *one* stuck frame end-to-end — this records *all* frames, so the dump shows the surrounding
//! history (what else was in flight, in what order, with what offsets/flags). Each `QueueDbg` we
//! receive [triggers](trigger) a background thread to write the whole ring to disk. Every dump
//! goes to its own sequence-numbered file (`…​.000.bin`, `…​.001.bin`, …), so the first dump — the
//! most valuable, taken closest to when the stream was noticed stuck — is never overwritten.
//!
//! The whole facility is gated behind [`super::dbg::ENABLED`]: in plain release builds [`record`]
//! and [`trigger`] fold to nothing and no ring is ever allocated. It is active under `test`, the
//! `testing` feature, or the `queue-dbg` feature — the same builds that already speak `QueueDbg`.
//!
//! Environment overrides (read once at first use): `S2N_DC_FRAME_TRACE_PATH` (base dump path),
//! `S2N_DC_FRAME_TRACE_BYTES` (ring capacity), and `S2N_DC_FRAME_TRACE_THROTTLE_MS` (minimum
//! interval between dumps; back-to-back triggers within it are dropped to cap disk churn).

use super::frame::Header;
use crate::sync::BumpRing;
use s2n_quic_core::varint::VarInt;
use std::{
    io::Write as _,
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Condvar, Mutex, OnceLock,
    },
    time::{Instant, SystemTime},
};

/// Magic bytes at the front of every [`FrameRecord`] payload. Combined with the ring's trailing
/// length suffix, this lets the backwards-walk reject records whose tail was overwritten.
const RECORD_MAGIC: u8 = 0xF7;

/// Magic byte at the front of every [`PacketRecord`] payload (distinct from [`RECORD_MAGIC`] so a
/// parser can tell the two record types apart). The records are also different sizes, so the parser
/// discriminates first on the ring's length suffix and then validates the matching magic.
const PACKET_RECORD_MAGIC: u8 = 0xF8;

/// On-disk file magic. Bumped if [`FrameRecord`] or the framing ever changes.
const FILE_MAGIC: [u8; 8] = *b"DCFTRC01";
/// Dump format version. v2 added [`Direction`] variants for the application edges and drop sites,
/// the [`FrameRecord::reason`]/[`FrameRecord::payload_len`] tail, and interleaved [`PacketRecord`]s.
/// A v1 parser reads v2 frame records correctly for the leading 80 bytes but does not know about the
/// new directions or packet records; bump again on any further layout change.
const FILE_VERSION: u32 = 2;

/// Default ring capacity (bytes) when `S2N_DC_FRAME_TRACE_BYTES` is unset. 16 MiB ≈ 256k records.
const DEFAULT_CAPACITY: usize = 16 << 20;

/// Sentinel stored in [`FrameRecord::packet_number`] when a frame has no packet number yet.
const NO_PACKET_NUMBER: u64 = u64::MAX;

/// The endpoint bit in byte 0 of a `credentials::Id` (mirrors the private `Id::ENDPOINT_BIT`). It
/// flips between the server and client view of the same id, so it is masked off before storing the
/// id in a record — see [`build`].
const CRED_ENDPOINT_BIT: u8 = 0x80;

/// Where a frame was observed. Stored in [`FrameRecord::direction`].
///
/// Discriminants are stable on-disk values — append new variants, never renumber. The offline
/// parser ([`examples/frame_trace_dump.rs`]) maps them to short labels and must be kept in sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Direction {
    /// Received in a multi-frame packet (slow path).
    Inbound = 0,
    /// Received as a single-QueueMsg fast-path packet.
    InboundFastPath = 1,
    /// Emitted at packet assembly.
    Outbound = 2,
    /// Acknowledged by the peer.
    AckCompleted = 3,
    /// Declared lost (will be retransmitted).
    AckLost = 4,
    /// Cancelled before delivery (sender gone) or TTL-exhausted.
    AckCancelled = 5,
    /// Submitted by the application into the send pipeline (before aggregation, credit, pacing, and
    /// assembly). The first sighting of an app-originated frame; pairs with [`Outbound`] to expose
    /// the submit→wire latency. `packet_number` is not yet assigned (sentinel).
    ///
    /// [`Outbound`]: Direction::Outbound
    AppSend = 6,
    /// Reassembled stream bytes delivered to the application by the reader. The last sighting of
    /// received data; pairs with [`Inbound`] to expose the wire→consume latency where reader
    /// lost-wakeup stalls hide. Synthesised from the reader's routing identity (there is no wire
    /// `Header` at this layer — see [`record_app_recv`]), so it is always a `QueueData`-kind record
    /// carrying the delivered byte count in [`FrameRecord::payload_len`] and the consumed stream
    /// offset in [`FrameRecord::offset`].
    ///
    /// [`Inbound`]: Direction::Inbound
    AppRecv = 7,
    /// Cancelled at assembly before ever reaching the wire — the emitting handle was already gone
    /// (`should_transmit()` was false) when the assembler popped it. Distinct from [`AckCancelled`],
    /// which is a *post-send* cancellation discovered during loss detection. `packet_number` is the
    /// PN the frame would have been assigned.
    ///
    /// [`AckCancelled`]: Direction::AckCancelled
    SendCancelled = 8,
    /// Rejected after decode on the receive path (e.g. unallocated/stale/future binding, half-closed,
    /// cap-exceeded, window violation). The frame authenticated and decoded but never reached the
    /// application; [`FrameRecord::reason`] carries a [`DropReason`].
    RxDropped = 9,
}

/// Why a frame or packet was dropped, stored in [`FrameRecord::reason`] / [`PacketRecord::reason`].
///
/// Discriminants are stable on-disk values — append, never renumber. `None` (0) is the resting
/// value for records that did not record a drop. The receive-side post-decode reasons mirror the
/// `crate::queue::Error` variants the dispatcher already counts; the pre-decode reasons have no
/// frame to attach to and only appear on a [`PacketRecord`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum DropReason {
    /// Not a drop (resting value).
    None = 0,
    // ── post-decode, per-frame (Direction::RxDropped) ──
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

/// A compact, fixed-size record of one observed frame.
///
/// Field order places the 8-byte fields first, then the 16-byte (1-aligned) credential id, then the
/// `u32`/`u8` tail — so the struct is naturally aligned with no implicit padding (a requirement for
/// the `zerocopy::IntoBytes` derive). Total size is 88 bytes.
///
/// The leading 80 bytes are unchanged from the v1 layout (a v1 parser still reads them correctly);
/// the `reason`/`_pad`/`payload_len` tail is appended for v2.
#[repr(C)]
#[derive(
    Clone,
    Copy,
    Debug,
    zerocopy::IntoBytes,
    zerocopy::FromBytes,
    zerocopy::Immutable,
    zerocopy::KnownLayout,
)]
pub struct FrameRecord {
    /// Nanoseconds since the Unix epoch (wall clock). In bach simulations this is the
    /// simulated time since epoch 0, deterministic and host-independent. In production the wall
    /// clock is latched once at recorder startup so that records from different hosts are directly
    /// comparable.
    pub timestamp_nanos: u64,
    /// Packet number, or [`NO_PACKET_NUMBER`] when not yet assigned.
    pub packet_number: u64,
    /// Primary stream/control offset for the frame type (see [`build`]).
    pub offset: u64,
    /// `QueueDbg` dump id, else 0.
    pub dump_id: u64,
    pub source_queue_id: u64,
    pub dest_queue_id: u64,
    pub binding_id: u64,
    /// The 16-byte credential id (`credentials::Id`) of the path this frame belongs to, big-endian
    /// as on the wire. This is the join key against the existing QueueDbg / dispatch log lines,
    /// which all print `credentials=0x…`; it also disambiguates streams that reuse a `queue_id`
    /// across different sessions. Zero when no credential is in scope (e.g. [`Header::Ping`] paths).
    pub cred_id: [u8; 16],
    /// Global monotonic sequence number — definitive ordering across all directions.
    pub seq: u32,
    /// Always [`RECORD_MAGIC`]; a secondary torn-record guard for the parser.
    pub magic: u8,
    /// A [`Direction`] discriminant.
    pub direction: u8,
    /// Frame kind, see [`header_kind`].
    pub frame_type: u8,
    /// Bit flags: see the `FLAG_*` constants.
    pub flags: u8,
    /// A [`DropReason`] discriminant for [`Direction::RxDropped`]; [`DropReason::None`] otherwise.
    pub reason: u8,
    /// Padding to keep the following `u32` naturally aligned and the struct free of implicit padding
    /// (required by `zerocopy::IntoBytes`). Always zero.
    pub _pad: [u8; 3],
    /// Payload byte count where meaningful: bytes delivered for [`Direction::AppRecv`], else 0.
    pub payload_len: u32,
}

// Flag bits packed into `FrameRecord::flags`.
const FLAG_FIN: u8 = 1 << 0;
const FLAG_BLOCKED: u8 = 1 << 1;
const FLAG_WAKEUP: u8 = 1 << 2;
const FLAG_ACK_ELICITING: u8 = 1 << 3;
const FLAG_INIT: u8 = 1 << 4;

// Stable frame-kind discriminants for the parser. Independent of the private wire type tags.
const KIND_QUEUE_DATA: u8 = 1;
const KIND_QUEUE_CONTROL: u8 = 2;
const KIND_QUEUE_MAX_DATA: u8 = 3;
const KIND_QUEUE_RESET: u8 = 4;
const KIND_QUEUE_FREE: u8 = 5;
const KIND_ACK: u8 = 6;
const KIND_QUEUE_MSG: u8 = 7;
const KIND_PING: u8 = 8;
const KIND_QUEUE_DATA_BLOCKED: u8 = 9;
const KIND_QUEUE_DBG: u8 = 10;

/// A packet's place in its lifecycle. Stored in [`PacketRecord::event`].
///
/// Discriminants are stable on-disk values — append, never renumber.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PacketEvent {
    /// Received off the wire — recorded the instant the packet number is known, *before* decrypt,
    /// dedup, and frame dispatch. This is the earliest possible sighting of an inbound packet, so a
    /// gap in the received-PN sequence here pins loss to the wire (vs. loss after processing).
    RxArrived = 0,
    /// Dropped before its frames could be decoded: see [`PacketRecord::reason`]
    /// ([`DropReason::DecryptFailed`], [`ReplayDetected`], [`Duplicate`], [`PathSecretNotFound`]).
    ///
    /// [`ReplayDetected`]: DropReason::ReplayDetected
    /// [`Duplicate`]: DropReason::Duplicate
    /// [`PathSecretNotFound`]: DropReason::PathSecretNotFound
    RxDropped = 1,
    /// Assembled and sent on the wire. [`PacketRecord::frame_count`] and [`PacketRecord::sent_bytes`]
    /// describe the segment; [`PacketRecord::linked_pn`] is the probed-from shell PN for a PTO probe.
    Sent = 2,
    /// Acknowledged by the peer.
    Acked = 3,
    /// Declared lost by loss detection. [`PacketRecord::reason`] distinguishes the trigger
    /// ([`DropReason::None`] = packet-number threshold; a time-threshold loss sets no frame reason
    /// either — both are recorded as `Lost`, see the call site).
    Lost = 4,
}

/// A compact per-packet record, interleaved in the same ring as [`FrameRecord`] but smaller (a
/// parser discriminates by the ring length suffix, then validates [`PACKET_RECORD_MAGIC`]).
///
/// Captures the packet-only signals a frame record can't carry: the wire byte count, how many
/// frames shared the packet, a probe's shell→pn linkage, and the pre-decode drop reasons that have
/// no decoded frame to attach to. Frames are still tied to their packet through the shared
/// `packet_number`.
///
/// Field order keeps the 8-byte fields first, then the 16-byte credential id, then the `u32`/`u8`
/// tail, so the layout has no implicit padding. Total size is 56 bytes — deliberately different
/// from [`FrameRecord`]'s 88 so the two are distinguishable by length alone.
#[repr(C)]
#[derive(
    Clone,
    Copy,
    Debug,
    zerocopy::IntoBytes,
    zerocopy::FromBytes,
    zerocopy::Immutable,
    zerocopy::KnownLayout,
)]
pub struct PacketRecord {
    /// Nanoseconds since the Unix epoch (shares the wall clock and `seq` space with [`FrameRecord`]).
    pub timestamp_nanos: u64,
    /// The packet number this record is about.
    pub packet_number: u64,
    /// For [`PacketEvent::Sent`] probes, the shell PN this packet was probed from; else
    /// [`NO_PACKET_NUMBER`]. Links a retransmit back to the transmission it replaced.
    pub linked_pn: u64,
    /// Canonicalized credential id (endpoint bit cleared), matching [`FrameRecord::cred_id`].
    pub cred_id: [u8; 16],
    /// Global monotonic sequence number — shares the counter with [`FrameRecord::seq`], so packet
    /// and frame records have a single definitive ordering.
    pub seq: u32,
    /// Wire byte count for [`PacketEvent::Sent`] (else 0).
    pub sent_bytes: u32,
    /// Number of frames in the packet for [`PacketEvent::Sent`] (else 0).
    pub frame_count: u16,
    /// Always [`PACKET_RECORD_MAGIC`]; torn-record guard and the type discriminator vs. a frame.
    pub magic: u8,
    /// A [`PacketEvent`] discriminant.
    pub event: u8,
    /// A [`DropReason`] discriminant for [`PacketEvent::RxDropped`]; [`DropReason::None`] otherwise.
    pub reason: u8,
    /// Padding to keep the struct free of implicit padding (required by `zerocopy::IntoBytes`).
    /// Always zero.
    pub _pad: [u8; 3],
}

/// Maps a [`Header`] to its stable kind discriminant.
pub fn header_kind(header: &Header) -> u8 {
    match header {
        Header::QueueData { .. } => KIND_QUEUE_DATA,
        Header::QueueControl { .. } => KIND_QUEUE_CONTROL,
        Header::QueueMaxData { .. } => KIND_QUEUE_MAX_DATA,
        Header::QueueReset { .. } => KIND_QUEUE_RESET,
        Header::QueueFree { .. } => KIND_QUEUE_FREE,
        Header::Ack { .. } => KIND_ACK,
        Header::QueueMsg { .. } => KIND_QUEUE_MSG,
        Header::Ping => KIND_PING,
        Header::QueueDataBlocked { .. } => KIND_QUEUE_DATA_BLOCKED,
        Header::QueueDbg { .. } => KIND_QUEUE_DBG,
    }
}

/// Builds a [`FrameRecord`] from a frame header. `pn` is the packet number if known; `cred_id` is
/// the 16-byte credential id of the path (all zeros when none is in scope).
///
/// `seq` and `timestamp_nanos` are stamped by the caller from the global recorder state.
fn build(
    seq: u32,
    timestamp_nanos: u64,
    dir: Direction,
    header: &Header,
    pn: Option<VarInt>,
    mut cred_id: [u8; 16],
) -> FrameRecord {
    // Canonicalize the credential id by masking off the endpoint bit (MSB of byte 0). That bit
    // encodes which side a `credentials::Id` is viewed from — the server and client hold the same
    // logical id with this bit flipped (see `Id::for_endpoint`/`for_peer`). Clearing it makes a
    // record from either endpoint share one id, so a single grep correlates both ends of a stream.
    cred_id[0] &= !CRED_ENDPOINT_BIT;
    let mut rec = FrameRecord {
        timestamp_nanos,
        packet_number: pn.map_or(NO_PACKET_NUMBER, |p| p.as_u64()),
        offset: 0,
        dump_id: 0,
        source_queue_id: 0,
        dest_queue_id: 0,
        binding_id: 0,
        cred_id,
        seq,
        magic: RECORD_MAGIC,
        direction: dir as u8,
        frame_type: header_kind(header),
        flags: 0,
        reason: DropReason::None as u8,
        _pad: [0; 3],
        payload_len: 0,
    };

    // Pull the routing identity and a single "primary" offset out of each variant. `flags` carries
    // the boolean signals that matter for stuck-stream diagnosis.
    match *header {
        Header::QueueData {
            queue_pair,
            binding_id,
            offset,
            is_fin,
            blocked,
            dest_acceptor_id,
            ..
        } => {
            rec.source_queue_id = queue_pair.source_queue_id.as_u64();
            rec.dest_queue_id = queue_pair.dest_queue_id.as_u64();
            rec.binding_id = binding_id.as_u64();
            rec.offset = offset.as_u64();
            if is_fin {
                rec.flags |= FLAG_FIN;
            }
            if blocked {
                rec.flags |= FLAG_BLOCKED;
            }
            if dest_acceptor_id.is_some() {
                rec.flags |= FLAG_INIT;
            }
        }
        Header::QueueMsg {
            queue_pair,
            binding_id,
            stream_offset,
            is_fin,
            is_wakeup,
            blocked,
            dest_acceptor_id,
            ..
        } => {
            rec.source_queue_id = queue_pair.source_queue_id.as_u64();
            rec.dest_queue_id = queue_pair.dest_queue_id.as_u64();
            rec.binding_id = binding_id.as_u64();
            rec.offset = stream_offset.as_u64();
            if is_fin {
                rec.flags |= FLAG_FIN;
            }
            if is_wakeup {
                rec.flags |= FLAG_WAKEUP;
            }
            if blocked {
                rec.flags |= FLAG_BLOCKED;
            }
            if dest_acceptor_id.is_some() {
                rec.flags |= FLAG_INIT;
            }
        }
        Header::QueueControl {
            queue_pair,
            binding_id,
        } => {
            rec.source_queue_id = queue_pair.source_queue_id.as_u64();
            rec.dest_queue_id = queue_pair.dest_queue_id.as_u64();
            rec.binding_id = binding_id.as_u64();
        }
        Header::QueueMaxData {
            queue_pair,
            binding_id,
            maximum_data,
        } => {
            rec.source_queue_id = queue_pair.source_queue_id.as_u64();
            rec.dest_queue_id = queue_pair.dest_queue_id.as_u64();
            rec.binding_id = binding_id.as_u64();
            rec.offset = maximum_data.as_u64();
        }
        Header::QueueDataBlocked {
            queue_pair,
            binding_id,
            desired_offset,
        } => {
            rec.source_queue_id = queue_pair.source_queue_id.as_u64();
            rec.dest_queue_id = queue_pair.dest_queue_id.as_u64();
            rec.binding_id = binding_id.as_u64();
            rec.offset = desired_offset.as_u64();
        }
        Header::QueueReset {
            queue_pair,
            binding_id,
            error_code,
            init,
            ..
        } => {
            rec.source_queue_id = queue_pair.source_queue_id.as_u64();
            rec.dest_queue_id = queue_pair.dest_queue_id.as_u64();
            rec.binding_id = binding_id.as_u64();
            rec.offset = error_code.as_u64();
            if init.is_some() {
                rec.flags |= FLAG_INIT;
            }
        }
        Header::QueueFree {
            free_request_id,
            smallest_queue_id,
        } => {
            rec.binding_id = free_request_id.as_u64();
            rec.offset = smallest_queue_id.as_u64();
        }
        Header::Ack {
            dest_sender_id,
            largest_acknowledged,
            is_ack_eliciting,
            ..
        } => {
            rec.dest_queue_id = dest_sender_id.as_u64();
            rec.offset = largest_acknowledged.as_u64();
            if is_ack_eliciting {
                rec.flags |= FLAG_ACK_ELICITING;
            }
        }
        Header::QueueDbg {
            dump_id,
            queue_pair,
            binding_id,
        } => {
            rec.source_queue_id = queue_pair.source_queue_id.as_u64();
            rec.dest_queue_id = queue_pair.dest_queue_id.as_u64();
            rec.binding_id = binding_id.as_u64();
            rec.dump_id = dump_id.as_u64();
        }
        Header::Ping => {}
    }

    rec
}

/// Records one observed frame into the global ring. A no-op (folded away) when the diagnostic is
/// disabled.
///
/// `cred_id` is the credential id of the path the frame belongs to — the join key against the
/// QueueDbg / dispatch logs. Pass [`crate::credentials::Id`] (it derefs to `[u8; 16]`).
#[inline]
pub(crate) fn record(
    dir: Direction,
    header: &Header,
    pn: Option<VarInt>,
    cred_id: crate::credentials::Id,
) {
    super::dbg::on_enabled(|| {
        push_frame(build_stamped(dir, header, pn, *cred_id));
    });
}

/// Records a frame that was rejected after decode on the receive path ([`Direction::RxDropped`]),
/// tagging it with the [`DropReason`]. A no-op when disabled.
///
/// Use this from the dispatch handlers' error arms (e.g. `handle_queue_data`) so a frame that
/// authenticated and decoded but never reached the application leaves a trace instead of vanishing.
#[inline]
pub(crate) fn record_drop(
    header: &Header,
    pn: VarInt,
    cred_id: crate::credentials::Id,
    reason: DropReason,
) {
    super::dbg::on_enabled(|| {
        let mut rec = build_stamped(Direction::RxDropped, header, Some(pn), *cred_id);
        rec.reason = reason as u8;
        push_frame(rec);
    });
}

/// Records the delivery of reassembled stream bytes to the application ([`Direction::AppRecv`]).
/// A no-op when disabled.
///
/// There is no wire [`Header`] at the reader's delivery point — only the routing identity and the
/// byte range just handed to the app — so this builds a `QueueData`-kind record directly from the
/// reader's `source`/`dest` queue ids, binding id, the consumed stream `offset`, and the number of
/// bytes `delivered` in this read. `pn` is unknown at this layer (sentinel).
#[inline]
pub(crate) fn record_app_recv(
    source_queue_id: VarInt,
    dest_queue_id: VarInt,
    binding_id: VarInt,
    offset: u64,
    delivered: u32,
    cred_id: crate::credentials::Id,
) {
    super::dbg::on_enabled(|| {
        push_frame(build_stream_record(
            Direction::AppRecv,
            source_queue_id,
            dest_queue_id,
            binding_id,
            offset,
            delivered,
            DropReason::None,
            *cred_id,
        ));
    });
}

/// Records a stream-layer receive drop ([`Direction::RxDropped`]) detected in the reader rather than
/// the dispatcher — there is no wire [`Header`] in scope here either. Used for the receive-window
/// violation ([`DropReason::WindowViolation`]): the peer sent data past the advertised window, which
/// resets the stream. `offset` is the rejected frame's stream offset.
#[inline]
pub(crate) fn record_reader_drop(
    source_queue_id: VarInt,
    dest_queue_id: VarInt,
    binding_id: VarInt,
    offset: u64,
    reason: DropReason,
    cred_id: crate::credentials::Id,
) {
    super::dbg::on_enabled(|| {
        push_frame(build_stream_record(
            Direction::RxDropped,
            source_queue_id,
            dest_queue_id,
            binding_id,
            offset,
            0,
            reason,
            *cred_id,
        ));
    });
}

/// Builds a `QueueData`-kind [`FrameRecord`] from a stream-layer routing identity (no wire `Header`
/// in scope). Shared by [`record_app_recv`] and [`record_reader_drop`]. Stamps a fresh seq/ts and
/// canonicalizes the credential id.
#[inline]
#[allow(clippy::too_many_arguments)]
fn build_stream_record(
    dir: Direction,
    source_queue_id: VarInt,
    dest_queue_id: VarInt,
    binding_id: VarInt,
    offset: u64,
    payload_len: u32,
    reason: DropReason,
    mut cred_id: [u8; 16],
) -> FrameRecord {
    let (seq, ts) = stamp(state());
    cred_id[0] &= !CRED_ENDPOINT_BIT;
    FrameRecord {
        timestamp_nanos: ts,
        packet_number: NO_PACKET_NUMBER,
        offset,
        dump_id: 0,
        source_queue_id: source_queue_id.as_u64(),
        dest_queue_id: dest_queue_id.as_u64(),
        binding_id: binding_id.as_u64(),
        cred_id,
        seq,
        magic: RECORD_MAGIC,
        direction: dir as u8,
        frame_type: KIND_QUEUE_DATA,
        flags: 0,
        reason: reason as u8,
        _pad: [0; 3],
        payload_len,
    }
}

/// Records a per-packet [`PacketRecord`] into the same ring. A no-op when disabled.
///
/// `linked_pn` is the probed-from shell PN for a [`PacketEvent::Sent`] probe (else `None`).
/// `sent_bytes`/`frame_count` describe a `Sent` segment (else 0). `reason` carries the pre-decode
/// drop reason for [`PacketEvent::RxDropped`] (else [`DropReason::None`]).
#[inline]
pub(crate) fn record_packet(
    event: PacketEvent,
    pn: VarInt,
    cred_id: crate::credentials::Id,
    sent_bytes: u32,
    frame_count: u16,
    linked_pn: Option<VarInt>,
    reason: DropReason,
) {
    super::dbg::on_enabled(|| {
        let st = state();
        let (seq, ts) = stamp(st);
        let mut cred = *cred_id;
        cred[0] &= !CRED_ENDPOINT_BIT;
        let rec = PacketRecord {
            timestamp_nanos: ts,
            packet_number: pn.as_u64(),
            linked_pn: linked_pn.map_or(NO_PACKET_NUMBER, |p| p.as_u64()),
            cred_id: cred,
            seq,
            sent_bytes,
            frame_count,
            magic: PACKET_RECORD_MAGIC,
            event: event as u8,
            reason: reason as u8,
            _pad: [0; 3],
        };
        use zerocopy::IntoBytes as _;
        st.ring.push(rec.as_bytes());
    });
}

/// Returns the current time as nanoseconds since the Unix epoch.
///
/// In bach simulations the simulated clock is used (bach time starts at epoch 0, so the value is
/// the simulated elapsed time). In production the wall clock latched at [`State`] initialization is
/// added to the monotonic elapsed duration, giving absolute timestamps that can be compared across
/// hosts.
#[inline]
fn wall_nanos_now(st: &State) -> u64 {
    #[cfg(any(test, feature = "testing"))]
    if ::bach::is_active() {
        // Bach simulations: time starts at the unix epoch (Duration::ZERO).
        return ::bach::time::Instant::now()
            .elapsed_since_start()
            .as_nanos() as u64;
    }

    st.wall_nanos_at_start + st.start.elapsed().as_nanos() as u64
}

/// Fetches the next sequence number and Unix-epoch nanosecond timestamp from the recorder state.
/// Shared by every record kind so frame and packet records draw from one monotonic `seq` space.
#[inline]
fn stamp(st: &State) -> (u32, u64) {
    let seq = st.seq.fetch_add(1, Ordering::Relaxed);
    let ts = wall_nanos_now(st);
    (seq, ts)
}

/// Builds a [`FrameRecord`] stamped with a fresh seq/timestamp from the global state.
#[inline]
fn build_stamped(
    dir: Direction,
    header: &Header,
    pn: Option<VarInt>,
    cred_id: [u8; 16],
) -> FrameRecord {
    let st = state();
    let (seq, ts) = stamp(st);
    build(seq, ts, dir, header, pn, cred_id)
}

/// Pushes a fully-built [`FrameRecord`] into the ring.
#[inline]
fn push_frame(rec: FrameRecord) {
    use zerocopy::IntoBytes as _;
    state().ring.push(rec.as_bytes());
}

/// Requests a dump of the whole ring to disk. A no-op when the diagnostic is disabled.
///
/// Sets the single handoff flag and wakes the dumper thread, which writes the next
/// sequence-numbered file (never overwriting an earlier dump). Concurrent/overlapping requests
/// coalesce: because the dumper is the only consumer, notifications that arrive while a dump is in
/// flight collapse into the one pending flag and yield one more dump afterward.
pub(crate) fn trigger() {
    super::dbg::on_enabled(|| {
        let st = state();
        let mut requested = st.dumper.requested.lock().unwrap();
        *requested = true;
        st.dumper.condvar.notify_one();
    });
}

/// Process-wide recorder state, lazily initialized on first use (only when the diagnostic is
/// enabled, since every entry point is behind [`super::dbg::on_enabled`]).
struct State {
    ring: Arc<BumpRing>,
    seq: AtomicU32,
    start: Instant,
    /// Unix-epoch nanoseconds at the moment `start` was captured. Used to convert the
    /// monotonic `start.elapsed()` to an absolute wall-clock timestamp in [`wall_nanos_now`].
    /// Always 0 in bach simulations (where simulated time already starts at the unix epoch).
    wall_nanos_at_start: u64,
    dumper: Arc<Dumper>,
}

/// The background-dump handoff. A single bool flag plus a condvar — see [`trigger`].
struct Dumper {
    requested: Mutex<bool>,
    condvar: Condvar,
    path: PathBuf,
    /// Minimum wall-clock interval between dumps. A request arriving within this window of the last
    /// dump is dropped, so a QueueDbg storm can't write a dump per trigger and fill the disk. The
    /// first dump is never throttled. Configurable via `S2N_DC_FRAME_TRACE_THROTTLE_MS`.
    throttle: std::time::Duration,
}

/// Default minimum interval between dumps (`S2N_DC_FRAME_TRACE_THROTTLE_MS`).
const DEFAULT_THROTTLE_MS: u64 = 1000;

static STATE: OnceLock<State> = OnceLock::new();

fn state() -> &'static State {
    STATE.get_or_init(|| {
        let capacity = std::env::var("S2N_DC_FRAME_TRACE_BYTES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_CAPACITY)
            // Floor at one page: the ring must hold at least a full record (`BumpRing::push`
            // panics on a record larger than capacity), and a sub-record ring is useless anyway.
            .max(4096);
        let ring = Arc::new(BumpRing::new(capacity));

        // Default base path is PID-qualified so concurrent processes (multiple endpoints on a host,
        // or `cargo nextest`'s per-test processes) don't clobber each other's numbered dumps.
        let path = std::env::var_os("S2N_DC_FRAME_TRACE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::temp_dir().join(format!("s2n_dc_frame_trace.{}.bin", std::process::id()))
            });
        let throttle_ms = std::env::var("S2N_DC_FRAME_TRACE_THROTTLE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_THROTTLE_MS);
        let dumper = Arc::new(Dumper {
            requested: Mutex::new(false),
            condvar: Condvar::new(),
            path,
            throttle: std::time::Duration::from_millis(throttle_ms),
        });

        spawn_dumper(ring.clone(), dumper.clone());

        // Latch the wall clock alongside the monotonic start instant so that every subsequent
        // `stamp()` call can produce an absolute Unix-epoch timestamp by adding the elapsed
        // monotonic duration. Sampling both near-simultaneously keeps the skew negligible.
        let start = Instant::now();
        let wall_nanos_at_start = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        State {
            ring,
            seq: AtomicU32::new(0),
            start,
            wall_nanos_at_start,
            dumper,
        }
    })
}

/// Spawns the single-consumer dumper thread. It blocks on the condvar and, on each request,
/// snapshots the ring and writes it to its own sequence-numbered file (`numbered_path`), so an
/// earlier dump is never overwritten.
fn spawn_dumper(ring: Arc<BumpRing>, dumper: Arc<Dumper>) {
    let _ = std::thread::Builder::new()
        .name("dc_quic::frame_trace_dump".into())
        .spawn(move || {
            // Each dump goes to its own numbered file so an earlier dump is never overwritten —
            // the *first* dump is the most valuable (closest to when the stream was noticed stuck),
            // so we must never clobber it. Later triggers still produce additional dumps.
            let mut seq = 0u64;
            let mut last_dump: Option<Instant> = None;
            loop {
                {
                    let mut requested = dumper.requested.lock().unwrap();
                    while !*requested {
                        requested = dumper.condvar.wait(requested).unwrap();
                    }
                    *requested = false;
                }

                // Throttle: drop a request that arrives within `throttle` of the last completed dump
                // (the first dump always proceeds). The ring already retains the history, so a dump
                // this close would be near-identical; this caps disk churn under a QueueDbg storm.
                if throttled(last_dump.map(|l| l.elapsed()), dumper.throttle) {
                    crate::tracing::debug!(
                        throttle_ms = dumper.throttle.as_millis() as u64,
                        "frame trace dump throttled"
                    );
                    continue;
                }

                let path = numbered_path(&dumper.path, seq);
                let mut region = vec![0u8; ring.capacity()];
                let head = ring.snapshot_into(&mut region);
                if let Err(err) = write_dump(&path, ring.capacity(), head, &region) {
                    // Best-effort: a failed dump must not take down the endpoint.
                    crate::tracing::error!(error = %err, path = ?path, "frame trace dump failed");
                } else {
                    crate::tracing::info!(path = ?path, seq, head, "frame trace dumped");
                }
                last_dump = Some(Instant::now());
                seq += 1;
            }
        });
}

/// Whether a dump request should be dropped. `since_last` is the time elapsed since the previous
/// completed dump (`None` if there hasn't been one — the first dump is never throttled). A request
/// is throttled iff a previous dump happened less than `throttle` ago.
fn throttled(since_last: Option<std::time::Duration>, throttle: std::time::Duration) -> bool {
    matches!(since_last, Some(elapsed) if elapsed < throttle)
}

/// Builds the per-dump filename by inserting a zero-padded sequence number before the base path's
/// extension: `s2n_dc_frame_trace.bin` → `s2n_dc_frame_trace.000.bin`, `.001.bin`, …
fn numbered_path(base: &std::path::Path, seq: u64) -> PathBuf {
    let stem = base.file_stem().map(|s| s.to_string_lossy().into_owned());
    let ext = base.extension().map(|s| s.to_string_lossy().into_owned());
    let name = match (stem, ext) {
        (Some(stem), Some(ext)) => format!("{stem}.{seq:03}.{ext}"),
        (Some(stem), None) => format!("{stem}.{seq:03}"),
        // No file name component — fall back to appending to the whole path.
        _ => format!("frame_trace.{seq:03}.bin"),
    };
    base.with_file_name(name)
}

/// Writes the dump file: a fixed header followed by the raw ring region.
///
/// Layout (all little-endian): `magic[8]`, `version u32`, `len_suffix u8 (=2)`,
/// `payload_stride u8 (= size_of::<FrameRecord>())`, `reserved u16`, `capacity u64`, `head u64`,
/// then `region[capacity]`. The offline parser ([`examples/frame_trace_dump.rs`]) reconstructs
/// absolute offsets from `head`/`capacity` and walks the region with [`crate::sync::bump_ring::walk`].
fn write_dump(
    path: &std::path::Path,
    capacity: usize,
    head: usize,
    region: &[u8],
) -> std::io::Result<()> {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    file.write_all(&FILE_MAGIC)?;
    file.write_all(&FILE_VERSION.to_le_bytes())?;
    file.write_all(&[2u8])?; // len_suffix
    file.write_all(&[core::mem::size_of::<FrameRecord>() as u8])?; // payload_stride
    file.write_all(&0u16.to_le_bytes())?; // reserved
    file.write_all(&(capacity as u64).to_le_bytes())?;
    file.write_all(&(head as u64).to_le_bytes())?;
    file.write_all(region)?;
    file.flush()
}

/// The configured dump path. Test-only: lets the end-to-end test poll for the dump the background
/// thread writes.
#[cfg(test)]
pub(crate) fn dump_path() -> PathBuf {
    state().dumper.path.clone()
}

/// Test-only snapshot of which record kinds are currently resident in the global ring: the set of
/// [`Direction`] discriminants across frame records and the set of [`PacketEvent`] discriminants
/// across packet records. Lets an end-to-end test assert the new trace points actually fire during
/// a real transfer without going through the on-disk dump.
#[cfg(test)]
pub(crate) fn resident_event_kinds() -> (
    std::collections::BTreeSet<u8>,
    std::collections::BTreeSet<u8>,
) {
    use zerocopy::FromBytes as _;
    let st = state();
    let mut region = vec![0u8; st.ring.capacity()];
    let head = st.ring.snapshot_into(&mut region);
    let mut directions = std::collections::BTreeSet::new();
    let mut packet_events = std::collections::BTreeSet::new();
    crate::sync::bump_ring::walk(&region, head, st.ring.capacity(), |payload| {
        if payload.len() == core::mem::size_of::<FrameRecord>() {
            if let Ok(rec) = FrameRecord::read_from_bytes(payload) {
                if rec.magic == RECORD_MAGIC {
                    directions.insert(rec.direction);
                }
            }
        } else if payload.len() == core::mem::size_of::<PacketRecord>() {
            if let Ok(rec) = PacketRecord::read_from_bytes(payload) {
                if rec.magic == PACKET_RECORD_MAGIC {
                    packet_events.insert(rec.event);
                }
            }
        }
    });
    (directions, packet_events)
}

/// Test-only discriminant accessors so integration tests in sibling modules can name the kinds
/// without the enums being `pub`.
#[cfg(test)]
impl Direction {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::bump_ring;
    use zerocopy::{FromBytes as _, IntoBytes as _};

    fn dummy_data_header() -> Header {
        Header::QueueData {
            queue_pair: crate::packet::datagram::QueuePair {
                source_queue_id: VarInt::from_u32(7),
                dest_queue_id: VarInt::from_u32(9),
            },
            binding_id: VarInt::from_u32(3),
            offset: VarInt::from_u32(100),
            largest_offset: VarInt::from_u32(200),
            is_fin: true,
            blocked: true,
            dest_acceptor_id: Some(VarInt::from_u32(1)),
            priority: crate::credit::Priority::default(),
        }
    }

    #[test]
    fn build_maps_queue_data_fields() {
        // A cred id with the endpoint bit set; `build` must canonicalize it off.
        let mut raw = [0u8; 16];
        raw[0] = 0x80 | 0x12;
        raw[1] = 0x34;
        let rec = build(
            42,
            1234,
            Direction::Inbound,
            &dummy_data_header(),
            Some(VarInt::from_u32(55)),
            raw,
        );
        assert_eq!(rec.magic, RECORD_MAGIC);
        assert_eq!(rec.seq, 42);
        // Endpoint bit cleared, rest preserved.
        assert_eq!(rec.cred_id[0], 0x12);
        assert_eq!(rec.cred_id[1], 0x34);
        assert_eq!(rec.timestamp_nanos, 1234);
        assert_eq!(rec.direction, Direction::Inbound as u8);
        assert_eq!(rec.frame_type, KIND_QUEUE_DATA);
        assert_eq!(rec.source_queue_id, 7);
        assert_eq!(rec.dest_queue_id, 9);
        assert_eq!(rec.binding_id, 3);
        assert_eq!(rec.offset, 100);
        assert_eq!(rec.packet_number, 55);
        assert_eq!(rec.flags, FLAG_FIN | FLAG_BLOCKED | FLAG_INIT);
    }

    #[test]
    fn no_packet_number_uses_sentinel() {
        let rec = build(0, 0, Direction::Outbound, &Header::Ping, None, [0u8; 16]);
        assert_eq!(rec.packet_number, NO_PACKET_NUMBER);
        assert_eq!(rec.frame_type, KIND_PING);
    }

    #[test]
    fn record_size_has_no_padding() {
        // zerocopy::IntoBytes derive already enforces no padding at compile time; this pins the
        // size: 7×u64 (56) + [u8;16] (16) + u32 seq (4) + 4×u8 (4) + reason u8 (1) + [u8;3] pad (3)
        // + u32 payload_len (4) = 88. The leading 80 bytes are byte-compatible with the v1 layout.
        assert_eq!(core::mem::size_of::<FrameRecord>(), 88);
    }

    #[test]
    fn packet_record_size_distinct_from_frame() {
        // 3×u64 (24) + [u8;16] (16) + 2×u32 (8) + u16 (2) + 3×u8 (3) + [u8;3] pad (3) = 56.
        // Must differ from FrameRecord's size so the offline parser can discriminate by the ring's
        // length suffix alone before validating the per-type magic.
        assert_eq!(core::mem::size_of::<PacketRecord>(), 56);
        assert_ne!(
            core::mem::size_of::<PacketRecord>(),
            core::mem::size_of::<FrameRecord>()
        );
    }

    #[test]
    fn round_trips_through_ring_and_walk() {
        let ring = BumpRing::new(4096);
        let headers = [
            (
                Direction::Inbound,
                dummy_data_header(),
                Some(VarInt::from_u32(1)),
            ),
            (Direction::Outbound, Header::Ping, None),
            (
                Direction::AckCompleted,
                Header::QueueDbg {
                    dump_id: VarInt::from_u32(77),
                    queue_pair: crate::packet::datagram::QueuePair {
                        source_queue_id: VarInt::from_u32(1),
                        dest_queue_id: VarInt::from_u32(2),
                    },
                    binding_id: VarInt::from_u32(5),
                },
                Some(VarInt::from_u32(2)),
            ),
        ];
        for (i, (dir, header, pn)) in headers.iter().enumerate() {
            let rec = build(i as u32, i as u64, *dir, header, *pn, [0u8; 16]);
            ring.push(rec.as_bytes());
        }

        let mut region = vec![0u8; ring.capacity()];
        let head = ring.snapshot_into(&mut region);
        let mut got = Vec::new();
        bump_ring::walk(&region, head, ring.capacity(), |payload| {
            let rec = FrameRecord::read_from_bytes(payload).expect("record decodes");
            assert_eq!(rec.magic, RECORD_MAGIC);
            got.push(rec);
        });

        // newest-first
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].frame_type, KIND_QUEUE_DBG);
        assert_eq!(got[0].dump_id, 77);
        assert_eq!(got[2].frame_type, KIND_QUEUE_DATA);
    }

    /// End-to-end: `record` + `trigger` (exactly what `handle_queue_dbg` does) must drive the
    /// background dumper to write a parseable file containing the recorded frames.
    ///
    /// Must be robust to a pre-initialized global recorder: other tests in this binary
    /// (e.g. `endpoint::tests::queue_dbg_dumps_state_end_to_end` via `emit_debug`) may have already
    /// initialized the `OnceLock` with a different path and written earlier numbered dumps. So this
    /// test does not assume control of the path or the dump sequence number — it tags its records
    /// with a unique `dump_id`, triggers a dump, then scans every numbered dump under the *actual*
    /// configured base path until it finds the one whose newest record carries that marker.
    #[test]
    fn trigger_drives_background_dump() {
        // A process-unique dump_id (masked to 32 bits, the FrameRecord field reading we assert on).
        let marker = (std::process::id() as u64) & 0x00ff_ffff | 0x0100_0000;
        // A cred id whose first byte has the endpoint bit set, so we can also assert canonicalization.
        let mut cred = crate::credentials::Id::from([0u8; 16]);
        cred[0] = CRED_ENDPOINT_BIT | 0x2a;
        cred[1] = (marker & 0xff) as u8;
        let want_cred = {
            let mut c = *cred;
            c[0] &= !CRED_ENDPOINT_BIT;
            c
        };

        // Record a couple of frames then trigger, just as the QueueDbg handler does. The QueueDbg's
        // dump_id is our unique marker so we can recognize *our* dump among any others.
        record(
            Direction::Outbound,
            &dummy_data_header(),
            Some(VarInt::from_u32(11)),
            cred,
        );
        record(
            Direction::Inbound,
            &Header::QueueDbg {
                dump_id: VarInt::new(marker).unwrap(),
                queue_pair: crate::packet::datagram::QueuePair {
                    source_queue_id: VarInt::from_u32(1),
                    dest_queue_id: VarInt::from_u32(2),
                },
                binding_id: VarInt::from_u32(4),
            },
            None,
            cred,
        );
        let base = dump_path();
        trigger();

        // The dumper is a fire-and-forget background thread; poll the numbered dump files (scanning
        // newest seq first) until one's newest record is our marked QueueDbg.
        let parse = |bytes: &[u8]| -> Option<Vec<FrameRecord>> {
            if bytes.len() < 32 || bytes[..8] != FILE_MAGIC {
                return None;
            }
            let capacity = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
            let head = u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as usize;
            if bytes.len() < 32 + capacity {
                return None;
            }
            let region = &bytes[32..32 + capacity];
            let mut recs = Vec::new();
            bump_ring::walk(region, head, capacity, |payload| {
                if let Ok(rec) = FrameRecord::read_from_bytes(payload) {
                    if rec.magic == RECORD_MAGIC {
                        recs.push(rec);
                    }
                }
            });
            Some(recs)
        };

        // A dump "matches" if it contains our marked QueueDbg anywhere. Under `cargo test` other
        // tests push to the same global ring concurrently, so our records can be interleaved with
        // theirs and need not be at any fixed position — search by content, not index.
        let mut found = None;
        'poll: for i in 0..300 {
            // The dumper throttles back-to-back dumps, so our first `trigger()` may have been
            // dropped if another test dumped moments earlier. Re-trigger periodically (the poll
            // window far exceeds the default throttle) so a request eventually lands.
            if i > 0 && i % 50 == 0 {
                trigger();
            }
            // Walk seq downward from a generous bound so we prefer the most recent dump.
            for seq in (0..64u64).rev() {
                let path = numbered_path(&base, seq);
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Some(recs) = parse(&bytes) {
                        if recs
                            .iter()
                            .any(|r| r.frame_type == KIND_QUEUE_DBG && r.dump_id == marker)
                        {
                            found = Some(recs);
                            break 'poll;
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let recs = found.expect("background dumper should write a dump with our marker");

        // Our marked QueueDbg must be present (proves record + trigger + dump + parse), and the
        // QueueData we recorded (unique pn=11 + source_queue_id=7 from `dummy_data_header`) must
        // also have made it into the same ring.
        assert!(
            recs.iter().any(|r| r.frame_type == KIND_QUEUE_DBG
                && r.dump_id == marker
                && r.cred_id == want_cred),
            "marked QueueDbg present with canonicalized cred id"
        );
        assert!(
            recs.iter().any(|r| r.frame_type == KIND_QUEUE_DATA
                && r.packet_number == 11
                && r.source_queue_id == 7),
            "recorded QueueData present"
        );
    }

    #[test]
    fn throttle_drops_only_within_window() {
        use std::time::Duration;
        let throttle = Duration::from_millis(1000);
        // First dump (no previous) is never throttled.
        assert!(!throttled(None, throttle));
        // A dump that just happened throttles the next request.
        assert!(throttled(Some(Duration::from_millis(10)), throttle));
        assert!(throttled(Some(Duration::from_millis(999)), throttle));
        // Past the window, the request proceeds.
        assert!(!throttled(Some(Duration::from_millis(1000)), throttle));
        assert!(!throttled(Some(Duration::from_millis(5000)), throttle));
        // A zero throttle never drops anything.
        assert!(!throttled(Some(Duration::ZERO), Duration::ZERO));
    }

    #[test]
    fn numbered_path_inserts_sequence_before_extension() {
        let base = PathBuf::from("/tmp/s2n_dc_frame_trace.bin");
        assert_eq!(
            numbered_path(&base, 0),
            PathBuf::from("/tmp/s2n_dc_frame_trace.000.bin")
        );
        assert_eq!(
            numbered_path(&base, 7),
            PathBuf::from("/tmp/s2n_dc_frame_trace.007.bin")
        );
        // No extension.
        let base = PathBuf::from("/tmp/trace");
        assert_eq!(numbered_path(&base, 3), PathBuf::from("/tmp/trace.003"));
    }

    #[test]
    fn write_dump_produces_parseable_file() {
        let ring = BumpRing::new(1024);
        let rec = build(
            1,
            1,
            Direction::Inbound,
            &dummy_data_header(),
            Some(VarInt::from_u32(9)),
            [0u8; 16],
        );
        ring.push(rec.as_bytes());

        let mut region = vec![0u8; ring.capacity()];
        let head = ring.snapshot_into(&mut region);

        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "s2n_dc_frame_trace_test_{}.bin",
            std::process::id()
        ));
        write_dump(&path, ring.capacity(), head, &region).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(&bytes[..8], &FILE_MAGIC);
        let capacity = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
        let head_read = u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as usize;
        assert_eq!(capacity, ring.capacity());
        assert_eq!(head_read, head);

        let region_read = &bytes[32..32 + capacity];
        let mut count = 0;
        bump_ring::walk(region_read, head_read, capacity, |payload| {
            let rec = FrameRecord::read_from_bytes(payload).unwrap();
            assert_eq!(rec.magic, RECORD_MAGIC);
            assert_eq!(rec.packet_number, 9);
            count += 1;
        });
        assert_eq!(count, 1);
    }

    /// Helper mirroring `record_packet`'s record construction without the global ring, so the field
    /// mapping can be asserted deterministically.
    fn build_packet(
        seq: u32,
        event: PacketEvent,
        pn: VarInt,
        sent_bytes: u32,
        frame_count: u16,
        linked_pn: Option<VarInt>,
        reason: DropReason,
    ) -> PacketRecord {
        PacketRecord {
            timestamp_nanos: 0,
            packet_number: pn.as_u64(),
            linked_pn: linked_pn.map_or(NO_PACKET_NUMBER, |p| p.as_u64()),
            cred_id: [0u8; 16],
            seq,
            sent_bytes,
            frame_count,
            magic: PACKET_RECORD_MAGIC,
            event: event as u8,
            reason: reason as u8,
            _pad: [0; 3],
        }
    }

    #[test]
    fn packet_record_has_no_padding() {
        // zerocopy::IntoBytes enforces this at compile time; the round-trip below also relies on it.
        let rec = build_packet(
            0,
            PacketEvent::Sent,
            VarInt::from_u32(1),
            1200,
            3,
            None,
            DropReason::None,
        );
        assert_eq!(rec.as_bytes().len(), core::mem::size_of::<PacketRecord>());
    }

    /// A frame record and a packet record pushed into the same ring must both round-trip, and the
    /// walker must be able to tell them apart by length + magic (the exact discrimination the
    /// offline parser performs). This is the load-bearing property for interleaving two record
    /// types in one ring.
    #[test]
    fn mixed_frame_and_packet_records_round_trip() {
        let ring = BumpRing::new(4096);

        let frame = build(
            1,
            10,
            Direction::Outbound,
            &dummy_data_header(),
            Some(VarInt::from_u32(42)),
            [0u8; 16],
        );
        let packet = build_packet(
            2,
            PacketEvent::Sent,
            VarInt::from_u32(42),
            1200,
            3,
            Some(VarInt::from_u32(7)),
            DropReason::None,
        );
        ring.push(frame.as_bytes());
        ring.push(packet.as_bytes());

        let mut region = vec![0u8; ring.capacity()];
        let head = ring.snapshot_into(&mut region);

        let mut frames = 0usize;
        let mut packets = 0usize;
        bump_ring::walk(&region, head, ring.capacity(), |payload| {
            // Discriminate exactly as the parser does: length first, then the per-type magic.
            if payload.len() == core::mem::size_of::<FrameRecord>() {
                let rec = FrameRecord::read_from_bytes(payload).unwrap();
                assert_eq!(rec.magic, RECORD_MAGIC);
                assert_eq!(rec.direction, Direction::Outbound as u8);
                assert_eq!(rec.packet_number, 42);
                frames += 1;
            } else if payload.len() == core::mem::size_of::<PacketRecord>() {
                let rec = PacketRecord::read_from_bytes(payload).unwrap();
                assert_eq!(rec.magic, PACKET_RECORD_MAGIC);
                assert_eq!(rec.event, PacketEvent::Sent as u8);
                assert_eq!(rec.packet_number, 42);
                assert_eq!(rec.sent_bytes, 1200);
                assert_eq!(rec.frame_count, 3);
                assert_eq!(rec.linked_pn, 7);
                packets += 1;
            } else {
                panic!("unexpected record length {}", payload.len());
            }
        });
        assert_eq!(frames, 1, "frame record must round-trip");
        assert_eq!(packets, 1, "packet record must round-trip");
    }

    #[test]
    fn packet_record_no_link_uses_sentinel() {
        let rec = build_packet(
            0,
            PacketEvent::RxArrived,
            VarInt::from_u32(5),
            0,
            0,
            None,
            DropReason::None,
        );
        assert_eq!(rec.linked_pn, NO_PACKET_NUMBER);
        assert_eq!(rec.event, PacketEvent::RxArrived as u8);
        assert_eq!(rec.reason, DropReason::None as u8);
    }

    #[test]
    fn packet_record_carries_drop_reason() {
        let rec = build_packet(
            0,
            PacketEvent::RxDropped,
            VarInt::from_u32(9),
            0,
            0,
            None,
            DropReason::Duplicate,
        );
        assert_eq!(rec.event, PacketEvent::RxDropped as u8);
        assert_eq!(rec.reason, DropReason::Duplicate as u8);
    }

    /// `record_drop` (via `build_stamped`) must preserve the header's routing identity and stamp the
    /// `RxDropped` direction plus the reason. Exercised through the real global API so the
    /// `state()`/`stamp()`/`push_frame` path is covered, then read back out of the ring by content.
    #[test]
    fn record_drop_tags_reason_and_direction() {
        // Unique queue id so this record is unmistakable among any others in the shared global ring.
        let marker_qid = ((std::process::id() as u64) & 0x00ff_ffff) | 0x0200_0000;
        let header = Header::QueueData {
            queue_pair: crate::packet::datagram::QueuePair {
                source_queue_id: VarInt::new(marker_qid).unwrap(),
                dest_queue_id: VarInt::from_u32(2),
            },
            binding_id: VarInt::from_u32(3),
            offset: VarInt::from_u32(0),
            largest_offset: VarInt::from_u32(0),
            is_fin: false,
            blocked: false,
            dest_acceptor_id: None,
            priority: crate::credit::Priority::default(),
        };
        record_drop(
            &header,
            VarInt::from_u32(17),
            crate::credentials::Id::from([0u8; 16]),
            DropReason::FutureBinding,
        );

        let st = state();
        let mut region = vec![0u8; st.ring.capacity()];
        let head = st.ring.snapshot_into(&mut region);
        let mut found = None;
        bump_ring::walk(&region, head, st.ring.capacity(), |payload| {
            if payload.len() == core::mem::size_of::<FrameRecord>() {
                if let Ok(rec) = FrameRecord::read_from_bytes(payload) {
                    if rec.magic == RECORD_MAGIC && rec.source_queue_id == marker_qid {
                        found = Some(rec);
                    }
                }
            }
        });
        let rec = found.expect("our RxDropped record must be in the ring");
        assert_eq!(rec.direction, Direction::RxDropped as u8);
        assert_eq!(rec.reason, DropReason::FutureBinding as u8);
        assert_eq!(rec.packet_number, 17);
        assert_eq!(rec.binding_id, 3);
    }

    /// `record_app_recv` synthesises a QueueData-kind AppRecv record from the reader's routing
    /// identity (there is no wire `Header` at delivery) and carries the delivered byte count.
    #[test]
    fn record_app_recv_maps_fields() {
        let marker_qid = ((std::process::id() as u64) & 0x00ff_ffff) | 0x0300_0000;
        record_app_recv(
            VarInt::from_u32(55),             // source (peer) queue id
            VarInt::new(marker_qid).unwrap(), // dest (our local) queue id
            VarInt::from_u32(8),              // binding id
            4096,                             // consumed offset
            1500,                             // bytes delivered this read
            crate::credentials::Id::from([0u8; 16]),
        );

        let st = state();
        let mut region = vec![0u8; st.ring.capacity()];
        let head = st.ring.snapshot_into(&mut region);
        let mut found = None;
        bump_ring::walk(&region, head, st.ring.capacity(), |payload| {
            if payload.len() == core::mem::size_of::<FrameRecord>() {
                if let Ok(rec) = FrameRecord::read_from_bytes(payload) {
                    if rec.magic == RECORD_MAGIC && rec.dest_queue_id == marker_qid {
                        found = Some(rec);
                    }
                }
            }
        });
        let rec = found.expect("our AppRecv record must be in the ring");
        assert_eq!(rec.direction, Direction::AppRecv as u8);
        assert_eq!(rec.frame_type, KIND_QUEUE_DATA);
        assert_eq!(rec.source_queue_id, 55);
        assert_eq!(rec.binding_id, 8);
        assert_eq!(rec.offset, 4096);
        assert_eq!(rec.payload_len, 1500);
        assert_eq!(
            rec.packet_number, NO_PACKET_NUMBER,
            "no PN at the app layer"
        );
    }

    /// `record_reader_drop` synthesises an `RxDropped` QueueData record (no wire `Header` at the
    /// reader layer) carrying the stream-layer drop reason.
    #[test]
    fn record_reader_drop_tags_window_violation() {
        let marker_qid = ((std::process::id() as u64) & 0x00ff_ffff) | 0x0500_0000;
        record_reader_drop(
            VarInt::from_u32(55),
            VarInt::new(marker_qid).unwrap(),
            VarInt::from_u32(8),
            9000,
            DropReason::WindowViolation,
            crate::credentials::Id::from([0u8; 16]),
        );

        let st = state();
        let mut region = vec![0u8; st.ring.capacity()];
        let head = st.ring.snapshot_into(&mut region);
        let mut found = None;
        bump_ring::walk(&region, head, st.ring.capacity(), |payload| {
            if payload.len() == core::mem::size_of::<FrameRecord>() {
                if let Ok(rec) = FrameRecord::read_from_bytes(payload) {
                    if rec.magic == RECORD_MAGIC && rec.dest_queue_id == marker_qid {
                        found = Some(rec);
                    }
                }
            }
        });
        let rec = found.expect("our reader-drop record must be in the ring");
        assert_eq!(rec.direction, Direction::RxDropped as u8);
        assert_eq!(rec.reason, DropReason::WindowViolation as u8);
        assert_eq!(rec.offset, 9000);
        assert_eq!(rec.frame_type, KIND_QUEUE_DATA);
        assert_eq!(rec.packet_number, NO_PACKET_NUMBER);
    }

    /// `record_packet` must push a 56-byte PacketRecord (not a FrameRecord) into the global ring.
    #[test]
    fn record_packet_pushes_packet_sized_record() {
        let marker_pn = ((std::process::id() as u64) & 0x00ff_ffff) | 0x0400_0000;
        record_packet(
            PacketEvent::Sent,
            VarInt::new(marker_pn).unwrap(),
            crate::credentials::Id::from([0u8; 16]),
            1234,
            5,
            Some(VarInt::from_u32(99)),
            DropReason::None,
        );

        let st = state();
        let mut region = vec![0u8; st.ring.capacity()];
        let head = st.ring.snapshot_into(&mut region);
        let mut found = None;
        bump_ring::walk(&region, head, st.ring.capacity(), |payload| {
            if payload.len() == core::mem::size_of::<PacketRecord>() {
                if let Ok(rec) = PacketRecord::read_from_bytes(payload) {
                    if rec.magic == PACKET_RECORD_MAGIC && rec.packet_number == marker_pn {
                        found = Some(rec);
                    }
                }
            }
        });
        let rec = found.expect("our Sent packet record must be in the ring");
        assert_eq!(rec.event, PacketEvent::Sent as u8);
        assert_eq!(rec.sent_bytes, 1234);
        assert_eq!(rec.frame_count, 5);
        assert_eq!(rec.linked_pn, 99);
    }
}
