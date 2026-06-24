// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Frame/packet flight recorder, backed by [`backbeat`].
//!
//! Every frame the endpoint observes is recorded as a compact [`FrameRecord`] [`backbeat`] event;
//! coarser per-packet signals are recorded as [`PacketRecord`] events. Both are captured into the
//! process-wide [`backbeat::global`] recorder — the hot path is a CPU-sharded ring push with no
//! allocation. Frames are captured across the whole lifecycle:
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
//! Interleaved with the per-frame records are coarser per-packet [`PacketRecord`]s. They capture
//! each packet's lifecycle — received off the wire (the earliest possible sighting of a packet
//! number, before decrypt/dedup, so loss *before* processing is immediately distinguishable from
//! loss on the wire), assembled/sent, acked, lost, or dropped pre-decode (decrypt/replay/dedup).
//! Because every frame record carries its `packet_number`, frames can be grouped into their packet;
//! the packet records add the packet-only signals a frame can't carry: wire byte count, frame
//! count, probe shell→pn linkage, and the pre-decode drop reasons that have no frame to attach to.
//!
//! Unlike the per-`dump_id` [`QueueDbg`](super::frame::Header::QueueDbg) diagnostic — which traces
//! *one* stuck frame end-to-end — this records *all* frames, so the dump shows the surrounding
//! history (what else was in flight, in what order, with what offsets/flags). Each `QueueDbg` we
//! receive [triggers](trigger) an asynchronous dump of the rings to disk.
//!
//! The whole facility is gated behind [`super::dbg::ENABLED`]: in plain release builds [`record`]
//! and [`trigger`] fold to nothing — the event is never constructed and the [`backbeat::global`]
//! recorder (and its dumper thread) is never built. It is active under `test`, the `testing`
//! feature, or the `queue-dbg` feature — the same builds that already speak `QueueDbg`.
//!
//! Dumps are self-describing `.bb` files written by `backbeat::global`'s background dumper, named by
//! UTC timestamp under [`BACKBEAT_PATH`] (default `${TMPDIR}/backbeat.<pid>.bb`). Read them with the
//! `backbeat` CLI: `backbeat inspect <file>.bb`, or `backbeat convert <file>.bb -o out.parquet`
//! (query with DuckDB) / `-o out.json` (load in Chrome/Perfetto). The recorder honours backbeat's
//! `BACKBEAT_*` environment overrides (`BACKBEAT_PATH`, `BACKBEAT_BYTES`, `BACKBEAT_THROTTLE_MS`,
//! `BACKBEAT_MAX_DUMPS`, `BACKBEAT_SIGNAL`, …).
//!
//! [`BACKBEAT_PATH`]: https://docs.rs/backbeat

use super::frame::Header;
// `Event`/`EventEnum` name both a trait and a derive macro in backbeat (it re-exports both under the
// same name); importing them plainly brings the derive macros — and their `#[event(...)]` helper
// attribute — into scope, and also the traits (so `FrameRecord::ID` resolves).
use backbeat::{Event, EventEnum};
use s2n_quic_core::varint::VarInt;
use zerocopy::{Immutable, IntoBytes};

/// Sentinel stored in [`FrameRecord::packet_number`] / [`PacketRecord::linked_pn`] when no packet
/// number is in scope yet.
const NO_PACKET_NUMBER: u64 = u64::MAX;

/// The endpoint bit in byte 0 of a `credentials::Id` (mirrors the private `Id::ENDPOINT_BIT`). It
/// flips between the server and client view of the same id, so it is masked off before storing the
/// id in a record — see [`canonicalize_cred`].
const CRED_ENDPOINT_BIT: u8 = 0x80;

/// Where a frame was observed. Stored in [`FrameRecord::direction`].
///
/// Discriminants are stable wire/dump values — append new variants, never renumber. `backbeat`
/// embeds the value→label map in the dump schema (via [`EventEnum`]), so the CLI renders these as
/// names without any out-of-band table.
#[derive(EventEnum, IntoBytes, Immutable, Clone, Copy, Debug, PartialEq, Eq)]
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
    /// `Header` at this layer — see [`FrameRecord::stream`]), so it is always a `QueueData`-kind
    /// record carrying the delivered byte count in [`FrameRecord::payload_len`] and the consumed
    /// stream offset in [`FrameRecord::offset`].
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
/// Discriminants are stable dump values — append, never renumber. `None` (0) is the resting value
/// for records that did not record a drop. The receive-side post-decode reasons mirror the
/// `crate::queue::Error` variants the dispatcher already counts; the pre-decode reasons have no
/// frame to attach to and only appear on a [`PacketRecord`].
#[derive(EventEnum, IntoBytes, Immutable, Clone, Copy, Debug, PartialEq, Eq)]
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

/// A packet's place in its lifecycle. Stored in [`PacketRecord::event`].
///
/// Discriminants are stable dump values — append, never renumber.
#[derive(EventEnum, IntoBytes, Immutable, Clone, Copy, Debug, PartialEq, Eq)]
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
    /// Declared lost by loss detection. [`PacketRecord::reason`] stays [`DropReason::None`]: the
    /// trigger (packet-number vs. time threshold) is not distinguished per packet.
    Lost = 4,
}

/// Stable frame-kind discriminant for the dump, independent of the private wire type tags. Stored
/// in [`FrameRecord::frame_type`]; `backbeat` renders these as names from the embedded schema.
#[derive(EventEnum, IntoBytes, Immutable, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum FrameKind {
    QueueData = 1,
    QueueControl = 2,
    QueueMaxData = 3,
    QueueReset = 4,
    QueueFree = 5,
    Ack = 6,
    QueueMsg = 7,
    Ping = 8,
    QueueDataBlocked = 9,
    QueueDbg = 10,
}

// Flag bits packed into `FrameRecord::flags`.
const FLAG_FIN: u8 = 1 << 0;
const FLAG_BLOCKED: u8 = 1 << 1;
const FLAG_WAKEUP: u8 = 1 << 2;
const FLAG_ACK_ELICITING: u8 = 1 << 3;
const FLAG_INIT: u8 = 1 << 4;

/// A compact record of one observed frame.
///
/// `backbeat` prepends a capture timestamp and the event id to every record and reconstructs global
/// order, so this struct carries only the frame's own fields. Field order places the 8-byte fields
/// first, then the 1-aligned credential id, then the `u8` enums and the `u32` tail — so the layout
/// is naturally aligned with no implicit padding (required by `zerocopy::IntoBytes`).
#[derive(Event, IntoBytes, Immutable, Debug)]
#[event(namespace = "s2n_quic_dc::frame")]
#[repr(C)]
pub struct FrameRecord {
    /// Packet number, or [`NO_PACKET_NUMBER`] when not yet assigned.
    #[event(key)]
    pub packet_number: u64,
    /// Primary stream/control offset for the frame type (see [`FrameRecord::from_header`]).
    #[event(unit = "bytes")]
    pub offset: u64,
    /// `QueueDbg` dump id, else 0.
    pub dump_id: u64,
    #[event(key)]
    pub source_queue_id: u64,
    #[event(key)]
    pub dest_queue_id: u64,
    pub binding_id: u64,
    /// The 16-byte credential id (`credentials::Id`) of the path this frame belongs to, big-endian
    /// as on the wire, with the endpoint bit masked off so a record from either endpoint shares one
    /// id (a single query correlates both ends of a stream). Zero when no credential is in scope.
    #[event(key)]
    pub cred_id: [u8; 16],
    /// Where the frame was observed.
    pub direction: Direction,
    /// The frame kind.
    pub frame_type: FrameKind,
    /// Bit flags: see the `FLAG_*` constants.
    pub flags: u8,
    /// A [`DropReason`] for [`Direction::RxDropped`]; [`DropReason::None`] otherwise.
    pub reason: DropReason,
    /// Payload byte count where meaningful: bytes delivered for [`Direction::AppRecv`], else 0.
    #[event(unit = "bytes")]
    pub payload_len: u32,
}

/// A compact per-packet record.
///
/// Captures the packet-only signals a frame record can't carry: the wire byte count, how many
/// frames shared the packet, a probe's shell→pn linkage, and the pre-decode drop reasons that have
/// no decoded frame to attach to. Frames are tied to their packet through the shared `packet_number`.
#[derive(Event, IntoBytes, Immutable, Debug)]
#[event(namespace = "s2n_quic_dc::packet")]
#[repr(C)]
pub struct PacketRecord {
    /// The packet number this record is about.
    #[event(key)]
    pub packet_number: u64,
    /// For [`PacketEvent::Sent`] probes, the shell PN this packet was probed from; else
    /// [`NO_PACKET_NUMBER`]. Links a retransmit back to the transmission it replaced.
    pub linked_pn: u64,
    /// Canonicalized credential id (endpoint bit cleared), matching [`FrameRecord::cred_id`].
    #[event(key)]
    pub cred_id: [u8; 16],
    /// Wire byte count for [`PacketEvent::Sent`] (else 0).
    #[event(unit = "bytes")]
    pub sent_bytes: u32,
    /// Number of frames in the packet for [`PacketEvent::Sent`] (else 0).
    pub frame_count: u16,
    /// The packet's place in its lifecycle.
    pub event: PacketEvent,
    /// A [`DropReason`] for [`PacketEvent::RxDropped`]; [`DropReason::None`] otherwise.
    pub reason: DropReason,
}

/// Maps a [`Header`] to its stable [`FrameKind`].
pub(crate) fn header_kind(header: &Header) -> FrameKind {
    match header {
        Header::QueueData { .. } => FrameKind::QueueData,
        Header::QueueControl { .. } => FrameKind::QueueControl,
        Header::QueueMaxData { .. } => FrameKind::QueueMaxData,
        Header::QueueReset { .. } => FrameKind::QueueReset,
        Header::QueueFree { .. } => FrameKind::QueueFree,
        Header::Ack { .. } => FrameKind::Ack,
        Header::QueueMsg { .. } => FrameKind::QueueMsg,
        Header::Ping => FrameKind::Ping,
        Header::QueueDataBlocked { .. } => FrameKind::QueueDataBlocked,
        Header::QueueDbg { .. } => FrameKind::QueueDbg,
    }
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

impl FrameRecord {
    /// Builds a [`FrameRecord`] from a frame header. `pn` is the packet number if known; `reason`
    /// tags a [`Direction::RxDropped`] record (pass [`DropReason::None`] otherwise); `cred_id` is the
    /// 16-byte credential id of the path (all zeros when none is in scope).
    pub(crate) fn from_header(
        dir: Direction,
        header: &Header,
        pn: Option<VarInt>,
        reason: DropReason,
        cred_id: crate::credentials::Id,
    ) -> FrameRecord {
        let mut rec = FrameRecord {
            packet_number: pn.map_or(NO_PACKET_NUMBER, |p| p.as_u64()),
            offset: 0,
            dump_id: 0,
            source_queue_id: 0,
            dest_queue_id: 0,
            binding_id: 0,
            cred_id: canonicalize_cred(*cred_id),
            direction: dir,
            frame_type: header_kind(header),
            flags: 0,
            reason,
            payload_len: 0,
        };

        // Pull the routing identity and a single "primary" offset out of each variant. `flags`
        // carries the boolean signals that matter for stuck-stream diagnosis.
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

    /// Builds a `QueueData`-kind [`FrameRecord`] from a stream-layer routing identity, used where
    /// there is no wire [`Header`] in scope: [`Direction::AppRecv`] (reassembled bytes crossing to
    /// the application, `payload_len` = bytes delivered, `offset` = consumed stream offset) and the
    /// reader-detected [`Direction::RxDropped`] (e.g. [`DropReason::WindowViolation`]). `pn` is
    /// unknown at this layer (sentinel).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn stream(
        dir: Direction,
        source_queue_id: VarInt,
        dest_queue_id: VarInt,
        binding_id: VarInt,
        offset: u64,
        payload_len: u32,
        reason: DropReason,
        cred_id: crate::credentials::Id,
    ) -> FrameRecord {
        FrameRecord {
            packet_number: NO_PACKET_NUMBER,
            offset,
            dump_id: 0,
            source_queue_id: source_queue_id.as_u64(),
            dest_queue_id: dest_queue_id.as_u64(),
            binding_id: binding_id.as_u64(),
            cred_id: canonicalize_cred(*cred_id),
            direction: dir,
            frame_type: FrameKind::QueueData,
            flags: 0,
            reason,
            payload_len,
        }
    }
}

impl PacketRecord {
    /// Builds a [`PacketRecord`]. `linked_pn` is the probed-from shell PN for a [`PacketEvent::Sent`]
    /// probe (else `None`); `sent_bytes`/`frame_count` describe a `Sent` segment (else 0); `reason`
    /// carries the pre-decode drop reason for [`PacketEvent::RxDropped`] (else [`DropReason::None`]).
    pub(crate) fn new(
        event: PacketEvent,
        pn: VarInt,
        cred_id: crate::credentials::Id,
        sent_bytes: u32,
        frame_count: u16,
        linked_pn: Option<VarInt>,
        reason: DropReason,
    ) -> PacketRecord {
        PacketRecord {
            packet_number: pn.as_u64(),
            linked_pn: linked_pn.map_or(NO_PACKET_NUMBER, |p| p.as_u64()),
            cred_id: canonicalize_cred(*cred_id),
            sent_bytes,
            frame_count,
            event,
            reason,
        }
    }
}

/// Records one event ([`FrameRecord`] or [`PacketRecord`]) into the global recorder. A no-op —
/// folded away entirely, with the event never constructed — when the diagnostic is disabled (the
/// production default), since the call sites wrap the construction in [`super::dbg::on_enabled`].
///
/// `backbeat`'s capture starts disabled; arming it is the application's job. In production set
/// `BACKBEAT_ENABLE=1` (or call [`backbeat::global::enable`] once at startup); the crate's own tests
/// enable it in [`crate::testing::init_tracing`]. Keeping the enable out of this hot path avoids a
/// per-record atomic — `record` is just `backbeat::global::record`, which itself folds to one
/// relaxed load when capture is off.
#[inline]
pub(crate) fn record<E: backbeat::Event>(event: &E) {
    super::dbg::on_enabled(|| backbeat::global::record(event));
}

/// Requests an asynchronous dump of the recorder's rings to disk. A no-op when disabled.
///
/// `backbeat`'s background dumper writes the next timestamp-named file and coalesces/throttles
/// back-to-back triggers, so a `QueueDbg` storm can't fill the disk.
#[inline]
pub(crate) fn trigger() {
    super::dbg::on_enabled(backbeat::global::trigger);
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

/// Test-only snapshot of which record kinds are currently resident in the global recorder: the set
/// of [`Direction`] discriminants across frame records and the set of [`PacketEvent`] discriminants
/// across packet records. Lets an end-to-end test assert the trace points actually fire during a
/// real transfer without going through an on-disk dump.
///
/// Dumps the live rings in-memory (`backbeat::global::recorder().dump(...)`), parses the dump, and
/// walks each shard, decoding any record whose `event_id` matches [`FrameRecord`]/[`PacketRecord`].
#[cfg(test)]
pub(crate) fn resident_event_kinds() -> (
    std::collections::BTreeSet<u8>,
    std::collections::BTreeSet<u8>,
) {
    use backbeat::record::RecordView;

    let bytes = backbeat::global::recorder().dump(
        backbeat::registry::schemas(),
        core::iter::empty(),
        "",
    );

    let mut directions = std::collections::BTreeSet::new();
    let mut packet_events = std::collections::BTreeSet::new();

    let Ok(reader) = backbeat::wire::DumpReader::new(bytes) else {
        return (directions, packet_events);
    };
    let Ok(shards) = reader.shards() else {
        return (directions, packet_events);
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
                // The fields are the struct's `IntoBytes` image (no padding), so the 1-byte enum
                // discriminant sits at its `offset_of!` within `view.fields`.
                if view.event_id == FrameRecord::ID {
                    let off = core::mem::offset_of!(FrameRecord, direction);
                    if let Some(&b) = view.fields.get(off) {
                        directions.insert(b);
                    }
                } else if view.event_id == PacketRecord::ID {
                    let off = core::mem::offset_of!(PacketRecord, event);
                    if let Some(&b) = view.fields.get(off) {
                        packet_events.insert(b);
                    }
                }
                true
            },
        );
    }

    (directions, packet_events)
}
