// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract tests for the `send_socket_assembler` pipeline function.
//!
//! The socket assembler pipeline wires together the per-socket `Assembler` combinator
//! (which seals frames into UDP datagrams), the `WheelRouter` (which re-schedules
//! the context into tx/PTO/idle wheels after assembly), pacing, and the socket sender.
//! These tests verify that a `Context` with queued frames results in an encrypted
//! datagram reaching the peer, and that the pipeline terminates cleanly when its
//! input closes.

use super::helpers::{test_batch, test_entry_at, TestReceiverExt as _};
use crate::{
    endpoint::{combinator::AssemblerCounters, frame, msg, send, tasks},
    socket::{
        channel::{intrusive::unsync, ReceiverExt as _, UnboundedSender as _},
        pool::Pool,
        rate::Rate,
    },
    testing::{ext::*, sim},
    time::{bach::Clock, precision::Clock as _},
};
use bach::net::UdpSocket;
use s2n_quic_core::varint::VarInt;
use s2n_quic_platform::features::Gso;
use std::{cell::RefCell, rc::Rc};

/// A fully-assembled `Context` with one queued FlowData frame is fed through the
/// pipeline.
///
/// After assembly we assert on every output channel:
/// - an encrypted datagram arrives at the peer (positive)
/// - the context is re-armed on the PTO wheel (inflight data present) and idle wheel
/// - it is NOT re-armed on the TX wheel (no more pending data)
/// - no frames are cancelled
/// - no ACK completions are generated (this is a plain FlowData, not an ACK frame)
///
/// The peer's address is resolved via Bach DNS from the "server" group registered by
/// `.group("server")` on the recv task, so no IP literals appear in the test.
#[test]
fn sends_encrypted_packet_to_peer() {
    sim(|| {
        let registry = crate::counter::Registry::default();
        let clock = Clock::default();
        let asm_clock = clock.clone();

        let (mut ctx_tx, ctx_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (tx_wheel_tx, mut tx_wheel_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (pto_wheel_tx, mut pto_wheel_rx) = unsync::new_with_adapter::<send::PtoWheelAdapter>();
        let (idle_wheel_tx, mut idle_wheel_rx) =
            unsync::new_with_adapter::<send::IdleWheelAdapter>();
        let (cancelled_tx, mut cancelled_rx) = unsync::new::<frame::Frame>();
        let (ack_completions_tx, mut ack_completions_rx) = unsync::new::<msg::Sender>();
        let asm_counters = AssemblerCounters::new(&registry);

        // Bind the recv socket in the "server" group so its IP can be resolved by name.
        async {
            let recv_socket = UdpSocket::bind("0.0.0.0:4433").await.unwrap();
            let mut buf = vec![0u8; 1500];
            let (n, _peer) = recv_socket.recv_from(&mut buf).await.unwrap();
            tracing::debug!(n, "received encrypted datagram at server");
            assert!(n > 0, "assembler should have sent an encrypted packet");
        }
        .group("server")
        .primary()
        .spawn();

        // Bind the send socket, run the assembler pipeline, then assert on all outputs.
        async move {
            let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
            let rx = tasks::send_socket_assembler(
                ctx_rx,
                asm_clock,
                VarInt::from_u8(0),
                0, // source_control_port
                Gso::default(),
                // Pool must hold at least one max-size datagram (1472 bytes);
                // u16::MAX is safe and matches the assemble tests.
                Pool::new(u16::MAX),
                cancelled_tx.into_list_sender(),
                ack_completions_tx.into_list_sender(),
                asm_counters,
                Rate::new(100.0),
                socket,
                tx_wheel_tx,
                pto_wheel_tx,
                idle_wheel_tx,
            );
            rx.drain_budgeted(Some(32)).await;
            // drain_budgeted consumes `rx`, dropping all internal senders (wheel,
            // cancelled, ack_completions) so empty receivers now return None.

            // After sending one FlowData frame the context has inflight data, so the
            // assembler re-arms both the PTO wheel and the idle wheel.  There is no
            // more pending data, so the TX wheel should NOT be re-armed.
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

            // No frames were cancelled and no ACK completions were generated.
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
            let mut ctx = send::Context::new(
                &entry,
                registry.register_queue_gauge("test.inflight"),
                registry.register_queue_gauge("test.ack"),
                registry.register_queue_gauge("test.pending"),
                0,
                &clock,
            )
            .expect("test context should be constructible");
            // Queue one frame and mark the tx wheel as immediately ready.
            let _ = ctx.push_batch(test_batch(&entry).into_inner(), &clock);
            ctx.tx_wheel.target_time = Some(clock.now());
            let ctx = Rc::new(RefCell::new(ctx));
            let _ = ctx_tx.send(ctx);
            drop(ctx_tx);
        }
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

        let (ctx_tx, ctx_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (tx_wheel_tx, _tx_wheel_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (pto_wheel_tx, _pto_wheel_rx) = unsync::new_with_adapter::<send::PtoWheelAdapter>();
        let (idle_wheel_tx, _idle_wheel_rx) =
            unsync::new_with_adapter::<send::IdleWheelAdapter>();
        let (cancelled_tx, _cancelled_rx) = unsync::new::<frame::Frame>();
        let (ack_completions_tx, _ack_completions_rx) = unsync::new::<msg::Sender>();
        let asm_counters = AssemblerCounters::new(&registry);

        // Close the input before anything is sent.
        drop(ctx_tx);

        async move {
            let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
            let rx = tasks::send_socket_assembler(
                ctx_rx,
                clock,
                VarInt::from_u8(0),
                0,
                Gso::default(),
                Pool::new(u16::MAX),
                cancelled_tx.into_list_sender(),
                ack_completions_tx.into_list_sender(),
                asm_counters,
                Rate::new(100.0),
                socket,
                tx_wheel_tx,
                pto_wheel_tx,
                idle_wheel_tx,
            );
            // Pipeline should drain immediately and return — no hang.
            rx.drain_budgeted(Some(32)).await;
        }
        .primary()
        .spawn();
    });
}
