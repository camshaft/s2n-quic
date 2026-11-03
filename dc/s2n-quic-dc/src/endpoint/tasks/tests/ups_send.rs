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
//! 8. Rejections of packets with many distinct credential ids that the map doesn't hold leave
//!    UnknownPathSecret responses in each shard up to its share of the bytes the pacer sends in
//!    one burst, and a ReplayDetected response still queues past them.
//! 9. Each response the sender drops counts in one of `ups::Pending`'s two counters: a response
//!    whose key waits in the queue in the first, and an UnknownPathSecret response past its
//!    shard's share of the burst in the second.

use super::helpers::test_entry;
use crate::{
    counter::{Counter, Registry},
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
    Entry::new(ups::Response {
        dest_addr: addr,
        packet: encode_ups_packet(id),
        key: ups::Key {
            credential_id: id,
            sender_id: VarInt::ZERO,
        },
        kind: ups::Kind::UnknownPathSecret,
    })
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

/// The two counters a test's `ups::Pending` counts the responses its sender drops in, which the
/// test keeps to read their counts.
struct DropCounters {
    /// The responses dropped because a response with their key waits in the queue.
    suppressed: Counter,
    /// The UnknownPathSecret responses dropped past their shard's share of the pacer's burst.
    unknown_dropped: Counter,
}

impl DropCounters {
    fn new(registry: &Registry) -> Self {
        Self {
            suppressed: registry.register("test.ups.pending_suppressed"),
            unknown_dropped: registry.register("test.ups.unknown_dropped"),
        }
    }

    /// An `ups::Pending` over `workers` shards, holding UnknownPathSecret responses up to
    /// `unknown_capacity` bytes, that counts its drops in these counters.
    fn pending(&self, workers: usize, unknown_capacity: u64) -> ups::Pending {
        ups::Pending::new(
            workers,
            self.suppressed.clone(),
            self.unknown_dropped.clone(),
            unknown_capacity,
        )
    }

    /// Asserts how many responses the sender dropped because a response with their key waits
    /// in the queue, and how many UnknownPathSecret responses it dropped past their shard's
    /// share of the pacer's burst.
    #[track_caller]
    fn assert_dropped(&self, suppressed: u64, unknown_dropped: u64) {
        assert_eq!(
            self.suppressed.count(),
            suppressed,
            "responses dropped because a response with their key waits in the queue"
        );
        assert_eq!(
            self.unknown_dropped.count(),
            unknown_dropped,
            "UnknownPathSecret responses dropped past their shard's share of the burst"
        );
    }
}

/// An `ups::Pending` over `RECV_DISPATCH_WORKERS` shards, holding UnknownPathSecret responses
/// up to `unknown_capacity()` bytes, and the counters it counts its drops in.
fn new_pending(registry: &Registry) -> (ups::Pending, DropCounters) {
    let drops = DropCounters::new(registry);
    let pending = drops.pending(RECV_DISPATCH_WORKERS, unknown_capacity());
    (pending, drops)
}

/// The bytes of UnknownPathSecret responses [`new_pending`] lets the queue hold: the burst of the
/// pacer `spawn_pipeline` runs.
fn unknown_capacity() -> u64 {
    crate::socket::rate::Rate::new(100.0).burst_bytes()
}

/// How many UnknownPathSecret responses of `len` bytes one shard of [`new_pending`] holds: its
/// share of the pacer's burst, and at least one.
fn shard_fit(len: u64) -> u64 {
    (unknown_capacity() / RECV_DISPATCH_WORKERS as u64 / len).max(1)
}

/// A credential id of its own for each `i`, which the path secret map doesn't hold.
fn unknown_id(i: u64) -> Id {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&i.to_be_bytes());
    Id::from(bytes)
}

/// The bytes of the UnknownPathSecret response to a packet with any credential id.
fn unknown_response_len() -> u64 {
    encode_ups_packet(unknown_id(0)).len() as u64
}

/// The first `count` credential ids of [`unknown_id`] whose keys, with `sender_id`, are in
/// `shard` of `pending`.
fn unknown_ids_in_shard(
    pending: &ups::Pending,
    shard: usize,
    sender_id: VarInt,
    count: usize,
) -> Vec<Id> {
    (0..)
        .map(unknown_id)
        .filter(|&credential_id| {
            let key = ups::Key {
                credential_id,
                sender_id,
            };
            pending.shard(&key) == shard
        })
        .take(count)
        .collect()
}

/// How many of the UnknownPathSecret responses to `ids` of [`unknown_id`], with sender id 0,
/// the queue takes: in each shard, the first ones up to `fit`.
fn queued_of(pending: &ups::Pending, ids: core::ops::Range<u64>, fit: u64) -> u64 {
    let mut per_shard = [0; RECV_DISPATCH_WORKERS];
    for i in ids {
        let key = ups::Key {
            credential_id: unknown_id(i),
            sender_id: VarInt::ZERO,
        };
        let queued = &mut per_shard[pending.shard(&key)];
        *queued = fit.min(*queued + 1);
    }
    per_shard.iter().sum()
}

/// Takes each response off the queue, as `ups_send` does, and counts them.
fn spawn_drain<R>(rx: R) -> Rc<RefCell<usize>>
where
    R: Receiver<Entry<ups::Response>> + 'static,
{
    let drained = Rc::new(RefCell::new(0));
    let count = drained.clone();
    async move {
        let rx = crate::socket::channel::Map::new(rx, move |_response: Entry<ups::Response>| {
            *count.borrow_mut() += 1;
        });
        rx.drain_budgeted(Some(32)).await;
    }
    .spawn();
    drained
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

/// The credential id and sender id of each ReplayDetected packet the server receives.
type Received = Rc<RefCell<Vec<(Id, Option<VarInt>)>>>;

/// Binds the server's socket, at port 4433 of group "server", and records each ReplayDetected
/// packet it receives.
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
            let secret_control::Packet::ReplayDetected(packet) = packet else {
                panic!("expected a ReplayDetected packet");
            };
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
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let id = Id::from(CREDENTIAL_BYTES);
        let sender_id = VarInt::from_u8(3);
        let registry = Registry::default();
        let (pending, drops) = new_pending(&registry);
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
            drops.assert_dropped(BURST as u64 - 1, 0);

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
            drops.assert_dropped(2 * (BURST as u64 - 1), 0);

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
        let (pending, drops) = new_pending(&registry);
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
            drops.assert_dropped(u64::from(CREDENTIALS) * (BURST as u64 - 1), 0);

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
        let (pending, drops) = new_pending(&registry);
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
            drops.assert_dropped(sender_ids.len() as u64 * (BURST as u64 - 1), 0);

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
    let (pending, drops) = new_pending(&registry);
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
        pending.queued_unknown_bytes(),
        0,
        "the refused response doesn't hold its bytes"
    );
    drops.assert_dropped(0, 0);
}

/// Rejections of packets whose credential the path secret map doesn't hold leave one
/// UnknownPathSecret response in the queue for each credential and sender id.
#[test]
fn unknown_credentials_queue_one_response_for_each_credential_and_sender_id() {
    let registry = Registry::default();
    let (pending, drops) = new_pending(&registry);
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
    drops.assert_dropped(
        (ids.len() * sender_ids.len()) as u64 * (BURST as u64 - 1),
        0,
    );
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
            registry.register("test.ups.pending_suppressed"),
            registry.register("test.ups.unknown_dropped"),
            unknown_capacity(),
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

/// A sender of many distinct credential ids that the path secret map doesn't hold chooses the
/// key of each UnknownPathSecret response, so the keys don't bound the queue. Each shard takes
/// those responses up to its share of the bytes the pacer sends in one burst, the queue drops
/// the rest, and each shard takes its share again once the background task has taken them off.
#[test]
fn a_burst_of_unknown_credentials_queues_up_to_the_pacers_burst() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let registry = Registry::default();
        let (pending, drops) = new_pending(&registry);
        let (tx, rx) = unsync::new::<ups::Response>();
        let drained = spawn_drain(pending.receiver(rx));

        async move {
            let dest = SocketAddr::from(([127, 0, 0, 1], 4433));
            let dispatch_counters = counters::Dispatch::new(&registry);
            let mut ups_tx = pending.sender(tx);
            let sender_id = VarInt::ZERO;
            let len = unknown_response_len();
            let fit = shard_fit(len);
            assert!(fit > 1, "a shard's share holds more than one response");
            let burst = 2 * fit * RECV_DISPATCH_WORKERS as u64;

            for i in 0..burst {
                let id = unknown_id(i);
                reject_unknown(&dispatch_counters, &mut ups_tx, dest, id, sender_id);
            }
            let queued = queued_of(&pending, 0..burst, fit);
            assert_eq!(
                pending.queued() as u64,
                queued,
                "each shard takes the responses that fit in its share of the burst"
            );
            assert!(queued < burst, "the queue drops the rest");
            assert_eq!(pending.queued_unknown_bytes(), queued * len);
            assert!(pending.queued_unknown_bytes() <= unknown_capacity());
            drops.assert_dropped(0, burst - queued);

            1.s().sleep().await;
            assert_eq!(
                *drained.borrow() as u64,
                queued,
                "the background task takes each queued response off the queue"
            );
            assert_eq!(pending.queued(), 0);
            assert_eq!(pending.queued_unknown_bytes(), 0);

            for i in burst..2 * burst {
                let id = unknown_id(i);
                reject_unknown(&dispatch_counters, &mut ups_tx, dest, id, sender_id);
            }
            let queued_again = queued_of(&pending, burst..2 * burst, fit);
            assert_eq!(
                pending.queued() as u64,
                queued_again,
                "once the queue is drained, each shard takes its share again"
            );
            drops.assert_dropped(0, (burst - queued) + (burst - queued_again));
        }
        .primary()
        .spawn();
    });
}

/// UnknownPathSecret responses that fill a shard's share of the pacer's burst don't keep a
/// ReplayDetected response of that shard out: the path secret map's entries bound the keys of
/// those, and the peer that holds the credential recovers its send contexts from them.
#[test]
fn unknown_credentials_past_the_burst_leave_room_for_rejections() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let registry = Registry::default();
        let (pending, drops) = new_pending(&registry);
        let (tx, rx) = unsync::new::<ups::Response>();

        async move {
            let _rx = rx;
            let dest = SocketAddr::from(([127, 0, 0, 1], 4433));
            let dispatch_counters = counters::Dispatch::new(&registry);
            let mut ups_tx = pending.sender(tx);
            let sender_id = VarInt::ZERO;
            let len = unknown_response_len();
            let fit = shard_fit(len);
            let id = Id::from(CREDENTIAL_BYTES);
            let shard = pending.shard(&ups::Key {
                credential_id: id,
                sender_id,
            });

            for unknown in unknown_ids_in_shard(&pending, shard, sender_id, 2 * fit as usize) {
                reject_unknown(&dispatch_counters, &mut ups_tx, dest, unknown, sender_id);
            }
            assert_eq!(
                pending.queued() as u64,
                fit,
                "the shard holds its share of the burst"
            );
            drops.assert_dropped(0, fit);

            reject(&dispatch_counters, &mut ups_tx, dest, id, sender_id);
            assert_eq!(
                pending.queued() as u64,
                fit + 1,
                "the ReplayDetected response queues in the full shard"
            );
            assert_eq!(
                pending.queued_unknown_bytes(),
                fit * len,
                "a ReplayDetected response doesn't count toward the burst"
            );
            drops.assert_dropped(0, fit);
        }
        .primary()
        .spawn();
    });
}

/// With none of a shard's UnknownPathSecret responses waiting, the shard takes one whatever its
/// size, so a burst smaller than one response doesn't keep each of them out, and that one
/// doesn't keep a ReplayDetected response out.
#[test]
fn a_burst_smaller_than_one_response_still_queues_one() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        let registry = Registry::default();
        let drops = DropCounters::new(&registry);
        let pending = drops.pending(RECV_DISPATCH_WORKERS, 0);
        let (tx, rx) = unsync::new::<ups::Response>();

        async move {
            let _rx = rx;
            let dest = SocketAddr::from(([127, 0, 0, 1], 4433));
            let dispatch_counters = counters::Dispatch::new(&registry);
            let mut ups_tx = pending.sender(tx);
            let sender_id = VarInt::ZERO;
            let id = Id::from(CREDENTIAL_BYTES);
            let shard = pending.shard(&ups::Key {
                credential_id: id,
                sender_id,
            });

            for unknown in unknown_ids_in_shard(&pending, shard, sender_id, BURST) {
                reject_unknown(&dispatch_counters, &mut ups_tx, dest, unknown, sender_id);
            }
            assert_eq!(pending.queued(), 1, "the shard takes one response");
            assert_eq!(pending.queued_unknown_bytes(), unknown_response_len());
            drops.assert_dropped(0, BURST as u64 - 1);

            reject(&dispatch_counters, &mut ups_tx, dest, id, sender_id);
            assert_eq!(
                pending.queued(),
                2,
                "the response past the burst doesn't keep a ReplayDetected response out"
            );
            drops.assert_dropped(0, BURST as u64 - 1);
        }
        .primary()
        .spawn();
    });
}
