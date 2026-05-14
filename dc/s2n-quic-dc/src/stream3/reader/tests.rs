// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests for the stream3 Reader.
//!
//! ## Organisation
//!
//! * **Synchronous unit tests** – exercise `write_data_reader` and
//!   `poll_read_into` directly with noop wakers; no async runtime needed.
//!
//! * **Bach async tests** – each test runs inside `crate::testing::sim` which
//!   provides a deterministic single-threaded cooperative scheduler.
//!   Tests are composed of two cooperatively-scheduled tasks:
//!
//!   * **Application task** – owns the [`Reader`] and calls `read_into`.
//!   * **Endpoint task** – owns a [`Pusher`] that injects [`msg::Stream`]
//!     messages into the flow queue, and drains [`Frame`]s that the Reader
//!     sends back (e.g. `MAX_DATA`, `STOP_SENDING`).
//!
//!   The two sides communicate through the real flow-queue / frame-submission
//!   channels, but without any actual UDP sockets or cryptography.
//!
//! ## Keeping the flow queue open
//!
//! The stream queue stays "open" (i.e. the `IS_OPEN` flag stays set) as long
//! as there is at least one `Sender` handle alive.  The `Pusher` holds the
//! `Dispatch` object which owns those sender handles.  Tests must therefore
//! ensure that `Pusher` is not dropped before the `Reader` finishes consuming
//! all its data.  Single-task tests accomplish this naturally; two-task tests
//! return the `Pusher` from the endpoint task via a `JoinHandle` so that the
//! primary task can hold it alive through the final reads.

use super::{msg, write_data_reader, Reader};
use crate::{
    flow,
    intrusive_queue,
    path::secret::map::Entry as PathSecretEntry,
    stream3::{
        endpoint::reset_error,
        frame::{self, Frame, Header, PriorityStorage, SubmissionReceiver},
    },
};
use bytes::BytesMut;
use s2n_quic_core::{buffer::Reassembler, endpoint, stream::testing::Data, varint::VarInt};
use std::net::SocketAddr;

// ─── Test helpers ─────────────────────────────────────────────────────────────

/// Creates a connected `(Reader, Pusher)` pair for use in tests.
///
/// * `Reader` – the component under test; owns the stream-side receive handle.
/// * `Pusher` – the mock endpoint side; can inject stream messages and drain
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
/// `push_*` injects [`msg::Stream`] messages through the flow-queue dispatcher,
/// automatically waking any waiting Reader task.  `drain_frames` collects
/// [`Frame`]s that the Reader submitted (e.g. `MAX_DATA`, `STOP_SENDING`).
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

    /// Synchronously drain all frames submitted by the Reader since the last
    /// call.  Uses a noop waker; the caller must ensure the Reader has had a
    /// chance to run before calling this.
    fn drain_frames(&mut self) -> Vec<Frame> {
        let mut storage = PriorityStorage::default();
        let waker = s2n_quic_core::task::waker::noop();
        let mut cx = core::task::Context::from_waker(&waker);
        let _ = self.frame_rx.poll_swap(&mut cx, &mut storage);
        let mut frames = Vec::new();
        for (_priority, queue) in storage.drain() {
            for entry in queue {
                frames.push(entry.into_inner());
            }
        }
        frames
    }
}

// ─── write_data_reader unit tests (no I/O, no wakers) ─────────────────────────

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

    assert!(app_buf.is_empty());
    assert_eq!(reassembler.consumed_len(), 0);
    assert_eq!(reassembler.total_received_len(), 0);
    assert!(reassembler.is_empty());
    assert!(!reassembler.is_reading_complete());

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

    assert!(app_buf.is_empty());
    assert_eq!(reassembler.len(), 8);
    assert_eq!(reassembler.total_received_len(), 8);
    assert!(!reassembler.is_empty());
}

#[test]
fn poll_read_into_counts_direct_interposer_writes() -> std::io::Result<()> {
    let expected = Data::send_one_at(0, 8);
    let (mut reader, mut pusher) = make_pair();
    pusher.push_data(0, &expected, true);

    let waker = s2n_quic_core::task::waker::noop();
    let mut cx = core::task::Context::from_waker(&waker);
    let mut out = Vec::new();

    match reader.poll_read_into(&mut cx, &mut out) {
        core::task::Poll::Ready(Ok(len)) => assert_eq!(len, 8),
        other => panic!("unexpected first poll result: {other:?}"),
    }
    assert_eq!(out, expected);
    assert!(reader.0.status.is_complete());

    Ok(())
}

// ─── Bach async tests ──────────────────────────────────────────────────────────
//
// Convention for tests that need the pusher to stay alive through reading:
//   * Single-task tests: pusher lives in the same closure as reader.
//   * Two-task tests: the endpoint task returns the Pusher through its
//     JoinHandle, and the primary task holds it until the app finishes.

/// Basic in-order read: endpoint sends data + FIN, application reads until EOF.
#[test]
fn basic_read() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        async move {
            let (mut reader, mut pusher) = make_pair();
            pusher.push_data(0, b"hello world", true);

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

/// Out-of-order delivery with proper two-task interleaving.
///
/// The app task starts polling before any data arrives.  The endpoint then:
/// 1. Pushes the tail (offset 5) → reader wakes, buffers in reassembler, blocks again.
/// 2. Yields to let the app process the tail.
/// 3. Pushes the head (offset 0) → reader wakes, reassembler now complete.
///
/// The Pusher is returned from the endpoint task via its `JoinHandle` so that
/// the primary task can keep the flow-queue channel open while the app reads.
#[test]
fn out_of_order_reassembly() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let (mut reader, mut pusher) = make_pair();

        // The app task must be spawned first so it starts polling (and
        // registers its waker) before the endpoint pushes any data.
        let app_handle = async move {
            let mut buf = BytesMut::with_capacity(32);
            loop {
                let n = reader.read_into(&mut buf).await.expect("read failed");
                if n == 0 {
                    break;
                }
            }
            buf
        }
        .spawn();

        // Endpoint task: push tail, yield so the app processes it, then head.
        let endpoint_handle = async move {
            pusher.push_data(5, b"world", true); // tail: out of order
            bach::task::yield_now().await; // give app a chance to see the tail
            pusher.push_data(0, b"hello", false); // head: fills the gap
            pusher // return so the primary can keep the channel open
        }
        .spawn();

        // Primary: collect results; hold the Pusher alive until app is done.
        async move {
            let pusher = endpoint_handle.await.expect("endpoint task panicked");
            let buf = app_handle.await.expect("app task panicked");
            assert_eq!(&buf[..], b"helloworld");
            drop(pusher);
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

        async move {
            let (mut reader, mut pusher) = make_pair();
            pusher.push_reset(VarInt::from_u8(42));

            let mut buf = BytesMut::with_capacity(32);
            let err = reader
                .read_into(&mut buf)
                .await
                .expect_err("expected reset error");
            assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
            assert!(reader.0.status.is_reset());
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

        async move {
            let (mut reader, mut pusher) = make_pair();
            pusher.push_data(0, b"partial", false);
            pusher.push_reset(VarInt::from_u8(1));

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
#[test]
fn max_data_sent_after_consuming() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        async move {
            let (mut reader, mut pusher) = make_pair();

            // A payload just over window_size ensures we cross the > window/2
            // threshold in a single read.
            let window_size = reader.0.window_size;
            let payload = vec![0xabu8; window_size as usize + 1];
            pusher.push_data(0, &payload, true);

            let mut buf = BytesMut::with_capacity(payload.len() + 16);
            loop {
                let n = reader.read_into(&mut buf).await.expect("read failed");
                if n == 0 {
                    break;
                }
            }
            assert_eq!(buf.len(), payload.len());

            // At least one MAX_DATA frame must have been sent.
            let frames = pusher.drain_frames();
            let has_max_data = frames
                .iter()
                .any(|f| matches!(f.header, Header::FlowControl { .. }));
            assert!(has_max_data, "expected at least one MAX_DATA frame");
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

        async move {
            let (mut reader, mut pusher) = make_server_pair();
            pusher.push_data(0, b"hello", true);

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

        async move {
            let (mut reader, mut pusher) = make_server_pair();
            pusher.push_flow_validated();
            pusher.push_data(0, b"hello", true);

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
#[test]
fn drop_before_fin_sends_stop_sending() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        async move {
            let (reader, mut pusher) = make_pair();
            // Push some data (no FIN) to activate the stream.
            pusher.push_data(0, b"some data", false);
            // Drop the reader without reading / without FIN.
            drop(reader);

            // Drop impl should have sent a STOP_SENDING FlowReset.
            let frames = pusher.drain_frames();
            let has_flow_reset = frames
                .iter()
                .any(|f| matches!(f.header, Header::FlowReset { .. }));
            assert!(has_flow_reset, "expected a FlowReset (STOP_SENDING) on drop");
        }
        .primary()
        .spawn();
    });
}

/// When the frame transmission channel is closed (simulating a dead endpoint)
/// the Reader handles the failure gracefully.
#[test]
fn broken_frame_channel_is_handled_gracefully() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        // Build a reader whose frame channel's receiver side is dropped
        // immediately, so any attempt to send MAX_DATA will fail.
        let stream_id = VarInt::from_u8(1);
        let peer: SocketAddr = "127.0.0.1:4433".parse().unwrap();
        let path_secret_entry =
            PathSecretEntry::fake_deterministic(peer, endpoint::Type::Client);
        let allocator = msg::queue::Allocator::new();
        let dispatcher = allocator.dispatcher();
        let handle = flow::Handle::client(stream_id, path_secret_entry.clone());
        let (_control, stream_rx) = dispatcher
            .alloc(handle, Some(VarInt::from_u8(2)))
            .expect("alloc should succeed");
        let queue_id = stream_rx.queue_id();
        let request = flow::Request {
            credential_id: *path_secret_entry.id(),
            stream_id,
        };
        // Drop the receiver immediately — frame channel is "broken".
        let (frame_tx, _frame_rx_closed) = frame::submission_channel(1);
        let mut reader =
            Reader::new_client(frame_tx, path_secret_entry, stream_id, stream_rx);
        let mut pusher = Pusher {
            dispatcher,
            queue_id,
            request,
            frame_rx: frame::submission_channel(1).1,
        };

        async move {
            // A payload large enough to trigger a MAX_DATA send.
            let window_size = reader.0.window_size;
            let payload = vec![0u8; window_size as usize + 1];
            pusher.push_data(0, &payload, true);

            let mut buf = BytesMut::with_capacity(window_size as usize + 16);
            // The Reader should either succeed (frames silently dropped through
            // the broken channel) or fail with BrokenPipe.  Either outcome is
            // acceptable; the test verifies the Reader does not panic.
            let _ = reader.read_into(&mut buf).await;
        }
        .primary()
        .spawn();
    });
}
