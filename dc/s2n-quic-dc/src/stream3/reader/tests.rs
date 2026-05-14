// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests for the stream3 Reader.
//!
//! ## Organization
//!
//! * **Synchronous unit tests** – exercise `write_data_reader` directly with no
//!   I/O or tasks needed; these test an internal helper function in isolation.
//!
//! * **Bach async tests** – each test runs inside `crate::testing::sim` and uses
//!   **two separate primary tasks** to model how a real application and endpoint
//!   interact:
//!
//!   * **Application task** (primary) – owns the [`Reader`]; calls `read_into`
//!     and asserts on the data it receives.
//!   * **Endpoint task** (primary) – owns the [`Pusher`]; sends [`msg::Stream`]
//!     messages into the flow queue and asserts on [`Frame`]s the Reader emits
//!     (e.g. `MAX_DATA`, `STOP_SENDING`).
//!
//!   Both tasks are marked `.primary()` so the sim runs until both complete.
//!   The two sides talk over the real flow-queue / frame-submission channels,
//!   without any actual UDP sockets or cryptography.

use super::{msg, write_data_reader, Reader};
use crate::{
    flow,
    intrusive_queue,
    path::secret::map::Entry as PathSecretEntry,
    stream3::frame::{self, Frame, Header, PriorityStorage, SubmissionReceiver},
};
use bytes::BytesMut;
use s2n_quic_core::{buffer::Reassembler, endpoint, stream::testing::Data, varint::VarInt};
use std::net::SocketAddr;

// ─── Test helpers ─────────────────────────────────────────────────────────────

/// Creates a connected `(Reader, Pusher)` pair for use in tests.
///
/// * `Reader` – the component under test; owns the stream-side receive handle.
/// * `Pusher` – the mock endpoint side; can inject stream messages and receive
///   outbound frames submitted by the Reader (e.g. `MAX_DATA`).
fn make_pair() -> (Reader, Pusher) {
    make_pair_with_type(endpoint::Type::Client)
}

/// Creates a server-side `(Reader, Pusher)` pair (starts in `PendingValidation`).
fn make_server_pair() -> (Reader, Pusher) {
    make_pair_with_type(endpoint::Type::Server)
}

fn make_pair_with_type(ep_type: endpoint::Type) -> (Reader, Pusher) {
    let stream_id = VarInt::from_u8(1);
    let peer: SocketAddr = "127.0.0.1:4433".parse().unwrap();
    let path_secret_entry = PathSecretEntry::fake_deterministic(peer, ep_type);

    let allocator = msg::queue::Allocator::new();
    let dispatcher = allocator.dispatcher();
    let handle = flow::Handle::client(stream_id, path_secret_entry.clone());
    let (_control, stream_rx) = dispatcher
        .alloc(handle, Some(VarInt::from_u8(2)))
        .expect("queue alloc should succeed");

    let queue_id = stream_rx.queue_id();
    let request = flow::Request {
        credential_id: *path_secret_entry.id(),
        stream_id,
    };

    let (frame_tx, frame_rx) = frame::submission_channel(1);

    let reader = match ep_type {
        endpoint::Type::Client => {
            Reader::new_client(frame_tx, path_secret_entry, stream_id, stream_rx)
        }
        endpoint::Type::Server => {
            Reader::new_server_pending(frame_tx, path_secret_entry, stream_id, stream_rx)
        }
    };

    let pusher = Pusher {
        dispatcher,
        queue_id,
        request,
        frame_rx,
    };

    (reader, pusher)
}

/// Mock endpoint side of a reader test.
///
/// `push_*` injects [`msg::Stream`] messages into the flow-queue dispatcher,
/// automatically waking any waiting Reader task.  `recv_frames` asynchronously
/// waits for [`Frame`]s that the Reader submitted (e.g. `MAX_DATA`,
/// `STOP_SENDING`).
struct Pusher {
    dispatcher: msg::queue::Dispatcher,
    queue_id: VarInt,
    request: flow::Request,
    /// Outbound frames submitted by the Reader (MAX_DATA, STOP_SENDING, …).
    frame_rx: SubmissionReceiver,
}

impl Pusher {
    fn push(&mut self, message: msg::Stream) {
        // `send_stream` returns an `AutoWake` that fires the registered waker
        // on drop.  Binding to `_` drops it immediately, waking a waiting
        // Reader task right away.
        let _ = self
            .dispatcher
            .send_stream(
                self.queue_id,
                None,
                &self.request,
                intrusive_queue::Entry::new(message),
            )
            .unwrap_or_else(|_| panic!("send_stream should succeed in tests"));
    }

    fn push_data(&mut self, offset: u64, data: &[u8], fin: bool) {
        self.push(msg::Stream::Data {
            offset: VarInt::new(offset).unwrap(),
            payload: BytesMut::from(data),
            fin,
        });
    }

    fn push_reset(&mut self, error_code: VarInt) {
        self.push(msg::Stream::Reset { error_code });
    }

    fn push_flow_validated(&mut self) {
        self.push(msg::Stream::FlowValidated);
    }

    /// Asynchronously wait for frames submitted by the Reader.
    ///
    /// Suspends until at least one frame (or a channel-close) is received,
    /// then returns all frames collected in that batch.
    async fn recv_frames(&mut self) -> Vec<Frame> {
        let mut storage = PriorityStorage::default();
        core::future::poll_fn(|cx| self.frame_rx.poll_swap(cx, &mut storage)).await;
        let mut frames = Vec::new();
        for (_priority, queue) in storage.drain() {
            for entry in queue {
                frames.push(entry.into_inner());
            }
        }
        frames
    }
}

// ─── write_data_reader unit tests (no I/O, no tasks) ──────────────────────────

#[test]
fn write_data_reader_bypasses_reassembler_for_in_order_data() {
    let mut reassembler = Reassembler::new();
    let mut reader = Data::new(8);
    let mut app_buf: Vec<u8> = Vec::new();

    write_data_reader(&mut reassembler, &mut reader, &mut app_buf, true).unwrap();

    assert_eq!(app_buf, Data::send_one_at(0, 8));
    assert_eq!(reassembler.consumed_len(), 8);
    assert_eq!(reassembler.final_size(), Some(8));
    assert!(reassembler.is_empty());
    assert!(reassembler.is_reading_complete());
}

#[test]
fn write_data_reader_keeps_out_of_order_data_in_reassembler() {
    let mut reassembler = Reassembler::new();
    let mut reader = Data::new(8);
    let mut app_buf: Vec<u8> = Vec::new();

    reader.seek_forward(4);
    write_data_reader(&mut reassembler, &mut reader, &mut app_buf, true).unwrap();

    // Nothing was delivered to the application yet — the tail (offset 4-7) is
    // buffered in the reassembler, but there is a gap at 0-3.  `is_empty()` and
    // `total_received_len()` both report zero because they only count bytes
    // contiguous from the current read position (offset 0).  `final_size()` is
    // set, confirming the tail and FIN were recorded internally.
    assert!(app_buf.is_empty());
    assert_eq!(reassembler.consumed_len(), 0);
    assert_eq!(reassembler.total_received_len(), 0);
    assert!(reassembler.is_empty());
    assert!(!reassembler.is_reading_complete());
    assert_eq!(
        reassembler.final_size(),
        Some(8),
        "FIN should be recorded even though the head is missing"
    );

    // Once the missing head is written, all 8 bytes become available.
    reassembler
        .write_at(0u32.into(), &Data::send_one_at(0, 4))
        .unwrap();
    assert_eq!(reassembler.len(), 8);
}

#[test]
fn write_data_reader_does_not_interpose_when_reassembler_has_head_data() {
    let mut reassembler = Reassembler::new();
    let mut reader = Data::new(8);
    let mut app_buf: Vec<u8> = Vec::new();

    reassembler
        .write_at(0u32.into(), &Data::send_one_at(0, 4))
        .unwrap();
    reader.seek_forward(4);

    write_data_reader(&mut reassembler, &mut reader, &mut app_buf, true).unwrap();

    // The interposer bypass is skipped because the reassembler already holds
    // data at the head (offset 0-3).  Both head and tail (reader, offset 4-7)
    // are stored in the reassembler; all 8 bytes are contiguous so they are
    // immediately accessible without a gap.
    assert!(app_buf.is_empty());
    assert_eq!(reassembler.len(), 8);
    assert_eq!(reassembler.total_received_len(), 8);
    assert!(!reassembler.is_empty());
}

// ─── Bach async tests ─────────────────────────────────────────────────────────
//
// Each test uses two *primary* tasks:
//   • endpoint task – owns Pusher; sends stream messages and asserts on frames.
//   • app task      – owns Reader; calls read_into and asserts on received data.
//
// Both tasks are marked `.primary()` so the bach sim runs until *both* complete,
// providing backpressure-free cooperative scheduling between the two sides.

/// Basic in-order read: endpoint sends data + FIN, application reads until EOF.
#[test]
fn basic_read() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let (mut reader, mut pusher) = make_pair();

        // Endpoint task: push data + FIN then exit.
        async move {
            pusher.push_data(0, b"hello world", true);
        }
        .primary()
        .spawn();

        // App task: read until EOF.
        async move {
            let mut buf = BytesMut::with_capacity(32);
            loop {
                let n = reader.read_into(&mut buf).await.expect("read failed");
                if n == 0 {
                    break;
                }
            }
            assert_eq!(&buf[..], b"hello world");
            assert!(reader.0.status.is_complete());
        }
        .primary()
        .spawn();
    });
}

/// In-order read counts bytes correctly and marks the stream complete.
///
/// Mirrors `poll_read_into_counts_direct_interposer_writes` but uses the
/// proper two-task async harness instead of a noop waker.
#[test]
fn in_order_read_reports_byte_count_and_completes() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let (mut reader, mut pusher) = make_pair();
        let expected = Data::send_one_at(0, 8);

        async move {
            pusher.push_data(0, &expected, true);
        }
        .primary()
        .spawn();

        async move {
            let mut out = Vec::new();
            let n = reader.read_into(&mut out).await.expect("read failed");
            assert_eq!(n, 8);
            assert_eq!(out, Data::send_one_at(0, 8));
            assert!(reader.0.status.is_complete());
        }
        .primary()
        .spawn();
    });
}

/// Out-of-order delivery: endpoint pushes tail then head; app reads complete
/// data after reassembly.  Both tasks are primaries so neither holds the other
/// open artificially.
#[test]
fn out_of_order_reassembly() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let (mut reader, mut pusher) = make_pair();

        // Endpoint task: push tail first so the app must wait for the head.
        async move {
            pusher.push_data(5, b"world", true); // tail: out of order
            bach::task::yield_now().await; // yield so app can process the tail
            pusher.push_data(0, b"hello", false); // head: fills the gap
        }
        .primary()
        .spawn();

        // App task: read until EOF.
        async move {
            let mut buf = BytesMut::with_capacity(32);
            loop {
                let n = reader.read_into(&mut buf).await.expect("read failed");
                if n == 0 {
                    break;
                }
            }
            assert_eq!(&buf[..], b"helloworld");
        }
        .primary()
        .spawn();
    });
}

/// A reset terminates a read with `ConnectionReset`.
#[test]
fn reset_terminates_read() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let (mut reader, mut pusher) = make_pair();

        async move {
            pusher.push_reset(VarInt::from_u8(42));
        }
        .primary()
        .spawn();

        async move {
            let mut buf = BytesMut::with_capacity(32);
            let err = reader
                .read_into(&mut buf)
                .await
                .expect_err("expected reset error");
            assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
            assert!(reader.0.status.is_reset());
            // Reassembler should be cleared on reset to free memory.
            assert!(reader.0.reassembler.is_empty());
        }
        .primary()
        .spawn();
    });
}

/// Data arrives then a reset: the stream must eventually surface the reset.
#[test]
fn reset_after_partial_data() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let (mut reader, mut pusher) = make_pair();

        async move {
            pusher.push_data(0, b"partial", false);
            pusher.push_reset(VarInt::from_u8(1));
        }
        .primary()
        .spawn();

        async move {
            let mut buf = BytesMut::with_capacity(64);
            // Read until we hit the reset error.
            loop {
                match reader.read_into(&mut buf).await {
                    Ok(0) => panic!("unexpected clean EOF, expected reset"),
                    Ok(_) => {}
                    Err(e) => {
                        assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset);
                        break;
                    }
                }
            }
            assert!(reader.0.status.is_reset());
        }
        .primary()
        .spawn();
    });
}

/// The Reader must emit a `MAX_DATA` (FlowControl) frame after the application
/// consumes enough bytes to cross the replenishment threshold (> window / 2).
///
/// The endpoint task waits for the MAX_DATA frame asynchronously — mirroring
/// how a real endpoint would receive and process such frames from the app side.
#[test]
fn max_data_sent_after_consuming() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let (mut reader, mut pusher) = make_pair();
        let window_size = reader.0.window_size;
        // A payload just over window_size ensures we cross the > window/2
        // threshold in a single read.
        let payload = vec![0xabu8; window_size as usize + 1];
        let payload_len = payload.len();

        // Endpoint task: push data, then wait for the MAX_DATA frame.
        async move {
            pusher.push_data(0, &payload, true);
            let frames = pusher.recv_frames().await;
            let has_max_data = frames
                .iter()
                .any(|f| matches!(f.header, Header::FlowControl { .. }));
            assert!(has_max_data, "expected at least one MAX_DATA frame");
        }
        .primary()
        .spawn();

        // App task: read until EOF.
        async move {
            let mut buf = BytesMut::with_capacity(payload_len + 16);
            loop {
                let n = reader.read_into(&mut buf).await.expect("read failed");
                if n == 0 {
                    break;
                }
            }
            assert_eq!(buf.len(), payload_len);
        }
        .primary()
        .spawn();
    });
}

/// A server-side stream starts in `PendingValidation`; calling `read_into`
/// before `validate` returns an `InvalidInput` error.
#[test]
fn server_read_before_validate_fails() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let (mut reader, mut pusher) = make_server_pair();

        async move {
            pusher.push_data(0, b"hello", true);
        }
        .primary()
        .spawn();

        async move {
            let mut buf = BytesMut::with_capacity(16);
            let err = reader
                .read_into(&mut buf)
                .await
                .expect_err("expected error before validation");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        }
        .primary()
        .spawn();
    });
}

/// A server-side stream becomes readable once `FlowValidated` is received.
#[test]
fn server_validates_then_reads() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let (mut reader, mut pusher) = make_server_pair();

        async move {
            pusher.push_flow_validated();
            pusher.push_data(0, b"hello", true);
        }
        .primary()
        .spawn();

        async move {
            reader.validate().await.expect("validate failed");
            let mut buf = BytesMut::with_capacity(16);
            loop {
                let n = reader.read_into(&mut buf).await.expect("read failed");
                if n == 0 {
                    break;
                }
            }
            assert_eq!(&buf[..], b"hello");
        }
        .primary()
        .spawn();
    });
}

/// Dropping the Reader before a FIN is received must send a `STOP_SENDING`
/// (FlowReset) frame so the peer knows to stop.
///
/// The endpoint task waits for the frame asynchronously, mirroring how a
/// real endpoint would process control frames from the application side.
#[test]
fn drop_before_fin_sends_stop_sending() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let (mut reader, mut pusher) = make_pair();

        // Endpoint task: push some data (no FIN), then wait for STOP_SENDING.
        async move {
            pusher.push_data(0, b"some data", false);
            let frames = pusher.recv_frames().await;
            let has_flow_reset = frames
                .iter()
                .any(|f| matches!(f.header, Header::FlowReset { .. }));
            assert!(has_flow_reset, "expected a FlowReset (STOP_SENDING) on drop");
        }
        .primary()
        .spawn();

        // App task: do one read then drop the reader without a FIN.
        async move {
            let mut buf = BytesMut::with_capacity(64);
            let _ = reader.read_into(&mut buf).await;
            drop(reader); // no FIN received → Drop sends STOP_SENDING
        }
        .primary()
        .spawn();
    });
}

/// When the frame channel receiver is dropped (simulating a dead endpoint) the
/// Reader handles the failure gracefully and does not panic.
///
/// Uses `make_pair()` but drops the `frame_rx` immediately so that any frames
/// the Reader tries to send are silently discarded.
#[test]
fn broken_frame_channel_is_handled_gracefully() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let (mut reader, pusher) = make_pair();
        let window_size = reader.0.window_size;

        // Destructure pusher to drop the frame_rx (breaks reader's frame_tx).
        let Pusher {
            dispatcher,
            queue_id,
            request,
            frame_rx: _closed,
        } = pusher;
        let mut pusher = Pusher {
            dispatcher,
            queue_id,
            request,
            // Dummy disconnected receiver — not used for assertions in this test.
            frame_rx: frame::submission_channel(1).1,
        };

        // Endpoint task: push enough data to trigger a MAX_DATA send.
        let payload = vec![0u8; window_size as usize + 1];
        let payload_len = payload.len();
        async move {
            pusher.push_data(0, &payload, true);
        }
        .primary()
        .spawn();

        // App task: read should succeed (frames are silently dropped) or fail
        // with BrokenPipe.  Either way the Reader must not panic.
        async move {
            let mut buf = BytesMut::with_capacity(payload_len + 16);
            let _ = reader.read_into(&mut buf).await;
        }
        .primary()
        .spawn();
    });
}
