// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract tests for extracted send-worker pipeline helpers.
use super::helpers::{
    build_send_context, event_count, insert_inflight_packet, test_batch, test_entry,
    test_map_with_subscriber, TestReceiver, TestReceiverExt as _,
};
use crate::{
    endpoint::{frame, id::Id, msg, send, tasks},
    socket::channel::{intrusive::unsync, ReceiverExt as _, UnboundedSender as _},
    testing::{ext::*, sim},
    time::{bach::Clock, precision},
    tracing::*,
    xorshift::Rng,
};
use bytes::BytesMut;
use core::time::Duration;
use s2n_quic_core::varint::VarInt;
use std::{cell::RefCell, rc::Rc};

#[test]
fn send_ack_processor_ignores_invalid_sender_id() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let registry = crate::counter::Registry::default();
        let send_caches: crate::endpoint::id::IdMap<crate::endpoint::id::LocalSendSocketId, _> =
            vec![Rc::new(RefCell::new(send::Cache::new(
                &registry,
                crate::endpoint::id::LocalSenderId::from_index(0),
            )))]
            .into();
        let sender_idx_to_local = crate::endpoint::id::IdMap::<
            crate::endpoint::id::LocalSenderId,
            crate::endpoint::id::LocalSendSocketId,
        >::new(1, crate::endpoint::id::LocalSendSocketId::new(0));

        let (mut ack_tx, ack_rx) = unsync::new::<msg::Sender>();
        let (tx_wheel_tx, mut tx_wheel_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (pto_wheel_tx, mut pto_wheel_rx) = unsync::new_with_adapter::<send::PtoWheelAdapter>();
        let (idle_wheel_tx, mut idle_wheel_rx) =
            unsync::new_with_adapter::<send::IdleWheelAdapter>();
        let (completed_tx, mut completed_rx) = unsync::new::<frame::Frame>();
        let (cancelled_tx, mut cancelled_rx) = unsync::new::<frame::Frame>();
        let (frame_tx, _frame_rx) = frame::submission_channel(1);

        let (immediate_tx_raw, _immediate_rx) =
            unsync::new_with_adapter::<send::TxImmediateAdapter>();
        let socket_immediate_txs: crate::endpoint::id::IdMap<
            crate::endpoint::id::LocalSendSocketId,
            _,
        > = core::iter::once((
            crate::endpoint::id::LocalSendSocketId::new(0),
            immediate_tx_raw,
        ))
        .collect();
        let mut sender_idx_to_local_imm =
            crate::endpoint::id::IdMap::<
                crate::endpoint::id::LocalSenderId,
                crate::endpoint::id::LocalSendSocketId,
            >::new(1, crate::endpoint::id::LocalSendSocketId::new(0));
        sender_idx_to_local_imm[crate::endpoint::id::LocalSenderId::from_index(0)] =
            crate::endpoint::id::LocalSendSocketId::new(0);
        let immediate_tx =
            send::ImmediateSender::new(socket_immediate_txs, sender_idx_to_local_imm);

        let (ack_completions_tx, _ack_completions_rx) = unsync::new::<msg::Sender>();
        let ack_completions_tx = ack_completions_tx.into_list_sender();
        let rx = tasks::send_ack_processor(
            ack_rx,
            send_caches,
            sender_idx_to_local,
            1,
            Clock::default(),
            Rng::new(),
            frame_tx,
            completed_tx,
            cancelled_tx,
            ack_completions_tx,
            registry.register("!send.invalid_sender_idx"),
            immediate_tx,
            tx_wheel_tx,
            pto_wheel_tx,
            idle_wheel_tx,
        );

        async move { rx.drain_budgeted(Some(32)).await }
            .primary()
            .spawn();

        async move {
            let entry = test_entry();
            let _ = ack_tx.send(crate::intrusive::Entry::new(msg::Sender::ReceivedAck {
                local_sender_id: crate::endpoint::id::LocalSenderId::new(VarInt::from_u8(3)),
                path_secret_entry: entry,
                payload: BytesMut::new(),
                ack_delay: Duration::ZERO,
                largest_acknowledged: VarInt::ZERO,
                ack_range: VarInt::ZERO,
                ecn_counts: Default::default(),
            }));
            drop(ack_tx);
        }
        .spawn();

        async move {
            assert!(
                tx_wheel_rx.recv().await.is_none(),
                "invalid sender id should not schedule tx wheel work"
            );
            assert!(
                pto_wheel_rx.recv().await.is_none(),
                "invalid sender id should not schedule pto wheel work"
            );
            assert!(
                idle_wheel_rx.recv().await.is_none(),
                "invalid sender id should not schedule idle wheel work"
            );
            assert!(
                completed_rx.recv().await.is_none(),
                "invalid sender id should not emit completions"
            );
            assert!(
                cancelled_rx.recv().await.is_none(),
                "invalid sender id should not emit cancellations"
            );
        }
        .primary()
        .spawn();
    });
}

#[test]
fn send_pto_timeout_routes_pending_context_to_tx_wheel() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let registry = crate::counter::Registry::default();
        let clock = Clock::default();
        let entry = test_entry();
        let mut ctx = send::Context::new(
            &entry,
            registry.register_queue_gauge("test.inflight"),
            registry.register_queue_gauge("test.ack"),
            registry.register_queue_gauge("test.pending"),
            crate::endpoint::id::LocalSenderId::from_index(0),
            &clock,
        )
        .expect("test context should be constructible");
        let _ = ctx.push_batch(test_batch(&entry).into_inner(), &clock);
        let input = TestReceiver::new([Rc::new(RefCell::new(ctx))]);

        let (tx_wheel_tx, mut tx_wheel_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (pto_wheel_tx, mut pto_wheel_rx) = unsync::new_with_adapter::<send::PtoWheelAdapter>();
        let (idle_wheel_tx, mut idle_wheel_rx) =
            unsync::new_with_adapter::<send::IdleWheelAdapter>();

        let (pto_immediate_tx_raw, _pto_immediate_rx) =
            unsync::new_with_adapter::<send::TxImmediateAdapter>();
        let pto_socket_immediate_txs: crate::endpoint::id::IdMap<
            crate::endpoint::id::LocalSendSocketId,
            _,
        > = core::iter::once((
            crate::endpoint::id::LocalSendSocketId::new(0),
            pto_immediate_tx_raw,
        ))
        .collect();
        let mut pto_sender_idx_to_local =
            crate::endpoint::id::IdMap::<
                crate::endpoint::id::LocalSenderId,
                crate::endpoint::id::LocalSendSocketId,
            >::new(1, crate::endpoint::id::LocalSendSocketId::new(0));
        pto_sender_idx_to_local[crate::endpoint::id::LocalSenderId::from_index(0)] =
            crate::endpoint::id::LocalSendSocketId::new(0);
        let pto_immediate_tx =
            send::ImmediateSender::new(pto_socket_immediate_txs, pto_sender_idx_to_local);
        let (retransmit_tx, _retransmit_rx) = unsync::new::<frame::Frame>();

        let rx = tasks::send_pto_timeout(
            input,
            clock,
            pto_immediate_tx,
            tx_wheel_tx,
            pto_wheel_tx,
            idle_wheel_tx,
            registry.register("tx.pto_check"),
            registry.register("tx.pto_requested"),
            crate::endpoint::id::IdMap::<crate::endpoint::id::LocalSendSocketId, _>::from(vec![
                Rc::new(RefCell::new(send::Cache::new(
                    &registry,
                    crate::endpoint::id::LocalSenderId::from_index(0),
                ))),
            ]),
            crate::endpoint::id::IdMap::<
                crate::endpoint::id::LocalSenderId,
                crate::endpoint::id::LocalSendSocketId,
            >::new(1, crate::endpoint::id::LocalSendSocketId::new(0)),
            retransmit_tx,
        );

        async move { rx.drain_budgeted(Some(32)).await }
            .primary()
            .spawn();

        async move {
            assert!(
                tx_wheel_rx.recv().await.is_some(),
                "pto timeout pipeline should route pending context to tx wheel"
            );
            assert!(
                pto_wheel_rx.recv().await.is_none(),
                "no inflight probe state should avoid pto re-scheduling in this scenario"
            );
            assert!(
                idle_wheel_rx.recv().await.is_some(),
                "pto timeout pipeline should preserve idle scheduling for active context"
            );
        }
        .primary()
        .spawn();
    });
}

#[test]
fn send_tx_wheel_drain_routes_expired_context_to_matching_socket() {
    sim(|| {
        let registry = crate::counter::Registry::default();
        let clock = Clock::default();
        let entry = test_entry();
        let mut ctx = send::Context::new(
            &entry,
            registry.register_queue_gauge("test.inflight"),
            registry.register_queue_gauge("test.ack"),
            registry.register_queue_gauge("test.pending"),
            crate::endpoint::id::LocalSenderId::from_index(1),
            &clock,
        )
        .expect("test context should be constructible");
        let _ = ctx.push_batch(test_batch(&entry).into_inner(), &clock);
        ctx.tx_wheel.target_time = Some(precision::Clock::now(&clock));
        let ctx = Rc::new(RefCell::new(ctx));

        let (mut tx_wheel_tx, tx_wheel_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (socket0_tx, mut socket0_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (socket1_tx, mut socket1_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();

        let socket_context_tx =
            crate::endpoint::combinator::MappedSender::new(vec![socket0_tx, socket1_tx].into(), {
                let mut m = crate::endpoint::id::IdMap::<
                    crate::endpoint::id::LocalSenderId,
                    crate::endpoint::id::LocalSendSocketId,
                >::new(
                    2, crate::endpoint::id::LocalSendSocketId::new(usize::MAX)
                );
                m[crate::endpoint::id::LocalSenderId::from_index(0)] =
                    crate::endpoint::id::LocalSendSocketId::new(0);
                m[crate::endpoint::id::LocalSenderId::from_index(1)] =
                    crate::endpoint::id::LocalSendSocketId::new(1);
                m
            });

        tasks::send_tx_wheel_drain(
            tx_wheel_rx,
            clock,
            registry.register_queue_gauge("test.tx_wheel"),
            socket_context_tx,
            32,
            registry.register_nominal_task("task.tx_wheel", "send.0"),
        )
        .spawn();

        async move {
            debug!("sending context to tx wheel");
            let _ = tx_wheel_tx.send(ctx);
            drop(tx_wheel_tx);
        }
        .spawn();

        async move {
            let routed = socket1_rx.recv().await.is_some();
            debug!(routed, "socket 1 routing result");
            assert!(
                routed,
                "sender_idx=1 context should route to socket queue 1"
            );
            let unexpected = socket0_rx.recv().await.is_some();
            debug!(unexpected, "socket 0 routing result");
            assert!(
                !unexpected,
                "socket queue 0 should not receive sender_idx=1 context"
            );
        }
        .primary()
        .spawn();
    });
}

/// A probe timeout that finds the peer silent on a pair for longer than the pair's persistent
/// congestion period marks the pair lost and moves its frames to the pairs that deliver, and
/// records the lost mark and the moved frames, each as an event and a count.
#[test]
fn send_pto_timeout_records_a_lost_mark_and_the_frames_it_moves() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let registry = crate::counter::Registry::default();
        let clock = Clock::default();
        let (map, subscriber) = test_map_with_subscriber();
        let send_caches: crate::endpoint::id::IdMap<crate::endpoint::id::LocalSendSocketId, _> =
            vec![Rc::new(RefCell::new(
                send::Cache::new(&registry, crate::endpoint::id::LocalSenderId::from_index(0))
                    .with_path_secret_map(map),
            ))]
            .into();
        let sender_idx_to_local = || {
            crate::endpoint::id::IdMap::<
                crate::endpoint::id::LocalSenderId,
                crate::endpoint::id::LocalSendSocketId,
            >::new(1, crate::endpoint::id::LocalSendSocketId::new(0))
        };

        // One sent packet that waits for an acknowledgement, on a path secret entry whose
        // other senders deliver.
        let entry = test_entry();
        let ctx = build_send_context(&entry, 0, &registry, &clock);
        {
            let mut ctx = ctx.borrow_mut();
            insert_inflight_packet(&mut ctx, &entry, &clock);
            ctx.ack_wait_since = Some(precision::Clock::now(&clock));
        }
        assert!(!ctx.borrow().is_path_lost());
        assert!(entry.has_delivering_sender());
        let input = TestReceiver::new([ctx.clone()]);

        let (tx_wheel_tx, _tx_wheel_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (pto_wheel_tx, _pto_wheel_rx) = unsync::new_with_adapter::<send::PtoWheelAdapter>();
        let (idle_wheel_tx, _idle_wheel_rx) =
            unsync::new_with_adapter::<send::IdleWheelAdapter>();
        let (immediate_tx_raw, _immediate_rx) =
            unsync::new_with_adapter::<send::TxImmediateAdapter>();
        let socket_immediate_txs: crate::endpoint::id::IdMap<
            crate::endpoint::id::LocalSendSocketId,
            _,
        > = core::iter::once((
            crate::endpoint::id::LocalSendSocketId::new(0),
            immediate_tx_raw,
        ))
        .collect();
        let immediate_tx = send::ImmediateSender::new(socket_immediate_txs, sender_idx_to_local());
        let (retransmit_tx, mut retransmit_rx) = unsync::new::<frame::Frame>();

        let rx = tasks::send_pto_timeout(
            input,
            clock,
            immediate_tx,
            tx_wheel_tx,
            pto_wheel_tx,
            idle_wheel_tx,
            registry.register("tx.pto_check"),
            registry.register("tx.pto_requested"),
            send_caches,
            sender_idx_to_local(),
            retransmit_tx,
        );

        async move {
            // The probe timeout fires once the peer has been silent on the pair for longer
            // than the pair's persistent congestion period.
            10.s().sleep().await;
            rx.drain_budgeted(Some(32)).await;

            assert!(ctx.borrow().is_path_lost(), "the silent pair is marked lost");
            assert!(
                retransmit_rx.recv().await.is_some(),
                "the lost pair's frame moves to the pairs that deliver"
            );
            assert_eq!(
                event_count(&subscriber.path_secret_map_interface_pair_lost),
                1
            );
            assert_eq!(
                event_count(&subscriber.path_secret_map_interface_pair_frames_moved),
                1
            );

            let line = registry
                .take_current_metrics_line()
                .expect("metrics were recorded");
            assert!(
                line.contains("tx.interface_pair.lost=1 Variant|send.0"),
                "got: {line}"
            );
            assert!(
                line.contains("tx.interface_pair.frames_moved=1 Variant|send.0"),
                "got: {line}"
            );
        }
        .primary()
        .spawn();
    });
}

/// An acknowledgement of a packet that a lost pair sent after its lost mark clears the mark,
/// and the ACK processor records the pair that delivers again as an event and a count.
#[test]
fn send_ack_processor_records_a_lost_pair_that_delivers_again() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let registry = crate::counter::Registry::default();
        let clock = Clock::default();
        let (map, subscriber) = test_map_with_subscriber();
        let cache = Rc::new(RefCell::new(
            send::Cache::new(&registry, crate::endpoint::id::LocalSenderId::from_index(0))
                .with_path_secret_map(map),
        ));
        let sender_idx_to_local = || {
            crate::endpoint::id::IdMap::<
                crate::endpoint::id::LocalSenderId,
                crate::endpoint::id::LocalSendSocketId,
            >::new(1, crate::endpoint::id::LocalSendSocketId::new(0))
        };

        // The pair is marked lost before its packet goes out, so an acknowledgement of the
        // packet shows that the pair delivers again.
        let entry = test_entry();
        let ctx = cache
            .borrow_mut()
            .get_or_insert(&entry, &clock)
            .expect("test context should be constructible");
        {
            let mut ctx = ctx.borrow_mut();
            ctx.set_path_lost(true);
            insert_inflight_packet(&mut ctx, &entry, &clock);
        }
        assert!(ctx.borrow().is_path_lost());
        let send_caches: crate::endpoint::id::IdMap<crate::endpoint::id::LocalSendSocketId, _> =
            vec![cache].into();

        let (mut ack_tx, ack_rx) = unsync::new::<msg::Sender>();
        let (tx_wheel_tx, _tx_wheel_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (pto_wheel_tx, _pto_wheel_rx) = unsync::new_with_adapter::<send::PtoWheelAdapter>();
        let (idle_wheel_tx, _idle_wheel_rx) =
            unsync::new_with_adapter::<send::IdleWheelAdapter>();
        let (completed_tx, _completed_rx) = unsync::new::<frame::Frame>();
        let (cancelled_tx, _cancelled_rx) = unsync::new::<frame::Frame>();
        let (frame_tx, _frame_rx) = frame::submission_channel(1);
        let (immediate_tx_raw, _immediate_rx) =
            unsync::new_with_adapter::<send::TxImmediateAdapter>();
        let socket_immediate_txs: crate::endpoint::id::IdMap<
            crate::endpoint::id::LocalSendSocketId,
            _,
        > = core::iter::once((
            crate::endpoint::id::LocalSendSocketId::new(0),
            immediate_tx_raw,
        ))
        .collect();
        let immediate_tx = send::ImmediateSender::new(socket_immediate_txs, sender_idx_to_local());
        let (ack_completions_tx, _ack_completions_rx) = unsync::new::<msg::Sender>();
        let ack_completions_tx = ack_completions_tx.into_list_sender();

        let rx = tasks::send_ack_processor(
            ack_rx,
            send_caches,
            sender_idx_to_local(),
            1,
            clock,
            Rng::new(),
            frame_tx,
            completed_tx,
            cancelled_tx,
            ack_completions_tx,
            registry.register("!send.invalid_sender_idx"),
            immediate_tx,
            tx_wheel_tx,
            pto_wheel_tx,
            idle_wheel_tx,
        );

        async move {
            // The acknowledgement arrives some time after the packet went out.
            Duration::from_millis(10).sleep().await;
            let _ = ack_tx.send(crate::intrusive::Entry::new(msg::Sender::ReceivedAck {
                local_sender_id: crate::endpoint::id::LocalSenderId::from_index(0),
                path_secret_entry: entry,
                payload: BytesMut::new(),
                ack_delay: Duration::ZERO,
                largest_acknowledged: VarInt::ZERO,
                ack_range: VarInt::ZERO,
                ecn_counts: Default::default(),
            }));
            drop(ack_tx);
        }
        .spawn();

        async move {
            rx.drain_budgeted(Some(32)).await;

            assert!(!ctx.borrow().is_path_lost(), "the acknowledgement clears the lost mark");
            assert_eq!(
                event_count(&subscriber.path_secret_map_interface_pair_delivering),
                1
            );
            assert_eq!(
                event_count(&subscriber.path_secret_map_interface_pair_lost),
                0,
                "the lost mark that the test sets on the context isn't recorded"
            );

            let line = registry
                .take_current_metrics_line()
                .expect("metrics were recorded");
            assert!(
                line.contains("tx.interface_pair.delivering=1 Variant|send.0"),
                "got: {line}"
            );
        }
        .primary()
        .spawn();
    });
}

/// The context resolver moves a batch pushed to a context whose interface pair is marked lost
/// to the frame channel at once, while another pair of the peer delivers, instead of keeping
/// it on the lost pair until the context's next probe timeout.
#[test]
fn context_resolver_moves_a_batch_pushed_to_a_lost_pair() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let registry = crate::counter::Registry::default();
        let entry = test_entry();
        let clock = Clock::default();
        let cache = Rc::new(RefCell::new(send::Cache::new(
            &registry,
            crate::endpoint::id::LocalSenderId::from_index(0),
        )));
        let ctx = cache
            .borrow_mut()
            .get_or_insert(&entry, &clock)
            .expect("the test entry has a peer data address");
        ctx.borrow_mut().set_path_lost(true);
        assert!(entry.has_delivering_sender());
        let send_caches: crate::endpoint::id::IdMap<crate::endpoint::id::LocalSendSocketId, _> =
            vec![cache].into();
        let sender_idx_to_local = crate::endpoint::id::IdMap::<
            crate::endpoint::id::LocalSenderId,
            crate::endpoint::id::LocalSendSocketId,
        >::new(1, crate::endpoint::id::LocalSendSocketId::new(0));

        let (tx_wheel_tx, tx_wheel_rx) = unsync::new_with_adapter::<send::TxWheelAdapter>();
        let (pto_wheel_tx, pto_wheel_rx) = unsync::new_with_adapter::<send::PtoWheelAdapter>();
        let (idle_wheel_tx, idle_wheel_rx) = unsync::new_with_adapter::<send::IdleWheelAdapter>();
        let (immediate_tx_raw, immediate_rx) =
            unsync::new_with_adapter::<send::TxImmediateAdapter>();
        let socket_immediate_txs: crate::endpoint::id::IdMap<
            crate::endpoint::id::LocalSendSocketId,
            _,
        > = core::iter::once((
            crate::endpoint::id::LocalSendSocketId::new(0),
            immediate_tx_raw,
        ))
        .collect();
        let immediate_tx = send::ImmediateSender::new(
            socket_immediate_txs,
            crate::endpoint::id::IdMap::new(1, crate::endpoint::id::LocalSendSocketId::new(0)),
        );
        let (retransmit_tx, mut retransmit_rx) = unsync::new::<frame::Frame>();
        let send_credit_pool = crate::sync::Arc::new(crate::credit::Pool::new(
            crate::credit::Config::new(1_000_000),
        ));

        let rx = tasks::context_resolver(
            TestReceiver::new([test_batch(&entry)]),
            send_caches,
            sender_idx_to_local,
            1,
            clock,
            immediate_tx,
            tx_wheel_tx,
            pto_wheel_tx,
            idle_wheel_tx,
            send_credit_pool,
            retransmit_tx,
        );

        async move { rx.drain_budgeted(Some(32)).await }.spawn();

        async move {
            // The wheels' receivers stay open until the resolver ends.
            let _wheels = (tx_wheel_rx, pto_wheel_rx, idle_wheel_rx, immediate_rx);
            assert!(
                retransmit_rx.recv().await.is_some(),
                "the batch pushed to the lost pair didn't go to the frame channel"
            );
            assert!(
                retransmit_rx.recv().await.is_none(),
                "the batch's one frame went to the frame channel more than once"
            );
        }
        .primary()
        .spawn();
    });
}

/// Accepts each value and drops it, for the send worker's outputs that a test doesn't read.
#[derive(Clone)]
struct DropSender;

impl<T> crate::socket::channel::UnboundedSender<T> for DropSender {
    fn send(&mut self, _value: T) -> Result<(), T> {
        Ok(())
    }
}

/// The send worker passes the endpoint's path secret map to each socket's assembler, which
/// publishes through that map the lost mark of a pair that sends acknowledgements alone, once
/// the pair's probe waits a persistent congestion period without an acknowledgement.
#[test]
fn send_worker_publishes_the_lost_mark_of_a_pair_that_sends_acknowledgements_alone() {
    use crate::endpoint::id::{
        IdMap, LocalSendSocketId, LocalSenderId, RecvDispatchWorkerId, RemoteSenderId, SendWorkerId,
    };

    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let registry = crate::counter::Registry::default();
        let clock = Clock::default();
        let (map, subscriber) = test_map_with_subscriber();

        // The peer receives each packet and doesn't acknowledge any of them.
        async {
            let socket = bach::net::UdpSocket::bind("0.0.0.0:4433").await.unwrap();
            let mut buf = vec![0u8; 1500];
            while socket.recv_from(&mut buf).await.is_ok() {}
        }
        .group("server")
        .spawn();

        async move {
            let entry = super::helpers::test_entry_at("server:4433").await;
            let fresh = build_send_context(&entry, 0, &registry, &clock);
            let threshold = fresh.borrow().rtt_estimator.persistent_congestion_threshold();
            let step = threshold / 10;

            // The socket's place counts two interfaces, so that a pair that sends
            // acknowledgements alone probes the peer and is marked lost once the probe waits a
            // persistent congestion period.
            let place = crate::socket::interface::SendPlace {
                rank: 0,
                count: 2,
                index: 0,
            };
            let parts = crate::endpoint::SendSocketParts {
                socket: bach::net::UdpSocket::bind("0.0.0.0:0").await.unwrap(),
                sender_idx: LocalSenderId::from_index(0),
                source_control_port: 0,
                gso: s2n_quic_platform::features::Gso::default(),
                pool: crate::socket::pool::Pool::new(u16::MAX),
                clock: clock.clone(),
                initial_tx_descriptor_allocs: 0,
                interface_place: Some(place),
            };
            let send_sockets: IdMap<LocalSendSocketId, _> = vec![parts].into();

            let (_batch_tx, batch_rx) = unsync::new::<crate::endpoint::combinator::FrameBatch>();
            let (mut ack_tx, ack_rx) = unsync::new::<msg::Sender>();
            let (_invalidation_tx, invalidation_rx) = unsync::new::<tasks::Invalidation>();
            let (frame_tx, _frame_rx) = frame::submission_channel(1);
            let (freed_batch_tx, _freed_batch_rx) = crate::queue::freed_batch_channel();
            let reader_metrics = std::sync::Arc::new(crate::stream::metrics::ReaderMetrics::new(
                &registry,
                "stream.reader",
            ));
            let writer_metrics = std::sync::Arc::new(crate::stream::metrics::WriterMetrics::new(
                &registry,
                "stream.writer",
            ));
            let send_credit_pool = crate::sync::Arc::new(crate::credit::Pool::new(
                crate::credit::Config::new(1_000_000),
            ));
            tasks::send_worker(
                &mut crate::runtime::bach::Local::new(0),
                SendWorkerId::new(0),
                batch_rx,
                ack_rx,
                invalidation_rx,
                1,
                send_sockets,
                clock.clone(),
                Rng::new(),
                frame_tx,
                DropSender,
                freed_batch_tx,
                DropSender,
                DropSender,
                Duration::from_secs(1),
                crate::endpoint::Budgets::default(),
                registry.clone(),
                crate::time::DefaultClock::default(),
                reader_metrics,
                writer_metrics,
                send_credit_pool,
                map,
            );

            // The pair sends an acknowledgement each step, and the first one is its probe.
            for packet_number in 0..20u8 {
                if packet_number == 5 {
                    assert_eq!(
                        event_count(&subscriber.path_secret_map_interface_pair_lost),
                        0,
                        "the probe hasn't waited a persistent congestion period yet"
                    );
                }
                let ack = crate::stream::endpoint::ack::state::Submission {
                    largest_acknowledged: VarInt::from_u8(packet_number),
                    ack_range: VarInt::ZERO,
                    extra_ranges: bytes::Bytes::new(),
                    ecn_counts: Default::default(),
                    largest_recv_time: precision::Clock::now(&clock),
                    path_secret_entry: entry.clone(),
                    local_sender_id: LocalSenderId::from_index(0),
                    remote_sender_id: RemoteSenderId::new(VarInt::ZERO),
                    recv_worker_id: RecvDispatchWorkerId::new(0),
                };
                let _ = ack_tx.send(crate::intrusive::Entry::new(msg::Sender::PendingAck(ack)));
                step.sleep().await;
            }
            assert_eq!(
                event_count(&subscriber.path_secret_map_interface_pair_lost),
                1,
                "the send worker publishes the lost mark through the endpoint's path secret map"
            );
        }
        .primary()
        .spawn();
    });
}
