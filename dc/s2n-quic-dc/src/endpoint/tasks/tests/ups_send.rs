// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract tests for the `ups_send` pipeline.
//!
//! Verifies that:
//! 1. A UPS response with a valid credential is sent to the destination address.
//! 2. A duplicate response for the same credential within the dedup window is suppressed.
//! 3. A burst of rejections for one credential and peer sender id leaves one response in the
//!    queue, which `ups_send` still sends, and the first rejection after it leaves the queue
//!    queues a new one.
//! 4. Rejections for many credentials, or for two sender ids of one credential, leave one
//!    response in the queue for each.
//! 5. A response the queue refuses doesn't keep a later response with its key out.
//! 6. Rejections of packets whose credential the path secret map doesn't hold leave one
//!    UnknownPathSecret response in the queue for each credential and sender id.
//! 7. Each key's shard of `ups::Pending` is the recv_dispatch worker the recv router hands the
//!    key's packets to, and `ups_send` takes each response's key out of the shard that holds it.
//! 8. Rejections for many credentials and sender ids keep the UnknownPathSecret responses in
//!    the queue within its limit, the queue takes each ReplayDetected response before and after
//!    they fill it, and a response the queue refuses returns its place.
//! 9. A queue full of UnknownPathSecret responses takes new ones once `ups_send` drains it, and
//!    `ups_send` sends the ReplayDetected response the full queue took.
//! 10. The default queue limit is the responses one burst of `ups_rate` sends.

use super::helpers::test_entry;
use crate::{
    counter::Registry,
    credentials::{Credentials, Id},
    endpoint::{counters, dispatch, id::RemoteSenderId, routing, tasks, ups},
    intrusive::Entry,
    msg::addr::Addr,
    packet::{secret_control, WireVersion},
    socket::channel::{intrusive::unsync, Receiver, ReceiverExt as _, UnboundedSender},
    testing::{ext::*, sim},
    time::bach::Clock,
};
use bach::net::UdpSocket;
use core::time::Duration;
use s2n_codec::{DecoderBufferMut, EncoderBuffer};
use s2n_quic_core::varint::VarInt;
use std::{cell::RefCell, collections::HashSet, net::SocketAddr, rc::Rc};

const STATELESS_RESET_TAG: &[u8; 16] = b"ups-test-signer!";
const CREDENTIAL_BYTES: [u8; 16] = [0xAB; 16];
/// How many of the peer's packets one burst rejects, each with a response of its own.
const BURST: usize = 100;
/// The recv_dispatch workers each test's `ups::Pending` holds a shard of keys for: more than
/// one, so that the tests' keys spread over several shards.
const RECV_DISPATCH_WORKERS: usize = 4;

/// The most responses each test's `ups::Pending` keeps in the queue together: more than the
/// keys of any test that doesn't set its own limit.
const QUEUE_LIMIT: usize = 64;

fn encode_ups_packet(id: Id) -> Vec<u8> {
    let mut buf = [0u8; secret_control::MAX_PACKET_SIZE];
    let stateless_reset: [u8; secret_control::TAG_LEN] = *STATELESS_RESET_TAG;
    let len = secret_control::UnknownPathSecret {
        wire_version: WireVersion::ZERO,
        credential_id: id,
        queue_id: None,
    }
    .encode(EncoderBuffer::new(&mut buf), &stateless_reset);
    buf[..len].to_vec()
}

fn make_response(dest: SocketAddr, id: Id) -> Entry<ups::Response> {
    let mut addr = Addr::default();
    addr.set(dest.into());
    let key = ups::Key {
        credential_id: id,
        sender_id: VarInt::ZERO,
    };
    let response = ups::Response::unknown_path_secret(addr, encode_ups_packet(id), key);
    Entry::new(response)
}

/// A UPS response with a valid credential reaches the destination socket.
#[test]
fn ups_response_is_delivered_to_destination() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let id = Id::from(CREDENTIAL_BYTES);

        // Create a channel so the feeder task can push a response after resolving the
        // server's simulated address.
        let (mut input_tx, input_rx) = unsync::new::<ups::Response>();

        // Server: bind and wait for exactly one datagram.
        async {
            let recv_socket = UdpSocket::bind("0.0.0.0:4433").await.unwrap();
            let mut buf = vec![0u8; 1500];
            let (n, _from) = recv_socket.recv_from(&mut buf).await.unwrap();
            assert!(n > 0, "UPS response should have been delivered");
        }
        .group("server")
        .primary()
        .spawn();

        // Feeder: resolve server address then push a UPS response into the pipeline.
        async move {
            let dest = bach::net::lookup_host("server:4433")
                .await
                .expect("lookup failed")
                .next()
                .expect("no address");
            let _ = input_tx.send(make_response(dest, id));
            drop(input_tx);
        }
        .spawn();

        // Pipeline: drain until the channel closes after sending the one response.
        async move {
            let send_socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
            let registry = Registry::default();
            let counters = ups::Counters::new(&registry);
            let clock = Clock::default();
            let rx = tasks::ups_send(
                input_rx,
                send_socket,
                clock,
                crate::socket::rate::Rate::new(100.0),
                1024,
                Duration::from_secs(60),
                counters,
            );
            rx.drain_budgeted(Some(32)).await;
        }
        .spawn();
    });
}

/// A second UPS response for the same credential within the dedup window is suppressed.
///
/// The server receives exactly one datagram (the first response). The duplicate is silently
/// dropped by `DedupFilter`. The `dedup_suppressed` counter reflects the suppression.
#[test]
fn duplicate_within_window_is_suppressed() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let id = Id::from(CREDENTIAL_BYTES);
        let (mut input_tx, input_rx) = unsync::new::<ups::Response>();

        // Server: bind and receive exactly one datagram (the first; the duplicate is dropped).
        async {
            let recv_socket = UdpSocket::bind("0.0.0.0:4433").await.unwrap();
            let mut buf = vec![0u8; 1500];
            let (n, _from) = recv_socket.recv_from(&mut buf).await.unwrap();
            assert!(n > 0, "first UPS response should be delivered");
        }
        .group("server")
        .primary()
        .spawn();

        // Feeder: push two responses for the same credential.
        async move {
            let dest = bach::net::lookup_host("server:4433")
                .await
                .expect("lookup failed")
                .next()
                .expect("no address");
            let _ = input_tx.send(make_response(dest, id));
            let _ = input_tx.send(make_response(dest, id));
            drop(input_tx);
        }
        .spawn();

        // Pipeline: drain with a large budget; first item passes, second is suppressed by dedup.
        // The server's primary task completes after receiving exactly one packet.
        async move {
            let send_socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
            let registry = Registry::default();
            let counters = ups::Counters::new(&registry);
            let clock = Clock::default();
            let rx = tasks::ups_send(
                input_rx,
                send_socket,
                clock,
                crate::socket::rate::Rate::new(100.0),
                1024,
                Duration::from_secs(60), // long window → second is suppressed
                counters,
            );
            rx.drain_budgeted(Some(32)).await;
        }
        .spawn();
    });
}

/// The ReplayDetected packet a recv_dispatch worker encodes for a packet from the peer's
/// `sender_id` on a key it has already seen.
fn encode_replay_detected(id: Id, sender_id: VarInt) -> Vec<u8> {
    let mut buf = [0u8; secret_control::MAX_PACKET_SIZE];
    let sealer = test_entry().control_sealer();
    let len = secret_control::ReplayDetected {
        credential_id: id,
        wire_version: WireVersion::ZERO,
        sender_id: Some(sender_id),
        rejected_key_id: VarInt::ZERO,
    }
    .encode(EncoderBuffer::new(&mut buf), &sealer);
    buf[..len].to_vec()
}

/// Hands `on_packet_dispatch_error` the rejection of a packet from the peer's `sender_id` on a
/// key the receiver has already seen, as `dispatch::process` returns it.
fn reject(
    dispatch_counters: &counters::Dispatch,
    ups_tx: &mut impl UnboundedSender<Entry<ups::Response>>,
    dest: SocketAddr,
    id: Id,
    sender_id: VarInt,
) {
    let mut dest_addr = Addr::default();
    dest_addr.set(dest.into());
    let err = dispatch::Error::StaleKey {
        dest_addr,
        credentials: Credentials {
            id,
            key_id: VarInt::ZERO,
        },
        sender_id,
        packet_number: VarInt::ZERO,
        control_out: encode_replay_detected(id, sender_id),
    };
    tasks::on_packet_dispatch_error(dispatch_counters, ups_tx, err);
}

/// Hands `on_packet_dispatch_error` the rejection of a packet from the peer's `sender_id` whose
/// credential the path secret map doesn't hold, with its UnknownPathSecret response.
fn reject_unknown(
    dispatch_counters: &counters::Dispatch,
    ups_tx: &mut impl UnboundedSender<Entry<ups::Response>>,
    dest: SocketAddr,
    id: Id,
    sender_id: VarInt,
) {
    let mut dest_addr = Addr::default();
    dest_addr.set(dest.into());
    let err = dispatch::Error::PeerStateLookup {
        dest_addr,
        credentials: Credentials {
            id,
            key_id: VarInt::ZERO,
        },
        sender_id,
        control_out: encode_ups_packet(id),
    };
    tasks::on_packet_dispatch_error(dispatch_counters, ups_tx, err);
}

fn new_pending(registry: &Registry) -> ups::Pending {
    new_pending_with_limit(registry, QUEUE_LIMIT)
}

/// A `ups::Pending` that keeps at most `limit` responses in the queue together.
fn new_pending_with_limit(registry: &Registry, limit: usize) -> ups::Pending {
    ups::Pending::new(
        RECV_DISPATCH_WORKERS,
        limit,
        registry.register("test.ups.pending_suppressed"),
        registry.register("test.ups.pending_dropped"),
    )
}

/// The recv_dispatch worker that route `R` picks, over `workers` workers, for the packets of
/// `key`, as the recv router picks it.
fn routed_worker<R: routing::SenderRoute>(workers: usize, key: ups::Key) -> usize {
    let credentials = Credentials {
        id: key.credential_id,
        key_id: VarInt::ZERO,
    };
    R::new(workers)
        .worker_id_for_recv(&credentials, RemoteSenderId::new(key.sender_id))
        .as_usize()
}

async fn server_addr() -> SocketAddr {
    bach::net::lookup_host("server:4433")
        .await
        .expect("lookup failed")
        .next()
        .expect("no address")
}

/// The credential id and sender id of each ReplayDetected or UnknownPathSecret packet the
/// server receives, with `None` for the sender id of an UnknownPathSecret packet, which
/// doesn't name one.
type Received = Rc<RefCell<Vec<(Id, Option<VarInt>)>>>;

/// Binds the server's socket, at port 4433 of group "server", and records each ReplayDetected
/// and UnknownPathSecret packet it receives.
fn spawn_server() -> Received {
    let received = Received::default();
    let record = received.clone();
    async move {
        let socket = UdpSocket::bind("0.0.0.0:4433").await.unwrap();
        let mut buf = vec![0u8; 1500];
        loop {
            let (n, _from) = socket.recv_from(&mut buf).await.unwrap();
            let decoder = DecoderBufferMut::new(&mut buf[..n]);
            let (packet, _) = secret_control::Packet::decode(decoder).expect("a control packet");
            if let secret_control::Packet::StaleKey(_) = packet {
                panic!("expected a ReplayDetected or UnknownPathSecret packet");
            }
            record
                .borrow_mut()
                .push((*packet.credential_id(), packet.sender_id()));
        }
    }
    .group("server")
    .spawn();
    received
}

/// Runs `ups_send` over `rx`, at a rate that doesn't hold the test back.
fn spawn_pipeline<R>(rx: R)
where
    R: Receiver<Entry<ups::Response>> + 'static,
{
    async move {
        let send_socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        let registry = Registry::default();
        let rx = tasks::ups_send(
            rx,
            send_socket,
            Clock::default(),
            crate::socket::rate::Rate::new(100.0),
            1024,
            Duration::from_secs(60),
            ups::Counters::new(&registry),
        );
        rx.drain_budgeted(Some(32)).await;
    }
    .spawn();
}

/// A peer that keeps sending on a key the receiver has already seen gets each of its packets
/// rejected, far faster than the pacer sends. A burst of those rejections for one credential
/// and sender id leaves one response in the queue, which `ups_send` still sends, and the first
/// rejection after that response leaves the queue queues a new one.
#[test]
fn a_burst_for_one_credential_queues_and_sends_one_response() {
    //= rendered/0002-path-secrets/09-control-packets.md#DC-0002-078
    //= type=test
    //# An endpoint MUST keep at most one stale key or replay detected packet waiting to be
    //# sent for each path secret and sender it answers.
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let id = Id::from(CREDENTIAL_BYTES);
        let sender_id = VarInt::from_u8(3);
        let registry = Registry::default();
        let pending = new_pending(&registry);
        let (tx, rx) = unsync::new::<ups::Response>();
        spawn_pipeline(pending.receiver(rx));
        let received = spawn_server();

        async move {
            let dest = server_addr().await;
            let dispatch_counters = counters::Dispatch::new(&registry);
            let mut ups_tx = pending.sender(tx);

            for _ in 0..BURST {
                reject(&dispatch_counters, &mut ups_tx, dest, id, sender_id);
            }
            assert_eq!(
                pending.queued(),
                1,
                "the burst leaves one response in the queue"
            );

            1.s().sleep().await;
            assert_eq!(
                pending.queued(),
                0,
                "ups_send took the response off the queue"
            );
            assert_eq!(*received.borrow(), [(id, Some(sender_id))]);

            for _ in 0..BURST {
                reject(&dispatch_counters, &mut ups_tx, dest, id, sender_id);
            }
            assert_eq!(
                pending.queued(),
                1,
                "a rejection after the response left the queue queues a new one"
            );

            1.s().sleep().await;
            assert_eq!(*received.borrow(), [(id, Some(sender_id)); 2]);
        }
        .primary()
        .spawn();
    });
}

/// Rejections for many credentials leave one response for each of them in the queue, and
/// `ups_send` sends each.
#[test]
fn rejections_for_many_credentials_queue_one_response_each() {
    const CREDENTIALS: u8 = 8;

    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let sender_id = VarInt::from_u8(3);
        let registry = Registry::default();
        let pending = new_pending(&registry);
        let (tx, rx) = unsync::new::<ups::Response>();
        spawn_pipeline(pending.receiver(rx));
        let received = spawn_server();

        async move {
            let dest = server_addr().await;
            let dispatch_counters = counters::Dispatch::new(&registry);
            let mut ups_tx = pending.sender(tx);
            let key = |i: u8| ups::Key {
                credential_id: Id::from([i; 16]),
                sender_id,
            };
            let shards: HashSet<_> = (0..CREDENTIALS).map(|i| pending.shard(&key(i))).collect();
            assert!(
                shards.len() > 1,
                "the credentials' keys spread over more than one shard"
            );

            for _ in 0..BURST {
                for i in 0..CREDENTIALS {
                    let id = Id::from([i; 16]);
                    reject(&dispatch_counters, &mut ups_tx, dest, id, sender_id);
                }
            }
            assert_eq!(pending.queued(), usize::from(CREDENTIALS));

            1.s().sleep().await;
            assert_eq!(
                pending.queued(),
                0,
                "ups_send took each response's key out of its shard"
            );
            let received = received.borrow();
            assert_eq!(received.len(), usize::from(CREDENTIALS));
            for i in 0..CREDENTIALS {
                let id = Id::from([i; 16]);
                assert_eq!(
                    received.iter().filter(|(got, _)| *got == id).count(),
                    1,
                    "credential {i} gets one response"
                );
            }
        }
        .primary()
        .spawn();
    });
}

/// The peer recovers the send contexts of one sender id from each response, which names that
/// sender id, so rejections for two sender ids of one credential leave one response for each.
#[test]
fn rejections_for_two_sender_ids_of_one_credential_queue_one_response_each() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let id = Id::from(CREDENTIAL_BYTES);
        let sender_ids = [VarInt::from_u8(0), VarInt::from_u8(1)];
        let registry = Registry::default();
        let pending = new_pending(&registry);
        let (tx, rx) = unsync::new::<ups::Response>();
        spawn_pipeline(pending.receiver(rx));
        let received = spawn_server();

        async move {
            let dest = server_addr().await;
            let dispatch_counters = counters::Dispatch::new(&registry);
            let mut ups_tx = pending.sender(tx);

            for _ in 0..BURST {
                for sender_id in sender_ids {
                    reject(&dispatch_counters, &mut ups_tx, dest, id, sender_id);
                }
            }
            assert_eq!(pending.queued(), sender_ids.len());

            1.s().sleep().await;
            let received = received.borrow();
            assert_eq!(received.len(), sender_ids.len());
            for sender_id in sender_ids {
                assert!(
                    received.contains(&(id, Some(sender_id))),
                    "sender id {sender_id:?} gets a response"
                );
            }
        }
        .primary()
        .spawn();
    });
}

/// A response the queue refuses, as once its receiver is gone, doesn't leave its key behind,
/// so the key doesn't keep a later response out.
#[test]
fn a_refused_response_does_not_hold_its_key() {
    let registry = Registry::default();
    let pending = new_pending(&registry);
    let (tx, rx) = unsync::new::<ups::Response>();
    let mut ups_tx = pending.sender(tx);
    drop(rx);

    let dest = SocketAddr::from(([127, 0, 0, 1], 4433));
    let id = Id::from(CREDENTIAL_BYTES);
    assert!(ups_tx.send(make_response(dest, id)).is_err());
    assert_eq!(
        pending.queued(),
        0,
        "the refused response doesn't hold its key"
    );
    assert_eq!(
        pending.waiting(),
        0,
        "the refused response doesn't hold a place in the queue"
    );
}

/// Rejections of packets whose credential the path secret map doesn't hold leave one
/// UnknownPathSecret response in the queue for each credential and sender id.
#[test]
fn unknown_credentials_queue_one_response_for_each_credential_and_sender_id() {
    let registry = Registry::default();
    let pending = new_pending(&registry);
    let dispatch_counters = counters::Dispatch::new(&registry);
    let (tx, _rx) = unsync::new::<ups::Response>();
    let mut ups_tx = pending.sender(tx);
    let dest = SocketAddr::from(([127, 0, 0, 1], 4433));
    let ids = [Id::from([1; 16]), Id::from([2; 16])];
    let sender_ids = [VarInt::from_u8(0), VarInt::from_u8(1)];

    for _ in 0..BURST {
        for id in ids {
            for sender_id in sender_ids {
                reject_unknown(&dispatch_counters, &mut ups_tx, dest, id, sender_id);
            }
        }
    }
    assert_eq!(pending.queued(), ids.len() * sender_ids.len());
}

/// Each key's shard of `ups::Pending` is the recv_dispatch worker the recv router hands the
/// key's packets to, for a power-of-two or any other number of workers, so the lock of each
/// shard is taken by one recv_dispatch worker and by `ups_send` alone.
#[test]
fn each_key_shards_by_the_recv_dispatch_worker_its_packets_route_to() {
    for workers in 1..=8 {
        let registry = Registry::default();
        let pending = ups::Pending::new(
            workers,
            QUEUE_LIMIT,
            registry.register("test.ups.pending_suppressed"),
            registry.register("test.ups.pending_dropped"),
        );
        for i in 0..=u8::MAX {
            for sender_id in 0..4 {
                let key = ups::Key {
                    credential_id: Id::from([i; 16]),
                    sender_id: VarInt::from_u8(sender_id),
                };
                let shard = pending.shard(&key);
                assert_eq!(shard, routed_worker::<routing::ModuloRoute>(workers, key));
                if workers.is_power_of_two() {
                    assert_eq!(
                        shard,
                        routed_worker::<routing::PowerOfTwoRoute>(workers, key)
                    );
                }
            }
        }
    }
}

/// A peer can send each packet with a credential id and a sender id of its choice, which the
/// endpoint can't authenticate, so one response for each key doesn't bound the queue. The queue
/// keeps at most its limit of UnknownPathSecret responses for all keys together, and drops each
/// one past it, and it takes each ReplayDetected response before and after the UnknownPathSecret
/// responses fill that limit.
#[test]
fn rejections_for_many_credentials_and_sender_ids_keep_the_queue_within_its_limit() {
    //= rendered/0002-path-secrets/09-control-packets.md#DC-0002-076
    //= type=test
    //# An endpoint MUST keep the unknown path secret packets waiting to be sent within a
    //# limit the endpoint configures for all path secrets and senders together, and drop
    //# each answer past that limit.

    //= rendered/0002-path-secrets/09-control-packets.md#DC-0002-077
    //= type=test
    //# An endpoint MUST NOT drop a stale key or replay detected packet waiting to be sent
    //# because the unknown path secret packets waiting beside it have reached DC-0002-076's
    //# limit.
    const LIMIT: usize = 16;

    let registry = Registry::default();
    let pending = new_pending_with_limit(&registry, LIMIT);
    let dispatch_counters = counters::Dispatch::new(&registry);
    let (tx, _rx) = unsync::new::<ups::Response>();
    let mut ups_tx = pending.sender(tx);
    let dest = SocketAddr::from(([127, 0, 0, 1], 4433));

    // Rejections on a key the receiver has already seen, before any UnknownPathSecret response
    // waits in the queue.
    let replay_id = Id::from(CREDENTIAL_BYTES);
    for sender_id in 4..8 {
        let sender_id = VarInt::from_u8(sender_id);
        reject(&dispatch_counters, &mut ups_tx, dest, replay_id, sender_id);
    }
    assert_eq!(
        pending.waiting(),
        0,
        "the ReplayDetected responses don't take places in the queue"
    );

    let mut shards = HashSet::new();
    for i in 0..=u8::MAX {
        for sender_id in 0..4 {
            let id = Id::from([i; 16]);
            let sender_id = VarInt::from_u8(sender_id);
            let key = ups::Key {
                credential_id: id,
                sender_id,
            };
            shards.insert(pending.shard(&key));
            reject_unknown(&dispatch_counters, &mut ups_tx, dest, id, sender_id);
            assert!(
                pending.waiting() <= LIMIT,
                "the queue never holds more than its limit of UnknownPathSecret responses"
            );
        }
    }
    assert!(
        shards.len() > 1,
        "the credentials' keys spread over more than one shard"
    );
    assert_eq!(
        pending.waiting(),
        LIMIT,
        "the queue holds its limit of UnknownPathSecret responses"
    );
    assert_eq!(
        pending.queued(),
        LIMIT + 4,
        "only the responses in the queue hold their keys, the ReplayDetected ones among them"
    );

    // Rejections on a key the receiver has already seen find the queue full of
    // UnknownPathSecret responses.
    for sender_id in 8..12 {
        let sender_id = VarInt::from_u8(sender_id);
        reject(&dispatch_counters, &mut ups_tx, dest, replay_id, sender_id);
    }
    assert_eq!(
        pending.queued(),
        LIMIT + 8,
        "the full queue takes each ReplayDetected response"
    );
    assert_eq!(
        pending.waiting(),
        LIMIT,
        "the ReplayDetected responses don't take places in the full queue"
    );

    // An UnknownPathSecret response still finds the queue full.
    let id = Id::from([0xCD; 16]);
    let sender_id = VarInt::from_u8(12);
    reject_unknown(&dispatch_counters, &mut ups_tx, dest, id, sender_id);
    assert_eq!(
        pending.queued(),
        LIMIT + 8,
        "the full queue drops the UnknownPathSecret response"
    );
}

/// A response the queue refuses, as once its receiver is gone, gives its place in the queue
/// back, so the place doesn't keep a later response out of a full queue.
#[test]
fn a_refused_response_gives_its_place_in_the_queue_back() {
    let registry = Registry::default();
    let pending = new_pending_with_limit(&registry, 1);
    let dest = SocketAddr::from(([127, 0, 0, 1], 4433));

    let (tx, rx) = unsync::new::<ups::Response>();
    let mut refused = pending.sender(tx);
    drop(rx);
    let id = Id::from([1; 16]);
    assert!(refused.send(make_response(dest, id)).is_err());

    let (tx, _rx) = unsync::new::<ups::Response>();
    let mut ups_tx = pending.sender(tx);
    let id = Id::from([2; 16]);
    assert!(ups_tx.send(make_response(dest, id)).is_ok());
    assert_eq!(
        pending.waiting(),
        1,
        "the queue takes the later response into the place the refused one gave back"
    );
}

/// Once `ups_send` takes the responses off a full queue, the queue takes new responses up to its
/// limit again, and `ups_send` sends the responses the queue took, and only those, the
/// ReplayDetected response the full queue took among them.
#[test]
fn a_full_queue_takes_new_responses_once_ups_send_drains_it() {
    const LIMIT: u8 = 4;
    const CREDENTIALS: u8 = 16;

    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let sender_id = VarInt::from_u8(3);
        let registry = Registry::default();
        let pending = new_pending_with_limit(&registry, usize::from(LIMIT));
        let (tx, rx) = unsync::new::<ups::Response>();
        spawn_pipeline(pending.receiver(rx));
        let received = spawn_server();

        async move {
            let dest = server_addr().await;
            let dispatch_counters = counters::Dispatch::new(&registry);
            let mut ups_tx = pending.sender(tx);

            for i in 0..CREDENTIALS {
                let id = Id::from([i; 16]);
                reject_unknown(&dispatch_counters, &mut ups_tx, dest, id, sender_id);
            }
            assert_eq!(
                pending.waiting(),
                usize::from(LIMIT),
                "the queue holds its limit of responses"
            );
            // A rejection on a key the receiver has already seen finds the queue full of
            // UnknownPathSecret responses, and the queue takes its ReplayDetected response.
            let replay_id = Id::from(CREDENTIAL_BYTES);
            reject(&dispatch_counters, &mut ups_tx, dest, replay_id, sender_id);
            assert_eq!(pending.queued(), usize::from(LIMIT) + 1);

            1.s().sleep().await;
            assert_eq!(
                pending.waiting(),
                0,
                "ups_send took the responses off the queue"
            );
            let expected: HashSet<_> = (0..LIMIT)
                .map(|i| (Id::from([i; 16]), None))
                .chain([(replay_id, Some(sender_id))])
                .collect();
            assert_eq!(received.borrow().len(), usize::from(LIMIT) + 1);
            assert_eq!(
                received.borrow().iter().copied().collect::<HashSet<_>>(),
                expected,
                "ups_send sends the responses the queue took, and only those"
            );

            for i in CREDENTIALS..2 * CREDENTIALS {
                let id = Id::from([i; 16]);
                reject_unknown(&dispatch_counters, &mut ups_tx, dest, id, sender_id);
            }
            assert_eq!(
                pending.waiting(),
                usize::from(LIMIT),
                "the drained queue takes new responses up to its limit"
            );

            1.s().sleep().await;
            let expected: HashSet<_> = (0..LIMIT)
                .chain(CREDENTIALS..CREDENTIALS + LIMIT)
                .map(|i| (Id::from([i; 16]), None))
                .chain([(replay_id, Some(sender_id))])
                .collect();
            assert_eq!(received.borrow().len(), 2 * usize::from(LIMIT) + 1);
            assert_eq!(
                received.borrow().iter().copied().collect::<HashSet<_>>(),
                expected
            );
        }
        .primary()
        .spawn();
    });
}

/// The default queue limit is the responses one burst of `ups_rate` sends, each counted at the
/// largest control packet, and at least one response.
#[test]
fn the_default_queue_limit_is_the_responses_one_burst_of_the_rate_sends() {
    use crate::{endpoint::default_ups_queue_limit, socket::rate::Rate};

    // The default burst of 65535 bytes holds 1023 control packets of 64 bytes, at any rate.
    assert_eq!(default_ups_queue_limit(Rate::new(0.001)), 1023);
    assert_eq!(default_ups_queue_limit(Rate::new(100.0)), 1023);
    assert_eq!(
        default_ups_queue_limit(Rate::new(0.001).with_burst_bytes(640)),
        10
    );
    assert_eq!(
        default_ups_queue_limit(Rate::new(0.001).with_burst_bytes(0)),
        1,
        "a rate without a burst still queues one response"
    );
}
