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
/// Field order places the 8-byte fields first so the struct is naturally aligned with no implicit
/// padding — a requirement for the `zerocopy::IntoBytes` derive. Total size is 64 bytes.
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

/// Builds a [`FrameRecord`] from a frame header. `pn` is the packet number if known.
///
/// `seq` and `timestamp_nanos` are stamped by the caller from the global recorder state.
fn build(
    seq: u32,
    timestamp_nanos: u64,
    dir: Direction,
    header: &Header,
    pn: Option<VarInt>,
) -> FrameRecord {
    let mut rec = FrameRecord {
        timestamp_nanos,
        packet_number: pn.map_or(NO_PACKET_NUMBER, |p| p.as_u64()),
        offset: 0,
        dump_id: 0,
        source_queue_id: 0,
        dest_queue_id: 0,
        binding_id: 0,
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
#[inline]
pub(crate) fn record(dir: Direction, header: &Header, pn: Option<VarInt>) {
    super::dbg::on_enabled(|| {
        let st = state();
        let seq = st.seq.fetch_add(1, Ordering::Relaxed);
        let ts = st.start.elapsed().as_nanos() as u64;
        let rec = build(seq, ts, dir, header, pn);
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
}

static STATE: OnceLock<State> = OnceLock::new();

fn state() -> &'static State {
    STATE.get_or_init(|| {
        let capacity = std::env::var("S2N_DC_FRAME_TRACE_BYTES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_CAPACITY);
        let ring = Arc::new(BumpRing::new(capacity));

        let path = std::env::var_os("S2N_DC_FRAME_TRACE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("s2n_dc_frame_trace.bin"));
        let dumper = Arc::new(Dumper {
            requested: Mutex::new(false),
            condvar: Condvar::new(),
            path,
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
/// snapshots the ring and writes it to disk (overwriting any previous dump).
fn spawn_dumper(ring: Arc<BumpRing>, dumper: Arc<Dumper>) {
    let _ = std::thread::Builder::new()
        .name("dc_quic::frame_trace_dump".into())
        .spawn(move || {
            // Each dump goes to its own numbered file so an earlier dump is never overwritten —
            // the *first* dump is the most valuable (closest to when the stream was noticed stuck),
            // so we must never clobber it. Later triggers still produce additional dumps.
            let mut seq = 0u64;
            loop {
                {
                    let mut requested = dumper.requested.lock().unwrap();
                    while !*requested {
                        requested = dumper.condvar.wait(requested).unwrap();
                    }
                    *requested = false;
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
                seq += 1;
            }
        });
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
        let rec = build(
            42,
            1234,
            Direction::Inbound,
            &dummy_data_header(),
            Some(VarInt::from_u32(55)),
        );
        assert_eq!(rec.magic, RECORD_MAGIC);
        assert_eq!(rec.seq, 42);
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
        let rec = build(0, 0, Direction::Outbound, &Header::Ping, None);
        assert_eq!(rec.packet_number, NO_PACKET_NUMBER);
        assert_eq!(rec.frame_type, KIND_PING);
    }

    #[test]
    fn record_is_64_bytes_with_no_padding() {
        // zerocopy::IntoBytes derive already enforces no padding at compile time; this pins the size.
        assert_eq!(core::mem::size_of::<FrameRecord>(), 64);
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
            let rec = build(i as u32, i as u64, *dir, header, *pn);
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
    /// Relies on nextest running each test in its own process, so setting the path env var before
    /// the first `record`/`trigger` (which lazily initializes the global state) is race-free.
    #[test]
    fn trigger_drives_background_dump() {
        let base = std::env::temp_dir().join(format!("s2n_dc_ft_e2e_{}.bin", std::process::id()));
        // The dumper writes the first dump to the `.000.bin` numbered file.
        let path = numbered_path(&base, 0);
        let _ = std::fs::remove_file(&path);
        // SAFETY: single-threaded test setup before the recorder thread is spawned.
        unsafe {
            std::env::set_var("S2N_DC_FRAME_TRACE_PATH", &base);
        }

        // Record a few frames then trigger, just as the QueueDbg handler does.
        record(
            Direction::Outbound,
            &dummy_data_header(),
            Some(VarInt::from_u32(11)),
        );
        record(
            Direction::Inbound,
            &Header::QueueDbg {
                dump_id: VarInt::from_u32(123),
                queue_pair: crate::packet::datagram::QueuePair {
                    source_queue_id: VarInt::from_u32(1),
                    dest_queue_id: VarInt::from_u32(2),
                },
                binding_id: VarInt::from_u32(4),
            },
            None,
        );
        assert_eq!(
            dump_path(),
            base,
            "global state must have used our env base path"
        );
        trigger();

        // The dumper is a fire-and-forget background thread; poll for the file to appear and
        // contain our QueueDbg dump_id as the newest record.
        let mut found = None;
        for _ in 0..200 {
            if let Ok(bytes) = std::fs::read(&path) {
                if bytes.len() >= 32 && bytes[..8] == FILE_MAGIC {
                    found = Some(bytes);
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let bytes = found.expect("background dumper should write the file");

        let capacity = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
        let head = u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as usize;
        let region = &bytes[32..32 + capacity];
        let mut recs = Vec::new();
        bump_ring::walk(region, head, capacity, |payload| {
            if let Ok(rec) = FrameRecord::read_from_bytes(payload) {
                if rec.magic == RECORD_MAGIC {
                    recs.push(rec);
                }
            }
        });
        let _ = std::fs::remove_file(&path);

        // Newest record is the QueueDbg we recorded last.
        assert!(recs.len() >= 2, "expected at least the two recorded frames");
        assert_eq!(recs[0].frame_type, KIND_QUEUE_DBG);
        assert_eq!(recs[0].dump_id, 123);
        assert_eq!(recs[1].frame_type, KIND_QUEUE_DATA);
        assert_eq!(recs[1].packet_number, 11);
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
