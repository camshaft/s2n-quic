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

use super::helpers::{test_batch, test_entry_at, Discard};
use crate::{
    endpoint::{combinator::AssemblerCounters, frame, send, tasks},
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
/// pipeline. We verify that an encrypted datagram arrives at the peer address
/// registered in the path secret entry (`10.0.0.1:4433`).
#[test]
fn sends_encrypted_packet_to_peer() {
    sim(|| {
        let registry = crate::counter::Registry::default();
        let clock = Clock::default();
        // The bach simulation assigns 10.0.0.1 to the default ("main") group.
        // The recv socket binds to 0.0.0.0:4433 which maps to 10.0.0.1:4433.
        // We create the path secret entry with the same address so the assembler
        // sends the encrypted datagrams to the right simulated host.
        let peer_addr: std::net::SocketAddr = "10.0.0.1:4433".parse().unwrap();
        let entry = test_entry_at(peer_addr);

        let ctx = {
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
            Rc::new(RefCell::new(ctx))
        };

        let (mut ctx_tx, ctx_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (tx_wheel_tx, _tx_wheel_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (pto_wheel_tx, _pto_wheel_rx) = unsync::new_with_adapter::<send::PtoWheelAdapter>();
        let (idle_wheel_tx, _idle_wheel_rx) = unsync::new_with_adapter::<send::IdleWheelAdapter>();
        let (cancelled_tx, _cancelled_rx) = unsync::new::<frame::Frame>();
        let asm_counters = AssemblerCounters::new(&registry);

        // Receive the assembled datagram at the peer address.
        async move {
            let recv_socket = UdpSocket::bind("0.0.0.0:4433").await.unwrap();
            let mut buf = vec![0u8; 1500];
            let (n, _peer) = recv_socket.recv_from(&mut buf).await.unwrap();
            assert!(n > 0, "assembler should have sent an encrypted packet");
        }
        .primary()
        .spawn();

        // Bind the send socket and drain the assembler pipeline.
        async move {
            let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
            let rx = tasks::send_socket_assembler(
                ctx_rx,
                clock,
                VarInt::from_u8(0),
                0, // source_control_port
                Gso::default(),
                // Pool must hold at least one max-size datagram (1472 bytes from
                // TEST_APPLICATION_PARAMS); u16::MAX is safe and matches the assemble tests.
                Pool::new(u16::MAX),
                cancelled_tx.into_list_sender(),
                Discard, // ack_completions_tx: not exercised for a plain FlowData frame
                asm_counters,
                Rate::new(100.0),
                socket,
                tx_wheel_tx,
                pto_wheel_tx,
                idle_wheel_tx,
            );
            rx.drain_budgeted(Some(32)).await;
        }
        .spawn();

        // Feed the context and close the input.
        async move {
            let _ = ctx_tx.send(ctx);
            drop(ctx_tx);
        }
        .spawn();
    });
}

/// When the context input channel closes with no items, the pipeline terminates
/// without hanging or panicking.
#[test]
fn shuts_down_on_closed_input() {
    sim(|| {
        let registry = crate::counter::Registry::default();
        let clock = Clock::default();

        let (ctx_tx, ctx_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (tx_wheel_tx, _) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (pto_wheel_tx, _) = unsync::new_with_adapter::<send::PtoWheelAdapter>();
        let (idle_wheel_tx, _) = unsync::new_with_adapter::<send::IdleWheelAdapter>();
        let (cancelled_tx, _) = unsync::new::<frame::Frame>();
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
                Discard,
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
