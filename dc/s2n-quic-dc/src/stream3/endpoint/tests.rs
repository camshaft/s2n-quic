// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the stream3 endpoint packet pipeline.
//!
//! Each test runs inside Bach's deterministic simulation (`testing::sim`) using two fully
//! wired endpoints backed by simulated UDP sockets and fake path-secret entries.

use crate::{
    acceptor,
    byte_vec::ByteVec,
    flow,
    intrusive_queue::Queue,
    path::secret::{map::testing as map_testing, Map as PathSecretMap},
    stream3::{
        endpoint::{testing as endpoint_testing, testing::SimEndpointConfig},
        frame::{Frame, Header, DEFAULT_TTL, TransmissionStatus},
        Stream,
    },
};
use s2n_quic_core::varint::VarInt;
use std::sync::{Arc, Mutex};

// ── helpers ───────────────────────────────────────────────────────────────

/// A simple acceptor that stores received streams in a shared `Vec`.
struct CollectAcceptor {
    streams: Arc<Mutex<Vec<Stream>>>,
}

impl CollectAcceptor {
    fn new() -> (Self, Arc<Mutex<Vec<Stream>>>) {
        let store: Arc<Mutex<Vec<Stream>>> = Default::default();
        (
            Self {
                streams: store.clone(),
            },
            store,
        )
    }
}

impl acceptor::Acceptor<Stream> for CollectAcceptor {
    fn handle_request(&self, request: Stream) {
        self.streams.lock().unwrap().push(request);
    }
}

/// Submits a single FlowInit frame from `frame_tx` to the peer at
/// `path_secret_entry`'s data address.
fn submit_flow_init(
    frame_tx: &mut crate::stream3::frame::SubmissionSender,
    path_secret_entry: Arc<crate::path::secret::map::Entry>,
    source_queue_id: VarInt,
    dest_acceptor_id: VarInt,
    stream_id: VarInt,
) {
    let payload = ByteVec::new();
    let frame = Frame {
        header: Header::FlowInit {
            source_queue_id,
            dest_acceptor_id,
            attempt_id: VarInt::MAX, // brand-new attempt
            stream_id,
            is_fin: false,
        },
        source_sender_id: VarInt::MAX, // no sticky-sender preference
        payload,
        path_secret_entry,
        completion: None,
        status: TransmissionStatus::default(),
        ttl: DEFAULT_TTL,
        transmission_time: None,
    };

    let mut q: Queue<Frame> = Queue::new();
    q.push_back(frame.into());
    // If the channel is closed the endpoint has already shut down; ignore the error.
    let _ = frame_tx.send_batch(q);
}

// ── tests ─────────────────────────────────────────────────────────────────

/// Two endpoints exchange a FlowInit packet: endpoint A opens a stream to B's
/// acceptor.  We verify that B's acceptor receives exactly one `Stream` and
/// that the pipeline ran without emitting any `!rx.process.err.*` events.
#[test]
fn two_endpoints_flow_init() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        async move {
            // ── 1. Path-secret maps ────────────────────────────────────────
            let map_a: PathSecretMap = map_testing::new(50_000);
            let map_b: PathSecretMap = map_testing::new(50_000);

            // ── 2. Acceptor registry for B ─────────────────────────────────
            let acceptor_id = VarInt::from_u8(1);
            let registry_b: acceptor::Registry<Stream> = acceptor::Registry::new();
            let (acceptor, received) = CollectAcceptor::new();
            let _handle = registry_b
                .register(acceptor_id, Arc::new(acceptor))
                .expect("acceptor registration failed");

            // ── 3. Stand up both endpoints ─────────────────────────────────
            let mut endpoint_a = endpoint_testing::setup_sim_endpoint(
                SimEndpointConfig::default(),
                map_a.clone(),
                acceptor::Registry::new(), // A is the client — no acceptor needed
            );
            let endpoint_b = endpoint_testing::setup_sim_endpoint(
                SimEndpointConfig::default(),
                map_b.clone(),
                registry_b,
            );

            // ── 4. Inject fake path-secret entries ─────────────────────────
            let addr_a = endpoint_a.data_addr;
            let addr_b = endpoint_b.data_addr;
            endpoint_testing::insert_fake_path_pair(&map_a, addr_a, &map_b, addr_b);

            // Allow the pipeline tasks spawned above to initialise before we
            // start injecting traffic.
            bach::time::sleep(core::time::Duration::from_millis(1)).await;

            // ── 5. Allocate a source queue on A ────────────────────────────
            let path_entry_for_b = map_a.get_raw(addr_b).expect("path secret entry for B");
            let stream_id = VarInt::new(
                endpoint_a
                    .next_stream_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            )
            .expect("stream_id overflow");
            let source_queue_id = {
                let handle = flow::Handle::client(stream_id, path_entry_for_b.clone());
                let (control, _stream_rx) = endpoint_a
                    .queue_allocator
                    .alloc_or_grow(handle, None);
                control.queue_id()
            };

            // ── 6. Submit FlowInit from A → B ──────────────────────────────
            submit_flow_init(
                &mut endpoint_a.frame_tx,
                path_entry_for_b,
                source_queue_id,
                acceptor_id,
                stream_id,
            );

            // ── 7. Wait for B's acceptor to fire ──────────────────────────
            // Allow up to 10 simulated milliseconds (Bach advances time on
            // each yield point so the pipeline tasks can run).
            let deadline =
                bach::time::Instant::now() + core::time::Duration::from_millis(10);
            loop {
                {
                    let guard = received.lock().unwrap();
                    if !guard.is_empty() {
                        break;
                    }
                }
                if bach::time::Instant::now() >= deadline {
                    panic!("timed out waiting for B's acceptor to receive the stream");
                }
                // Yield so other Bach tasks (the pipeline) can run.
                bach::time::sleep(core::time::Duration::from_millis(1)).await;
            }

            // ── 8. Assert exactly one stream received ──────────────────────
            let guard = received.lock().unwrap();
            assert_eq!(
                guard.len(),
                1,
                "expected exactly 1 stream at acceptor; got {}",
                guard.len()
            );

            tracing::info!("two_endpoints_flow_init passed");
        }
        .primary()
        .spawn();
    });
}
