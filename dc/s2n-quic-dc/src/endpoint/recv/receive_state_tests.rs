// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests of the receive state that `dispatch::process` keeps in the receive [`Cache`]: packet
//! number and acknowledgement state for each pair of peer credential and sender id, and no state
//! for a key until a packet under that key passes both authentication and the replay check.

use super::{AckState, Cache, Context, Key};
use crate::{
    acceptor,
    credentials::Credentials,
    endpoint::{
        counters,
        dispatch::{self, Error},
        frame::{Header, SubmissionReceiver, SubmissionSender},
        id::{RecvDispatchWorkerId, RemoteSenderId},
        routing,
    },
    intrusive::Entry,
    packet::datagram::{self, QueuePair},
    path::secret::{map::Entry as PathSecretEntry, Map as PathSecretMap},
    socket::{
        channel,
        pool::{self, descriptor::SyncRecycler},
    },
    stream::Stream,
};
use core::time::Duration;
use s2n_codec::{DecoderBufferMut, Encoder as _};
use s2n_quic_core::{endpoint, packet::number::PacketNumberSpace, varint::VarInt};
use std::{cell::RefCell, net::SocketAddr, rc::Rc, sync::Arc};

const TAG_LEN: usize = 16;

type Packet = datagram::decoder::Packet<pool::descriptor::Filled>;

/// One sender of a peer: the peer's address, which sets the path secret and so the credential its
/// packets carry, the key id it seals under, and its sender id.
#[derive(Clone, Copy, Debug)]
struct Source {
    peer: SocketAddr,
    key_id: VarInt,
    sender_id: VarInt,
}

impl Source {
    fn new(port: u16, key_id: u8, sender_id: u8) -> Self {
        Self {
            peer: SocketAddr::from(([127, 0, 0, 1], port)),
            key_id: VarInt::from_u8(key_id),
            sender_id: VarInt::from_u8(sender_id),
        }
    }

    /// The same sender, sealing under `key_id`.
    fn with_key(self, key_id: u8) -> Self {
        Self {
            key_id: VarInt::from_u8(key_id),
            ..self
        }
    }

    /// The client entry for `peer`. Its credential id is the id of the server entry for `peer`
    /// that [`Receiver::new`] puts in the receiver's path secret map.
    fn client_entry(&self) -> Arc<PathSecretEntry> {
        PathSecretEntry::builder(self.peer)
            .endpoint_type(endpoint::Type::Client)
            .build(None)
    }

    fn credentials(&self) -> Credentials {
        Credentials {
            id: self.client_entry().secret().peer_id(),
            key_id: self.key_id,
        }
    }

    /// The key the receive cache holds this sender's state under.
    fn key(&self) -> Key {
        Key {
            id: self.credentials().id,
            remote_sender_id: RemoteSenderId::new(self.sender_id),
        }
    }

    /// A packet that authenticates, numbered `packet_number`.
    fn packet(&self, packet_number: u8) -> Packet {
        sealed_packet(self, packet_number, false)
    }

    /// A packet numbered `packet_number` whose authentication tag doesn't match its contents.
    fn tampered_packet(&self, packet_number: u8) -> Packet {
        sealed_packet(self, packet_number, true)
    }
}

/// A single QueueMsg frame, routed at an unallocated queue, sealed under the key id of `source` by
/// its client entry, with its sender id in the routing info and `packet_number` as the packet
/// number. With `tamper`, the last byte of the packet, which belongs to its authentication tag, is
/// flipped.
fn sealed_packet(source: &Source, packet_number: u8, tamper: bool) -> Packet {
    let peer = source.peer;
    let source_sender_id = source.sender_id;
    let sealer_entry = source.client_entry();
    let sealer = sealer_entry.secret().application_sealer(source.key_id);
    let credentials = source.credentials();

    let payload = b"queue-msg-payload".to_vec();
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
        let mut enc = s2n_codec::EncoderBuffer::new(&mut header_buf);
        enc.encode(&header);
        enc.encode(&payload_len);
    }

    let mut buf = vec![0u8; 65536];
    let mut header_reader = &header_buf[..];
    let mut payload_reader = &payload[..];
    let encoded_len = datagram::encoder::encode(
        s2n_codec::EncoderBuffer::new(&mut buf),
        443,
        datagram::RoutingInfo::SenderId { source_sender_id },
        Some(VarInt::from_u8(packet_number)),
        VarInt::try_from(header_buf.len() as u64).unwrap(),
        &mut header_reader,
        payload_len,
        &mut payload_reader,
        &sealer,
        &credentials,
    );
    assert!(encoded_len > 0);
    if tamper {
        buf[encoded_len - 1] ^= 0xff;
    }

    let pool = pool::Pool::new(u16::MAX);
    let unfilled = pool.alloc::<SyncRecycler>().expect("pool alloc");
    let segments = unfilled
        .fill_with(|addr, _cmsg, mut iov| {
            iov[..encoded_len].copy_from_slice(&buf[..encoded_len]);
            addr.set(peer.into());
            Ok::<_, core::convert::Infallible>(encoded_len)
        })
        .expect("fill_with");
    let mut filled = segments.take_filled();

    let meta = {
        let decode_buf = DecoderBufferMut::new(&mut filled[..]);
        datagram::decoder::Meta::decode(&decode_buf, (), TAG_LEN).expect("meta decode")
    };
    meta.with_storage(filled).expect("with_storage")
}

/// Drops each value it's sent: the tests read the receive state from the receive cache.
struct Discard;

impl<T> channel::UnboundedSender<T> for Discard {
    fn send(&mut self, _value: T) -> Result<(), T> {
        Ok(())
    }
}

/// The variant of `result`, for an assertion's message: `Error` doesn't implement `Debug`.
fn outcome(result: &Result<(), Error>) -> &'static str {
    match result {
        Ok(()) => "Ok",
        Err(Error::PeerStateLookup { .. }) => "PeerStateLookup",
        Err(Error::Decryption { .. }) => "Decryption",
        Err(Error::Duplicate { .. }) => "Duplicate",
        Err(Error::StaleKey { .. }) => "StaleKey",
        Err(Error::MissingSenderId) => "MissingSenderId",
    }
}

/// A receiver whose receive cache lasts across the packets of a test, with a path secret map that
/// holds the server entry of each peer it's made for.
struct Receiver {
    map: PathSecretMap,
    cache: Cache,
    acceptor_registry: acceptor::LocalRegistry<Stream>,
    frame_tx: SubmissionSender,
    freed_batch_tx: crate::queue::FreedBatchTx,
    counters: Arc<counters::Dispatch>,
    clock: crate::time::testing::Clock,
    route: routing::ModuloRoute,
    stream_clock: crate::time::DefaultClock,
    reader_metrics: Arc<crate::stream::metrics::ReaderMetrics>,
    writer_metrics: Arc<crate::stream::metrics::WriterMetrics>,
    send_credit_pool: crate::sync::Arc<crate::credit::Pool>,
    recv_credit_pool: crate::sync::Arc<crate::credit::Pool>,
    _frame_rx: SubmissionReceiver,
    _freed_batch_rx: crate::queue::FreedBatchRx,
}

impl Receiver {
    fn new(peers: &[SocketAddr]) -> Self {
        let map = crate::path::secret::map::testing::new(16);
        for peer in peers {
            map.test_insert(*peer, endpoint::Type::Server);
        }
        let registry = crate::counter::Registry::default();
        let (frame_tx, _frame_rx) = crate::endpoint::frame::submission_channel(1);
        let (freed_batch_tx, _freed_batch_rx) = crate::queue::freed_batch_channel();
        Self {
            map,
            cache: Cache::new(RecvDispatchWorkerId::new(0)),
            acceptor_registry: acceptor::Registry::<Stream>::new().local(),
            frame_tx,
            freed_batch_tx,
            counters: counters::Dispatch::new(&registry),
            clock: crate::time::testing::Clock::new(Duration::from_secs(1)),
            route: <routing::ModuloRoute as routing::SenderRoute>::new(1),
            stream_clock: crate::time::DefaultClock::default(),
            reader_metrics: Arc::new(crate::stream::metrics::ReaderMetrics::new(&registry, "rx")),
            writer_metrics: Arc::new(crate::stream::metrics::WriterMetrics::new(&registry, "tx")),
            send_credit_pool: crate::sync::Arc::new(crate::credit::Pool::new(
                crate::credit::Config::default(),
            )),
            recv_credit_pool: crate::sync::Arc::new(crate::credit::Pool::new(
                crate::credit::Config::default(),
            )),
            _frame_rx,
            _freed_batch_rx,
        }
    }

    /// Runs `process` on `packet`, with the receive cache and path secret map of this receiver.
    fn receive(&mut self, packet: Packet) -> Result<(), Error> {
        dispatch::process(
            Entry::new(packet),
            &mut self.cache,
            &mut Discard,
            &mut Discard,
            &self.map,
            &mut self.acceptor_registry,
            &mut self.frame_tx,
            &mut Discard,
            &mut self.freed_batch_tx,
            &self.clock,
            &self.counters,
            &self.route,
            &mut Discard,
            &self.stream_clock,
            &self.reader_metrics,
            &self.writer_metrics,
            &self.send_credit_pool,
            &self.recv_credit_pool,
        )
    }

    /// Delivers packet `packet_number` of `source`, which the receiver must accept.
    #[track_caller]
    fn accept(&mut self, source: &Source, packet_number: u8) {
        let result = self.receive(source.packet(packet_number));
        let got = outcome(&result);
        assert!(
            result.is_ok(),
            "packet {packet_number} of {source:?} must be accepted, not {got}"
        );
    }

    /// Delivers another copy of packet `packet_number` of `source`, which the receiver must reject
    /// as a duplicate.
    #[track_caller]
    fn reject_duplicate(&mut self, source: &Source, packet_number: u8) {
        let result = self.receive(source.packet(packet_number));
        let got = outcome(&result);
        let Err(Error::Duplicate {
            credentials,
            packet_number: rejected,
        }) = result
        else {
            panic!("packet {packet_number} of {source:?} again must be a Duplicate, not {got}");
        };
        assert_eq!(credentials, source.credentials());
        assert_eq!(rejected, VarInt::from_u8(packet_number));
    }

    /// Records the key id of `source` in the receiver's replay window, as an earlier packet under
    /// that key would have.
    #[track_caller]
    fn mark_key_seen(&self, source: &Source) {
        let map = &self.map;
        let credentials = source.credentials();
        let entry = map
            .get_by_id(&credentials.id)
            .expect("the map must hold the server entry for the credential");
        let first = map.check_dedup(&entry, &credentials, None, &mut Vec::new());
        assert!(
            first.is_ok(),
            "the replay window must accept a key id the first time it sees it"
        );
    }

    /// The receive state the cache holds for the credential and sender id of `source`.
    #[track_caller]
    fn context(&self, source: &Source) -> Rc<RefCell<Context>> {
        let key = source.key();
        let Some(context) = self.cache.senders.get(&key) else {
            panic!("the receiver must hold receive state for {source:?}");
        };
        Rc::clone(context)
    }

    #[track_caller]
    fn snapshot(&self, source: &Source) -> Snapshot {
        Snapshot::of(&self.context(source).borrow())
    }
}

/// The packet numbers below 16 that `context` holds as accepted, which its acknowledgements
/// report.
fn acked(context: &Context) -> Vec<u64> {
    let packets = context.ack_ranges.packets_for_test();
    let mut acked = Vec::new();
    for number in 0u8..16 {
        let packet_number = PacketNumberSpace::Initial.new_packet_number(VarInt::from_u8(number));
        if packets.contains(&packet_number) {
            acked.push(u64::from(number));
        }
    }
    acked
}

/// What a test compares of a receive context: the key id it holds, the packet numbers it holds as
/// accepted, and its acknowledgement state.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    key_id: VarInt,
    acked: Vec<u64>,
    ack_state: AckState,
}

impl Snapshot {
    fn new(key_id: u8, acked: &[u64], ack_state: AckState) -> Self {
        Self {
            key_id: VarInt::from_u8(key_id),
            acked: acked.to_vec(),
            ack_state,
        }
    }

    fn of(context: &Context) -> Self {
        Self {
            key_id: context.current_key_id,
            acked: acked(context),
            ack_state: context.ack_state,
        }
    }
}

/// Checks that `a` and `b`, two senders that differ in credential or in sender id, each have packet
/// number and acknowledgement state of their own.
#[track_caller]
fn assert_separate_state(rx: &mut Receiver, a: &Source, b: &Source) {
    // The same packet number from each sender is a packet of its own, not a copy of the other's.
    rx.accept(a, 0);
    rx.accept(b, 0);
    assert_eq!(rx.cache.senders.len(), 2);
    let context_a = rx.context(a);
    let context_b = rx.context(b);
    assert!(
        !Rc::ptr_eq(&context_a, &context_b),
        "each sender must have receive state of its own"
    );

    // The packets of `a` don't enter the packet number state of `b`.
    rx.accept(a, 1);
    rx.accept(a, 2);
    assert_eq!(acked(&context_a.borrow()), [0, 1, 2]);
    assert_eq!(acked(&context_b.borrow()), [0]);

    // `b` accepts a packet number that `a` accepted already, and each rejects a copy of its own.
    rx.accept(b, 2);
    rx.reject_duplicate(a, 2);
    rx.reject_duplicate(b, 2);
    rx.reject_duplicate(a, 1);
    assert_eq!(acked(&context_a.borrow()), [0, 1, 2]);
    assert_eq!(acked(&context_b.borrow()), [0, 2]);

    // Sending the acknowledgement of `a` leaves the acknowledgement of `b` pending.
    assert_eq!(context_a.borrow().ack_state, AckState::Scheduled);
    assert_eq!(context_b.borrow().ack_state, AckState::Scheduled);
    let submission = context_a
        .borrow_mut()
        .encode_and_flush(RecvDispatchWorkerId::new(0));
    assert!(
        submission.is_some(),
        "a context with pending acknowledgements must encode them"
    );
    assert_eq!(context_a.borrow().ack_state, AckState::Flushed);
    assert_eq!(context_b.borrow().ack_state, AckState::Scheduled);

    // A packet of `b` leaves the acknowledgement state of `a` as it was, and a packet of `a` makes
    // the sent acknowledgement of `a`, whose completion hasn't returned, stale.
    rx.accept(b, 3);
    assert_eq!(context_a.borrow().ack_state, AckState::Flushed);
    assert_eq!(context_b.borrow().ack_state, AckState::Scheduled);
    rx.accept(a, 3);
    assert_eq!(context_a.borrow().ack_state, AckState::FlushedStale);
    assert_eq!(context_b.borrow().ack_state, AckState::Scheduled);
    assert_eq!(acked(&context_a.borrow()), [0, 1, 2, 3]);
    assert_eq!(acked(&context_b.borrow()), [0, 2, 3]);

    // Each packet went to the receive state made for the first packet of its sender.
    assert_eq!(rx.cache.senders.len(), 2);
    assert!(Rc::ptr_eq(&context_a, &rx.context(a)));
    assert!(Rc::ptr_eq(&context_b, &rx.context(b)));
}

/// Checks that the receiver holds the receive state it held before a rejected packet: the same
/// context, for the same key, with the same accepted packets and acknowledgement state.
#[track_caller]
fn assert_state_unchanged(
    rx: &Receiver,
    source: &Source,
    context: &Rc<RefCell<Context>>,
    before: &Snapshot,
) {
    assert_eq!(rx.cache.senders.len(), 1);
    assert!(
        Rc::ptr_eq(context, &rx.context(source)),
        "the receiver must keep the receive state of the key it holds"
    );
    assert_eq!(&rx.snapshot(source), before);
}

//= rendered/0005-endpoint/02-receive.md#DC-0005-007
//= type=test
//# A receiver MUST keep separate packet number and acknowledgement state for each pair of peer credential and sender identifier.
#[test]
fn separate_state_for_two_sender_ids_under_one_credential() {
    // The replay window takes each key id of a credential once, so the second sender seals under
    // a key id of its own.
    let a = Source::new(8080, 0, 3);
    let b = Source::new(8080, 1, 9);
    assert_eq!(a.credentials().id, b.credentials().id);
    assert_ne!(a.sender_id, b.sender_id);

    let mut rx = Receiver::new(&[a.peer]);
    assert_separate_state(&mut rx, &a, &b);
}

//= rendered/0005-endpoint/02-receive.md#DC-0005-007
//= type=test
//# A receiver MUST keep separate packet number and acknowledgement state for each pair of peer credential and sender identifier.
#[test]
fn separate_state_for_two_credentials_with_one_sender_id() {
    let a = Source::new(8080, 0, 3);
    let b = Source::new(8081, 0, 3);
    assert_ne!(a.credentials().id, b.credentials().id);
    assert_eq!(a.sender_id, b.sender_id);

    let mut rx = Receiver::new(&[a.peer, b.peer]);
    assert_separate_state(&mut rx, &a, &b);
}

//= rendered/0005-endpoint/02-receive.md#DC-0005-013
//= type=test
//# A receiver MUST NOT keep receive state for a new key until a packet under that key has passed both authentication and the replay check.
#[test]
fn no_state_for_a_first_key_whose_packet_fails_authentication() {
    let source = Source::new(8080, 0, 3);
    let mut rx = Receiver::new(&[source.peer]);

    let result = rx.receive(source.tampered_packet(0));
    let got = outcome(&result);
    let Err(Error::Decryption {
        credentials,
        packet_number,
    }) = result
    else {
        panic!("a packet that fails authentication must be a Decryption error, not {got}");
    };
    assert_eq!(credentials, source.credentials());
    assert_eq!(packet_number, VarInt::ZERO);
    assert!(
        rx.cache.senders.is_empty(),
        "the receiver must not keep receive state for a key whose packet failed authentication"
    );

    // The rejected packet didn't take the key's place in the replay window: a packet that
    // authenticates under the same key is accepted, into receive state that holds it alone.
    rx.accept(&source, 0);
    assert_eq!(rx.cache.senders.len(), 1);
    assert_eq!(
        rx.snapshot(&source),
        Snapshot::new(0, &[0], AckState::Scheduled)
    );
}

//= rendered/0005-endpoint/02-receive.md#DC-0005-013
//= type=test
//# A receiver MUST NOT keep receive state for a new key until a packet under that key has passed both authentication and the replay check.
#[test]
fn no_state_for_a_first_key_whose_packet_fails_the_replay_check() {
    let source = Source::new(8080, 0, 3);
    let mut rx = Receiver::new(&[source.peer]);
    rx.mark_key_seen(&source);

    let result = rx.receive(source.packet(0));
    let got = outcome(&result);
    let Err(Error::StaleKey {
        credentials,
        sender_id,
        packet_number,
        ..
    }) = result
    else {
        panic!("a packet under a key the receiver has seen must be a StaleKey error, not {got}");
    };
    assert_eq!(credentials, source.credentials());
    assert_eq!(sender_id, source.sender_id);
    assert_eq!(packet_number, VarInt::ZERO);
    assert!(
        rx.cache.senders.is_empty(),
        "the receiver must not keep receive state for a key whose packet failed the replay check"
    );

    // A packet under a key the replay window hasn't seen is accepted, from the same credential and
    // sender id.
    let next = source.with_key(1);
    rx.accept(&next, 0);
    assert_eq!(rx.cache.senders.len(), 1);
    assert_eq!(
        rx.snapshot(&next),
        Snapshot::new(1, &[0], AckState::Scheduled)
    );
}

//= rendered/0005-endpoint/02-receive.md#DC-0005-013
//= type=test
//# A receiver MUST NOT keep receive state for a new key until a packet under that key has passed both authentication and the replay check.
#[test]
fn no_state_for_a_newer_key_whose_packet_fails_authentication() {
    let old = Source::new(8080, 0, 3);
    let newer = old.with_key(1);
    let mut rx = Receiver::new(&[old.peer]);
    rx.accept(&old, 0);
    rx.accept(&old, 1);
    let context = rx.context(&old);
    let before = rx.snapshot(&old);
    assert_eq!(before, Snapshot::new(0, &[0, 1], AckState::Scheduled));

    let result = rx.receive(newer.tampered_packet(2));
    let got = outcome(&result);
    let Err(Error::Decryption {
        credentials,
        packet_number,
    }) = result
    else {
        panic!("a packet that fails authentication must be a Decryption error, not {got}");
    };
    assert_eq!(credentials, newer.credentials());
    assert_eq!(packet_number, VarInt::from_u8(2));
    assert_state_unchanged(&rx, &old, &context, &before);

    // The receiver goes on with the state of the old key: it rejects a copy of a packet it
    // accepted, and accepts a packet it hasn't.
    rx.reject_duplicate(&old, 1);
    rx.accept(&old, 2);
    assert_eq!(
        rx.snapshot(&old),
        Snapshot::new(0, &[0, 1, 2], AckState::Scheduled)
    );

    // The rejected packet didn't take the newer key's place in the replay window: a packet that
    // authenticates under that key replaces the state with state that holds that packet alone.
    rx.accept(&newer, 5);
    assert_eq!(rx.cache.senders.len(), 1);
    assert_eq!(
        rx.snapshot(&newer),
        Snapshot::new(1, &[5], AckState::Scheduled)
    );
}

//= rendered/0005-endpoint/02-receive.md#DC-0005-013
//= type=test
//# A receiver MUST NOT keep receive state for a new key until a packet under that key has passed both authentication and the replay check.
#[test]
fn no_state_for_a_newer_key_whose_packet_fails_the_replay_check() {
    let old = Source::new(8080, 0, 3);
    let replayed = old.with_key(1);
    let newer = old.with_key(2);
    let mut rx = Receiver::new(&[old.peer]);
    rx.accept(&old, 0);
    rx.accept(&old, 1);
    let context = rx.context(&old);
    let before = rx.snapshot(&old);
    assert_eq!(before, Snapshot::new(0, &[0, 1], AckState::Scheduled));
    rx.mark_key_seen(&replayed);

    let result = rx.receive(replayed.packet(2));
    let got = outcome(&result);
    let Err(Error::StaleKey {
        credentials,
        sender_id,
        packet_number,
        ..
    }) = result
    else {
        panic!("a packet under a key the receiver has seen must be a StaleKey error, not {got}");
    };
    assert_eq!(credentials, replayed.credentials());
    assert_eq!(sender_id, replayed.sender_id);
    assert_eq!(packet_number, VarInt::from_u8(2));
    assert_state_unchanged(&rx, &old, &context, &before);

    // The receiver goes on with the state of the old key: it rejects a copy of a packet it
    // accepted, and accepts a packet it hasn't.
    rx.reject_duplicate(&old, 1);
    rx.accept(&old, 2);
    assert_eq!(
        rx.snapshot(&old),
        Snapshot::new(0, &[0, 1, 2], AckState::Scheduled)
    );

    // A packet that passes both checks under a key the replay window hasn't seen replaces the
    // state with state that holds that packet alone.
    rx.accept(&newer, 5);
    assert_eq!(rx.cache.senders.len(), 1);
    assert_eq!(
        rx.snapshot(&newer),
        Snapshot::new(2, &[5], AckState::Scheduled)
    );
}
