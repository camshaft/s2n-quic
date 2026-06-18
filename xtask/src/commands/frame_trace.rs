// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Converts a frame-trace dump file (written by `endpoint::frame_trace`) into a queryable Parquet
//! file.
//!
//! The dump is the raw snapshot of a [`BumpRing`] of two interleaved fixed-size record types — a
//! per-frame [`FrameRecord`] (88 bytes) and a coarser per-packet [`PacketRecord`] (56 bytes). The
//! offline text dumper (`cargo run --example frame_trace_dump`) prints them newest-first; this
//! command instead flattens both record types into one Parquet table (one row per record, a `kind`
//! column discriminating the two) so external tooling — DuckDB, pandas, etc. — can slice and join
//! the trace instead of grepping text.
//!
//! The on-disk framing, the [`FrameRecord`]/[`PacketRecord`] layouts, and every discriminant map
//! are re-declared here to keep `xtask` decoupled from the `pub(crate)` recorder module — they must
//! stay byte-compatible with `dc/s2n-quic-dc/src/endpoint/frame_trace.rs`. The dump's `FILE_MAGIC`
//! and `FILE_VERSION` are validated on read, so a layout bump that forgets to update this code
//! fails loudly rather than decoding garbage.

use anyhow::{Context, Result, bail};
use arrow::{
    array::*,
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
use clap::Args;
use parquet::{arrow::ArrowWriter, basic::Compression, file::properties::WriterProperties};
use std::{path::PathBuf, sync::Arc};
use xshell::Shell;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// ── On-disk framing (must match endpoint::frame_trace::write_dump) ──────────────────────────────

const FILE_MAGIC: [u8; 8] = *b"DCFTRC01";
const FILE_VERSION: u32 = 3;
const RECORD_MAGIC: u8 = 0xF7;
const PACKET_RECORD_MAGIC: u8 = 0xF8;
const NO_PACKET_NUMBER: u64 = u64::MAX;

/// Width of the ring's trailing little-endian length suffix (see `sync::bump_ring`).
const LEN_SUFFIX: usize = 2;

// Must match `endpoint::frame_trace::FrameRecord` (88 bytes).
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
    cred_id: [u8; 16],
    seq: u32,
    magic: u8,
    direction: u8,
    frame_type: u8,
    flags: u8,
    reason: u8,
    _pad: [u8; 3],
    payload_len: u32,
}

// Must match `endpoint::frame_trace::PacketRecord` (56 bytes).
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

// ── Discriminant maps (must match the enums in endpoint::frame_trace) ───────────────────────────

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

fn frame_kind_str(k: u8) -> &'static str {
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

/// Maps a `DropReason` discriminant to its label, or `None` for the resting value (`DropReason::None`).
fn reason_str(r: u8) -> Option<&'static str> {
    Some(match r {
        0 => return None,
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
    })
}

// Flag bits packed into `FrameRecord::flags`.
const FLAG_FIN: u8 = 1 << 0;
const FLAG_BLOCKED: u8 = 1 << 1;
const FLAG_WAKEUP: u8 = 1 << 2;
const FLAG_ACK_ELICITING: u8 = 1 << 3;
const FLAG_INIT: u8 = 1 << 4;

/// Formats a 16-byte credential id the way `credentials::Id` Displays in the logs: `0x` + big-endian
/// hex of the bytes, so a row joins against the existing `credentials=0x…` log lines.
fn cred_str(id: &[u8; 16]) -> String {
    format!("{:#01x}", u128::from_be_bytes(*id))
}

#[derive(Args)]
pub struct FrameTrace {
    /// Frame-trace dump file (a numbered `…​.000.bin` written by the recorder).
    dump: PathBuf,

    /// Output Parquet path (defaults to the dump path with a `.parquet` extension).
    #[arg(long, short = 'o')]
    output: Option<PathBuf>,
}

impl FrameTrace {
    pub fn run(self, _sh: &Shell) -> Result<()> {
        let bytes = std::fs::read(&self.dump)
            .with_context(|| format!("failed to read dump {}", self.dump.display()))?;

        let (capacity, head, region) = parse_header(&bytes)?;

        let output = self
            .output
            .unwrap_or_else(|| self.dump.with_extension("parquet"));

        let mut builder = RowBuilder::new();
        let mut frames = 0u64;
        let mut packets = 0u64;
        walk(region, head, capacity, |payload| {
            if payload.len() == core::mem::size_of::<FrameRecord>() {
                if let Ok(rec) = FrameRecord::read_from_bytes(payload)
                    && rec.magic == RECORD_MAGIC
                {
                    builder.push_frame(&rec);
                    frames += 1;
                }
            } else if payload.len() == core::mem::size_of::<PacketRecord>()
                && let Ok(rec) = PacketRecord::read_from_bytes(payload)
                && rec.magic == PACKET_RECORD_MAGIC
            {
                builder.push_packet(&rec);
                packets += 1;
            }
        });

        let batch = builder.finish();
        let file = std::fs::File::create(&output)
            .with_context(|| format!("failed to create {}", output.display()))?;
        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(Default::default()))
            .build();
        let mut writer = ArrowWriter::try_new(file, schema(), Some(props))
            .context("failed to create parquet writer")?;
        writer.write(&batch).context("failed to write batch")?;
        writer.close().context("failed to finalize parquet file")?;

        eprintln!(
            "wrote {} records ({frames} frames, {packets} packets) → {}",
            frames + packets,
            output.display()
        );
        Ok(())
    }
}

/// Validates the dump header and returns `(capacity, head, region)`. The header layout is fixed by
/// `endpoint::frame_trace::write_dump`: `magic[8]`, `version u32`, `len_suffix u8`,
/// `payload_stride u8`, `reserved u16`, `capacity u64`, `head u64`, then `region[capacity]`.
fn parse_header(bytes: &[u8]) -> Result<(usize, usize, &[u8])> {
    if bytes.len() < 32 || bytes[..8] != FILE_MAGIC {
        bail!("not a frame-trace dump (bad magic)");
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if version != FILE_VERSION {
        bail!("unsupported dump version {version} (this build expects v{FILE_VERSION})");
    }
    let capacity = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
    let head = u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as usize;
    let region = bytes
        .get(32..32 + capacity)
        .context("dump truncated: region shorter than declared capacity")?;
    Ok((capacity, head, region))
}

/// Walks the ring snapshot newest-first, yielding each record's payload. A faithful re-implementation
/// of `sync::bump_ring::walk` (kept here to avoid depending on the `pub(crate)` recorder crate).
fn walk(region: &[u8], head: usize, capacity: usize, mut f: impl FnMut(&[u8])) {
    if region.len() != capacity || !capacity.is_power_of_two() {
        return;
    }
    let mask = capacity - 1;
    let valid_low = head.saturating_sub(capacity);

    let read_wrapping = |abs: usize, len: usize, out: &mut [u8]| {
        let begin = abs & mask;
        let first = (capacity - begin).min(len);
        out[..first].copy_from_slice(&region[begin..begin + first]);
        if first < len {
            out[first..len].copy_from_slice(&region[..len - first]);
        }
    };

    let mut abs = head;
    while abs >= valid_low + LEN_SUFFIX {
        let suffix_start = abs - LEN_SUFFIX;
        let mut len_bytes = [0u8; LEN_SUFFIX];
        read_wrapping(suffix_start, LEN_SUFFIX, &mut len_bytes);
        let payload_len = u16::from_le_bytes(len_bytes) as usize;
        let rec_total = payload_len + LEN_SUFFIX;

        if rec_total > abs {
            break;
        }
        let payload_start = abs - rec_total;
        if payload_start < valid_low {
            break;
        }

        let mut payload = vec![0u8; payload_len];
        read_wrapping(payload_start, payload_len, &mut payload);
        f(&payload);

        abs = payload_start;
    }
}

// ── Parquet schema + row builder ────────────────────────────────────────────────────────────────

/// One unified table for both record kinds. Columns that don't apply to a record kind are left null
/// (e.g. `event` is set only for packet rows, the `flag_*` / `offset` columns only for frame rows),
/// so a query can filter `WHERE kind = 'frame'` or join the two on `packet_number`.
fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("seq", DataType::UInt32, false),
        Field::new("ts_nanos", DataType::UInt64, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("cred_id", DataType::Utf8, false),
        // shared, but sentinel-valued → null
        Field::new("packet_number", DataType::UInt64, true),
        // frame-only
        Field::new("direction", DataType::Utf8, true),
        Field::new("frame_type", DataType::Utf8, true),
        Field::new("source_queue_id", DataType::UInt64, true),
        Field::new("dest_queue_id", DataType::UInt64, true),
        Field::new("binding_id", DataType::UInt64, true),
        // The frame's "primary" offset (its meaning is frame-type-specific — see the recorder's
        // `build`). Named `stream_offset` rather than `offset` because `offset` is a SQL reserved
        // word that would force every query to quote it.
        Field::new("stream_offset", DataType::UInt64, true),
        Field::new("dump_id", DataType::UInt64, true),
        Field::new("payload_len", DataType::UInt32, true),
        Field::new("flag_fin", DataType::Boolean, true),
        Field::new("flag_blocked", DataType::Boolean, true),
        Field::new("flag_wakeup", DataType::Boolean, true),
        Field::new("flag_ack_eliciting", DataType::Boolean, true),
        Field::new("flag_init", DataType::Boolean, true),
        // packet-only
        Field::new("event", DataType::Utf8, true),
        Field::new("linked_pn", DataType::UInt64, true),
        Field::new("sent_bytes", DataType::UInt32, true),
        Field::new("frame_count", DataType::UInt32, true),
        // both (drop reason; null when not a drop)
        Field::new("reason", DataType::Utf8, true),
    ]))
}

struct RowBuilder {
    seq: UInt32Builder,
    ts_nanos: UInt64Builder,
    kind: StringBuilder,
    cred_id: StringBuilder,
    packet_number: UInt64Builder,
    direction: StringBuilder,
    frame_type: StringBuilder,
    source_queue_id: UInt64Builder,
    dest_queue_id: UInt64Builder,
    binding_id: UInt64Builder,
    offset: UInt64Builder,
    dump_id: UInt64Builder,
    payload_len: UInt32Builder,
    flag_fin: BooleanBuilder,
    flag_blocked: BooleanBuilder,
    flag_wakeup: BooleanBuilder,
    flag_ack_eliciting: BooleanBuilder,
    flag_init: BooleanBuilder,
    event: StringBuilder,
    linked_pn: UInt64Builder,
    sent_bytes: UInt32Builder,
    frame_count: UInt32Builder,
    reason: StringBuilder,
}

impl RowBuilder {
    fn new() -> Self {
        Self {
            seq: UInt32Builder::new(),
            ts_nanos: UInt64Builder::new(),
            kind: StringBuilder::new(),
            cred_id: StringBuilder::new(),
            packet_number: UInt64Builder::new(),
            direction: StringBuilder::new(),
            frame_type: StringBuilder::new(),
            source_queue_id: UInt64Builder::new(),
            dest_queue_id: UInt64Builder::new(),
            binding_id: UInt64Builder::new(),
            offset: UInt64Builder::new(),
            dump_id: UInt64Builder::new(),
            payload_len: UInt32Builder::new(),
            flag_fin: BooleanBuilder::new(),
            flag_blocked: BooleanBuilder::new(),
            flag_wakeup: BooleanBuilder::new(),
            flag_ack_eliciting: BooleanBuilder::new(),
            flag_init: BooleanBuilder::new(),
            event: StringBuilder::new(),
            linked_pn: UInt64Builder::new(),
            sent_bytes: UInt32Builder::new(),
            frame_count: UInt32Builder::new(),
            reason: StringBuilder::new(),
        }
    }

    fn push_frame(&mut self, rec: &FrameRecord) {
        self.seq.append_value(rec.seq);
        self.ts_nanos.append_value(rec.timestamp_nanos);
        self.kind.append_value("frame");
        self.cred_id.append_value(cred_str(&rec.cred_id));
        append_pn(&mut self.packet_number, rec.packet_number);

        self.direction.append_value(direction_str(rec.direction));
        self.frame_type.append_value(frame_kind_str(rec.frame_type));
        self.source_queue_id.append_value(rec.source_queue_id);
        self.dest_queue_id.append_value(rec.dest_queue_id);
        self.binding_id.append_value(rec.binding_id);
        self.offset.append_value(rec.offset);
        // dump_id is only meaningful for QueueDbg frames; 0 elsewhere → null.
        match rec.dump_id {
            0 => self.dump_id.append_null(),
            v => self.dump_id.append_value(v),
        }
        // payload_len carries the delivered byte count for app-recv; 0 elsewhere → null.
        match rec.payload_len {
            0 => self.payload_len.append_null(),
            v => self.payload_len.append_value(v),
        }
        self.flag_fin.append_value(rec.flags & FLAG_FIN != 0);
        self.flag_blocked.append_value(rec.flags & FLAG_BLOCKED != 0);
        self.flag_wakeup.append_value(rec.flags & FLAG_WAKEUP != 0);
        self.flag_ack_eliciting
            .append_value(rec.flags & FLAG_ACK_ELICITING != 0);
        self.flag_init.append_value(rec.flags & FLAG_INIT != 0);

        // packet-only columns: null for a frame row.
        self.event.append_null();
        self.linked_pn.append_null();
        self.sent_bytes.append_null();
        self.frame_count.append_null();

        append_reason(&mut self.reason, rec.reason);
    }

    fn push_packet(&mut self, rec: &PacketRecord) {
        self.seq.append_value(rec.seq);
        self.ts_nanos.append_value(rec.timestamp_nanos);
        self.kind.append_value("packet");
        self.cred_id.append_value(cred_str(&rec.cred_id));
        append_pn(&mut self.packet_number, rec.packet_number);

        // frame-only columns: null for a packet row.
        self.direction.append_null();
        self.frame_type.append_null();
        self.source_queue_id.append_null();
        self.dest_queue_id.append_null();
        self.binding_id.append_null();
        self.offset.append_null();
        self.dump_id.append_null();
        self.payload_len.append_null();
        self.flag_fin.append_null();
        self.flag_blocked.append_null();
        self.flag_wakeup.append_null();
        self.flag_ack_eliciting.append_null();
        self.flag_init.append_null();

        self.event.append_value(packet_event_str(rec.event));
        append_pn(&mut self.linked_pn, rec.linked_pn);
        // sent_bytes/frame_count describe a Sent segment; 0 elsewhere → null.
        match rec.sent_bytes {
            0 => self.sent_bytes.append_null(),
            v => self.sent_bytes.append_value(v),
        }
        match rec.frame_count {
            0 => self.frame_count.append_null(),
            v => self.frame_count.append_value(v as u32),
        }

        append_reason(&mut self.reason, rec.reason);
    }

    fn finish(mut self) -> RecordBatch {
        RecordBatch::try_new(
            schema(),
            vec![
                Arc::new(self.seq.finish()),
                Arc::new(self.ts_nanos.finish()),
                Arc::new(self.kind.finish()),
                Arc::new(self.cred_id.finish()),
                Arc::new(self.packet_number.finish()),
                Arc::new(self.direction.finish()),
                Arc::new(self.frame_type.finish()),
                Arc::new(self.source_queue_id.finish()),
                Arc::new(self.dest_queue_id.finish()),
                Arc::new(self.binding_id.finish()),
                Arc::new(self.offset.finish()),
                Arc::new(self.dump_id.finish()),
                Arc::new(self.payload_len.finish()),
                Arc::new(self.flag_fin.finish()),
                Arc::new(self.flag_blocked.finish()),
                Arc::new(self.flag_wakeup.finish()),
                Arc::new(self.flag_ack_eliciting.finish()),
                Arc::new(self.flag_init.finish()),
                Arc::new(self.event.finish()),
                Arc::new(self.linked_pn.finish()),
                Arc::new(self.sent_bytes.finish()),
                Arc::new(self.frame_count.finish()),
                Arc::new(self.reason.finish()),
            ],
        )
        .expect("schema mismatch in frame-trace row builder")
    }
}

/// Appends a packet number, mapping the `NO_PACKET_NUMBER` sentinel to null.
fn append_pn(builder: &mut UInt64Builder, pn: u64) {
    match pn {
        NO_PACKET_NUMBER => builder.append_null(),
        v => builder.append_value(v),
    }
}

/// Appends a drop-reason label, mapping `DropReason::None` to null.
fn append_reason(builder: &mut StringBuilder, reason: u8) {
    match reason_str(reason) {
        Some(s) => builder.append_value(s),
        None => builder.append_null(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_sizes_match_recorder() {
        // The length-based discrimination in `walk`'s callback depends on these exact sizes; if a
        // layout change in the recorder bumps them, this test (and the FILE_VERSION check) catch it.
        assert_eq!(core::mem::size_of::<FrameRecord>(), 88);
        assert_eq!(core::mem::size_of::<PacketRecord>(), 56);
    }

    #[test]
    fn cred_str_matches_log_format() {
        let mut id = [0u8; 16];
        id[15] = 0x2a;
        assert_eq!(cred_str(&id), "0x2a");
    }
}
