// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Diagnostic subscriber that silently accumulates events for each stream and
//! materializes the full event history as JSON when an error occurs.
//!
//! # Overview
//!
//! Most streams complete successfully. The diagnostic subscriber captures events
//! with minimal overhead and simply drops the buffer when a stream closes
//! normally. When a stream experiences an error, the subscriber writes the
//! complete event trace to the configured output directory as a JSON file.
//!
//! # Usage
//!
//! ```no_run
//! use s2n_quic_dc::event::diagnostic::Subscriber;
//! use std::path::PathBuf;
//!
//! let subscriber = Subscriber::new(PathBuf::from("/tmp/dc-traces"));
//! // Use with the event system, e.g.: (tracing_sub, diagnostic_sub)
//! ```

use crate::event::{self, api, Event};
use std::{
    fmt::Write as _,
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
};

/// A diagnostic subscriber that captures event traces for streams that experience errors.
#[derive(Clone, Debug)]
pub struct Subscriber {
    output_dir: PathBuf,
}

impl Subscriber {
    /// Creates a new diagnostic subscriber that writes error traces to the given directory.
    ///
    /// The directory will be created if it does not exist.
    pub fn new(output_dir: PathBuf) -> Self {
        // Best-effort directory creation at construction time
        let _ = std::fs::create_dir_all(&output_dir);
        Self { output_dir }
    }
}

/// Per-connection context that accumulates events.
///
/// Multiple tasks (read app, read worker, write app, write worker) may emit
/// events concurrently for the same stream, so all mutable state is properly
/// synchronized.
pub struct ConnectionContext {
    /// Unique connection ID
    connection_id: u64,
    /// The credential ID as hex
    credential_id: String,
    /// The key ID
    key_id: u64,
    /// The remote peer address
    remote_address: String,
    /// Whether this is the server side
    is_server: bool,
    /// Event buffer protected by a mutex for concurrent access from multiple tasks
    events: Mutex<Vec<EventRecord>>,
    /// Whether any error was observed (lock-free for fast checking)
    has_error: AtomicBool,
    /// Output directory for traces
    output_dir: PathBuf,
}

struct EventRecord {
    /// Event sequence number within this connection
    seq: u32,
    name: &'static str,
    detail: String,
}

/// Counter for generating unique trace filenames
static TRACE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn credential_id_to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl event::Subscriber for Subscriber {
    type ConnectionContext = ConnectionContext;

    fn create_connection_context(
        &self,
        meta: &api::ConnectionMeta,
        info: &api::ConnectionInfo,
    ) -> Self::ConnectionContext {
        ConnectionContext {
            connection_id: meta.id,
            credential_id: credential_id_to_hex(info.credential_id),
            key_id: info.key_id,
            remote_address: format!("{}", info.remote_address),
            is_server: info.is_server,
            events: Mutex::new(Vec::with_capacity(64)),
            has_error: AtomicBool::new(false),
            output_dir: self.output_dir.clone(),
        }
    }

    #[inline]
    fn on_connection_event<E: Event>(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &E,
    ) {
        // Safety: we have &self so we can't mutate context.
        // We need interior mutability. Since ConnectionContext is always
        // accessed behind a shared reference, we use a different approach:
        // we'll record in the specific event callbacks instead.
        let _ = (context, meta, event);
    }

    #[inline]
    fn on_stream_write_flushed(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteFlushed,
    ) {
        record_event(context, meta, "StreamWriteFlushed", event);
    }

    #[inline]
    fn on_stream_write_fin_flushed(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteFinFlushed,
    ) {
        record_event(context, meta, "StreamWriteFinFlushed", event);
    }

    #[inline]
    fn on_stream_write_blocked(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteBlocked,
    ) {
        record_event(context, meta, "StreamWriteBlocked", event);
    }

    #[inline]
    fn on_stream_write_errored(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteErrored,
    ) {
        record_event(context, meta, "StreamWriteErrored", event);
        mark_error(context);
    }

    #[inline]
    fn on_stream_write_key_updated(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteKeyUpdated,
    ) {
        record_event(context, meta, "StreamWriteKeyUpdated", event);
    }

    #[inline]
    fn on_stream_write_shutdown(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteShutdown,
    ) {
        record_event(context, meta, "StreamWriteShutdown", event);
    }

    #[inline]
    fn on_stream_write_socket_flushed(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteSocketFlushed,
    ) {
        record_event(context, meta, "StreamWriteSocketFlushed", event);
    }

    #[inline]
    fn on_stream_write_socket_blocked(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteSocketBlocked,
    ) {
        record_event(context, meta, "StreamWriteSocketBlocked", event);
    }

    #[inline]
    fn on_stream_write_socket_errored(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteSocketErrored,
    ) {
        record_event(context, meta, "StreamWriteSocketErrored", event);
        mark_error(context);
    }

    #[inline]
    fn on_stream_read_flushed(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadFlushed,
    ) {
        record_event(context, meta, "StreamReadFlushed", event);
    }

    #[inline]
    fn on_stream_read_fin_flushed(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadFinFlushed,
    ) {
        record_event(context, meta, "StreamReadFinFlushed", event);
    }

    #[inline]
    fn on_stream_read_blocked(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadBlocked,
    ) {
        record_event(context, meta, "StreamReadBlocked", event);
    }

    #[inline]
    fn on_stream_read_errored(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadErrored,
    ) {
        record_event(context, meta, "StreamReadErrored", event);
        mark_error(context);
    }

    #[inline]
    fn on_stream_read_shutdown(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadShutdown,
    ) {
        record_event(context, meta, "StreamReadShutdown", event);
    }

    #[inline]
    fn on_stream_read_socket_flushed(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadSocketFlushed,
    ) {
        record_event(context, meta, "StreamReadSocketFlushed", event);
    }

    #[inline]
    fn on_stream_read_socket_blocked(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadSocketBlocked,
    ) {
        record_event(context, meta, "StreamReadSocketBlocked", event);
    }

    #[inline]
    fn on_stream_read_socket_errored(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadSocketErrored,
    ) {
        record_event(context, meta, "StreamReadSocketErrored", event);
        mark_error(context);
    }

    #[inline]
    fn on_stream_packet_transmitted(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamPacketTransmitted,
    ) {
        record_event(context, meta, "StreamPacketTransmitted", event);
    }

    #[inline]
    fn on_stream_probe_transmitted(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamProbeTransmitted,
    ) {
        record_event(context, meta, "StreamProbeTransmitted", event);
    }

    #[inline]
    fn on_stream_packet_received(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamPacketReceived,
    ) {
        record_event(context, meta, "StreamPacketReceived", event);
    }

    #[inline]
    fn on_stream_packet_lost(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamPacketLost,
    ) {
        record_event(context, meta, "StreamPacketLost", event);
    }

    #[inline]
    fn on_stream_packet_acked(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamPacketAcked,
    ) {
        record_event(context, meta, "StreamPacketAcked", event);
    }

    #[inline]
    fn on_stream_packet_abandoned(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamPacketAbandoned,
    ) {
        record_event(context, meta, "StreamPacketAbandoned", event);
    }

    #[inline]
    fn on_stream_receiver_errored(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReceiverErrored,
    ) {
        record_event(context, meta, "StreamReceiverErrored", event);
        mark_error(context);
    }

    #[inline]
    fn on_stream_sender_errored(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamSenderErrored,
    ) {
        record_event(context, meta, "StreamSenderErrored", event);
        mark_error(context);
    }

    #[inline]
    fn on_stream_decrypt_packet(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamDecryptPacket,
    ) {
        record_event(context, meta, "StreamDecryptPacket", event);
    }

    #[inline]
    fn on_stream_max_data_received(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamMaxDataReceived,
    ) {
        record_event(context, meta, "StreamMaxDataReceived", event);
    }

    #[inline]
    fn on_stream_data_blocked_transmitted(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamDataBlockedTransmitted,
    ) {
        record_event(context, meta, "StreamDataBlockedTransmitted", event);
    }

    #[inline]
    fn on_stream_data_blocked_received(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamDataBlockedReceived,
    ) {
        record_event(context, meta, "StreamDataBlockedReceived", event);
    }

    #[inline]
    fn on_stream_control_packet_transmitted(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamControlPacketTransmitted,
    ) {
        record_event(context, meta, "StreamControlPacketTransmitted", event);
    }

    #[inline]
    fn on_stream_control_packet_received(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamControlPacketReceived,
    ) {
        record_event(context, meta, "StreamControlPacketReceived", event);
    }

    #[inline]
    fn on_connection_closed(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::ConnectionClosed,
    ) {
        let _ = (meta, event);
        // If no error was recorded, the buffer is simply dropped (the common case).
        if !context.has_error.load(Ordering::Acquire) {
            return;
        }

        // Materialize the event trace as JSON and write to file.
        if let Err(e) = write_trace(context) {
            tracing::warn!(
                connection_id = context.connection_id,
                error = %e,
                "failed to write diagnostic trace"
            );
        }
    }
}

/// Records an event into the connection context's buffer.
///
/// Acquires the mutex to append the event. Multiple tasks may call this
/// concurrently (read app, read worker, write app, write worker).
#[inline]
fn record_event<E: core::fmt::Debug>(
    context: &ConnectionContext,
    meta: &api::ConnectionMeta,
    name: &'static str,
    event: &E,
) {
    let _ = meta;
    let mut events = context.events.lock().unwrap_or_else(|e| e.into_inner());
    let seq = events.len() as u32;
    events.push(EventRecord {
        seq,
        name,
        detail: format!("{event:?}"),
    });
}

/// Marks a connection context as having experienced an error (lock-free).
#[inline]
fn mark_error(context: &ConnectionContext) {
    context.has_error.store(true, Ordering::Release);
}

/// Writes the accumulated trace for an errored connection to a JSON file.
fn write_trace(context: &ConnectionContext) -> std::io::Result<()> {
    let events = context.events.lock().unwrap_or_else(|e| e.into_inner());

    let seq = TRACE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let role = if context.is_server {
        "server"
    } else {
        "client"
    };
    let filename = format!(
        "trace-{role}-{seq}-{}.json",
        &context.credential_id[..8.min(context.credential_id.len())]
    );
    let path = context.output_dir.join(&filename);

    let mut buf = String::with_capacity(4096);

    buf.push_str("{\n");
    let _ = write!(buf, "  \"connection_id\": {},\n", context.connection_id);
    let _ = write!(buf, "  \"credential_id\": \"{}\",\n", context.credential_id);
    let _ = write!(buf, "  \"key_id\": {},\n", context.key_id);
    let _ = write!(
        buf,
        "  \"remote_address\": \"{}\",\n",
        context.remote_address
    );
    let _ = write!(buf, "  \"role\": \"{role}\",\n");
    let _ = write!(buf, "  \"event_count\": {},\n", events.len());
    buf.push_str("  \"events\": [\n");

    for (i, record) in events.iter().enumerate() {
        let comma = if i + 1 < events.len() { "," } else { "" };
        // Escape the debug string for JSON
        let escaped = escape_json(&record.detail);
        let _ = write!(
            buf,
            "    {{\"seq\": {}, \"event\": \"{}\", \"detail\": \"{escaped}\"}}{comma}\n",
            record.seq, record.name,
        );
    }

    buf.push_str("  ]\n");
    buf.push_str("}\n");

    let mut file = std::fs::File::create(&path)?;
    file.write_all(buf.as_bytes())?;

    Ok(())
}

/// Escapes a string for inclusion in a JSON string value.
fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

// ConnectionContext is Send + Sync because:
// - All immutable fields (connection_id, credential_id, etc.) are Send + Sync
// - events: Mutex<Vec<EventRecord>> is Send + Sync
// - has_error: AtomicBool is Send + Sync
// No manual unsafe impls needed — Rust derives them automatically.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_json_basic() {
        assert_eq!(escape_json("hello"), "hello");
        assert_eq!(escape_json("he\"llo"), "he\\\"llo");
        assert_eq!(escape_json("line1\nline2"), "line1\\nline2");
    }

    #[test]
    fn credential_id_hex() {
        assert_eq!(credential_id_to_hex(&[0xab, 0xcd, 0x01, 0x23]), "abcd0123");
    }
}
