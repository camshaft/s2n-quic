// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Offline parser for frame-trace dump files written by `endpoint::frame_trace`.
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
const NO_PACKET_NUMBER: u64 = u64::MAX;

// Must match `endpoint::frame_trace::FrameRecord`.
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
    seq: u32,
    magic: u8,
    direction: u8,
    frame_type: u8,
    flags: u8,
}

fn direction_str(d: u8) -> &'static str {
    match d {
        0 => "in",
        1 => "in-fast",
        2 => "out",
        3 => "ack-ok",
        4 => "ack-lost",
        5 => "ack-cancel",
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

fn main() {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("S2N_DC_FRAME_TRACE_PATH").map(PathBuf::from))
        .unwrap_or_else(|| std::env::temp_dir().join("s2n_dc_frame_trace.bin"));

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
         capacity={capacity} head={head}) — records newest-first:"
    );

    let region = &bytes[32..32 + capacity.min(bytes.len() - 32)];
    let mut n = 0usize;
    walk(region, head, capacity, |payload| {
        match FrameRecord::read_from_bytes(payload) {
            Ok(rec) if rec.magic == RECORD_MAGIC => {
                let pn = if rec.packet_number == NO_PACKET_NUMBER {
                    "-".to_string()
                } else {
                    rec.packet_number.to_string()
                };
                println!(
                    "seq={:>10} t={:>14}ns {:>10} {:<16} src={} dst={} bind={} pn={} off={} dump_id={} [{}]",
                    rec.seq,
                    rec.timestamp_nanos,
                    direction_str(rec.direction),
                    kind_str(rec.frame_type),
                    rec.source_queue_id,
                    rec.dest_queue_id,
                    rec.binding_id,
                    pn,
                    rec.offset,
                    rec.dump_id,
                    flags_str(rec.flags),
                );
                n += 1;
            }
            // A bad magic means we walked into an overwritten/torn tail — stop reporting.
            _ => {}
        }
    });
    eprintln!("{n} records");
}
