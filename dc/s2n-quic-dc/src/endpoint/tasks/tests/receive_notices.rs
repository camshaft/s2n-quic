// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Contract tests for the notices the receive path sends a packet's sender.
//!
//! Each test runs the receive worker's tasks over bach UDP sockets: `socket_recv` hands each
//! segment to the endpoint's `FanOutRouter`, `packet_dispatch` authenticates each packet
//! against the receiver's path secret map, and `ups_send` sends each notice that dispatch
//! queues. The sender sends its packets from a data socket and puts the port of a second,
//! control socket in each packet, so a notice that reaches the control socket shows that the
//! receiver sent it to the packet's control port.
//!
//! 1. A packet whose credential the receiver's map doesn't hold gets an UnknownPathSecret
//!    notice at the sender's control port.
//! 2. The first packet under a key the receiver has seen already gets a ReplayDetected notice.
//! 3. The first packet under a fresh key is accepted, and doesn't get a notice.
//! 4. A packet that fails authentication doesn't get a notice, and doesn't mark its key as
//!    seen, so the next packet under that key is accepted.

use super::helpers::{TestReceiverExt as _, WakeNowSender};
use crate::{
    counter::Registry,
    credentials::Credentials,
    endpoint::{
        frame::{self, Header},
        id::RecvDispatchWorkerId,
        recv,
        routing,
        tasks,
        ups,
        worker::FanOutRouter,
    },
    intrusive::EntryAdapter,
    packet::{
        datagram::{self, QueuePair},
        secret_control,
    },
    path::secret::{map::Entry as PathSecretEntry, Map},
    socket::{
        channel::{intrusive::unsync, ReceiverExt as _},
        pool::{descriptor, Pool, SyncReusePool},
        rate::Rate,
        recv::router::Router,
    },
    testing::{ext::*, sim},
    time::bach::Clock,
};
use bach::net::UdpSocket;
use core::time::Duration;
use s2n_codec::{DecoderBufferMut, Encoder as _, EncoderBuffer};
use s2n_quic_core::{endpoint, varint::VarInt};
use std::{cell::RefCell, net::SocketAddr, rc::Rc, sync::Arc};

/// The address the receiver binds for packets.
const RECEIVER: &str = "receiver:4433";
/// The port the sender's data socket sends each packet from.
const SENDER_DATA_PORT: u16 = 5000;
/// The control port the sender puts in each packet, which its control socket binds.
const SENDER_CONTROL_PORT: u16 = 6000;
/// The sender id each packet carries.
const SENDER_ID: u8 = 7;
/// The key the receiver has seen before the replay test's packet arrives.
const REPLAYED_KEY: u8 = 5;
/// The key of the fresh-key test's packet, which the receiver hasn't seen.
const FRESH_KEY: u8 = 7;
/// The key of the authentication-failure test's packets.
const TAMPERED_KEY: u8 = 9;

/// A peer whose path secret the receiver's map holds.
fn known_peer() -> SocketAddr {
    "127.0.0.1:8080".parse().unwrap()
}

/// A peer whose path secret the receiver's map doesn't hold. A test whose earlier packets
/// mustn't get a notice sends a packet under this peer's credential last, so the first
/// notice the sender receives shows whether an earlier packet got one.
fn unknown_peer() -> SocketAddr {
    "127.0.0.1:9090".parse().unwrap()
}

/// The sender's entry for `peer`'s path secret.
fn client_entry(peer: SocketAddr) -> Arc<PathSecretEntry> {
    PathSecretEntry::builder(peer)
        .endpoint_type(endpoint::Type::Client)
        .build(None)
}

/// Seals one QueueMsg datagram under `peer`'s path secret and `key_id`, with the sender's
/// `SENDER_ID` and `SENDER_CONTROL_PORT`, and returns its bytes and credentials.
fn sealed_packet(peer: SocketAddr, key_id: u8) -> (Vec<u8>, Credentials) {
    let entry = client_entry(peer);
    let key_id = VarInt::from_u8(key_id);
    let sealer = entry.secret().application_sealer(key_id);
    let credentials = Credentials {
        id: entry.secret().peer_id(),
        key_id,
    };

    let payload = b"receive-notice-payload".to_vec();
    let payload_len = VarInt::new(payload.len() as u64).unwrap();
    let header = Header::QueueMsg {
        queue_pair: QueuePair {
            source_queue_id: VarInt::from_u8(7),
            dest_queue_id: VarInt::ZERO,
        },
        binding_id: VarInt::from_u8(1),
        msg_id: VarInt::ZERO,
        stream_offset: VarInt::ZERO,
        largest_offset: payload_len,
        message_size: payload_len,
        chunk_size: payload_len,
        chunk_index: VarInt::ZERO,
        is_fin: true,
        is_wakeup: true,
        blocked: false,
        dest_acceptor_id: None,
        priority: crate::credit::Priority::default(),
    };

    let mut header_buf = vec![0u8; header.metadata_len(payload.len())];
    {
        let mut enc = EncoderBuffer::new(&mut header_buf);
        enc.encode(&header);
        enc.encode(&payload_len);
    }

    let mut buf = vec![0u8; 65536];
    let mut header_reader = &header_buf[..];
    let mut payload_reader = &payload[..];
    let len = datagram::encoder::encode(
        EncoderBuffer::new(&mut buf),
        SENDER_CONTROL_PORT,
        datagram::RoutingInfo::SenderId {
            source_sender_id: VarInt::from_u8(SENDER_ID),
        },
        Some(VarInt::ZERO),
        VarInt::try_from(header_buf.len() as u64).unwrap(),
        &mut header_reader,
        payload_len,
        &mut payload_reader,
        &sealer,
        &credentials,
    );
    assert!(len > 0);
    buf.truncate(len);
    (buf, credentials)
}

/// Hands each segment to the endpoint's router, and closes once `remaining` segments came
/// in, which ends `socket_recv` and drops the router's senders.
struct ExpectSegments<R> {
    inner: R,
    remaining: usize,
}

impl<R: Router> Router for ExpectSegments<R> {
    fn is_open(&self) -> bool {
        self.remaining > 0
    }

    fn tag_len(&self) -> usize {
        self.inner.tag_len()
    }

    fn on_segment(&mut self, segment: descriptor::Filled) {
        self.remaining = self.remaining.saturating_sub(1);
        self.inner.on_segment(segment);
    }
}

/// The receiver's outputs that each test checks once its pipeline ends.
struct Outputs {
    /// Each recv context that dispatch created for an accepted packet.
    idle_wheel_rx: unsync::Receiver<recv::IdleWheelAdapter>,
    /// Each secret control packet the router took off the data socket.
    invalidation_rx: unsync::Receiver<EntryAdapter<descriptor::Filled>>,
}

impl Outputs {
    /// Checks that dispatch accepted `accepted` packets, each with a recv context of its
    /// own, and that the router didn't take a secret control packet off the data socket.
    async fn assert(mut self, accepted: usize) {
        for i in 0..accepted {
            assert!(
                self.idle_wheel_rx.recv().await.is_some(),
                "dispatch must create a recv context for accepted packet {i}"
            );
        }
        assert!(
            self.idle_wheel_rx.recv().await.is_none(),
            "dispatch must not create a recv context for a rejected packet"
        );
        assert!(
            self.invalidation_rx.recv().await.is_none(),
            "the receiver's data socket must not get a secret control packet"
        );
    }
}

/// Runs the receive worker's tasks on the receiver's socket until `segments` segments came
/// in, then sends each notice that dispatch queued, and returns the outputs to check.
async fn run_receiver(map: Map, segments: usize) -> Outputs {
    let registry = Registry::default();
    let socket = UdpSocket::bind("0.0.0.0:4433").await.unwrap();

    let (packet_tx, packet_rx) = unsync::new::<datagram::decoder::Packet<descriptor::Filled>>();
    let (invalidation_tx, invalidation_rx) = unsync::new::<descriptor::Filled>();
    let txs = [(RecvDispatchWorkerId::new(0), packet_tx)]
        .into_iter()
        .collect();
    let router = FanOutRouter::<_, routing::ModuloRoute, _>::new(txs, invalidation_tx, &registry);
    let router = ExpectSegments {
        inner: router,
        remaining: segments,
    };
    let recycle_pool = SyncReusePool::new().into_local();
    let rx = tasks::socket_recv(socket, Pool::new(1200), recycle_pool.handle(), router);
    rx.drain_budgeted(Some(32)).await;

    let (ack_burst_tx, _ack_burst_rx) = unsync::new_with_adapter::<recv::AckBurstAdapter>();
    let (idle_wheel_tx, idle_wheel_rx) = unsync::new_with_adapter::<recv::IdleWheelAdapter>();
    let (frame_tx, _frame_rx) = frame::submission_channel(1);
    let (ack_sender, _ack_rx) = unsync::new::<crate::endpoint::msg::Sender>();
    let (freed_batch_tx, _freed_batch_rx) = crate::queue::freed_batch_channel();
    let (ups_tx, ups_rx) = unsync::new::<ups::Response>();
    let cache = recv::Cache::new(RecvDispatchWorkerId::new(0));
    let rx = tasks::packet_dispatch(
        packet_rx,
        Rc::new(RefCell::new(cache)),
        ack_burst_tx,
        idle_wheel_tx,
        map,
        crate::acceptor::Registry::<crate::stream::Stream>::new(),
        frame_tx,
        ack_sender,
        freed_batch_tx,
        crate::endpoint::counters::Dispatch::new(&registry),
        Clock::default(),
        <routing::ModuloRoute as routing::SenderRoute>::new(1),
        WakeNowSender,
        ups_tx,
        crate::time::DefaultClock::default(),
        Arc::new(crate::stream::metrics::ReaderMetrics::new(
            &registry,
            "stream.reader",
        )),
        Arc::new(crate::stream::metrics::WriterMetrics::new(
            &registry,
            "stream.writer",
        )),
        crate::sync::Arc::new(crate::credit::Pool::new(Default::default())),
        crate::sync::Arc::new(crate::credit::Pool::new(Default::default())),
    );
    rx.drain_budgeted(Some(32)).await;

    let ups_socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let rx = tasks::ups_send(
        ups_rx,
        ups_socket,
        Clock::default(),
        Rate::new(100.0),
        1024,
        Duration::from_secs(60),
        ups::Counters::new(&registry),
    );
    rx.drain_budgeted(Some(32)).await;

    Outputs {
        idle_wheel_rx,
        invalidation_rx,
    }
}

/// Sends `packets` to the receiver from the sender's data socket, and returns the first
/// notice that the sender's control socket receives.
async fn send_and_await_notice(packets: &[Vec<u8>]) -> Vec<u8> {
    let data_addr = format!("0.0.0.0:{SENDER_DATA_PORT}");
    let control_addr = format!("0.0.0.0:{SENDER_CONTROL_PORT}");
    let data = UdpSocket::bind(data_addr.as_str()).await.unwrap();
    let control = UdpSocket::bind(control_addr.as_str()).await.unwrap();
    for packet in packets {
        data.send_to(packet, RECEIVER).await.unwrap();
    }
    let mut buf = vec![0u8; 1500];
    let (len, _from) = control.recv_from(&mut buf).await.unwrap();
    buf.truncate(len);
    buf
}

/// Decodes the secret control packet that the sender received as `notice`.
fn decode_notice(notice: &mut [u8]) -> secret_control::Packet<'_> {
    match secret_control::Packet::decode(DecoderBufferMut::new(notice)) {
        Ok((packet, _)) => packet,
        Err(error) => panic!("the sender must receive a secret control packet: {error:?}"),
    }
}

/// Checks that `notice` is an UnknownPathSecret notice for `credentials` and the sender's
/// `SENDER_ID`.
fn assert_unknown_path_secret(notice: &mut [u8], credentials: &Credentials) {
    match decode_notice(notice) {
        secret_control::Packet::UnknownPathSecret(packet) => {
            assert_eq!(
                *packet.credential_id(),
                credentials.id,
                "the notice must carry the packet's credential id"
            );
            assert_eq!(
                packet.queue_id(),
                Some(VarInt::from_u8(SENDER_ID)),
                "the notice must carry the packet's sender id"
            );
        }
        other => panic!("the sender must receive an UnknownPathSecret notice, not {other:?}"),
    }
}

//= rendered/0005-endpoint/02-receive.md#DC-0005-011
//= type=test
//# When a receiver doesn't hold the path secret that a packet's credential names, it MUST send the packet's sender an unknown path secret notice at the sender's control port.
/// A packet whose credential the receiver's map doesn't hold gets an UnknownPathSecret notice
/// at the control port that the packet carries, which isn't the port it was sent from.
#[test]
fn unknown_path_secret_notice_goes_to_the_senders_control_port() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        async move {
            let map = crate::path::secret::map::testing::new(16);
            let (_, credentials) = sealed_packet(unknown_peer(), 0);
            assert!(
                map.get_by_id(&credentials.id).is_none(),
                "the receiver's map must not hold the packet's credential"
            );
            run_receiver(map, 1).await.assert(0).await;
        }
        .group("receiver")
        .primary()
        .spawn();

        async move {
            let (packet, credentials) = sealed_packet(unknown_peer(), 0);
            let mut notice = send_and_await_notice(&[packet]).await;
            assert_unknown_path_secret(&mut notice, &credentials);
        }
        .group("sender")
        .primary()
        .spawn();
    });
}

//= rendered/0005-endpoint/02-receive.md#DC-0005-012
//= type=test
//# When the first packet a receiver authenticates under a new key fails the replay check, the receiver MUST send the packet's sender a replay notice.
/// The first packet the receiver authenticates under a key it has seen already gets a
/// ReplayDetected notice that names the packet's credential, sender id and key.
#[test]
fn replay_notice_goes_to_the_sender_of_a_replayed_key() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        async move {
            let map = crate::path::secret::map::testing::new(16);
            map.test_insert(known_peer(), endpoint::Type::Server);
            let (_, credentials) = sealed_packet(known_peer(), REPLAYED_KEY);
            let entry = map
                .get_by_id(&credentials.id)
                .expect("the receiver's map must hold the packet's credential");
            let first = map.check_dedup(&entry, &credentials, None, &mut Vec::new());
            assert!(
                first.is_ok(),
                "the receiver must accept the key the first time it sees it"
            );
            run_receiver(map, 1).await.assert(0).await;
        }
        .group("receiver")
        .primary()
        .spawn();

        async move {
            let (packet, credentials) = sealed_packet(known_peer(), REPLAYED_KEY);
            let mut notice = send_and_await_notice(&[packet]).await;
            let secret_control::Packet::ReplayDetected(packet) = decode_notice(&mut notice) else {
                panic!("the sender must receive a ReplayDetected notice");
            };
            assert_eq!(
                *packet.credential_id(),
                credentials.id,
                "the notice must carry the packet's credential id"
            );
            assert_eq!(
                packet.sender_id(),
                Some(VarInt::from_u8(SENDER_ID)),
                "the notice must carry the packet's sender id"
            );
            let opener = client_entry(known_peer()).control_opener();
            let notice = packet
                .authenticate(&opener)
                .expect("the notice must authenticate under the path secret's control key");
            assert_eq!(
                notice.rejected_key_id,
                VarInt::from_u8(REPLAYED_KEY),
                "the notice must name the replayed key"
            );
        }
        .group("sender")
        .primary()
        .spawn();
    });
}

/// The first packet under a key the receiver hasn't seen is accepted without a notice, and
/// marks the key as seen.
#[test]
fn fresh_key_is_accepted_without_a_notice() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        async move {
            let map = crate::path::secret::map::testing::new(16);
            map.test_insert(known_peer(), endpoint::Type::Server);
            run_receiver(map.clone(), 2).await.assert(1).await;

            let (_, credentials) = sealed_packet(known_peer(), FRESH_KEY);
            let entry = map
                .get_by_id(&credentials.id)
                .expect("the receiver's map must hold the packet's credential");
            let again = map.check_dedup(&entry, &credentials, None, &mut Vec::new());
            assert!(
                again.is_err(),
                "the accepted packet must mark its key as seen"
            );
        }
        .group("receiver")
        .primary()
        .spawn();

        async move {
            let (fresh, _) = sealed_packet(known_peer(), FRESH_KEY);
            let (marker, marker_credentials) = sealed_packet(unknown_peer(), 0);
            let mut notice = send_and_await_notice(&[fresh, marker]).await;
            assert_unknown_path_secret(&mut notice, &marker_credentials);
        }
        .group("sender")
        .primary()
        .spawn();
    });
}

/// A packet that fails authentication isn't treated as a replay: it doesn't get a notice and
/// doesn't mark its key as seen, so the next packet under that key is accepted.
#[test]
fn authentication_failure_is_not_a_replay() {
    let _guard = crate::testing::without_snapshots();
    sim(|| {
        async move {
            let map = crate::path::secret::map::testing::new(16);
            map.test_insert(known_peer(), endpoint::Type::Server);
            run_receiver(map, 3).await.assert(1).await;
        }
        .group("receiver")
        .primary()
        .spawn();

        async move {
            let (mut tampered, _) = sealed_packet(known_peer(), TAMPERED_KEY);
            *tampered.last_mut().unwrap() ^= 0xFF;
            let (valid, _) = sealed_packet(known_peer(), TAMPERED_KEY);
            let (marker, marker_credentials) = sealed_packet(unknown_peer(), 0);
            let mut notice = send_and_await_notice(&[tampered, valid, marker]).await;
            assert_unknown_path_secret(&mut notice, &marker_credentials);
        }
        .group("sender")
        .primary()
        .spawn();
    });
}
