// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests for how the endpoint treats its peers: the notices a peer sends about the path secrets
//! and keys it no longer accepts, the idle expiry of send and receive state, the dead-peer
//! declaration and its cooldown.

use super::helpers::{
    test_entry, test_frame, RecvContextBuilder, TestReceiver, TestReceiverExt, WakeNowSender,
};
use crate::{
    credit::Priority,
    endpoint::{
        frame::{self, Frame},
        id::{Id, IdMap, LocalSendSocketId, LocalSenderId, SendWorkerId},
        inflight::{Packet, TransmissionInfo},
        recv, send, tasks,
    },
    intrusive::{Entry, Queue},
    packet::{secret_control, WireVersion},
    path::secret::{map::Map, stateless_reset},
    psk,
    socket::{
        channel::{intrusive::unsync, ReceiverExt as _, UnboundedSender as _},
        pool::Pool,
    },
    stream::client::Client,
    testing::{ext::*, server_name, sim, NoopSubscriber, TestTlsProvider},
    time::{bach::Clock, precision},
};
use s2n_codec::EncoderBuffer;
use s2n_quic_core::{packet::number::PacketNumberSpace, time::Clock as _, varint::VarInt};
use std::{cell::RefCell, net::SocketAddr, rc::Rc, sync::Arc, time::Duration};

type PathSecretEntry = crate::path::secret::map::Entry;
type QueueState = crate::path::secret::map::entry::QueueState;
type SendCaches = IdMap<LocalSendSocketId, Rc<RefCell<send::Cache>>>;
type SendContext = Rc<RefCell<send::Context>>;

/// The peers the dead-peer broadcast asked a background handshake for, in order.
type Rehandshakes = Rc<RefCell<Vec<SocketAddr>>>;

const SIGNER_SECRET: &[u8] = b"peers-test-signer";

/// A dead-peer cooldown longer than any test runs.
const LONG_COOLDOWN: Duration = Duration::from_secs(3600);

// -- Shared setup ---------------------------------------------------------------------------------

fn send_caches() -> SendCaches {
    let registry = crate::counter::Registry::default();
    vec![Rc::new(RefCell::new(send::Cache::new(
        &registry,
        LocalSenderId::from_index(0),
    )))]
    .into()
}

fn context(send_caches: &SendCaches, pse: &Arc<PathSecretEntry>, clock: &Clock) -> SendContext {
    send_caches[LocalSendSocketId::new(0)]
        .borrow_mut()
        .get_or_insert(pse, clock)
        .unwrap()
}

fn context_count(send_caches: &SendCaches) -> usize {
    send_caches[LocalSendSocketId::new(0)]
        .borrow()
        .context_count()
}

/// Records a sent, unacknowledged packet that holds one frame, as the assembler does when it
/// sends one.
fn send_unacknowledged(ctx: &SendContext, pse: &Arc<PathSecretEntry>, clock: &Clock) {
    let mut ctx = ctx.borrow_mut();
    let now = clock.get_time();
    let rtt = ctx.rtt_estimator;
    let cc_info = ctx.cca.on_packet_sent(now, 200, false, &rtt);
    let mut frames = Queue::new();
    frames.push_back(test_frame(pse));
    let pn = PacketNumberSpace::Initial.new_packet_number(VarInt::ZERO);
    ctx.inflight.insert(
        pn,
        Packet::new(
            frames,
            TransmissionInfo {
                cc_info,
                time_sent: now,
                sent_bytes: 200,
            },
        ),
    );
}

fn invalidation_counters() -> tasks::SendInvalidationCounters {
    let registry = crate::counter::Registry::default();
    tasks::SendInvalidationCounters {
        unknown_path_secret_events: registry.register("test.peers.ups.events"),
        unknown_path_secret_contexts: registry.register("test.peers.ups.contexts"),
        unknown_path_secret_frames_failed: registry.register("test.peers.ups.frames_failed"),
        stale_or_replay_events: registry.register("test.peers.stale.events"),
        stale_or_replay_contexts: registry.register("test.peers.stale.contexts"),
        stale_or_replay_frames_requeued: registry.register("test.peers.stale.frames_requeued"),
    }
}

fn validator_counters() -> tasks::ValidatorInvalidationCounters {
    let registry = crate::counter::Registry::default();
    tasks::ValidatorInvalidationCounters {
        unknown_path_secret_validated: registry.register("test.peers.validator.ups"),
        stale_key_validated: registry.register("test.peers.validator.stale"),
        replay_detected_validated: registry.register("test.peers.validator.replay"),
    }
}

struct ValidatorSetup {
    local_map: Map,
    local_id: crate::credentials::Id,
    local_entry: Arc<PathSecretEntry>,
    peer_entry: Arc<PathSecretEntry>,
}

/// A path secret pair between a local map and a peer's map, as a handshake makes one.
fn validator_setup(local_addr: SocketAddr, peer_addr: SocketAddr) -> ValidatorSetup {
    let local_map = Map::builder()
        .with_signer(stateless_reset::Signer::new(SIGNER_SECRET))
        .with_capacity(16)
        .with_evict_on_unknown_path_secret(true)
        .with_clock(s2n_quic_core::time::NoopClock)
        .with_subscriber(crate::event::tracing::Subscriber::default())
        .build()
        .unwrap();
    local_map.test_stop_cleaner();

    let peer_map = Map::builder()
        .with_signer(stateless_reset::Signer::new(b"peers-test-peer-signer"))
        .with_capacity(16)
        .with_evict_on_unknown_path_secret(true)
        .with_clock(s2n_quic_core::time::NoopClock)
        .with_subscriber(crate::event::tracing::Subscriber::default())
        .build()
        .unwrap();
    peer_map.test_stop_cleaner();

    let ids = local_map.test_insert_pair(
        PathSecretEntry::builder(peer_addr).local(local_addr),
        &peer_map,
        PathSecretEntry::builder(local_addr).local(peer_addr),
    );

    let local_entry = local_map
        .get_by_id(&ids.local)
        .expect("local entry should exist");
    let peer_entry = peer_map
        .get_by_id(&ids.peer)
        .expect("peer entry should exist");

    ValidatorSetup {
        local_map,
        local_id: *local_entry.id(),
        local_entry,
        peer_entry,
    }
}

fn encode_unknown_path_secret(
    credential_id: crate::credentials::Id,
    tag: [u8; secret_control::TAG_LEN],
) -> Vec<u8> {
    let mut out = [0u8; secret_control::MAX_PACKET_SIZE];
    let len = secret_control::UnknownPathSecret {
        wire_version: WireVersion::ZERO,
        credential_id,
        queue_id: None,
    }
    .encode(EncoderBuffer::new(&mut out), &tag);
    out[..len].to_vec()
}

fn encode_stale_key(
    credential_id: crate::credentials::Id,
    sender_id: VarInt,
    min_key_id: VarInt,
    control_sealer: crate::crypto::awslc::seal::control::Secret,
) -> Vec<u8> {
    let mut out = [0u8; secret_control::MAX_PACKET_SIZE];
    let len = secret_control::StaleKey {
        wire_version: WireVersion::ZERO,
        credential_id,
        sender_id: Some(sender_id),
        min_key_id,
    }
    .encode(EncoderBuffer::new(&mut out), &control_sealer);
    out[..len].to_vec()
}

fn encode_replay_detected(
    credential_id: crate::credentials::Id,
    sender_id: VarInt,
    rejected_key_id: VarInt,
    control_sealer: crate::crypto::awslc::seal::control::Secret,
) -> Vec<u8> {
    let mut out = [0u8; secret_control::MAX_PACKET_SIZE];
    let len = secret_control::ReplayDetected {
        wire_version: WireVersion::ZERO,
        credential_id,
        rejected_key_id,
        sender_id: Some(sender_id),
    }
    .encode(EncoderBuffer::new(&mut out), &control_sealer);
    out[..len].to_vec()
}

fn packet_entry(
    payload: &[u8],
    peer: SocketAddr,
) -> Entry<crate::socket::pool::descriptor::Filled> {
    let unfilled = Pool::new(1200)
        .alloc::<crate::socket::pool::descriptor::SyncRecycler>()
        .expect("packet allocation should succeed");
    let segments = unfilled
        .fill_with(|addr, _cmsg, mut buffer| {
            addr.set(peer.into());
            buffer[..payload.len()].copy_from_slice(payload);
            Ok::<usize, core::convert::Infallible>(payload.len())
        })
        .expect("packet fill should succeed");
    Entry::new(segments.take_filled())
}

struct IdleSend {
    send_caches: SendCaches,
    idle_wheel_tx: unsync::Sender<send::IdleWheelAdapter>,
    completed_rx: unsync::Receiver<crate::intrusive::EntryAdapter<Frame>>,
    clock: Clock,
    rehandshakes: Rehandshakes,
}

/// Runs the send idle wheel with `cooldown` as its dead-peer cooldown, beside the dead-peer
/// broadcast, which records each background handshake it asks for.
fn idle_send(cooldown: Duration) -> IdleSend {
    let registry = crate::counter::Registry::default();
    let clock = Clock::default();
    let send_caches = send_caches();
    let sender_idx_to_local =
        IdMap::<LocalSenderId, LocalSendSocketId>::new(1, LocalSendSocketId::new(0));
    let sender_local_addrs =
        IdMap::<LocalSendSocketId, SocketAddr>::new(1, "127.0.0.1:0".parse().unwrap());
    let rehandshakes = Rehandshakes::default();
    let recorded = rehandshakes.clone();

    let (idle_wheel_tx, idle_wheel_rx) = unsync::new_with_adapter::<send::IdleWheelAdapter>();
    let (completed_tx, completed_rx) = unsync::new::<Frame>();
    let (ack_completions_tx, _ack_completions_rx) = unsync::new::<crate::endpoint::msg::Sender>();
    let ack_completions_tx = ack_completions_tx.into_list_sender();
    let (peer_dead_tx, peer_dead_rx) = unsync::new::<tasks::PeerDead>();
    let q_gauge = registry.register_queue_gauge("test.peers.idle_wheel");

    tasks::send_idle_wheel_drain(
        idle_wheel_rx,
        idle_wheel_tx.clone(),
        clock.clone(),
        q_gauge,
        send_caches.clone(),
        sender_idx_to_local,
        sender_local_addrs,
        completed_tx,
        ack_completions_tx,
        peer_dead_tx,
        cooldown,
        registry.register("idle.send.expired"),
        registry.register("idle.send.rescheduled"),
        registry.register_nominal_timer("idle.send.lifetime", "send.0"),
        32,
        registry.register_nominal_task("task.idle_wheel", "send.0"),
    )
    .spawn();

    let peer_dead_broadcast_task = tasks::peer_dead_broadcast(
        peer_dead_rx,
        WakeNowSender,
        tasks::PeerDeadCounters {
            events: registry.register("test.peers.peer_dead.events"),
            broadcasted: registry.register("test.peers.peer_dead.broadcasted"),
        },
        move |peer| recorded.borrow_mut().push(peer),
    );
    async move { peer_dead_broadcast_task.drain_budgeted(Some(32)).await }.spawn();

    IdleSend {
        send_caches,
        idle_wheel_tx,
        completed_rx,
        clock,
        rehandshakes,
    }
}

/// Schedules `ctx` into the send idle wheel at one idle timeout from now.
fn schedule_idle(
    idle_wheel_tx: &mut unsync::Sender<send::IdleWheelAdapter>,
    ctx: SendContext,
    clock: &Clock,
) {
    {
        let mut c = ctx.borrow_mut();
        let timeout = c.path_secret_entry.idle_timeout();
        c.idle_wheel.target_time = Some(precision::Clock::now(clock) + timeout);
    }
    let _ = idle_wheel_tx.send(ctx);
}

// -- Notices that fail validation -----------------------------------------------------------------

/// A notice that a path secret is unknown, whose stateless reset tag isn't the one of the path
/// secret it names, reaches neither the send nor the receive workers, and the path secret stays.
#[test]
fn a_forged_unknown_path_secret_notice_changes_no_state() {
    //= rendered/0005-endpoint/06-peers.md#DC-0005-040
    //= type=test
    //# The endpoint MUST NOT change any send or receive state on a notice from a peer that fails
    //# validation against its path secrets.
    sim(|| {
        let local_addr: SocketAddr = "127.0.0.1:3333".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:4444".parse().unwrap();
        let setup = validator_setup(local_addr, peer);
        let local_id = setup.local_id;
        let map = setup.local_map;

        let forged_tag = [0u8; secret_control::TAG_LEN];
        let payload = encode_unknown_path_secret(local_id.for_peer(), forged_tag);
        let input = TestReceiver::new([packet_entry(&payload, peer)]);
        let (send_tx, mut send_rx) = unsync::new::<tasks::Invalidation>();
        let (recv_tx, mut recv_rx) = unsync::new::<tasks::Invalidation>();
        let mut validator = tasks::invalidation_validator(
            input,
            map.clone(),
            vec![send_tx].into(),
            vec![recv_tx].into(),
            vec![SendWorkerId::new(0)].into(),
            validator_counters(),
        );

        async move {
            assert!(TestReceiverExt::recv(&mut validator).await.is_some());
            drop(validator);

            assert!(
                TestReceiverExt::recv(&mut send_rx).await.is_none(),
                "the send workers don't get the forged notice"
            );
            assert!(
                TestReceiverExt::recv(&mut recv_rx).await.is_none(),
                "the receive workers don't get the forged notice"
            );
            assert!(
                map.get_by_id(&local_id).is_some(),
                "the path secret the forged notice names stays in the map"
            );
        }
        .primary()
        .spawn();
    });
}

/// A stale key notice sealed with a key of another path secret doesn't reach a send worker.
#[test]
fn a_forged_stale_key_notice_changes_no_state() {
    //= rendered/0005-endpoint/06-peers.md#DC-0005-040
    //= type=test
    //# The endpoint MUST NOT change any send or receive state on a notice from a peer that fails
    //# validation against its path secrets.
    sim(|| {
        let local_addr: SocketAddr = "127.0.0.1:3333".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:6666".parse().unwrap();
        let setup = validator_setup(local_addr, peer);
        let unrelated = validator_setup(peer, local_addr);
        let local_id = setup.local_id;
        let map = setup.local_map;

        let sender_id = VarInt::from_u8(7);
        let min_key_id = VarInt::from_u8(3);
        let sealer = unrelated.peer_entry.control_sealer();
        let payload = encode_stale_key(local_id.for_peer(), sender_id, min_key_id, sealer);
        let input = TestReceiver::new([packet_entry(&payload, peer)]);
        let (send_tx, mut send_rx) = unsync::new::<tasks::Invalidation>();
        let (recv_tx, mut recv_rx) = unsync::new::<tasks::Invalidation>();
        let mut validator = tasks::invalidation_validator(
            input,
            map.clone(),
            vec![send_tx].into(),
            vec![recv_tx].into(),
            vec![SendWorkerId::new(0); 8].into(),
            validator_counters(),
        );

        async move {
            assert!(TestReceiverExt::recv(&mut validator).await.is_some());
            drop(validator);

            assert!(
                TestReceiverExt::recv(&mut send_rx).await.is_none(),
                "the send workers don't get the forged notice"
            );
            assert!(
                TestReceiverExt::recv(&mut recv_rx).await.is_none(),
                "the receive workers don't get the forged notice"
            );
            assert!(
                map.get_by_id(&local_id).is_some(),
                "the path secret the forged notice names stays in the map"
            );
        }
        .primary()
        .spawn();
    });
}

/// A replay notice sealed with a key of another path secret doesn't reach a send worker.
#[test]
fn a_forged_replay_notice_changes_no_state() {
    //= rendered/0005-endpoint/06-peers.md#DC-0005-040
    //= type=test
    //# The endpoint MUST NOT change any send or receive state on a notice from a peer that fails
    //# validation against its path secrets.
    sim(|| {
        let local_addr: SocketAddr = "127.0.0.1:3333".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:7777".parse().unwrap();
        let setup = validator_setup(local_addr, peer);
        let unrelated = validator_setup(peer, local_addr);
        let local_id = setup.local_id;
        let map = setup.local_map;

        let sender_id = VarInt::from_u8(9);
        let rejected_key_id = VarInt::from_u8(2);
        let sealer = unrelated.peer_entry.control_sealer();
        let payload =
            encode_replay_detected(local_id.for_peer(), sender_id, rejected_key_id, sealer);
        let input = TestReceiver::new([packet_entry(&payload, peer)]);
        let (send_tx, mut send_rx) = unsync::new::<tasks::Invalidation>();
        let (recv_tx, mut recv_rx) = unsync::new::<tasks::Invalidation>();
        let mut validator = tasks::invalidation_validator(
            input,
            map.clone(),
            vec![send_tx].into(),
            vec![recv_tx].into(),
            vec![SendWorkerId::new(0); 10].into(),
            validator_counters(),
        );

        async move {
            assert!(TestReceiverExt::recv(&mut validator).await.is_some());
            drop(validator);

            assert!(
                TestReceiverExt::recv(&mut send_rx).await.is_none(),
                "the send workers don't get the forged notice"
            );
            assert!(
                TestReceiverExt::recv(&mut recv_rx).await.is_none(),
                "the receive workers don't get the forged notice"
            );
            assert!(
                map.get_by_id(&local_id).is_some(),
                "the path secret the forged notice names stays in the map"
            );
        }
        .primary()
        .spawn();
    });
}

// -- Unknown path secret ---------------------------------------------------------------------------

/// A validated unknown path secret notice fails the frame waiting to be sent and the frame of the
/// packet the peer never acknowledged, and removes the send state of that path secret.
#[test]
fn an_unknown_path_secret_fails_the_pending_and_unacknowledged_frames() {
    //= rendered/0005-endpoint/06-peers.md#DC-0005-041
    //= type=test
    //# On a validated unknown path secret notice, the endpoint MUST fail every pending and
    //# unacknowledged frame sent under that credential with the unknown path secret reason.
    sim(|| {
        let clock = Clock::default();
        let send_caches = send_caches();
        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let ctx = context(&send_caches, &pse, &clock);
        ctx.borrow_mut().queues[1].push_back(test_frame(&pse));
        send_unacknowledged(&ctx, &pse, &clock);

        let invalidation = tasks::Invalidation::UnknownPathSecret {
            credential_id: *pse.id(),
        };
        let (cancelled_tx, mut cancelled_rx) = unsync::new::<Frame>();
        let (retransmit_tx, mut retransmit_rx) = unsync::new::<Frame>();
        let (ack_completions_tx, _ack_completions_rx) =
            unsync::new::<crate::endpoint::msg::Sender>();
        let ack_completions_tx = ack_completions_tx.into_list_sender();
        let mut rx = tasks::send_invalidation(
            TestReceiver::new(vec![Entry::new(invalidation)]),
            send_caches.clone(),
            IdMap::<LocalSenderId, LocalSendSocketId>::new(1, LocalSendSocketId::new(0)),
            cancelled_tx,
            retransmit_tx,
            ack_completions_tx,
            invalidation_counters(),
        );

        async move {
            TestReceiverExt::recv(&mut rx).await;
            drop(rx);

            assert_eq!(
                context_count(&send_caches),
                0,
                "the send state of the unknown path secret is removed"
            );
            for _ in 0..2 {
                let frame = TestReceiverExt::recv(&mut cancelled_rx)
                    .await
                    .expect("each pending and unacknowledged frame is failed");
                assert_eq!(
                    frame.status,
                    frame::TransmissionStatus::Failed(frame::FailureReason::UnknownPathSecret),
                );
            }
            assert!(
                TestReceiverExt::recv(&mut cancelled_rx).await.is_none(),
                "only the pending and the unacknowledged frame are failed"
            );
            assert!(
                TestReceiverExt::recv(&mut retransmit_rx).await.is_none(),
                "no frame is queued for retransmission"
            );
        }
        .primary()
        .spawn();
    });
}

/// A validated unknown path secret notice removes each receive state of that path secret.
#[test]
fn an_unknown_path_secret_discards_its_receive_state() {
    //= rendered/0005-endpoint/06-peers.md#DC-0005-042
    //= type=test
    //# On a validated unknown path secret notice, the endpoint MUST discard its receive state for
    //# that credential.
    sim(|| {
        let recv_cache = Rc::new(RefCell::new(recv::Cache::new(
            crate::endpoint::id::RecvDispatchWorkerId::new(0),
        )));
        let ctx_a = RecvContextBuilder::default()
            .remote_sender_id(VarInt::from_u8(0))
            .build();
        let ctx_b = RecvContextBuilder::default()
            .remote_sender_id(VarInt::from_u8(1))
            .build();
        let id = *ctx_a.borrow().path_entry.id();
        let key_a = recv::Key {
            id,
            remote_sender_id: crate::endpoint::id::RemoteSenderId::new(VarInt::from_u8(0)),
        };
        let key_b = recv::Key {
            id,
            remote_sender_id: crate::endpoint::id::RemoteSenderId::new(VarInt::from_u8(1)),
        };
        recv_cache.borrow_mut().senders.insert(key_a, ctx_a);
        recv_cache.borrow_mut().senders.insert(key_b, ctx_b);
        assert_eq!(recv_cache.borrow().senders.len(), 2);

        let invalidation = tasks::Invalidation::UnknownPathSecret { credential_id: id };
        let mut rx = tasks::recv_invalidation(
            TestReceiver::new(vec![Entry::new(invalidation)]),
            recv_cache.clone(),
        );

        async move {
            TestReceiverExt::recv(&mut rx).await;

            assert_eq!(
                recv_cache.borrow().senders.len(),
                0,
                "each receive state of the unknown path secret is removed"
            );
        }
        .primary()
        .spawn();
    });
}

// -- Stale key and replay ---------------------------------------------------------------------------

/// A validated stale key notice moves the frame waiting to be sent and the frame of the packet
/// the peer never acknowledged to the retransmission queue, and fails neither.
#[test]
fn a_stale_key_queues_the_pending_and_unacknowledged_frames_for_retransmission() {
    //= rendered/0005-endpoint/06-peers.md#DC-0005-043
    //= type=test
    //# On a validated stale key or replay notice, the sender MUST queue for retransmission the
    //# pending and unacknowledged frames of the send state that used the rejected key.
    sim(|| {
        let clock = Clock::default();
        let send_caches = send_caches();
        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let ctx = context(&send_caches, &pse, &clock);
        ctx.borrow_mut().queues[1].push_back(test_frame(&pse));
        send_unacknowledged(&ctx, &pse, &clock);
        let rejected_key_id = ctx.borrow().credentials.key_id;

        let invalidation = tasks::Invalidation::StaleKey {
            credential_id: *pse.id(),
            sender_id: LocalSenderId::from_index(0),
            rejected_key_id,
        };
        let (cancelled_tx, mut cancelled_rx) = unsync::new::<Frame>();
        let (retransmit_tx, mut retransmit_rx) = unsync::new::<Frame>();
        let (ack_completions_tx, _ack_completions_rx) =
            unsync::new::<crate::endpoint::msg::Sender>();
        let ack_completions_tx = ack_completions_tx.into_list_sender();
        let mut rx = tasks::send_invalidation(
            TestReceiver::new(vec![Entry::new(invalidation)]),
            send_caches.clone(),
            IdMap::<LocalSenderId, LocalSendSocketId>::new(1, LocalSendSocketId::new(0)),
            cancelled_tx,
            retransmit_tx,
            ack_completions_tx,
            invalidation_counters(),
        );

        async move {
            TestReceiverExt::recv(&mut rx).await;
            drop(rx);

            assert_eq!(
                context_count(&send_caches),
                0,
                "the send state that used the rejected key is removed"
            );
            for _ in 0..2 {
                let frame = TestReceiverExt::recv(&mut retransmit_rx)
                    .await
                    .expect("each pending and unacknowledged frame is queued again");
                assert_eq!(frame.status, frame::TransmissionStatus::Pending);
            }
            assert!(
                TestReceiverExt::recv(&mut retransmit_rx).await.is_none(),
                "only the pending and the unacknowledged frame are queued again"
            );
            assert!(
                TestReceiverExt::recv(&mut cancelled_rx).await.is_none(),
                "no frame is failed on a stale key"
            );
        }
        .primary()
        .spawn();
    });
}

/// A replay notice the endpoint validates reaches the send worker as a stale key invalidation,
/// which moves the pending and unacknowledged frames of the rejected key to retransmission.
#[test]
fn a_validated_replay_notice_queues_the_frames_of_the_rejected_key_for_retransmission() {
    //= rendered/0005-endpoint/06-peers.md#DC-0005-043
    //= type=test
    //# On a validated stale key or replay notice, the sender MUST queue for retransmission the
    //# pending and unacknowledged frames of the send state that used the rejected key.
    sim(|| {
        let local_addr: SocketAddr = "127.0.0.1:3333".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:8888".parse().unwrap();
        let setup = validator_setup(local_addr, peer);
        let local_id = setup.local_id;
        let local_entry = setup.local_entry;

        let clock = Clock::default();
        let send_caches = send_caches();
        local_entry.touch_activity(precision::Clock::now(&clock));
        let ctx = context(&send_caches, &local_entry, &clock);
        ctx.borrow_mut().queues[1].push_back(test_frame(&local_entry));
        send_unacknowledged(&ctx, &local_entry, &clock);
        let rejected_key_id = ctx.borrow().credentials.key_id;

        let sealer = setup.peer_entry.control_sealer();
        let payload =
            encode_replay_detected(local_id.for_peer(), VarInt::ZERO, rejected_key_id, sealer);
        let input = TestReceiver::new([packet_entry(&payload, peer)]);
        let (send_tx, mut send_rx) = unsync::new::<tasks::Invalidation>();
        let mut validator = tasks::invalidation_validator(
            input,
            setup.local_map,
            vec![send_tx].into(),
            vec![].into(),
            vec![SendWorkerId::new(0)].into(),
            validator_counters(),
        );

        async move {
            assert!(TestReceiverExt::recv(&mut validator).await.is_some());
            drop(validator);

            let invalidation = TestReceiverExt::recv(&mut send_rx)
                .await
                .expect("the validated replay notice reaches the send worker");
            assert_eq!(
                *invalidation,
                tasks::Invalidation::StaleKey {
                    credential_id: local_id,
                    sender_id: LocalSenderId::new(VarInt::ZERO),
                    rejected_key_id,
                }
            );

            let (cancelled_tx, mut cancelled_rx) = unsync::new::<Frame>();
            let (retransmit_tx, mut retransmit_rx) = unsync::new::<Frame>();
            let (ack_completions_tx, _ack_completions_rx) =
                unsync::new::<crate::endpoint::msg::Sender>();
            let ack_completions_tx = ack_completions_tx.into_list_sender();
            let mut rx = tasks::send_invalidation(
                TestReceiver::new(vec![invalidation]),
                send_caches.clone(),
                IdMap::<LocalSenderId, LocalSendSocketId>::new(1, LocalSendSocketId::new(0)),
                cancelled_tx,
                retransmit_tx,
                ack_completions_tx,
                invalidation_counters(),
            );
            TestReceiverExt::recv(&mut rx).await;
            drop(rx);

            assert_eq!(
                context_count(&send_caches),
                0,
                "the send state that used the rejected key is removed"
            );
            for _ in 0..2 {
                let frame = TestReceiverExt::recv(&mut retransmit_rx)
                    .await
                    .expect("each pending and unacknowledged frame is queued again");
                assert_eq!(frame.status, frame::TransmissionStatus::Pending);
            }
            assert!(
                TestReceiverExt::recv(&mut retransmit_rx).await.is_none(),
                "only the pending and the unacknowledged frame are queued again"
            );
            assert!(
                TestReceiverExt::recv(&mut cancelled_rx).await.is_none(),
                "no frame is failed on a replay notice"
            );
        }
        .primary()
        .spawn();
    });
}

/// A stale key notice that rejects a key older than the one the send state uses leaves the send
/// state, its pending frame and its unacknowledged packet as they are.
#[test]
fn a_stale_key_older_than_the_send_state_key_leaves_its_frames() {
    //= rendered/0005-endpoint/06-peers.md#DC-0005-044
    //= type=test
    //# A sender MUST NOT discard or retransmit the frames of a send state whose key is newer than
    //# the key a stale key or replay notice rejects.
    sim(|| {
        let clock = Clock::default();
        let send_caches = send_caches();
        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let ctx = context(&send_caches, &pse, &clock);
        ctx.borrow_mut().queues[1].push_back(test_frame(&pse));
        send_unacknowledged(&ctx, &pse, &clock);
        ctx.borrow_mut().credentials.key_id = VarInt::from_u8(1);

        let invalidation = tasks::Invalidation::StaleKey {
            credential_id: *pse.id(),
            sender_id: LocalSenderId::from_index(0),
            rejected_key_id: VarInt::ZERO,
        };
        let (cancelled_tx, mut cancelled_rx) = unsync::new::<Frame>();
        let (retransmit_tx, mut retransmit_rx) = unsync::new::<Frame>();
        let (ack_completions_tx, _ack_completions_rx) =
            unsync::new::<crate::endpoint::msg::Sender>();
        let ack_completions_tx = ack_completions_tx.into_list_sender();
        let mut rx = tasks::send_invalidation(
            TestReceiver::new(vec![Entry::new(invalidation)]),
            send_caches.clone(),
            IdMap::<LocalSenderId, LocalSendSocketId>::new(1, LocalSendSocketId::new(0)),
            cancelled_tx,
            retransmit_tx,
            ack_completions_tx,
            invalidation_counters(),
        );

        async move {
            TestReceiverExt::recv(&mut rx).await;
            drop(rx);

            assert_eq!(
                context_count(&send_caches),
                1,
                "the send state with the newer key stays"
            );
            assert_eq!(
                ctx.borrow().queues[1].len(),
                1,
                "the pending frame of the newer key stays queued"
            );
            assert!(
                ctx.borrow().inflight.has_inflight(),
                "the packet of the newer key stays unacknowledged"
            );
            assert!(
                TestReceiverExt::recv(&mut retransmit_rx).await.is_none(),
                "no frame of the newer key is queued again"
            );
            assert!(
                TestReceiverExt::recv(&mut cancelled_rx).await.is_none(),
                "no frame of the newer key is failed"
            );
        }
        .primary()
        .spawn();
    });
}

// -- Idle send state --------------------------------------------------------------------------------

/// An acknowledgement and any other packet from the peer each keep the send state for one more
/// idle timeout, and the send state is discarded once the peer sends nothing for an idle timeout.
#[test]
fn a_send_state_is_discarded_after_an_idle_timeout_without_a_packet_from_its_peer() {
    //= rendered/0005-endpoint/06-peers.md#DC-0005-045
    //= type=test
    //# A sender MUST discard a send state once neither an acknowledgement nor any other packet has
    //# arrived from its peer for the idle timeout.
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let IdleSend {
            send_caches,
            mut idle_wheel_tx,
            clock,
            ..
        } = idle_send(crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN);

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let ctx = context(&send_caches, &pse, &clock);
        schedule_idle(&mut idle_wheel_tx, ctx.clone(), &clock);

        async move {
            let idle_timeout = pse.idle_timeout();

            // An acknowledgement arrives halfway through the first idle timeout.
            (idle_timeout / 2).sleep().await;
            ctx.borrow_mut().last_peer_activity = precision::Clock::now(&Clock::default());

            (idle_timeout / 2 + 1.s()).sleep().await;
            assert_eq!(
                context_count(&send_caches),
                1,
                "an acknowledgement within the idle timeout keeps the send state"
            );

            // Another packet from the peer arrives.
            pse.touch_activity(precision::Clock::now(&Clock::default()));

            (idle_timeout / 2 + 1.s()).sleep().await;
            assert_eq!(
                context_count(&send_caches),
                1,
                "a packet from the peer within the idle timeout keeps the send state"
            );

            (idle_timeout / 2 + 4.s()).sleep().await;
            assert_eq!(
                context_count(&send_caches),
                0,
                "the send state is discarded once its peer is silent for the idle timeout"
            );
        }
        .primary()
        .spawn();
    });
}

/// A send state discarded for idleness fails its frame waiting to be sent and the frame of its
/// packet the peer never acknowledged with the peer dead reason.
#[test]
fn an_idle_send_state_fails_its_frames_with_the_peer_dead_reason() {
    //= rendered/0005-endpoint/06-peers.md#DC-0005-046
    //= type=test
    //# When a sender discards a send state for idleness, it MUST fail each of its pending and
    //# unacknowledged frames with the peer dead reason.
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let IdleSend {
            send_caches,
            mut idle_wheel_tx,
            mut completed_rx,
            clock,
            ..
        } = idle_send(crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN);

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let ctx = context(&send_caches, &pse, &clock);
        ctx.borrow_mut().queues[1].push_back(test_frame(&pse));
        send_unacknowledged(&ctx, &pse, &clock);
        schedule_idle(&mut idle_wheel_tx, ctx, &clock);

        async move {
            (pse.idle_timeout() + 2.s()).sleep().await;
            assert_eq!(
                context_count(&send_caches),
                0,
                "the send state is discarded for idleness"
            );

            for _ in 0..2 {
                let frame = TestReceiverExt::recv(&mut completed_rx)
                    .await
                    .expect("each pending and unacknowledged frame is failed");
                assert_eq!(
                    frame.status,
                    frame::TransmissionStatus::Failed(frame::FailureReason::PeerDead),
                    "the frame is failed with the peer dead reason"
                );
            }
        }
        .primary()
        .spawn();
    });
}

/// A client send state discarded for idleness with a packet its peer never acknowledged declares
/// the peer dead: the open stream with that peer is reset, and the client asks for a new
/// handshake with the peer.
#[test]
fn a_dead_peer_has_its_streams_reset_and_a_client_handshakes_again() {
    //= rendered/0005-endpoint/06-peers.md#DC-0005-047
    //= type=test
    //# When a send state discarded for idleness still held packets its peer never acknowledged,
    //# and the cooldown of an earlier declaration of the peer as dead isn't running, the endpoint
    //# MUST reset every stream with that peer.

    //= rendered/0005-endpoint/06-peers.md#DC-0005-048
    //= type=test
    //# When the peer of a client path is declared dead, the client MUST request a new handshake
    //# with that peer.
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let IdleSend {
            send_caches,
            mut idle_wheel_tx,
            clock,
            rehandshakes,
            ..
        } = idle_send(crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN);

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let QueueState::Client(state) = pse.queue_state() else {
            panic!("the test entry is on a client path");
        };
        let alloc = state
            .alloc_local(VarInt::ZERO)
            .expect("a new path secret entry has a free queue slot");

        let ctx = context(&send_caches, &pse, &clock);
        send_unacknowledged(&ctx, &pse, &clock);
        schedule_idle(&mut idle_wheel_tx, ctx, &clock);

        async move {
            (pse.idle_timeout() + 2.s()).sleep().await;
            assert_eq!(
                context_count(&send_caches),
                0,
                "the send state is discarded for idleness"
            );

            let error_code = match alloc.stream.try_recv() {
                Ok(Some(msg)) => match &*msg {
                    crate::endpoint::msg::Stream::Reset { error_code } => *error_code,
                    _ => panic!("the stream with the dead peer gets a reset"),
                },
                _ => panic!("the stream with the dead peer gets a message"),
            };
            assert_eq!(
                error_code,
                crate::endpoint::error::IDLE_TIMEOUT,
                "the stream is reset with the idle timeout error"
            );
            assert_eq!(
                rehandshakes.borrow().as_slice(),
                &[*pse.peer()],
                "the client asks for one new handshake with the dead peer"
            );
        }
        .primary()
        .spawn();
    });
}

/// A send state discarded for idleness during the cooldown of an earlier dead declaration of its
/// peer doesn't reset a stream or ask for a new handshake.
#[test]
fn a_dead_peer_in_its_cooldown_has_no_stream_reset() {
    //= rendered/0005-endpoint/06-peers.md#DC-0005-047
    //= type=test
    //# When a send state discarded for idleness still held packets its peer never acknowledged,
    //# and the cooldown of an earlier declaration of the peer as dead isn't running, the endpoint
    //# MUST reset every stream with that peer.
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let IdleSend {
            send_caches,
            mut idle_wheel_tx,
            clock,
            rehandshakes,
            ..
        } = idle_send(LONG_COOLDOWN);

        let pse = test_entry();
        pse.touch_activity(precision::Clock::now(&clock));
        let QueueState::Client(state) = pse.queue_state() else {
            panic!("the test entry is on a client path");
        };
        let alloc = state
            .alloc_local(VarInt::ZERO)
            .expect("a new path secret entry has a free queue slot");

        // An earlier declaration of the peer as dead, whose cooldown runs for the whole test.
        let now = precision::Clock::now(&clock);
        assert!(pse.mark_dead_if_cooldown_elapsed(now, LONG_COOLDOWN));

        let ctx = context(&send_caches, &pse, &clock);
        send_unacknowledged(&ctx, &pse, &clock);
        schedule_idle(&mut idle_wheel_tx, ctx, &clock);

        async move {
            (pse.idle_timeout() + 2.s()).sleep().await;
            assert_eq!(
                context_count(&send_caches),
                0,
                "the send state is discarded for idleness"
            );
            assert!(
                matches!(alloc.stream.try_recv(), Ok(None)),
                "no stream is reset during the cooldown of the earlier dead declaration"
            );
            assert!(
                rehandshakes.borrow().is_empty(),
                "no new handshake is asked for during the cooldown"
            );
        }
        .primary()
        .spawn();
    });
}

// -- Idle receive state -----------------------------------------------------------------------------

/// A receive state that goes idle is discarded without declaring its peer dead.
#[test]
fn an_idle_receive_state_doesnt_declare_its_peer_dead() {
    //= rendered/0005-endpoint/06-peers.md#DC-0005-049
    //= type=test
    //# A receiver MUST NOT declare a peer dead because its receive state went idle.
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let registry = crate::counter::Registry::default();
        let clock = Clock::default();
        let recv_cache = Rc::new(RefCell::new(recv::Cache::new(
            crate::endpoint::id::RecvDispatchWorkerId::new(0),
        )));
        let (mut idle_tx, idle_rx) = unsync::new_with_adapter::<recv::IdleWheelAdapter>();

        tasks::recv_idle_wheel_drain(
            idle_rx,
            idle_tx.clone(),
            clock.clone(),
            registry.register_queue_gauge("test.peers.recv_idle_wheel"),
            recv_cache.clone(),
            registry.register("idle.recv.expired"),
            registry.register("idle.recv.rescheduled"),
            registry.register_nominal_timer("idle.recv.lifetime", "recv.0"),
            32,
            registry.register_nominal_task("task.recv_idle_wheel", "recv.0"),
        )
        .spawn();

        let ctx = RecvContextBuilder::default().build();
        let pse = ctx.borrow().path_entry.clone();
        pse.touch_activity(precision::Clock::now(&clock));
        let key = {
            let c = ctx.borrow();
            recv::Key {
                id: *c.path_entry.id(),
                remote_sender_id: c.remote_sender_id,
            }
        };
        recv_cache.borrow_mut().senders.insert(key, ctx.clone());
        let _ = idle_tx.send(ctx);

        async move {
            (pse.idle_timeout() + 1.s()).sleep().await;
            assert!(
                recv_cache.borrow().senders.is_empty(),
                "the idle receive state is discarded"
            );

            let now = precision::Clock::now(&Clock::default());
            assert!(
                !pse.is_dead_during_cooldown(now, crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN),
                "an idle receive state doesn't declare its peer dead"
            );
        }
        .primary()
        .spawn();
    });
}

// -- Dead-peer cooldown -----------------------------------------------------------------------------

/// A client opens a stream to a peer that isn't dead, and once the peer is declared dead, its
/// next connect fails for the cooldown without opening a stream.
#[tokio::test]
async fn a_client_opens_no_stream_to_a_peer_during_its_dead_cooldown() {
    //= rendered/0005-endpoint/06-peers.md#DC-0005-050
    //= type=test
    //# The endpoint MUST NOT open a new stream to a peer during the cooldown that follows the peer
    //# being declared dead.
    let tls = TestTlsProvider {};
    let server_map = Map::builder()
        .with_signer(stateless_reset::Signer::new(b"default"))
        .with_capacity(50_000)
        .with_clock(s2n_quic_core::time::StdClock::default())
        .with_subscriber(NoopSubscriber {})
        .build()
        .unwrap();
    let server = psk::server::Provider::builder()
        .start(
            "[::1]:0".parse().unwrap(),
            tls.clone(),
            NoopSubscriber {},
            server_map,
        )
        .await
        .unwrap();
    let server_addr = server.local_addr();

    let map = Map::builder()
        .with_signer(stateless_reset::Signer::new(b"default"))
        .with_capacity(50_000)
        .with_clock(s2n_quic_core::time::StdClock::default())
        .with_subscriber(NoopSubscriber {})
        .build()
        .unwrap();
    map.test_stop_cleaner();
    let handshakes = psk::client::Provider::builder()
        .start(
            "[::]:0".parse().unwrap(),
            map.clone(),
            tls,
            NoopSubscriber {},
            server_name(),
        )
        .unwrap();

    let cooldown = crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN;
    let (endpoint, _frames) = crate::endpoint::Endpoint::without_workers(
        map.clone(),
        crate::time::DefaultClock::busy_poll_from(std::time::Instant::now()),
        cooldown,
    );
    let endpoint = Arc::new(endpoint);
    let mut client = Client::new(endpoint.clone(), handshakes, server_name());
    let acceptor_id = VarInt::from_u8(0);

    let stream = client
        .connect(server_addr, acceptor_id, Priority::default())
        .await
        .expect("a stream opens to a peer that isn't dead");
    drop(stream);

    let entry = map
        .get_untracked(server_addr)
        .expect("the connect's path secret entry")
        .into_raw();
    let now = endpoint.clock.now();
    assert!(entry.mark_dead_if_cooldown_elapsed(now, cooldown));

    let result = client
        .connect(server_addr, acceptor_id, Priority::default())
        .await;
    let error = match result {
        Ok(_) => panic!("no stream opens to the peer during its dead cooldown"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
    assert!(
        error.to_string().contains("dead cooldown"),
        "the connect fails for the dead cooldown: {error}"
    );
}
