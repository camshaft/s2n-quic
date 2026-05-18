// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract tests for the `send_socket_assembler` pipeline function.
//!
//! The socket assembler pipeline wires together the per-socket `Assembler` combinator
//! (which seals frames into UDP datagrams), the `WheelRouter` (which re-schedules
//! the context into tx/PTO/idle wheels after assembly), pacing, and the socket sender.
//! These tests verify the routing behavior of every output channel under different
//! pending-data and frame-cancellation scenarios.

use super::helpers::{
    assembler_pipeline, build_send_context, test_batch, test_batch_with_payload, test_entry_at,
    TestReceiverExt as _,
};
use crate::{
    endpoint::{combinator::AssemblerCounters, frame, msg, send},
    socket::channel::{intrusive::unsync, UnboundedSender as _},
    testing::{ext::*, sim},
    time::{bach::Clock, precision::Clock as _},
};
use s2n_quic_core::varint::VarInt;

// ── helper ───────────────────────────────────────────────────────────────────

/// Create all the channels required to wire up one `send_socket_assembler` pipeline.
///
/// Returns a tuple of `(input, outputs, pipeline_args)` where:
/// - `ctx_tx` is the sender used by the feeder task
/// - `{tx,pto,idle}_wheel_rx`, `cancelled_rx`, `ack_completions_rx` are for assertions
/// - the last 6 values are consumed by [`assembler_pipeline`]
type AssemblerChannels = (
    // feeder side
    unsync::Sender<send::TxWheelAdapter>,
    // assertion side
    unsync::Receiver<send::TxWheelAdapter>,
    unsync::Receiver<send::PtoWheelAdapter>,
    unsync::Receiver<send::IdleWheelAdapter>,
    unsync::Receiver<crate::intrusive::EntryAdapter<frame::Frame>>,
    unsync::Receiver<crate::intrusive::EntryAdapter<msg::Sender>>,
    // pipeline side
    unsync::Receiver<send::TxWheelAdapter>,
    unsync::ListSender<crate::intrusive::EntryAdapter<frame::Frame>>,
    unsync::ListSender<crate::intrusive::EntryAdapter<msg::Sender>>,
    AssemblerCounters,
    unsync::Sender<send::TxWheelAdapter>,
    unsync::Sender<send::PtoWheelAdapter>,
    unsync::Sender<send::IdleWheelAdapter>,
);

fn assembler_channels(registry: &crate::counter::Registry) -> AssemblerChannels {
    let (ctx_tx, ctx_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
    let (tx_wheel_tx, tx_wheel_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
    let (pto_wheel_tx, pto_wheel_rx) = unsync::new_with_adapter::<send::PtoWheelAdapter>();
    let (idle_wheel_tx, idle_wheel_rx) = unsync::new_with_adapter::<send::IdleWheelAdapter>();
    let (cancelled_tx, cancelled_rx) = unsync::new::<frame::Frame>();
    let (ack_completions_tx, ack_completions_rx) = unsync::new::<msg::Sender>();
    let asm_counters = AssemblerCounters::new(registry);
    (
        ctx_tx,
        tx_wheel_rx,
        pto_wheel_rx,
        idle_wheel_rx,
        cancelled_rx,
        ack_completions_rx,
        ctx_rx,
        cancelled_tx.into_list_sender(),
        ack_completions_tx.into_list_sender(),
        asm_counters,
        tx_wheel_tx,
        pto_wheel_tx,
        idle_wheel_tx,
    )
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// A fully-assembled `Context` with one queued FlowData frame is fed through the
/// pipeline.
///
/// Output-channel assertions after the drain:
/// - an encrypted datagram arrives at the peer (positive — frame was sent)
/// - context re-armed on PTO wheel (inflight data present after the send)
/// - context re-armed on idle wheel (context is active)
/// - TX wheel NOT re-armed (no more pending data)
/// - `cancelled` empty (no frames were dropped)
/// - `ack_completions` empty (no ACK frame in this batch)
#[test]
fn sends_encrypted_packet_to_peer() {
    sim(|| {
        let registry = crate::counter::Registry::default();
        let clock = Clock::default();

        let (
            mut ctx_tx,
            mut tx_wheel_rx,
            mut pto_wheel_rx,
            mut idle_wheel_rx,
            mut cancelled_rx,
            mut ack_completions_rx,
            ctx_rx,
            cancelled_tx,
            ack_completions_tx,
            asm_counters,
            tx_wheel_tx,
            pto_wheel_tx,
            idle_wheel_tx,
        ) = assembler_channels(&registry);

        // Bind the recv socket in the "server" group so its IP can be resolved by name.
        async {
            let recv_socket = bach::net::UdpSocket::bind("0.0.0.0:4433").await.unwrap();
            let mut buf = vec![0u8; 1500];
            let (n, _peer) = recv_socket.recv_from(&mut buf).await.unwrap();
            tracing::debug!(n, "received encrypted datagram at server");
            assert!(n > 0, "assembler should have sent an encrypted packet");
        }
        .group("server")
        .primary()
        .spawn();

        // Run the assembler pipeline.
        let asm_clock = clock.clone();
        async move {
            assembler_pipeline(
                ctx_rx,
                asm_clock,
                cancelled_tx,
                ack_completions_tx,
                asm_counters,
                tx_wheel_tx,
                pto_wheel_tx,
                idle_wheel_tx,
            )
            .await;

            // drain_budgeted consumes the pipeline, dropping all internal senders,
            // so empty receivers now return None immediately.
            tracing::debug!("asserting wheel routing after assembly");
            assert!(
                pto_wheel_rx.recv().await.is_some(),
                "context with inflight data should be routed to PTO wheel"
            );
            assert!(
                idle_wheel_rx.recv().await.is_some(),
                "active context should always be routed to idle wheel"
            );
            assert!(
                tx_wheel_rx.recv().await.is_none(),
                "no pending data after send — TX wheel should not be re-armed"
            );
            assert!(
                cancelled_rx.recv().await.is_none(),
                "FlowData frame should not be cancelled"
            );
            assert!(
                ack_completions_rx.recv().await.is_none(),
                "no ACK completions for a plain FlowData frame"
            );
            tracing::debug!("all output assertions passed");
        }
        .spawn();

        // Resolve the server address via Bach DNS, build the context, and feed it.
        async move {
            let entry = test_entry_at("server:4433").await;
            let ctx = build_send_context(&entry, 0, &registry, &clock);
            let _ = ctx.borrow_mut()
                .push_batch(test_batch(&entry).into_inner(), &clock);
            ctx.borrow_mut().tx_wheel.target_time = Some(clock.now());
            let _ = ctx_tx.send(ctx);
            drop(ctx_tx);
        }
        .spawn();
    });
}

/// Two large frames (~1300 bytes of payload each) are pushed into a single context.
/// The first frame fills one MTU-sized segment; the second is pushed back by the
/// assembler and remains pending.  After the drain:
///
/// - TX wheel IS re-armed (pending second frame)
/// - PTO wheel IS re-armed (first frame is inflight)
/// - idle wheel IS re-armed (context is active)
/// - exactly one datagram arrives at the peer (only the first frame was sent)
/// - `cancelled` and `ack_completions` remain empty
#[test]
fn reassembles_context_to_tx_wheel_when_data_remains() {
    sim(|| {
        let registry = crate::counter::Registry::default();
        let clock = Clock::default();

        let (
            mut ctx_tx,
            mut tx_wheel_rx,
            mut pto_wheel_rx,
            mut idle_wheel_rx,
            mut cancelled_rx,
            mut ack_completions_rx,
            ctx_rx,
            cancelled_tx,
            ack_completions_tx,
            asm_counters,
            tx_wheel_tx,
            pto_wheel_tx,
            idle_wheel_tx,
        ) = assembler_channels(&registry);

        // Recv side: exactly one datagram should arrive (only the first frame fits).
        async {
            let recv_socket = bach::net::UdpSocket::bind("0.0.0.0:4433").await.unwrap();
            let mut buf = vec![0u8; 2000];
            let (n, _peer) = recv_socket.recv_from(&mut buf).await.unwrap();
            tracing::debug!(n, "received encrypted datagram at server");
            assert!(n > 0, "assembler should have sent the first large frame");
        }
        .group("server")
        .primary()
        .spawn();

        let asm_clock = clock.clone();
        async move {
            assembler_pipeline(
                ctx_rx,
                asm_clock,
                cancelled_tx,
                ack_completions_tx,
                asm_counters,
                tx_wheel_tx,
                pto_wheel_tx,
                idle_wheel_tx,
            )
            .await;

            // The second frame remains pending → TX wheel must be re-armed.
            assert!(
                tx_wheel_rx.recv().await.is_some(),
                "remaining pending data should re-arm the TX wheel"
            );
            // First frame is in-flight → PTO must be scheduled.
            assert!(
                pto_wheel_rx.recv().await.is_some(),
                "inflight data should arm the PTO wheel"
            );
            // Context is still active → idle timeout must be scheduled.
            assert!(
                idle_wheel_rx.recv().await.is_some(),
                "active context should always be routed to idle wheel"
            );
            // Neither frame was cancelled and no ACK completions are expected.
            assert!(
                cancelled_rx.recv().await.is_none(),
                "no frames should be cancelled"
            );
            assert!(
                ack_completions_rx.recv().await.is_none(),
                "no ACK completions for FlowData frames"
            );
        }
        .spawn();

        async move {
            let entry = test_entry_at("server:4433").await;
            let ctx = build_send_context(&entry, 0, &registry, &clock);
            {
                let mut c = ctx.borrow_mut();
                // Push two large frames.  Each has ~1300 bytes of payload;
                // combined they exceed one MTU (1472 bytes), so the assembler
                // can only pack the first into the single allowed segment.
                let _ = c.push_batch(test_batch_with_payload(&entry, 1300).into_inner(), &clock);
                let _ = c.push_batch(test_batch_with_payload(&entry, 1300).into_inner(), &clock);
                c.tx_wheel.target_time = Some(clock.now());
            }
            let _ = ctx_tx.send(ctx);
            drop(ctx_tx);
        }
        .spawn();
    });
}

/// A frame whose `CompletionReceiver` is explicitly cancelled (via `rx.cancel()`)
/// before being pushed into the context is routed to the `cancelled` output channel
/// because the assembler sees `!frame.should_transmit()`.
///
/// Output-channel assertions:
/// - `cancelled` receives exactly one frame (the cancelled one)
/// - no datagram reaches the peer (nothing was encoded)
/// - TX wheel NOT re-armed (no pending data after the cancelled frame is discarded)
/// - PTO wheel NOT re-armed (no inflight data)
/// - idle wheel IS re-armed (context is still active)
/// - `ack_completions` empty
#[test]
fn cancelled_frame_emitted_when_completion_is_cancelled() {
    sim(|| {
        let registry = crate::counter::Registry::default();
        let clock = Clock::default();

        let (
            mut ctx_tx,
            mut tx_wheel_rx,
            mut pto_wheel_rx,
            mut idle_wheel_rx,
            mut cancelled_rx,
            mut ack_completions_rx,
            ctx_rx,
            cancelled_tx,
            ack_completions_tx,
            asm_counters,
            tx_wheel_tx,
            pto_wheel_tx,
            idle_wheel_tx,
        ) = assembler_channels(&registry);

        let asm_clock = clock.clone();
        async move {
            assembler_pipeline(
                ctx_rx,
                asm_clock,
                cancelled_tx,
                ack_completions_tx,
                asm_counters,
                tx_wheel_tx,
                pto_wheel_tx,
                idle_wheel_tx,
            )
            .await;

            // The cancelled frame must appear on the cancelled output.
            assert!(
                cancelled_rx.recv().await.is_some(),
                "assembler should route a cancelled frame to the cancelled channel"
            );
            // No data was encoded, so TX and PTO wheels must not be re-armed.
            assert!(
                tx_wheel_rx.recv().await.is_none(),
                "no pending data after cancellation — TX wheel must not be re-armed"
            );
            assert!(
                pto_wheel_rx.recv().await.is_none(),
                "no inflight data — PTO wheel must not be re-armed"
            );
            // The context is still live, so idle timeout is re-armed.
            assert!(
                idle_wheel_rx.recv().await.is_some(),
                "active context should be routed to idle wheel even when no data was sent"
            );
            assert!(
                ack_completions_rx.recv().await.is_none(),
                "no ACK completions when only a cancelled data frame was processed"
            );
            tracing::debug!("all output assertions passed");
        }
        .primary()
        .spawn();

        async move {
            let entry = test_entry_at("server:4433").await;
            // Create a completion receiver and cancel it immediately.
            // frame.should_transmit() will return false, triggering the cancelled path.
            let completion_rx = frame::completion_channel();
            let completion_sender = completion_rx.sender();
            completion_rx.cancel();

            let ctx = build_send_context(&entry, 0, &registry, &clock);
            {
                let mut c = ctx.borrow_mut();
                let mut batch = crate::endpoint::combinator::FrameBatch::single(
                    crate::intrusive::Entry::new(frame::Frame {
                        header: frame::Header::FlowData {
                            queue_pair: crate::packet::datagram::QueuePair {
                                source_queue_id: VarInt::from_u8(1),
                                dest_queue_id: VarInt::from_u8(2),
                            },
                            stream_id: VarInt::from_u8(1),
                            offset: VarInt::ZERO,
                            is_fin: false,
                        },
                        source_sender_id: VarInt::MAX,
                        payload: Default::default(),
                        path_secret_entry: entry.clone(),
                        completion: Some(completion_sender),
                        status: frame::TransmissionStatus::Pending,
                        ttl: 3,
                        transmission_time: None,
                    }),
                );
                batch.set_sender_id(0);
                let _ = c.push_batch(batch, &clock);
                c.tx_wheel.target_time = Some(clock.now());
            }
            let _ = ctx_tx.send(ctx);
            drop(ctx_tx);
        }
        .group("server")
        .spawn();
    });
}

/// When the context input channel closes with no items, the pipeline terminates
/// without hanging or panicking, and no output channels receive any items.
#[test]
fn shuts_down_on_closed_input() {
    sim(|| {
        let registry = crate::counter::Registry::default();
        let clock = Clock::default();

        let (
            ctx_tx,
            _tx_wheel_rx,
            _pto_wheel_rx,
            _idle_wheel_rx,
            _cancelled_rx,
            _ack_completions_rx,
            ctx_rx,
            cancelled_tx,
            ack_completions_tx,
            asm_counters,
            tx_wheel_tx,
            pto_wheel_tx,
            idle_wheel_tx,
        ) = assembler_channels(&registry);

        // Close the input before anything is sent.
        drop(ctx_tx);

        async move {
            assembler_pipeline(
                ctx_rx,
                clock,
                cancelled_tx,
                ack_completions_tx,
                asm_counters,
                tx_wheel_tx,
                pto_wheel_tx,
                idle_wheel_tx,
            )
            .await;
        }
        .primary()
        .spawn();
    });
}
