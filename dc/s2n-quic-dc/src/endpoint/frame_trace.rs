// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Global frame flight recorder.
//!
//! Every frame the endpoint observes — inbound (slow path and fast path), outbound at assembly,
//! and at ack-completion — is recorded as a compact fixed-size [`FrameRecord`] into a single
//! process-wide [`BumpRing`]. The hot path is just an atomic `fetch_add` plus a memcpy (see
//! [`crate::sync::bump_ring`]).
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
    time::Instant,
};

/// Magic bytes at the front of every [`FrameRecord`] payload. Combined with the ring's trailing
/// length suffix, this lets the backwards-walk reject records whose tail was overwritten.
const RECORD_MAGIC: u8 = 0xF7;

/// On-disk file magic. Bumped if [`FrameRecord`] or the framing ever changes.
const FILE_MAGIC: [u8; 8] = *b"DCFTRC01";
const FILE_VERSION: u32 = 1;

/// Default ring capacity (bytes) when `S2N_DC_FRAME_TRACE_BYTES` is unset. 16 MiB ≈ 256k records.
const DEFAULT_CAPACITY: usize = 16 << 20;

/// Sentinel stored in [`FrameRecord::packet_number`] when a frame has no packet number yet.
const NO_PACKET_NUMBER: u64 = u64::MAX;

/// The endpoint bit in byte 0 of a `credentials::Id` (mirrors the private `Id::ENDPOINT_BIT`). It
/// flips between the server and client view of the same id, so it is masked off before storing the
/// id in a record — see [`build`].
const CRED_ENDPOINT_BIT: u8 = 0x80;

/// Where a frame was observed. Stored in [`FrameRecord::direction`].
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
}

/// A compact, fixed-size record of one observed frame.
///
/// Field order places the 8-byte fields first, then the 16-byte (1-aligned) credential id, then the
/// `u32`/`u8` tail — so the struct is naturally aligned with no implicit padding (a requirement for
/// the `zerocopy::IntoBytes` derive). Total size is 80 bytes.
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
    /// Nanoseconds since the recorder started (a process-local monotonic clock).
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
        let st = state();
        let seq = st.seq.fetch_add(1, Ordering::Relaxed);
        let ts = st.start.elapsed().as_nanos() as u64;
        let rec = build(seq, ts, dir, header, pn, *cred_id);
        use zerocopy::IntoBytes as _;
        st.ring.push(rec.as_bytes());
    });
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

        State {
            ring,
            seq: AtomicU32::new(0),
            start: Instant::now(),
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
        // size: 7×u64 (56) + [u8;16] (16) + u32 (4) + 4×u8 (4) = 80.
        assert_eq!(core::mem::size_of::<FrameRecord>(), 80);
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
}
