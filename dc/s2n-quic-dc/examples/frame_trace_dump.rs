// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Offline parser for frame-trace dump files written by `endpoint::frame_trace`.
//!
//! This prints the dump as text (newest-first) for quick inspection. For analysis with external
//! tooling (DuckDB, pandas, …), prefer `cargo run -p xtask -- frame-trace <PATH>`, which emits the
//! same records as a queryable Parquet file.
//!
//! Usage: `cargo run --example frame_trace_dump -- <PATH>`
//!
//! `PATH` is a numbered dump file (`…​.000.bin`, `…​.001.bin`, …) written by the recorder — the
//! first dump (`.000.bin`) is usually the most useful. If omitted, falls back to
//! `$S2N_DC_FRAME_TRACE_PATH` (the base path) and then the temp dir's `s2n_dc_frame_trace.bin`,
//! neither of which is numbered, so prefer passing the explicit numbered file. Records are printed
//! newest-first (the order [`walk`] yields them).
//!
//! The parser reuses the in-crate backwards [`walk`] so it can never disagree with the producer
//! about framing. The [`FrameRecord`] layout is re-declared here (it lives in a `pub(crate)`
//! module) — keep it byte-compatible with `src/endpoint/frame_trace.rs`.

use s2n_quic_dc::sync::bump_ring::walk;
use std::path::PathBuf;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

const FILE_MAGIC: [u8; 8] = *b"DCFTRC01";
const RECORD_MAGIC: u8 = 0xF7;
const PACKET_RECORD_MAGIC: u8 = 0xF8;
const NO_PACKET_NUMBER: u64 = u64::MAX;

// Must match `endpoint::frame_trace::FrameRecord` (88 bytes). The leading 80 bytes are the v1
// layout; `reason`/`_pad`/`payload_len` were appended in v2.
#[repr(C)]
#[derive(Clone, Copy, Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
struct FrameRecord {
    timestamp_nanos: u64,
    packet_number: u64,
    offset: u64,
    dump_id: u64,
    source_queue_id: u64,
    dest_queue_id: u64,
    binding_id: u64,
    /// Canonicalized 16-byte credential id (endpoint bit cleared), big-endian as on the wire — the
    /// join key against `credentials=0x…` log lines.
    cred_id: [u8; 16],
    seq: u32,
    magic: u8,
    direction: u8,
    frame_type: u8,
    flags: u8,
    /// v2: a `DropReason` discriminant for `RxDropped`, else 0.
    reason: u8,
    _pad: [u8; 3],
    /// v2: bytes delivered for `AppRecv`, else 0.
    payload_len: u32,
}

// Must match `endpoint::frame_trace::PacketRecord` (56 bytes). Discriminated from a frame record by
// its distinct length and `PACKET_RECORD_MAGIC`.
#[repr(C)]
#[derive(Clone, Copy, Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
struct PacketRecord {
    timestamp_nanos: u64,
    packet_number: u64,
    linked_pn: u64,
    cred_id: [u8; 16],
    seq: u32,
    sent_bytes: u32,
    frame_count: u16,
    magic: u8,
    event: u8,
    reason: u8,
    _pad: [u8; 3],
}

fn packet_event_str(e: u8) -> &'static str {
    match e {
        0 => "rx-arrived",
        1 => "rx-dropped",
        2 => "sent",
        3 => "acked",
        4 => "lost",
        _ => "?",
    }
}

fn reason_str(r: u8) -> &'static str {
    match r {
        0 => "",
        1 => "unallocated",
        2 => "half-closed",
        3 => "stale-binding",
        4 => "future-binding",
        5 => "sender-closed",
        6 => "cap-exceeded",
        7 => "window-violation",
        8 => "msg-insert-rejected",
        9 => "decrypt-failed",
        10 => "replay-detected",
        11 => "duplicate",
        12 => "path-secret-not-found",
        _ => "?",
    }
}

/// Format the credential id the same way `credentials::Id` Displays in the logs: `0x` + big-endian
/// hex of the 16 bytes (`{:#01x}` of the u128), so a grep matches.
fn cred_str(id: &[u8; 16]) -> String {
    format!("{:#01x}", u128::from_be_bytes(*id))
}

fn direction_str(d: u8) -> &'static str {
    match d {
        0 => "in",
        1 => "in-fast",
        2 => "out",
        3 => "ack-ok",
        4 => "ack-lost",
        5 => "ack-cancel",
        6 => "app-send",
        7 => "app-recv",
        8 => "send-cancel",
        9 => "rx-drop",
        _ => "?",
    }
}

fn kind_str(k: u8) -> &'static str {
    match k {
        1 => "QueueData",
        2 => "QueueControl",
        3 => "QueueMaxData",
        4 => "QueueReset",
        5 => "QueueFree",
        6 => "Ack",
        7 => "QueueMsg",
        8 => "Ping",
        9 => "QueueDataBlocked",
        10 => "QueueDbg",
        _ => "?",
    }
}

fn flags_str(f: u8) -> String {
    let mut parts = Vec::new();
    if f & (1 << 0) != 0 {
        parts.push("fin");
    }
    if f & (1 << 1) != 0 {
        parts.push("blocked");
    }
    if f & (1 << 2) != 0 {
        parts.push("wakeup");
    }
    if f & (1 << 3) != 0 {
        parts.push("ack-eliciting");
    }
    if f & (1 << 4) != 0 {
        parts.push("init");
    }
    parts.join("|")
}

/// Inserts a zero-padded sequence number before the extension, matching the recorder's
/// `numbered_path`: `s2n_dc_frame_trace.bin` → `s2n_dc_frame_trace.000.bin`.
fn numbered(base: &std::path::Path, seq: u64) -> PathBuf {
    let stem = base.file_stem().map(|s| s.to_string_lossy().into_owned());
    let ext = base.extension().map(|s| s.to_string_lossy().into_owned());
    let name = match (stem, ext) {
        (Some(stem), Some(ext)) => format!("{stem}.{seq:03}.{ext}"),
        (Some(stem), None) => format!("{stem}.{seq:03}"),
        _ => format!("frame_trace.{seq:03}.bin"),
    };
    base.with_file_name(name)
}

fn main() {
    let base = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("S2N_DC_FRAME_TRACE_PATH").map(PathBuf::from))
        .unwrap_or_else(|| std::env::temp_dir().join("s2n_dc_frame_trace.bin"));

    // The recorder writes numbered dumps, so the literal base path usually does not exist. Prefer
    // it if it does (an explicit numbered file passed as the arg), else fall back to the first
    // numbered dump `.000` — usually the most useful.
    let path = if base.exists() {
        base.clone()
    } else {
        numbered(&base, 0)
    };

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("failed to read {}: {e}", path.display());
            std::process::exit(1);
        }
    };

    if bytes.len() < 32 || bytes[..8] != FILE_MAGIC {
        eprintln!("not a frame-trace dump (bad magic): {}", path.display());
        std::process::exit(1);
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let len_suffix = bytes[12];
    let payload_stride = bytes[13];
    let capacity = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
    let head = u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as usize;

    eprintln!(
        "frame-trace dump v{version} (len_suffix={len_suffix} stride={payload_stride} \
         capacity={capacity} head={head}) — records newest-first \
         (v3+: t= is Unix-epoch nanoseconds):"
    );

    let region = &bytes[32..32 + capacity.min(bytes.len() - 32)];
    let mut n = 0usize;
    // The ring interleaves two record types of different sizes. Discriminate on payload length
    // first (the framing the producer guarantees), then validate the per-type magic — a length that
    // matches but whose magic doesn't means we walked into an overwritten/torn tail, so we stop.
    walk(region, head, capacity, |payload| {
        if payload.len() == core::mem::size_of::<FrameRecord>() {
            if let Ok(rec) = FrameRecord::read_from_bytes(payload) {
                if rec.magic == RECORD_MAGIC {
                    print_frame(&rec);
                    n += 1;
                }
            }
        } else if payload.len() == core::mem::size_of::<PacketRecord>() {
            if let Ok(rec) = PacketRecord::read_from_bytes(payload) {
                if rec.magic == PACKET_RECORD_MAGIC {
                    print_packet(&rec);
                    n += 1;
                }
            }
        }
    });
    eprintln!("{n} records");
}

fn pn_str(pn: u64) -> String {
    if pn == NO_PACKET_NUMBER {
        "-".to_string()
    } else {
        pn.to_string()
    }
}

fn print_frame(rec: &FrameRecord) {
    // Append the v2 extras only when they carry information, so v1-shaped records read identically.
    let mut extra = flags_str(rec.flags);
    let reason = reason_str(rec.reason);
    if !reason.is_empty() {
        if !extra.is_empty() {
            extra.push('|');
        }
        extra.push_str(reason);
    }
    if rec.payload_len != 0 {
        if !extra.is_empty() {
            extra.push('|');
        }
        extra.push_str(&format!("len={}", rec.payload_len));
    }
    println!(
        "seq={:>10} t={:>14}ns {:>11} {:<16} cred={} src={} dst={} bind={} pn={} off={} dump_id={} [{}]",
        rec.seq,
        rec.timestamp_nanos,
        direction_str(rec.direction),
        kind_str(rec.frame_type),
        cred_str(&rec.cred_id),
        rec.source_queue_id,
        rec.dest_queue_id,
        rec.binding_id,
        pn_str(rec.packet_number),
        rec.offset,
        rec.dump_id,
        extra,
    );
}

fn print_packet(rec: &PacketRecord) {
    let mut extra = Vec::new();
    if rec.sent_bytes != 0 {
        extra.push(format!("bytes={}", rec.sent_bytes));
    }
    if rec.frame_count != 0 {
        extra.push(format!("frames={}", rec.frame_count));
    }
    if rec.linked_pn != NO_PACKET_NUMBER {
        extra.push(format!("linked_pn={}", rec.linked_pn));
    }
    let reason = reason_str(rec.reason);
    if !reason.is_empty() {
        extra.push(reason.to_string());
    }
    // Packet records carry no frame type / offsets; the leading columns line up with frame records
    // so a mixed dump stays readable, and the PACKET marker makes the row type obvious.
    println!(
        "seq={:>10} t={:>14}ns {:>11} {:<16} cred={} pn={} [{}]",
        rec.seq,
        rec.timestamp_nanos,
        packet_event_str(rec.event),
        "PACKET",
        cred_str(&rec.cred_id),
        pn_str(rec.packet_number),
        extra.join("|"),
    );
}
