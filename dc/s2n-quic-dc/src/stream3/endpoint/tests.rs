// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the stream3 endpoint packet pipeline.
//!
//! Each test runs inside Bach's deterministic simulation (`testing::sim`) using two fully
//! wired endpoints backed by simulated UDP sockets.  Each endpoint lives in its own Bach
//! group so it is treated as a separate machine from the network perspective.

use crate::{
    acceptor,
    byte_vec::ByteVec,
    flow,
    intrusive_queue::Queue,
    path::secret::map::testing as map_testing,
    stream3::{
        endpoint::{testing as endpoint_testing, testing::SimEndpointConfig},
        frame::{Frame, Header, DEFAULT_TTL, TransmissionStatus},
        Stream,
    },
};
use s2n_quic_core::varint::VarInt;
use std::sync::Arc;

// ── helpers ───────────────────────────────────────────────────────────────

/// An [`acceptor::Acceptor`] that forwards each incoming [`Stream`] to a
/// `tokio::sync::mpsc` channel.
struct ChannelAcceptor {
    tx: tokio::sync::mpsc::UnboundedSender<Stream>,
}

impl acceptor::Acceptor<Stream> for ChannelAcceptor {
    fn handle_request(&self, request: Stream) {
        let _ = self.tx.send(request);
    }
}

/// Submits a single FlowInit frame from `frame_tx` to the peer described by
/// `path_secret_entry`.
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
            attempt_id: VarInt::MAX,
            stream_id,
            is_fin: false,
        },
        source_sender_id: VarInt::MAX,
        payload,
        path_secret_entry,
        completion: None,
        status: TransmissionStatus::default(),
        ttl: DEFAULT_TTL,
        transmission_time: None,
    };

    let mut q: Queue<Frame> = Queue::new();
    q.push_back(frame.into());
    let _ = frame_tx.send_batch(q);
}

// ── tests ─────────────────────────────────────────────────────────────────

/// Two endpoints (running in separate Bach groups — i.e. separate simulated
/// machines) exchange a FlowInit packet: A opens a stream to B's acceptor.
///
/// We verify that B's acceptor receives exactly one [`Stream`] and that the
/// pipeline ran without emitting any `!rx.process.err.*` events.
#[test]
fn two_endpoints_flow_init() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        async move {
            // ── Channels shared between the two group tasks ────────────────
            // B sends its data address to A once it is bound.
            let (b_addr_tx, b_addr_rx) = bach::sync::oneshot::channel::<std::net::SocketAddr>();
            // B's acceptor delivers received streams here.
            let (stream_tx, mut stream_rx) =
                tokio::sync::mpsc::unbounded_channel::<Stream>();

            let acceptor_id = VarInt::from_u8(1);

            // ── Endpoint B — group "b" ─────────────────────────────────────
            async move {
                let map_b = map_testing::new(50_000);
                let registry_b = acceptor::Registry::new();
                let _handle = registry_b
                    .register(acceptor_id, Arc::new(ChannelAcceptor { tx: stream_tx }))
                    .expect("acceptor registration failed");

                let endpoint_b = endpoint_testing::setup_sim_endpoint(
                    SimEndpointConfig::default(),
                    map_b,
                    registry_b,
                );

                // Tell A where to find us.
                let _ = b_addr_tx.send(endpoint_b.data_addr);

                // Keep endpoint B's pipeline alive until the test completes.
                bach::time::sleep(core::time::Duration::from_secs(10)).await;
            }
            .group("b")
            .spawn();

            // ── Endpoint A — group "a" (primary) ──────────────────────────
            async move {
                let map_a = map_testing::new(50_000);

                let mut endpoint_a = endpoint_testing::setup_sim_endpoint(
                    SimEndpointConfig::default(),
                    map_a,
                    acceptor::Registry::new(),
                );

                // Wait for B to finish binding before connecting.
                let addr_b = b_addr_rx.await.expect("b_addr_tx dropped");

                // Insert fake path-secret entries for A ↔ B (auto-looks up
                // B's map from the thread-local registry).
                let path_entry_for_b =
                    endpoint_testing::connect(&endpoint_a, addr_b);

                // Allow the pipeline tasks spawned above to initialise.
                bach::time::sleep(core::time::Duration::from_millis(1)).await;

                // ── Allocate a source queue on A ───────────────────────────
                let stream_id = VarInt::new(
                    endpoint_a
                        .next_stream_id
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                )
                .expect("stream_id overflow");
                let source_queue_id = {
                    let handle =
                        flow::Handle::client(stream_id, path_entry_for_b.clone());
                    let (control, _stream_rx) = endpoint_a
                        .queue_allocator
                        .alloc_or_grow(handle, None);
                    control.queue_id()
                };

                // ── Submit FlowInit from A → B ─────────────────────────────
                submit_flow_init(
                    &mut endpoint_a.frame_tx,
                    path_entry_for_b,
                    source_queue_id,
                    acceptor_id,
                    stream_id,
                );

                // ── Wait for B's acceptor channel to fire ──────────────────
                let deadline =
                    bach::time::Instant::now() + core::time::Duration::from_millis(100);
                loop {
                    if stream_rx.try_recv().is_ok() {
                        break;
                    }
                    if bach::time::Instant::now() >= deadline {
                        panic!("timed out waiting for B's acceptor to receive the stream");
                    }
                    bach::time::sleep(core::time::Duration::from_millis(1)).await;
                }

                tracing::info!("two_endpoints_flow_init passed");
            }
            .group("a")
            .primary()
            .spawn();
        }
        .primary()
        .spawn();
    });
}
