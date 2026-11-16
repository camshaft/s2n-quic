// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests of the acknowledgement requirements of the endpoint specification, in
//! `rendered/0005-endpoint/04-acknowledgement.md`.
//!
//! The receiver's tests seal packets as a peer's sender seals them and pass them through
//! `dispatch::process`, then read the receive context it keeps for that sender. The sender's tests
//! drive `assemble`, `process_ack` and `detect_loss` on a send context.

use super::*;
use crate::{
    acceptor,
    byte_vec::ByteVec,
    counter::Registry,
    credentials::Credentials,
    endpoint::{
        ack::state::Submission,
        assemble::assemble,
        combinator::AssemblerCounters,
        counters, decode, dispatch,
        frame::{CompletionSender, FailureReason, Header, SubmissionSender, DEFAULT_TTL},
        id::{LocalSenderId, RecvDispatchWorkerId, RemoteSenderId},
        inflight::{Packet, TransmissionInfo},
        msg,
        recv::{self, AckState},
        routing::{ModuloRoute, SenderRoute},
    },
    packet::datagram::{self, QueuePair, RoutingInfo},
    path::secret::map::Entry as PathSecretEntry,
    socket::{
        channel::ImmediateQueueStatus,
        pool::{self, descriptor::SyncRecycler},
    },
    stream::{
        metrics::{ReaderMetrics, WriterMetrics},
        Stream,
    },
    time::testing::Clock,
};
use core::cell::RefCell;
use s2n_codec::{DecoderBufferMut, Encoder as _, EncoderBuffer};
use s2n_quic_core::{endpoint, inet::ExplicitCongestionNotification as Ecn};
use s2n_quic_platform::features::{gso::MaxSegments, Gso};
use std::{net::SocketAddr, rc::Rc, sync::Arc};

/// The sender id of the peer's sender in the receiver's tests.
const SENDER: u8 = 9;

/// The length of the authentication tag of each packet.
const TAG_LEN: usize = 16;

fn peer() -> SocketAddr {
    "127.0.0.1:8080".parse().unwrap()
}

fn worker() -> RecvDispatchWorkerId {
    RecvDispatchWorkerId::new(0)
}

fn route() -> ModuloRoute {
    <ModuloRoute as SenderRoute>::new(1)
}

fn credit_pool() -> crate::sync::Arc<crate::credit::Pool> {
    crate::sync::Arc::new(crate::credit::Pool::new(crate::credit::Config::default()))
}

/// Drops each value it's sent.
struct Discard;

impl<T> UnboundedSender<T> for Discard {
    fn send(&mut self, _value: T) -> Result<(), T> {
        Ok(())
    }
}

/// Keeps each value it's sent, in the order it was sent.
struct Collect<T>(Vec<T>);

impl<T> Default for Collect<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> UnboundedSender<T> for Collect<T> {
    fn send(&mut self, value: T) -> Result<(), T> {
        self.0.push(value);
        Ok(())
    }
}

fn frames() -> Collect<Entry<Frame>> {
    Collect::default()
}

/// The receive side of a dispatch worker: a path secret map that holds the server entry for
/// `peer()`, and the receive cache in which `dispatch::process` keeps a context for each sender.
struct Receiver {
    path_secret_map: crate::path::secret::Map,
    cache: recv::Cache,
    /// Each context `dispatch::process` queues to send an acknowledgement.
    ack_burst: Collect<Rc<RefCell<recv::Context>>>,
    acceptor_registry: acceptor::LocalRegistry<Stream>,
    frame_tx: SubmissionSender,
    freed_batch_tx: crate::queue::FreedBatchTx,
    _freed_batch_rx: crate::queue::FreedBatchRx,
    counters: Arc<counters::Dispatch>,
    clock: Clock,
    route: ModuloRoute,
    stream_clock: crate::time::DefaultClock,
    reader_metrics: Arc<ReaderMetrics>,
    writer_metrics: Arc<WriterMetrics>,
    send_credit_pool: crate::sync::Arc<crate::credit::Pool>,
    recv_credit_pool: crate::sync::Arc<crate::credit::Pool>,
}

impl Receiver {
    fn new() -> Self {
        let registry = Registry::default();
        let (frame_tx, _frame_rx) = crate::endpoint::frame::submission_channel(1);
        let (freed_batch_tx, _freed_batch_rx) = crate::queue::freed_batch_channel();
        let path_secret_map = crate::path::secret::map::testing::new(16);
        path_secret_map.test_insert(peer(), endpoint::Type::Server);
        Self {
            path_secret_map,
            cache: recv::Cache::new(worker()),
            ack_burst: Collect::default(),
            acceptor_registry: acceptor::Registry::<Stream>::new().local(),
            frame_tx,
            freed_batch_tx,
            _freed_batch_rx,
            counters: counters::Dispatch::new(&registry),
            clock: Clock::new(Duration::from_secs(1)),
            route: route(),
            stream_clock: crate::time::DefaultClock::default(),
            reader_metrics: Arc::new(ReaderMetrics::new(&registry, "rx")),
            writer_metrics: Arc::new(WriterMetrics::new(&registry, "tx")),
            send_credit_pool: credit_pool(),
            recv_credit_pool: credit_pool(),
        }
    }

    /// Passes one packet from the peer's sender `source_sender_id` through `dispatch::process`:
    /// the packet has packet number `packet_number`, carries `header` alone and arrives with the
    /// `ecn` mark.
    fn receive(&mut self, source_sender_id: u8, packet_number: u8, header: &Header, ecn: Ecn) {
        let packet = sealed_packet(
            VarInt::from_u8(source_sender_id),
            VarInt::from_u8(packet_number),
            header,
            ecn,
        );
        let result = dispatch::process(
            Entry::new(packet),
            &mut self.cache,
            &mut self.ack_burst,
            &mut Discard,
            &self.path_secret_map,
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
        );
        assert!(
            result.is_ok(),
            "the receiver must accept packet {packet_number} from sender {source_sender_id}"
        );
        self.clock.advance(Duration::from_millis(1));
    }

    /// The receive context `dispatch::process` keeps for the peer's sender.
    fn context(&self) -> Rc<RefCell<recv::Context>> {
        let mut contexts = self.cache.senders.values();
        let context = contexts
            .next()
            .expect("the receiver must keep a context for the sender")
            .clone();
        assert!(
            contexts.next().is_none(),
            "the receiver must keep exactly one context for the one sender it received packets from"
        );
        context
    }
}

/// A packet that the peer's sender `source_sender_id` seals under key id 0, with packet number
/// `packet_number`, that carries `header` alone and arrives with the `ecn` mark.
fn sealed_packet(
    source_sender_id: VarInt,
    packet_number: VarInt,
    header: &Header,
    ecn: Ecn,
) -> datagram::decoder::Packet<pool::descriptor::Filled> {
    let sealer_entry = PathSecretEntry::builder(peer())
        .endpoint_type(endpoint::Type::Client)
        .build(None);
    let key_id = VarInt::ZERO;
    let sealer = sealer_entry.secret().application_sealer(key_id);
    let credentials = Credentials {
        id: sealer_entry.secret().peer_id(),
        key_id,
    };

    let mut header_buf = vec![0u8; 128];
    let header_len = {
        let mut encoder = EncoderBuffer::new(&mut header_buf);
        encoder.encode(header);
        if header.has_payload_length() {
            encoder.encode(&VarInt::ZERO);
        }
        encoder.len()
    };
    header_buf.truncate(header_len);

    let mut buf = vec![0u8; 65536];
    let mut header_reader = &header_buf[..];
    let mut payload_reader: &[u8] = &[];
    let encoded_len = datagram::encoder::encode(
        EncoderBuffer::new(&mut buf),
        443,
        RoutingInfo::SenderId { source_sender_id },
        Some(packet_number),
        VarInt::try_from(header_buf.len() as u64).unwrap(),
        &mut header_reader,
        VarInt::ZERO,
        &mut payload_reader,
        &sealer,
        &credentials,
    );
    assert!(encoded_len > 0);

    let pool = pool::Pool::new(u16::MAX);
    let unfilled = pool.alloc::<SyncRecycler>().expect("pool alloc");
    let segments = unfilled
        .fill_with(|addr, _cmsg, mut iov| {
            iov[..encoded_len].copy_from_slice(&buf[..encoded_len]);
            addr.set(peer().into());
            Ok::<_, core::convert::Infallible>(encoded_len)
        })
        .expect("fill_with");
    let mut filled = segments.take_filled();
    filled.set_ecn(ecn);

    let meta = {
        let decode_buf = DecoderBufferMut::new(&mut filled[..]);
        datagram::decoder::Meta::decode(&decode_buf, (), TAG_LEN).expect("meta decode")
    };
    meta.with_storage(filled).expect("with_storage")
}

/// An acknowledgement frame from the peer, marked ack-eliciting where `is_ack_eliciting` is set.
fn ack_frame(is_ack_eliciting: bool) -> Header {
    Header::Ack {
        dest_sender_id: VarInt::ZERO,
        ack_delay: VarInt::ZERO,
        largest_acknowledged: VarInt::ZERO,
        ack_range: VarInt::ZERO,
        ecn_counts: EcnCounts::default(),
        is_ack_eliciting,
    }
}

/// A send context for a path to `peer()` whose datagrams hold up to 1500 bytes, and the path's
/// entry.
fn send_context() -> (send::Context, Arc<PathSecretEntry>) {
    let entry = PathSecretEntry::builder(peer()).build(None);
    entry.update_max_datagram_size(1500);
    let registry = Registry::default();
    let context = send::Context::new(
        &entry,
        registry.register_queue_gauge("test.inflight"),
        registry.register_queue_gauge("test.ack"),
        registry.register_queue_gauge("test.pending"),
        LocalSenderId::from_index(0),
        &crate::time::bach::Clock::default(),
    )
    .expect("the send context must initialize");
    (context, entry)
}

/// A data frame of one byte on the path `entry`, with `completion` and `ttl` transmissions left.
fn data_frame(
    entry: &Arc<PathSecretEntry>,
    completion: Option<CompletionSender>,
    ttl: u16,
) -> Entry<Frame> {
    let mut payload = ByteVec::new();
    payload.push_back(bytes::Bytes::from_static(b"x"));
    Frame {
        header: Header::QueueData {
            queue_pair: QueuePair {
                source_queue_id: VarInt::from_u8(1),
                dest_queue_id: VarInt::from_u8(2),
            },
            binding_id: VarInt::from_u8(1),
            offset: VarInt::ZERO,
            largest_offset: VarInt::ZERO,
            is_fin: false,
            blocked: false,
            dest_acceptor_id: None,
            priority: crate::credit::Priority::default(),
        },
        payload,
        path_secret_entry: entry.clone(),
        completion,
        status: TransmissionStatus::Pending,
        ttl,
        enqueued_at: None,
        flow_credits: 0,
    }
    .into()
}

/// A packet sent at `time_sent` that carries `frames`, as the inflight map holds it.
fn inflight_packet(
    context: &mut send::Context,
    frames: Vec<Entry<Frame>>,
    time_sent: s2n_quic_core::time::Timestamp,
) -> Packet {
    let mut queue = crate::intrusive::Queue::new();
    for frame in frames {
        queue.push_back(frame);
    }
    let cc_info = context
        .cca
        .on_packet_sent(time_sent, 100, false, &context.rtt_estimator);
    Packet::new(
        queue,
        TransmissionInfo {
            cc_info,
            time_sent,
            sent_bytes: 100,
        },
    )
}

fn make_ts(millis: u64) -> s2n_quic_core::time::Timestamp {
    unsafe { s2n_quic_core::time::Timestamp::from_duration(Duration::from_millis(millis)) }
}

fn make_pn(n: u64) -> PacketNumber {
    PacketNumberSpace::Initial.new_packet_number(VarInt::new(n).unwrap())
}

/// Assembles one batch of datagrams from `context`, at the time `clock` reads.
fn assemble_once(
    context: &mut send::Context,
    clock: &Clock,
) -> Option<pool::descriptor::Segments<SyncRecycler>> {
    let registry = Registry::default();
    let gso: Gso = MaxSegments::try_from(1usize).unwrap().into();
    let pool = pool::Pool::new(u16::MAX);
    let mut header_buf = Vec::new();
    let mut cancelled = crate::intrusive::Queue::new();
    let mut ack_completions = crate::intrusive::Queue::new();
    let (mut freed_batch_tx, _freed_batch_rx) = crate::queue::freed_batch_channel();
    assemble::<SyncRecycler, _>(
        context,
        ImmediateQueueStatus::Empty,
        clock,
        LocalSenderId::new(VarInt::from_u8(1)),
        443,
        &gso,
        pool.alloc::<SyncRecycler>().expect("pool alloc failed"),
        &mut header_buf,
        &mut cancelled,
        &mut ack_completions,
        &mut freed_batch_tx,
        &AssemblerCounters::new(&registry),
        &counters::Send::new(&Registry::default(), LocalSenderId::from_index(0)),
        &crate::credit::Pool::new(crate::credit::Config::default()),
    )
}

/// Assembles one datagram from `context` and returns its packet number and the frames its
/// application header holds.
fn sent_packet(context: &mut send::Context, clock: &Clock) -> (VarInt, Vec<Header>) {
    let segments = assemble_once(context, clock).expect("the context must assemble a datagram");
    let filled = segments.take_filled();
    let mut buf = filled.payload().to_vec();
    let tag_len = crate::crypto::seal::Application::tag_len(&context.sealer);
    let decode_buf = DecoderBufferMut::new(&mut buf[..]);
    let (packet, _) = datagram::decoder::Packet::decode(decode_buf, (), tag_len)
        .expect("the assembled datagram must decode");
    let headers = decode::decode_frames(packet.application_header())
        .map(|item| item.expect("each frame header must decode").0)
        .collect();
    (packet.packet_number(), headers)
}

/// Assembles one datagram from `context` and returns its packet number and the acknowledgement
/// frame it carries.
fn sent_ack(context: &mut send::Context, clock: &Clock) -> (VarInt, Header) {
    let (packet_number, headers) = sent_packet(context, clock);
    let ack = headers
        .into_iter()
        .find(|header| matches!(header, Header::Ack { .. }))
        .expect("the datagram must carry an acknowledgement");
    (packet_number, ack)
}

/// Queues `submission` on `context`, as the receive path submits an acknowledgement.
fn push_submission(context: &mut send::Context, submission: Submission) {
    context
        .pending_acks
        .push_back(Entry::new(msg::Sender::PendingAck(submission)));
}

/// Queues an acknowledgement of packets 0 to `largest_acknowledged` from the peer on `context`.
fn push_ack(
    context: &mut send::Context,
    entry: &Arc<PathSecretEntry>,
    clock: &Clock,
    largest_acknowledged: u8,
) {
    let submission = Submission {
        largest_acknowledged: VarInt::from_u8(largest_acknowledged),
        ack_range: VarInt::ZERO,
        extra_ranges: bytes::Bytes::new(),
        ecn_counts: EcnCounts::default(),
        largest_recv_time: clock.get_time(),
        path_secret_entry: entry.clone(),
        local_sender_id: LocalSenderId::from_index(0),
        remote_sender_id: RemoteSenderId::new(VarInt::ZERO),
        recv_worker_id: worker(),
    };
    push_submission(context, submission);
}

/// What the receive path submits, and the acknowledgement frame the send path sends from it, once
/// the peer's sender `SENDER` has sent one ping in a packet for each mark of `marks`.
struct SentAcknowledgement {
    /// The sender id the receive path's submission is for.
    remote_sender_id: VarInt,
    /// The ECN counts of the receive path's submission.
    ecn_counts: EcnCounts,
    /// The acknowledgement frame the send path sends.
    ack: Header,
}

fn sent_acknowledgement(marks: &[Ecn]) -> SentAcknowledgement {
    let mut receiver = Receiver::new();
    for (packet_number, ecn) in marks.iter().enumerate() {
        receiver.receive(SENDER, packet_number as u8, &Header::Ping, *ecn);
    }
    let submission = receiver
        .context()
        .borrow_mut()
        .encode_and_flush(worker())
        .expect("the receiver must submit an acknowledgement");
    let remote_sender_id = submission.remote_sender_id.as_varint();
    let ecn_counts = submission.ecn_counts;

    let (mut context, entry) = send_context();
    let clock = Clock::new(Duration::from_secs(1));
    let submission = Submission {
        largest_recv_time: clock.get_time(),
        path_secret_entry: entry,
        ..submission
    };
    push_submission(&mut context, submission);
    let (_, ack) = sent_ack(&mut context, &clock);
    SentAcknowledgement {
        remote_sender_id,
        ecn_counts,
        ack,
    }
}

#[test]
fn a_receiver_acknowledges_each_packet_that_carries_an_ack_eliciting_frame() {
    //= rendered/0005-endpoint/04-acknowledgement.md#DC-0005-022
    //= type=test
    //# A receiver MUST acknowledge each packet it accepts that carries an ack-eliciting frame.
    for header in [Header::Ping, ack_frame(true)] {
        let mut receiver = Receiver::new();
        receiver.receive(SENDER, 0, &header, Ecn::NotEct);

        let context = receiver.context();
        assert_eq!(
            context.borrow().ack_state,
            AckState::Scheduled,
            "a packet that carries {header:?} must schedule an acknowledgement"
        );
        assert_eq!(
            receiver.ack_burst.0.len(),
            1,
            "the receiver must queue the sender's context to send the acknowledgement"
        );
        let submission = context
            .borrow_mut()
            .encode_and_flush(worker())
            .expect("the scheduled acknowledgement must be encoded");
        assert_eq!(
            submission.largest_acknowledged,
            VarInt::ZERO,
            "the acknowledgement must cover the packet that carries {header:?}"
        );
        assert_eq!(submission.ack_range, VarInt::ZERO);
    }
}

#[test]
fn a_receiver_doesnt_acknowledge_a_packet_without_an_ack_eliciting_frame() {
    //= rendered/0005-endpoint/04-acknowledgement.md#DC-0005-023
    //= type=test
    //# A receiver MUST NOT send an acknowledgement in response to a packet that doesn't carry an ack-eliciting frame.
    let mut receiver = Receiver::new();
    receiver.receive(SENDER, 0, &ack_frame(false), Ecn::NotEct);

    let context = receiver.context();
    assert!(
        context.borrow().ack_ranges.largest_recv_time().is_some(),
        "the receiver must record the packet it accepted"
    );
    assert_eq!(
        context.borrow().ack_state,
        AckState::Idle,
        "a packet whose only frame isn't ack-eliciting must leave the acknowledgement unscheduled"
    );
    assert!(
        receiver.ack_burst.0.is_empty(),
        "the receiver must leave the sender's context out of the acknowledgements to send"
    );
    assert!(
        context.borrow_mut().encode_and_flush(worker()).is_none(),
        "the receiver must not submit an acknowledgement for the packet"
    );
}

#[test]
fn a_receiver_has_at_most_one_acknowledgement_waiting_to_be_sent_for_each_sender() {
    //= rendered/0005-endpoint/04-acknowledgement.md#DC-0005-024
    //= type=test
    //# A receiver MUST have at most one acknowledgement for each sender waiting to be sent at any time.
    let mut receiver = Receiver::new();
    receiver.receive(SENDER, 0, &Header::Ping, Ecn::NotEct);
    receiver.receive(SENDER, 1, &Header::Ping, Ecn::NotEct);

    // Each packet may queue the sender's context to send its acknowledgement, and the context
    // submits one acknowledgement however many times it is queued.
    let mut submissions = Vec::new();
    for context in &receiver.ack_burst.0 {
        submissions.extend(context.borrow_mut().encode_and_flush(worker()));
    }
    assert_eq!(
        submissions.len(),
        1,
        "two ack-eliciting packets must leave one acknowledgement waiting to be sent"
    );
    assert_eq!(submissions[0].largest_acknowledged, VarInt::from_u8(1));
    assert_eq!(submissions[0].ack_range, VarInt::ZERO);

    // A packet that arrives while that acknowledgement is waiting to be sent doesn't add another.
    receiver.receive(SENDER, 2, &Header::Ping, Ecn::NotEct);
    let context = receiver.context();
    assert_eq!(context.borrow().ack_state, AckState::FlushedStale);
    assert!(
        context.borrow_mut().encode_and_flush(worker()).is_none(),
        "the receiver must not submit a second acknowledgement while one is waiting to be sent"
    );
}

#[test]
fn a_receiver_sends_a_further_acknowledgement_once_the_waiting_one_is_sent() {
    //= rendered/0005-endpoint/04-acknowledgement.md#DC-0005-025
    //= type=test
    //# When an ack-eliciting packet arrives while an acknowledgement for its sender is waiting to be sent, the receiver MUST send a further acknowledgement that covers it after the waiting one is sent.
    let mut receiver = Receiver::new();
    receiver.receive(SENDER, 0, &Header::Ping, Ecn::NotEct);
    let context = receiver.context();
    let waiting = context
        .borrow_mut()
        .encode_and_flush(worker())
        .expect("the first packet must schedule an acknowledgement");
    assert_eq!(waiting.largest_acknowledged, VarInt::ZERO);

    receiver.receive(SENDER, 1, &Header::Ping, Ecn::NotEct);
    assert_eq!(context.borrow().ack_state, AckState::FlushedStale);

    // The send path reports that the waiting acknowledgement was sent.
    let (further, _) = context.borrow_mut().on_ack_completion(worker());
    let further = further.expect("the receiver must submit a further acknowledgement");
    assert_eq!(
        further.largest_acknowledged,
        VarInt::from_u8(1),
        "the further acknowledgement must cover the packet that arrived while the first one waited"
    );
    assert_eq!(context.borrow().ack_state, AckState::Flushed);
}

#[test]
fn an_acknowledgement_carries_the_sender_identifier_of_the_packets_it_acknowledges() {
    //= rendered/0005-endpoint/04-acknowledgement.md#DC-0005-026
    //= type=test
    //# An acknowledgement MUST carry the sender identifier of the packets it acknowledges.
    let sent = sent_acknowledgement(&[Ecn::NotEct]);
    assert_eq!(
        sent.remote_sender_id,
        VarInt::from_u8(SENDER),
        "the receive path must submit the acknowledgement for the sender of the packets"
    );
    let Header::Ack { dest_sender_id, .. } = sent.ack else {
        panic!("the send path must send an acknowledgement frame");
    };
    assert_eq!(
        dest_sender_id,
        VarInt::from_u8(SENDER),
        "the acknowledgement frame must carry the sender id of the packets it acknowledges"
    );
}

#[test]
fn an_acknowledgement_carries_the_ecn_counts_of_the_packets_received_from_its_sender() {
    //= rendered/0005-endpoint/04-acknowledgement.md#DC-0005-027
    //= type=test
    //# An acknowledgement MUST carry the counts of explicit congestion notification marks on the packets received from its sender.
    let marks = [Ecn::Ce, Ecn::Ect0, Ecn::NotEct];
    let mut expected = EcnCounts::default();
    for ecn in marks {
        expected.increment(ecn);
    }
    assert_ne!(expected, EcnCounts::default());

    let sent = sent_acknowledgement(&marks);
    assert_eq!(
        sent.ecn_counts, expected,
        "the receive path must submit the counts of the marks on the packets from the sender"
    );
    let Header::Ack { ecn_counts, .. } = sent.ack else {
        panic!("the send path must send an acknowledgement frame");
    };
    assert_eq!(
        ecn_counts, expected,
        "the acknowledgement frame must carry the counts of the marks on the packets"
    );
}

#[test]
fn on_an_acknowledgement_a_sender_reports_each_frame_of_each_newly_acknowledged_packet() {
    //= rendered/0005-endpoint/04-acknowledgement.md#DC-0005-028
    //= type=test
    //# On an acknowledgement, a sender MUST report each frame of each newly acknowledged packet to its writer as acknowledged.
    let (mut context, entry) = send_context();
    let acknowledged = vec![
        data_frame(&entry, None, DEFAULT_TTL),
        data_frame(&entry, None, DEFAULT_TTL),
    ];
    let packet = inflight_packet(&mut context, acknowledged, make_ts(100));
    context.inflight.insert(make_pn(1), packet);
    let unacknowledged = vec![data_frame(&entry, None, DEFAULT_TTL)];
    let packet = inflight_packet(&mut context, unacknowledged, make_ts(100));
    context.inflight.insert(make_pn(2), packet);
    context.next_packet_number = VarInt::from_u8(3);

    let send_counters = counters::Send::new(&Registry::default(), LocalSenderId::from_index(0));
    let mut completed = frames();
    let mut lost = frames();
    let mut cancelled = frames();
    process_ack(
        VarInt::from_u8(1),
        VarInt::from_u8(1),
        &[],
        EcnCounts::default(),
        Duration::ZERO,
        &mut context,
        &send_counters,
        &mut completed,
        &mut lost,
        &mut cancelled,
        &make_ts(110),
        &mut crate::xorshift::Rng::with_seed(1),
        &mut Vec::new(),
    );

    assert_eq!(
        completed.0.len(),
        2,
        "the sender must report each frame of the acknowledged packet to its writer"
    );
    for frame in &completed.0 {
        assert_eq!(
            frame.status,
            TransmissionStatus::Acknowledged,
            "the sender must report the frame as acknowledged"
        );
    }
    assert!(lost.0.is_empty());
    assert!(cancelled.0.is_empty());
    assert!(
        context.inflight.remove(make_pn(2)).is_some(),
        "the packet the acknowledgement doesn't cover must stay unacknowledged"
    );
}

#[test]
fn a_sender_queues_the_frames_of_a_lost_packet_for_retransmission_except_cancelled_ones() {
    //= rendered/0005-endpoint/04-acknowledgement.md#DC-0005-029
    //= type=test
    //# When a packet is declared lost, the sender MUST queue each of its frames for retransmission, except a frame that has been cancelled or has reached its bound on transmissions.
    let (mut context, entry) = send_context();
    let completion = crate::endpoint::frame::completion_channel();
    let cancelled_frame = data_frame(&entry, Some(completion.sender()), DEFAULT_TTL);
    completion.cancel();
    let lost_frames = vec![data_frame(&entry, None, DEFAULT_TTL), cancelled_frame];
    // Packet 2, sent 4ms after packet 1, declares packet 1 lost by time.
    let packet = inflight_packet(&mut context, lost_frames, make_ts(100));
    context.inflight.insert(make_pn(1), packet);
    let later_frames = vec![data_frame(&entry, None, DEFAULT_TTL)];
    let packet = inflight_packet(&mut context, later_frames, make_ts(104));
    context.inflight.insert(make_pn(2), packet);
    context.next_packet_number = VarInt::from_u8(3);

    let send_counters = counters::Send::new(&Registry::default(), LocalSenderId::from_index(0));
    let mut completed = frames();
    let mut lost = frames();
    let mut cancelled = frames();
    detect_loss(
        &mut context,
        VarInt::from_u8(2),
        make_ts(104),
        &send_counters,
        &mut completed,
        &mut lost,
        &mut cancelled,
        make_ts(110),
        &mut crate::xorshift::Rng::with_seed(1),
    );

    assert_eq!(
        lost.0.len(),
        1,
        "the sender must queue the frame that is still transmittable for retransmission"
    );
    assert!(lost.0[0].completion.is_none());
    assert_eq!(
        lost.0[0].ttl,
        DEFAULT_TTL - 1,
        "the retransmission must count against the frame's bound on transmissions"
    );
    assert_eq!(
        cancelled.0.len(),
        1,
        "the sender must leave the cancelled frame out of the retransmissions"
    );
    assert_eq!(
        cancelled.0[0].status,
        TransmissionStatus::Failed(FailureReason::Cancelled)
    );
    assert!(completed.0.is_empty());
    assert!(context.inflight.remove(make_pn(2)).is_some());
}

/// A frame reaches its bound on transmissions when its `ttl` is 0. The sender then reports the
/// frame to its writer as failed instead of queueing it for retransmission. A debug build treats
/// that case as a broken invariant and panics, since a frame's `ttl` of `DEFAULT_TTL` isn't
/// expected to run out.
#[test]
#[cfg_attr(
    debug_assertions,
    should_panic(expected = "frame TTL should never be exhausted")
)]
fn a_sender_doesnt_queue_a_frame_at_its_bound_on_transmissions_for_retransmission() {
    //= rendered/0005-endpoint/04-acknowledgement.md#DC-0005-029
    //= type=test
    //# When a packet is declared lost, the sender MUST queue each of its frames for retransmission, except a frame that has been cancelled or has reached its bound on transmissions.
    let (mut context, entry) = send_context();
    let lost_frames = vec![data_frame(&entry, None, 0)];
    let packet = inflight_packet(&mut context, lost_frames, make_ts(100));
    context.inflight.insert(make_pn(1), packet);
    let later_frames = vec![data_frame(&entry, None, DEFAULT_TTL)];
    let packet = inflight_packet(&mut context, later_frames, make_ts(104));
    context.inflight.insert(make_pn(2), packet);
    context.next_packet_number = VarInt::from_u8(3);

    let send_counters = counters::Send::new(&Registry::default(), LocalSenderId::from_index(0));
    let mut completed = frames();
    let mut lost = frames();
    let mut cancelled = frames();
    detect_loss(
        &mut context,
        VarInt::from_u8(2),
        make_ts(104),
        &send_counters,
        &mut completed,
        &mut lost,
        &mut cancelled,
        make_ts(110),
        &mut crate::xorshift::Rng::with_seed(1),
    );

    assert!(
        lost.0.is_empty(),
        "the sender must leave the frame at its bound out of the retransmissions"
    );
    assert!(cancelled.0.is_empty());
    assert_eq!(completed.0.len(), 1);
    assert_eq!(
        completed.0[0].status,
        TransmissionStatus::Failed(FailureReason::TransmissionError)
    );
}

#[test]
fn with_no_data_awaiting_acknowledgement_a_sender_marks_one_acknowledgement_ack_eliciting() {
    //= rendered/0005-endpoint/04-acknowledgement.md#DC-0005-030
    //= type=test
    //# When a sender has no packet that carries data awaiting acknowledgement, it MAY mark one acknowledgement it sends as ack-eliciting to measure the round-trip time.
    let (mut context, entry) = send_context();
    let clock = Clock::new(Duration::from_secs(1));
    // The send of a packet that carries data leaves the tracker in this state, so that it measures
    // the round-trip time again once that data is no longer awaiting acknowledgement.
    context.rtt_tracker.clear();
    assert!(!context.inflight.has_inflight());

    push_ack(&mut context, &entry, &clock, 0);
    let (_, ack) = sent_ack(&mut context, &clock);
    assert!(
        ack.is_ack_eliciting(),
        "with no data awaiting acknowledgement, the sender must mark the acknowledgement"
    );
    assert!(
        context.rtt_tracker.is_pending(),
        "the sender must wait for the marked acknowledgement's sample"
    );

    push_ack(&mut context, &entry, &clock, 1);
    let (_, ack) = sent_ack(&mut context, &clock);
    assert!(
        !ack.is_ack_eliciting(),
        "the sender must mark one acknowledgement while it waits for that one's sample"
    );
}

#[test]
fn with_data_awaiting_acknowledgement_a_sender_doesnt_mark_an_acknowledgement_ack_eliciting() {
    //= rendered/0005-endpoint/04-acknowledgement.md#DC-0005-030
    //= type=test
    //# When a sender has no packet that carries data awaiting acknowledgement, it MAY mark one acknowledgement it sends as ack-eliciting to measure the round-trip time.
    let (mut context, entry) = send_context();
    let clock = Clock::new(Duration::from_secs(1));
    context.push_back_frame(data_frame(&entry, None, DEFAULT_TTL));
    let (_, headers) = sent_packet(&mut context, &clock);
    assert!(
        headers
            .iter()
            .any(|header| matches!(header, Header::QueueData { .. })),
        "the first datagram must carry the data frame"
    );
    assert!(context.inflight.has_inflight());

    push_ack(&mut context, &entry, &clock, 0);
    let (_, ack) = sent_ack(&mut context, &clock);
    assert!(
        !ack.is_ack_eliciting(),
        "with data awaiting acknowledgement, the sender must leave the acknowledgement unmarked"
    );
}

#[test]
fn after_a_marked_acknowledgement_yields_a_sample_a_sender_sends_data_before_marking_another() {
    //= rendered/0005-endpoint/04-acknowledgement.md#DC-0005-031
    //= type=test
    //# After an acknowledgement that a sender marked ack-eliciting to measure the round-trip time yields a sample, the sender MUST NOT mark another acknowledgement as ack-eliciting until it has sent a packet that carries data.
    let (mut context, entry) = send_context();
    let clock = Clock::new(Duration::from_secs(1));
    context.rtt_tracker.clear();

    push_ack(&mut context, &entry, &clock, 0);
    let (probe, ack) = sent_ack(&mut context, &clock);
    assert!(ack.is_ack_eliciting());

    // The peer acknowledges the marked acknowledgement 10ms after the sender sent it.
    clock.advance(Duration::from_millis(10));
    let send_counters = counters::Send::new(&Registry::default(), LocalSenderId::from_index(0));
    let mut completed = frames();
    let mut lost = frames();
    let mut cancelled = frames();
    process_ack(
        probe,
        probe,
        &[],
        EcnCounts::default(),
        Duration::ZERO,
        &mut context,
        &send_counters,
        &mut completed,
        &mut lost,
        &mut cancelled,
        &clock,
        &mut crate::xorshift::Rng::with_seed(1),
        &mut Vec::new(),
    );
    assert_eq!(
        context.rtt_estimator.latest_rtt(),
        Duration::from_millis(10),
        "the acknowledgement of the marked acknowledgement must yield a sample"
    );

    push_ack(&mut context, &entry, &clock, 1);
    let (_, ack) = sent_ack(&mut context, &clock);
    assert!(
        !ack.is_ack_eliciting(),
        "after the sample, the sender must not mark another acknowledgement before it sends data"
    );

    context.push_back_frame(data_frame(&entry, None, DEFAULT_TTL));
    let _ = sent_packet(&mut context, &clock);
    assert!(
        !context.rtt_tracker.is_pending(),
        "once the sender sends data, it may mark an acknowledgement again"
    );
}
