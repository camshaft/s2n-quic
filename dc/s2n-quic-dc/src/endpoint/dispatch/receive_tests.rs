// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Conformance tests of the receive requirements in spec/0005-endpoint/02-receive.toml: how
//! `process` treats a packet that fails authentication, a packet number it has seen, the frames
//! of an accepted packet, a frame whose declared length runs past the packet's payload, and the
//! activity of the packet's peer.
//!
//! Each test seals its packets with the client entry of a path secret, and runs `process` with
//! the server entry of that path secret in its map, so `process` authenticates each packet as it
//! does a peer's packet. Each packet holds QueueData frames for a stream the test binds before it
//! sends the first packet, so each frame `process` dispatches shows as an entry of that stream.
//! `process` dispatches QueueData frames in its slow path: its fast path takes only a packet of a
//! single QueueMsg frame (`decode::detect_single_queue_msg`).

use super::*;
use crate::{
    packet::datagram::{self, QueuePair},
    path::secret::map::{entry::QueueState, Entry as PathSecretEntry},
    queue::{ControlReceiver, StreamReceiver},
    socket::pool::{self, descriptor::SyncRecycler},
};
use s2n_codec::{DecoderBufferMut, Encoder as _};
use s2n_quic_core::{endpoint, packet::number::PacketNumberSpace};
use std::net::SocketAddr;

const TAG_LEN: usize = 16;

/// The binding id of the stream each test binds at queue 0.
const BINDING_ID: u8 = 1;

/// The sender id each packet carries in its routing info.
const SENDER_ID: u8 = 3;

type SealedPacket = datagram::decoder::Packet<descriptor::Filled>;

/// One QueueData frame of a packet: its stream offset, the payload length its metadata declares,
/// and the payload bytes the packet carries for it.
struct DataFrame {
    offset: u64,
    declared_len: usize,
    payload: &'static [u8],
}

/// A QueueData frame whose metadata declares the length of the payload it carries.
fn data(offset: u64, payload: &'static [u8]) -> DataFrame {
    DataFrame {
        offset,
        declared_len: payload.len(),
        payload,
    }
}

impl DataFrame {
    /// The frame's header, for the stream at queue 0 with `BINDING_ID`.
    fn header(&self) -> Header {
        let largest_offset = self.offset + self.declared_len as u64;
        Header::QueueData {
            queue_pair: QueuePair {
                source_queue_id: VarInt::from_u8(7),
                dest_queue_id: VarInt::ZERO,
            },
            binding_id: VarInt::from_u8(BINDING_ID),
            offset: VarInt::new(self.offset).unwrap(),
            largest_offset: VarInt::new(largest_offset).unwrap(),
            is_fin: false,
            blocked: false,
            dest_acceptor_id: None,
            priority: crate::credit::Priority::default(),
        }
    }
}

/// An entry of the stream, as `Receiver::delivered` lists it.
fn delivery(offset: u64, payload: &[u8]) -> (u64, Vec<u8>) {
    (offset, payload.to_vec())
}

/// The sending peer: the client entry of the path secret, which seals each packet under key id 0.
struct Peer {
    address: SocketAddr,
    entry: Arc<PathSecretEntry>,
}

impl Peer {
    fn new(address: SocketAddr) -> Self {
        let entry = PathSecretEntry::builder(address)
            .endpoint_type(endpoint::Type::Client)
            .build(None);
        Self { address, entry }
    }

    /// The credentials of each packet the peer seals. Their id is the id of the server entry for
    /// the peer's address.
    fn credentials(&self) -> Credentials {
        Credentials {
            id: self.entry.secret().peer_id(),
            key_id: VarInt::ZERO,
        }
    }

    /// A packet numbered `packet_number` that holds `frames`, sealed by the peer.
    fn seal(&self, packet_number: u64, frames: &[DataFrame]) -> SealedPacket {
        let credentials = self.credentials();
        let sealer = self.entry.secret().application_sealer(credentials.key_id);

        let headers: Vec<Header> = frames.iter().map(DataFrame::header).collect();
        let header_len: usize = frames
            .iter()
            .zip(&headers)
            .map(|(frame, header)| header.metadata_len(frame.declared_len))
            .sum();
        let mut header_buf = vec![0u8; header_len];
        {
            let mut encoder = s2n_codec::EncoderBuffer::new(&mut header_buf);
            for (frame, header) in frames.iter().zip(&headers) {
                encoder.encode(header);
                encoder.encode(&VarInt::new(frame.declared_len as u64).unwrap());
            }
        }
        let payload: Vec<u8> = frames
            .iter()
            .flat_map(|frame| frame.payload)
            .copied()
            .collect();

        let source_sender_id = VarInt::from_u8(SENDER_ID);
        let mut buf = vec![0u8; 65536];
        let mut header_reader = &header_buf[..];
        let mut payload_reader = &payload[..];
        let encoded_len = datagram::encoder::encode(
            s2n_codec::EncoderBuffer::new(&mut buf),
            443,
            datagram::RoutingInfo::SenderId { source_sender_id },
            Some(VarInt::new(packet_number).unwrap()),
            VarInt::try_from(header_buf.len() as u64).unwrap(),
            &mut header_reader,
            VarInt::try_from(payload.len() as u64).unwrap(),
            &mut payload_reader,
            &sealer,
            &credentials,
        );
        assert!(encoded_len > 0, "the peer must seal the packet");

        let pool = pool::Pool::new(u16::MAX);
        let unfilled = pool.alloc::<SyncRecycler>().expect("pool alloc");
        let segments = unfilled
            .fill_with(|addr, _cmsg, mut iov| {
                iov[..encoded_len].copy_from_slice(&buf[..encoded_len]);
                addr.set(self.address.into());
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

    /// A packet as `seal` returns it, with the first byte of its sealed payload flipped, so that
    /// it fails authentication.
    fn tamper(&self, packet_number: u64, frames: &[DataFrame]) -> SealedPacket {
        let mut packet = self.seal(packet_number, frames);
        packet.payload_mut()[0] ^= 0xFF;
        packet
    }
}

/// Drops each value it's sent.
struct Discard;

impl<T> channel::UnboundedSender<T> for Discard {
    fn send(&mut self, _value: T) -> Result<(), T> {
        Ok(())
    }
}

/// Records each peer `process` schedules an ACK for. The recorder doesn't link a peer into a
/// burst queue, so `process` sends the peer here once for each ack-eliciting packet it accepts,
/// and doesn't send it here for a packet it rejects.
#[derive(Default)]
struct AckRecorder {
    scheduled: Vec<Rc<RefCell<recv::Context>>>,
}

impl channel::UnboundedSender<Rc<RefCell<recv::Context>>> for AckRecorder {
    fn send(
        &mut self,
        value: Rc<RefCell<recv::Context>>,
    ) -> Result<(), Rc<RefCell<recv::Context>>> {
        self.scheduled.push(value);
        Ok(())
    }
}

/// The receiving endpoint of a test: the path secret map that holds the server entry for the
/// peer, the receive cache, the stream bound at queue 0 with `BINDING_ID`, and the state
/// `process` needs beside them.
struct Receiver {
    path_secret_map: PathSecretMap,
    entry: Arc<PathSecretEntry>,
    recv_cache: recv::Cache,
    acks: AckRecorder,
    clock: crate::time::testing::Clock,
    stream: StreamReceiver,
    acceptor_registry: acceptor::LocalRegistry<Stream>,
    frame_tx: SubmissionSender,
    freed_batch_tx: crate::queue::FreedBatchTx,
    counters: Arc<counters::Dispatch>,
    stream_clock: crate::time::DefaultClock,
    reader_metrics: Arc<crate::stream::metrics::ReaderMetrics>,
    writer_metrics: Arc<crate::stream::metrics::WriterMetrics>,
    send_credit_pool: crate::sync::Arc<crate::credit::Pool>,
    recv_credit_pool: crate::sync::Arc<crate::credit::Pool>,
    // Dropping the stream's receivers would free its binding, so the receiver keeps them, and the
    // view the test bound the stream through, for as long as the test runs.
    _control: ControlReceiver,
    _bind_view: recv::QueueView,
    _freed_batch_rx: crate::queue::FreedBatchRx,
}

impl Receiver {
    fn new(peer: &Peer) -> Self {
        let path_secret_map = crate::path::secret::map::testing::new(16);
        path_secret_map.test_insert(peer.address, endpoint::Type::Server);
        let entry = path_secret_map
            .get_by_id(&peer.credentials().id)
            .expect("the map must hold the server entry for the peer's credentials");

        let mut bind_view = match entry.queue_state() {
            QueueState::Server(state) => recv::QueueView::Server(state.view()),
            _ => unreachable!("the map's entry for the peer must be a server entry"),
        };
        let (mut freed_batch_tx, freed_batch_rx) = crate::queue::freed_batch_channel();
        let queue_id = VarInt::ZERO;
        let binding_id = VarInt::from_u8(BINDING_ID);
        let server = bind_view.as_server_mut().expect("server view");
        let bound = server.bind_for_msg(queue_id, binding_id, &entry, &mut freed_batch_tx);
        let crate::queue::BindResult::NewBinding {
            stream, control, ..
        } = bound.expect("the slot must bind for the test setup")
        else {
            panic!("expected NewBinding for a fresh slot");
        };
        assert!(
            matches!(stream.try_recv(), Ok(None)),
            "the bound stream must be open and empty before the test sends a packet"
        );

        let registry = crate::counter::Registry::default();
        let (frame_tx, _frame_rx) = crate::endpoint::frame::submission_channel(1);
        Self {
            path_secret_map,
            entry,
            recv_cache: recv::Cache::new(crate::endpoint::id::RecvDispatchWorkerId::new(0)),
            acks: AckRecorder::default(),
            clock: crate::time::testing::Clock::new(Duration::from_secs(1)),
            stream,
            acceptor_registry: acceptor::Registry::<Stream>::new().local(),
            frame_tx,
            freed_batch_tx,
            counters: counters::Dispatch::new(&registry),
            stream_clock: crate::time::DefaultClock::default(),
            reader_metrics: Arc::new(crate::stream::metrics::ReaderMetrics::new(&registry, "rx")),
            writer_metrics: Arc::new(crate::stream::metrics::WriterMetrics::new(&registry, "tx")),
            send_credit_pool: crate::sync::Arc::new(crate::credit::Pool::new(
                crate::credit::Config::default(),
            )),
            recv_credit_pool: crate::sync::Arc::new(crate::credit::Pool::new(
                crate::credit::Config::default(),
            )),
            _control: control,
            _bind_view: bind_view,
            _freed_batch_rx: freed_batch_rx,
        }
    }

    /// Runs `process` on `packet`.
    fn receive(&mut self, packet: SealedPacket) -> Result<(), Error> {
        let route = <routing::ModuloRoute as routing::SenderRoute>::new(1);
        process(
            Entry::new(packet),
            &mut self.recv_cache,
            &mut self.acks,
            &mut Discard,
            &self.path_secret_map,
            &mut self.acceptor_registry,
            &mut self.frame_tx,
            &mut Discard,
            &mut self.freed_batch_tx,
            &self.clock,
            &self.counters,
            &route,
            &mut Discard,
            &self.stream_clock,
            &self.reader_metrics,
            &self.writer_metrics,
            &self.send_credit_pool,
            &self.recv_credit_pool,
        )
    }

    /// The receive state `process` keeps for the peer. The tests send from one peer, with one
    /// sender id, so the cache holds one context once `process` accepts a packet.
    fn context(&self) -> Rc<RefCell<recv::Context>> {
        assert_eq!(
            self.recv_cache.senders.len(),
            1,
            "the receive cache must hold one context, the peer's"
        );
        self.recv_cache.senders.values().next().unwrap().clone()
    }

    /// Whether the peer's ACK ranges hold `packet_number`.
    fn acks_packet(&self, packet_number: u64) -> bool {
        let packet_number = VarInt::new(packet_number).unwrap();
        let packet_number = PacketNumberSpace::Initial.new_packet_number(packet_number);
        let context = self.context();
        let context = context.borrow();
        let ranges = context.ack_ranges.packets_for_test();
        ranges.contains(&packet_number)
    }

    /// The offset and payload of each data entry `process` pushed to the stream since the last
    /// call, in the order the stream holds them.
    fn delivered(&self) -> Vec<(u64, Vec<u8>)> {
        let mut delivered = Vec::new();
        loop {
            match self.stream.try_recv() {
                Ok(Some(entry)) => match &*entry {
                    msg::Stream::Data {
                        offset, payload, ..
                    } => delivered.push(delivery(offset.as_u64(), payload)),
                    _ => panic!("the stream must hold only the data entries the tests send"),
                },
                Ok(None) => return delivered,
                Err(_) => panic!("the stream must stay open"),
            }
        }
    }
}

/// A peer and a receiver whose stream is bound before the peer sends its first packet.
fn setup() -> (Peer, Receiver) {
    let peer = Peer::new("127.0.0.1:8080".parse().unwrap());
    let receiver = Receiver::new(&peer);
    (peer, receiver)
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

#[track_caller]
fn assert_accepted(result: Result<(), Error>) {
    let got = outcome(&result);
    assert_eq!(got, "Ok", "process must accept the packet, not reject it as {got}");
}

#[track_caller]
fn assert_rejected_as(result: Result<(), Error>, expected: &str) {
    let got = outcome(&result);
    assert_eq!(
        got, expected,
        "process must reject the packet as {expected}, not {got}"
    );
}

/// A tampered packet that comes before any packet of its peer doesn't reach the stream, doesn't
/// make receive state for the peer, and doesn't schedule an ACK. The peer's next valid packet then
/// makes that state, reaches the stream, and is acknowledged, without the tampered packet's number.
#[test]
fn a_tampered_first_packet_is_neither_delivered_nor_acknowledged() {
    //= rendered/0005-endpoint/02-receive.md#DC-0005-008
    //= type=test
    //# A receiver MUST NOT deliver to a stream the payload of a frame whose packet fails
    //# authentication.

    //= rendered/0005-endpoint/02-receive.md#DC-0005-009
    //= type=test
    //# A receiver MUST NOT acknowledge a packet that fails authentication.
    let (peer, mut receiver) = setup();

    let tampered = peer.tamper(0, &[data(0, b"XXXXX")]);
    assert_rejected_as(receiver.receive(tampered), "Decryption");
    assert!(
        receiver.delivered().is_empty(),
        "the tampered packet's frame must not reach the stream"
    );
    assert!(
        receiver.recv_cache.senders.is_empty(),
        "the tampered packet must not make receive state for its peer"
    );
    assert!(
        receiver.acks.scheduled.is_empty(),
        "process must not schedule an ACK for the tampered packet"
    );

    assert_accepted(receiver.receive(peer.seal(1, &[data(0, b"AAAAA")])));
    assert_eq!(
        receiver.delivered(),
        [delivery(0, b"AAAAA")],
        "the peer's valid packet must reach the same stream"
    );
    assert!(
        receiver.acks_packet(1),
        "the ACK ranges must hold the valid packet"
    );
    assert!(
        !receiver.acks_packet(0),
        "the ACK ranges must not hold the tampered packet"
    );
    assert_eq!(
        receiver.acks.scheduled.len(),
        1,
        "process must schedule one ACK, for the valid packet"
    );
}

/// A tampered packet from a peer whose receive state the cache holds doesn't reach the stream,
/// doesn't enter the peer's ACK ranges, and doesn't schedule an ACK. It doesn't take its packet
/// number either: the peer's valid packet of the same number reaches the stream and is
/// acknowledged.
#[test]
fn a_tampered_packet_from_a_known_peer_is_neither_delivered_nor_acknowledged() {
    //= rendered/0005-endpoint/02-receive.md#DC-0005-008
    //= type=test
    //# A receiver MUST NOT deliver to a stream the payload of a frame whose packet fails
    //# authentication.

    //= rendered/0005-endpoint/02-receive.md#DC-0005-009
    //= type=test
    //# A receiver MUST NOT acknowledge a packet that fails authentication.
    let (peer, mut receiver) = setup();

    assert_accepted(receiver.receive(peer.seal(0, &[data(0, b"AAAAA")])));
    assert_eq!(receiver.delivered(), [delivery(0, b"AAAAA")]);
    assert_eq!(receiver.acks.scheduled.len(), 1);
    let context = receiver.context();

    let tampered = peer.tamper(1, &[data(5, b"XXXXX")]);
    assert_rejected_as(receiver.receive(tampered), "Decryption");
    assert!(
        Rc::ptr_eq(&context, &receiver.context()),
        "the tampered packet must reach the peer's cached receive state"
    );
    assert!(
        receiver.delivered().is_empty(),
        "the tampered packet's frame must not reach the stream"
    );
    assert!(
        !receiver.acks_packet(1),
        "the ACK ranges must not hold the tampered packet"
    );
    assert_eq!(
        receiver.acks.scheduled.len(),
        1,
        "process must not schedule an ACK for the tampered packet"
    );

    assert_accepted(receiver.receive(peer.seal(1, &[data(5, b"BBBBB")])));
    assert_eq!(
        receiver.delivered(),
        [delivery(5, b"BBBBB")],
        "the peer's valid packet must reach the same stream"
    );
    assert!(
        receiver.acks_packet(1),
        "the ACK ranges must hold the valid packet"
    );
    assert_eq!(
        receiver.acks.scheduled.len(),
        2,
        "process must schedule an ACK for the valid packet"
    );
}

/// A packet whose number `process` accepted already, and a packet whose number is too far below
/// the largest number it accepted for it to tell, don't reach the stream, whatever frames they
/// hold. Each packet here holds QueueData frames, which `process` dispatches in its slow path.
#[test]
fn a_repeated_or_too_old_packet_number_is_not_dispatched() {
    //= rendered/0005-endpoint/02-receive.md#DC-0005-010
    //= type=test
    //# A receiver MUST NOT dispatch the frames of a packet whose packet number it has already
    //# accepted from the same sender, or whose packet number is too old for it to tell.
    let (peer, mut receiver) = setup();

    let first = [data(0, b"AAAAA"), data(5, b"BBBBB")];
    assert_accepted(receiver.receive(peer.seal(1, &first)));
    assert_eq!(
        receiver.delivered(),
        [delivery(0, b"AAAAA"), delivery(5, b"BBBBB")]
    );

    // Packet 1 again, with other frames.
    let repeated = [data(10, b"XXXXX"), data(15, b"YYYYY")];
    assert_rejected_as(receiver.receive(peer.seal(1, &repeated)), "Duplicate");
    assert!(
        receiver.delivered().is_empty(),
        "the frames of a repeated packet number must not reach the stream"
    );

    assert_accepted(receiver.receive(peer.seal(300, &[data(10, b"CCCCC")])));
    assert_eq!(receiver.delivered(), [delivery(10, b"CCCCC")]);

    // Packet 100 is 200 below packet 300, past the 128 packet numbers the receiver's window
    // holds, so the receiver can't tell whether it accepted packet 100 before.
    let too_old = peer.seal(100, &[data(15, b"ZZZZZ")]);
    assert_rejected_as(receiver.receive(too_old), "Duplicate");
    assert!(
        receiver.delivered().is_empty(),
        "the frames of a packet number too old to tell must not reach the stream"
    );
    assert_eq!(
        receiver.acks.scheduled.len(),
        2,
        "process must schedule an ACK for each accepted packet alone"
    );
}

/// `process` pushes the frames of an accepted packet to the stream in the order the packet
/// carries them, whatever their offsets.
#[test]
fn the_frames_of_an_accepted_packet_are_dispatched_in_the_packets_order() {
    //= rendered/0005-endpoint/02-receive.md#DC-0005-014
    //= type=test
    //# A receiver MUST dispatch the frames of an accepted packet in the order the packet carries
    //# them.
    let (peer, mut receiver) = setup();

    let frames = [data(0, b"AAAAA"), data(5, b"BBBBB"), data(10, b"CCCCC")];
    assert_accepted(receiver.receive(peer.seal(0, &frames)));
    let expected = [
        delivery(0, b"AAAAA"),
        delivery(5, b"BBBBB"),
        delivery(10, b"CCCCC"),
    ];
    assert_eq!(receiver.delivered(), expected);

    // Frames whose offsets aren't in order reach the stream in the packet's order all the same.
    let frames = [data(25, b"FFFFF"), data(15, b"DDDDD"), data(20, b"EEEEE")];
    assert_accepted(receiver.receive(peer.seal(1, &frames)));
    let expected = [
        delivery(25, b"FFFFF"),
        delivery(15, b"DDDDD"),
        delivery(20, b"EEEEE"),
    ];
    assert_eq!(receiver.delivered(), expected);
}

/// `process` stops at a frame whose metadata declares more payload than the packet has left, and
/// doesn't dispatch that frame or any frame after it.
#[test]
fn dispatch_stops_at_the_first_frame_that_runs_past_the_payload() {
    //= rendered/0005-endpoint/02-receive.md#DC-0005-015
    //= type=test
    //# A receiver MUST stop dispatching a packet's frames at the first frame whose declared length
    //# runs past the end of the packet's payload.
    let (peer, mut receiver) = setup();

    // Frame B's metadata declares 1000 bytes, while the packet carries 5 bytes for it and 5 for
    // frame C: the payload holds 15 bytes in all.
    let frames = [
        data(0, b"AAAAA"),
        DataFrame {
            offset: 5,
            declared_len: 1000,
            payload: b"BBBBB",
        },
        data(10, b"CCCCC"),
    ];
    let packet = peer.seal(0, &frames);

    // The metadata decodes as three frames, so `process` stops at frame B for its length, and
    // not for metadata it can't decode.
    let declared: Vec<Option<usize>> = decode::decode_frames(packet.application_header())
        .map(|frame| frame.ok().map(|(_, len)| len))
        .collect();
    assert_eq!(
        declared,
        [Some(5), Some(1000), Some(5)],
        "the packet's metadata must decode as frames A, B and C"
    );

    assert_accepted(receiver.receive(packet));
    assert_eq!(
        receiver.delivered(),
        [delivery(0, b"AAAAA")],
        "frame A alone must reach the stream"
    );
}

/// Each packet `process` accepts from the peer, on a cache miss and on a cache hit, sets the
/// last activity of the peer's path secret entry to the receiver's time. A repeated packet and a
/// tampered packet don't.
#[test]
fn each_accepted_packet_counts_as_activity_from_its_peer() {
    //= rendered/0005-endpoint/02-receive.md#DC-0005-016
    //= type=test
    //# A receiver MUST count each packet it accepts as activity from the packet's peer.
    let (peer, mut receiver) = setup();
    let entry = receiver.entry.clone();

    receiver.clock.advance(Duration::from_secs(5));
    let now = crate::time::precision::Clock::now(&receiver.clock);
    assert_ne!(
        entry.last_activity(),
        now,
        "the peer's entry must not show activity at the receiver's time before the first packet"
    );

    // The peer's first packet: the receive cache doesn't hold its state yet.
    assert_accepted(receiver.receive(peer.seal(0, &[data(0, b"AAAAA")])));
    assert!(
        Arc::ptr_eq(&receiver.context().borrow().path_entry, &entry),
        "the peer's receive state must hold the peer's entry"
    );
    assert_eq!(
        entry.last_activity(),
        now,
        "the first packet must count as activity from the peer"
    );

    // The peer's next packet: the receive cache holds its state.
    receiver.clock.advance(Duration::from_secs(5));
    let now = crate::time::precision::Clock::now(&receiver.clock);
    assert_accepted(receiver.receive(peer.seal(1, &[data(5, b"BBBBB")])));
    assert_eq!(receiver.recv_cache.senders.len(), 1);
    assert_eq!(
        entry.last_activity(),
        now,
        "the next packet must count as activity from the peer"
    );

    // A repeated packet and a tampered packet don't count.
    receiver.clock.advance(Duration::from_secs(5));
    let repeated = peer.seal(1, &[data(10, b"CCCCC")]);
    assert_rejected_as(receiver.receive(repeated), "Duplicate");
    let tampered = peer.tamper(2, &[data(10, b"CCCCC")]);
    assert_rejected_as(receiver.receive(tampered), "Decryption");
    assert_eq!(
        entry.last_activity(),
        now,
        "a rejected packet must not count as activity from the peer"
    );
    assert_eq!(
        receiver.delivered(),
        [delivery(0, b"AAAAA"), delivery(5, b"BBBBB")]
    );
}
