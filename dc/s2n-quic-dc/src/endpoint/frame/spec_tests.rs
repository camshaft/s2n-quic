// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests of the frame requirements of the endpoint specification
//! (`rendered/0005-endpoint/01-frames.md`): how a peer's send context packs, orders,
//! retransmits and reports its frames.

use super::*;
use crate::{
    counter::Registry,
    endpoint::{
        ack, assemble,
        combinator::AssemblerCounters,
        counters, id,
        inflight::{Packet, TransmissionInfo},
        msg, send,
    },
    socket::{
        channel::ImmediateQueueStatus,
        pool::{descriptor::SyncRecycler, Pool},
    },
    time::testing::Clock,
    xorshift,
};
use bytes::Bytes;
use core::{ops::RangeInclusive, time::Duration};
use s2n_quic_core::{
    packet::number::{PacketNumber, PacketNumberSpace},
    time::Timestamp,
};
use s2n_quic_platform::features::{gso::MaxSegments, Gso};

/// The stream of the frames in the packets that a test acknowledges after an earlier packet, so
/// that the ACK declares the earlier packet lost.
const FILLER: u8 = 9;

/// The bytes each test packet counts against the congestion window.
const PACKET_BYTES: u16 = 1_200;

const CANCELLED: TransmissionStatus = TransmissionStatus::Failed(FailureReason::Cancelled);
const TRANSMISSION_ERROR: TransmissionStatus =
    TransmissionStatus::Failed(FailureReason::TransmissionError);

/// The send context of one peer, with the queues that the ACK path and the assembler deliver
/// frames to.
struct Harness {
    entry: Arc<PathSecretEntry>,
    context: send::Context,
    clock: Clock,
    counters: counters::Send,
    completed: Queue<Frame>,
    lost: Queue<Frame>,
    cancelled: Queue<Frame>,
    ack_completions: Queue<msg::Sender>,
}

impl Harness {
    fn new() -> Self {
        let peer: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let entry = PathSecretEntry::builder(peer).build(None);
        entry.update_max_datagram_size(1500);
        let registry = Registry::new();
        let context = send::Context::new(
            &entry,
            registry.register_queue_gauge("test.inflight"),
            registry.register_queue_gauge("test.ack"),
            registry.register_queue_gauge("test.pending"),
            sender_id(),
            &crate::time::bach::Clock::default(),
        )
        .expect("the send context initializes");
        Self {
            entry,
            context,
            clock: Clock::new(Duration::from_micros(1)),
            counters: counters::Send::new(&Registry::default(), sender_id()),
            completed: Queue::new(),
            lost: Queue::new(),
            cancelled: Queue::new(),
            ack_completions: Queue::new(),
        }
    }

    /// A pending frame of this peer, with the default bound on its transmissions.
    fn frame(&self, header: Header, completion: Option<CompletionSender>) -> Entry<Frame> {
        let mut payload = ByteVec::new();
        if header.has_payload_length() {
            payload.push_back(Bytes::from_static(b"hello"));
        }
        Frame {
            header,
            payload,
            path_secret_entry: self.entry.clone(),
            completion,
            status: TransmissionStatus::Pending,
            ttl: DEFAULT_TTL,
            enqueued_at: None,
            flow_credits: 0,
        }
        .into()
    }

    fn now(&self) -> Timestamp {
        self.clock.get_time().into()
    }

    /// Records the frames as sent in the packet `pn` at `time_sent`, as the assembler does.
    fn insert(&mut self, pn: u64, frames: Vec<Entry<Frame>>, time_sent: Timestamp) {
        let mut queue = Queue::new();
        for frame in frames {
            queue.push_back(frame);
        }
        let context = &mut self.context;
        let cc_info = context
            .cca
            .on_packet_sent(time_sent, PACKET_BYTES, false, &context.rtt_estimator);
        let info = TransmissionInfo {
            cc_info,
            time_sent,
            sent_bytes: PACKET_BYTES,
        };
        context.inflight.insert(make_pn(pn), Packet::new(queue, info));
        context.next_packet_number = VarInt::new(pn + 1).unwrap();
    }

    fn send(&mut self, pn: u64, frames: Vec<Entry<Frame>>, time_ms: u64) {
        self.insert(pn, frames, make_ts(time_ms));
    }

    /// Sends one filler frame in each of the packets `pns`.
    fn send_fillers(&mut self, pns: RangeInclusive<u64>, time_ms: u64) {
        for pn in pns {
            let filler = self.frame(data_header(FILLER), None);
            self.send(pn, vec![filler], time_ms);
        }
    }

    /// Processes an ACK of the packets `smallest..=largest` that arrives at `now_ms`.
    fn ack(&mut self, largest: u64, smallest: u64, now_ms: u64) {
        let mut random = xorshift::Rng::with_seed(1);
        ack::process_ack(
            VarInt::new(largest).unwrap(),
            VarInt::new(smallest).unwrap(),
            &[],
            EcnCounts::default(),
            Duration::ZERO,
            &mut self.context,
            &self.counters,
            &mut self.completed,
            &mut self.lost,
            &mut self.cancelled,
            &make_ts(now_ms),
            &mut random,
            &mut Vec::new(),
        );
    }

    /// Runs one round of the assembler, and returns whether it sent a packet.
    fn assemble_once(&mut self) -> bool {
        let registry = Registry::new();
        let pool = Pool::new(u16::MAX);
        let gso: Gso = MaxSegments::try_from(1usize).unwrap().into();
        let mut header_buf = Vec::new();
        let (mut freed_batch_tx, _freed_batch_rx) = crate::queue::freed_batch_channel();
        let send_counters = counters::Send::new(&registry, sender_id());
        let credit_pool = crate::credit::Pool::new(crate::credit::Config::default());
        let segments = assemble::assemble::<SyncRecycler, _>(
            &mut self.context,
            ImmediateQueueStatus::Empty,
            &self.clock,
            id::LocalSenderId::new(VarInt::from_u8(1)),
            443,
            &gso,
            pool.alloc::<SyncRecycler>().expect("a pool buffer"),
            &mut header_buf,
            &mut self.cancelled,
            &mut self.ack_completions,
            &mut freed_batch_tx,
            &AssemblerCounters::new(&registry),
            &send_counters,
            &credit_pool,
        );
        segments.is_some()
    }

    /// Removes every packet from the inflight map, and returns the frames of each packet,
    /// oldest packet first.
    fn take_inflight(&mut self) -> Vec<Vec<Entry<Frame>>> {
        if !self.context.inflight.has_inflight() {
            return Vec::new();
        }
        let range = self.context.inflight.get_range();
        self.context
            .inflight
            .remove_range(range)
            .map(|(_pn, packet)| packet.frames.into_iter().collect::<Vec<_>>())
            .collect()
    }
}

fn sender_id() -> id::LocalSenderId {
    id::LocalSenderId::from_index(0)
}

fn make_ts(millis: u64) -> Timestamp {
    // SAFETY: the tests compare these times only with each other.
    unsafe { Timestamp::from_duration(Duration::from_millis(millis)) }
}

fn make_pn(pn: u64) -> PacketNumber {
    PacketNumberSpace::Initial.new_packet_number(VarInt::new(pn).unwrap())
}

fn queue_pair(stream: u8) -> QueuePair {
    QueuePair {
        source_queue_id: VarInt::from_u8(stream),
        dest_queue_id: VarInt::from_u8(stream),
    }
}

/// A data frame of `stream`, on a binding the peer has confirmed.
fn data_header(stream: u8) -> Header {
    Header::QueueData {
        queue_pair: queue_pair(stream),
        binding_id: VarInt::from_u8(stream),
        offset: VarInt::ZERO,
        largest_offset: VarInt::ZERO,
        is_fin: false,
        blocked: false,
        dest_acceptor_id: None,
        priority: crate::credit::Priority::default(),
    }
}

/// The first data frame of `stream`, which asks the peer to create the binding.
fn init_header(stream: u8) -> Header {
    Header::QueueData {
        queue_pair: queue_pair(stream),
        binding_id: VarInt::from_u8(stream),
        offset: VarInt::ZERO,
        largest_offset: VarInt::ZERO,
        is_fin: false,
        blocked: false,
        dest_acceptor_id: Some(VarInt::from_u8(1)),
        priority: crate::credit::Priority::default(),
    }
}

fn max_data_header(stream: u8) -> Header {
    Header::QueueMaxData {
        queue_pair: queue_pair(stream),
        binding_id: VarInt::from_u8(stream),
        maximum_data: VarInt::from_u8(64),
    }
}

fn reset_header(stream: u8) -> Header {
    Header::QueueReset {
        queue_pair: queue_pair(stream),
        binding_id: VarInt::from_u8(stream),
        reset_target: ResetTarget::Both,
        error_code: VarInt::from_u8(1),
        init: None,
    }
}

fn stream_of(header: &Header) -> u64 {
    match *header {
        Header::QueueData { binding_id, .. }
        | Header::QueueMaxData { binding_id, .. }
        | Header::QueueReset { binding_id, .. } => binding_id.as_u64(),
        other => panic!("the tests don't send a {other:?} frame"),
    }
}

fn streams(frames: &[Entry<Frame>]) -> Vec<u64> {
    frames.iter().map(|f| stream_of(&f.header)).collect()
}

/// Removes every frame from `queue`, and returns them.
fn drain(queue: &mut Queue<Frame>) -> Vec<Entry<Frame>> {
    core::mem::take(queue).into_iter().collect()
}

/// Removes every frame from `queue`, and returns the frames of `stream`.
fn frames_of(queue: &mut Queue<Frame>, stream: u64) -> Vec<Entry<Frame>> {
    let mut frames = drain(queue);
    frames.retain(|frame| stream_of(&frame.header) == stream);
    frames
}

/// An ACK that the peer's receiver asks the sender to transmit.
fn pending_ack(
    entry: &Arc<PathSecretEntry>,
    largest_recv_time: precision::Timestamp,
) -> Entry<msg::Sender> {
    Entry::new(msg::Sender::PendingAck(ack::state::Submission {
        largest_acknowledged: VarInt::from_u8(7),
        ack_range: VarInt::from_u8(7),
        extra_ranges: Bytes::new(),
        ecn_counts: EcnCounts::default(),
        largest_recv_time,
        path_secret_entry: entry.clone(),
        local_sender_id: sender_id(),
        remote_sender_id: id::RemoteSenderId::new(VarInt::from_u8(0)),
        recv_worker_id: id::RecvDispatchWorkerId::new(0),
    }))
}

//= rendered/0005-endpoint/01-frames.md#DC-0005-001
//= type=test
//# A sender MAY carry several frames, of one or more streams to the same peer, in one packet.
#[test]
fn a_packet_carries_frames_of_several_streams() {
    let mut h = Harness::new();
    for stream in [1, 2] {
        let frame = h.frame(data_header(stream), None);
        h.context.push_back_frame(frame);
    }

    assert!(h.assemble_once(), "the assembler sends a packet");
    let packets = h.take_inflight();
    assert_eq!(packets.len(), 1, "one packet carries both frames");
    assert_eq!(streams(&packets[0]), [1, 2]);
    assert!(!h.context.has_pending_data());
}

//= rendered/0005-endpoint/01-frames.md#DC-0005-002
//= type=test
//# For each frame its writer hasn't cancelled, the endpoint MUST report exactly one outcome to
//# the writer: acknowledged, or failed with its reason.
#[test]
fn a_frame_gets_exactly_one_outcome() {
    let mut h = Harness::new();
    let frame = h.frame(data_header(1), None);
    h.send(0, vec![frame], 100);
    h.send_fillers(1..=3, 100);
    h.ack(3, 1, 110);

    let reported = frames_of(&mut h.completed, 1);
    assert!(reported.is_empty(), "a loss isn't an outcome");
    let lost = drain(&mut h.lost);
    assert_eq!(lost.len(), 1);

    h.send(4, lost, 200);
    h.ack(4, 4, 210);
    // A late ACK of every packet repeats the acknowledgement of packet 4.
    h.ack(4, 0, 220);

    let outcomes = frames_of(&mut h.completed, 1);
    assert_eq!(outcomes.len(), 1, "the frame is reported once");
    assert_eq!(outcomes[0].status, TransmissionStatus::Acknowledged);
    assert!(h.lost.is_empty());
    assert!(h.cancelled.is_empty());
}

//= rendered/0005-endpoint/01-frames.md#DC-0005-003
//= type=test
//# The endpoint MUST NOT transmit a frame whose writer has cancelled it or gone away.
#[test]
fn the_frames_of_a_cancelled_or_vanished_writer_are_not_sent() {
    let mut h = Harness::new();
    let cancelling = completion_channel();
    let vanishing = completion_channel();
    let frames = [
        h.frame(data_header(1), Some(cancelling.sender())),
        h.frame(data_header(2), Some(vanishing.sender())),
        h.frame(data_header(3), None),
    ];
    for frame in frames {
        h.context.push_back_frame(frame);
    }
    cancelling.cancel();
    drop(vanishing);

    assert!(h.assemble_once(), "the assembler sends the live frame");
    // The cancelled frames leave without an outcome, so the cancelling writer goes away before
    // they are dropped.
    drop(cancelling);
    assert_eq!(streams(&drain(&mut h.cancelled)), [1, 2]);
    let packets = h.take_inflight();
    assert_eq!(packets.len(), 1);
    assert_eq!(streams(&packets[0]), [3]);
}

//= rendered/0005-endpoint/01-frames.md#DC-0005-003
//= type=test
//# The endpoint MUST NOT transmit a frame whose writer has cancelled it or gone away.
#[test]
fn a_frame_cancelled_after_its_send_is_not_retransmitted() {
    let mut h = Harness::new();
    let writer = completion_channel();
    let frame = h.frame(data_header(1), Some(writer.sender()));
    h.send(0, vec![frame], 100);
    h.send_fillers(1..=3, 100);
    writer.cancel();
    h.ack(3, 1, 110);

    let cancelled = frames_of(&mut h.cancelled, 1);
    assert_eq!(cancelled.len(), 1, "the frame isn't retransmitted");
    assert_eq!(cancelled[0].status, CANCELLED);
    assert!(h.lost.is_empty());
    assert!(frames_of(&mut h.completed, 1).is_empty());
}

//= rendered/0005-endpoint/01-frames.md#DC-0005-004
//= type=test
//# The endpoint MUST bound the number of transmissions of each frame.
#[test]
fn a_frame_is_transmitted_a_bounded_number_of_times() {
    let mut h = Harness::new();
    let mut frame = h.frame(data_header(1), None);
    frame.ttl = 2;
    let mut frames = vec![frame];
    let mut transmissions = 0;
    for round in 0..10 {
        let base = round * 4;
        let sent_at = 100 + round * 100;
        h.send(base, frames, sent_at);
        h.send_fillers(base + 1..=base + 3, sent_at);
        transmissions += 1;
        h.ack(base + 3, base + 1, sent_at + 10);
        frames = frames_of(&mut h.lost, 1);
        if frames.is_empty() {
            break;
        }
    }

    assert_eq!(transmissions, 3, "the first send and two retries");
    let outcomes = frames_of(&mut h.completed, 1);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].status, TRANSMISSION_ERROR);
}

//= rendered/0005-endpoint/01-frames.md#DC-0005-051
//= type=test
//# When a frame reaches its bound on transmissions without an acknowledgement, the endpoint MUST
//# report the frame to its writer as failed with the transmission error reason.
#[test]
fn a_frame_at_its_bound_fails_with_the_transmission_error() {
    let mut h = Harness::new();
    let mut frame = h.frame(data_header(1), None);
    frame.ttl = 0;
    h.send(0, vec![frame], 100);
    h.send_fillers(1..=3, 100);
    h.ack(3, 1, 110);

    let outcomes = frames_of(&mut h.completed, 1);
    assert_eq!(outcomes.len(), 1, "the frame is reported once");
    assert_eq!(outcomes[0].status, TRANSMISSION_ERROR);
    assert!(h.lost.is_empty(), "the frame isn't retransmitted");
}

//= rendered/0005-endpoint/01-frames.md#DC-0005-005
//= type=test
//# A sender MUST send a peer's pending frames of a higher priority class before its pending frames
//# of a lower priority class.
#[test]
fn pending_frames_leave_in_priority_class_order() {
    let mut h = Harness::new();
    let headers = [
        data_header(1),
        init_header(2),
        max_data_header(3),
        reset_header(4),
    ];
    for header in headers {
        let frame = h.frame(header, None);
        h.context.push_back_frame(frame);
    }

    let mut order = Vec::new();
    while let Some(frame) = h.context.pop_pending() {
        order.push(frame.priority());
    }
    let expected = [
        Priority::QueueReset,
        Priority::QueueControl,
        Priority::QueueInit,
        Priority::QueueData,
    ];
    assert_eq!(order, expected);
}

//= rendered/0005-endpoint/01-frames.md#DC-0005-005
//= type=test
//# A sender MUST send a peer's pending frames of a higher priority class before its pending frames
//# of a lower priority class.
#[test]
fn a_packet_carries_the_higher_priority_class_first() {
    let mut h = Harness::new();
    let data = h.frame(data_header(1), None);
    h.context.push_back_frame(data);
    let reset = h.frame(reset_header(2), None);
    h.context.push_back_frame(reset);

    assert!(h.assemble_once(), "the assembler sends a packet");
    let packets = h.take_inflight();
    assert_eq!(packets.len(), 1, "one packet carries both frames");
    assert_eq!(streams(&packets[0]), [2, 1]);
}

//= rendered/0005-endpoint/01-frames.md#DC-0005-006
//= type=test
//# A sender MUST NOT hold back an acknowledgement because the congestion window is full.
#[test]
fn a_full_congestion_window_does_not_hold_back_an_acknowledgement() {
    let mut h = Harness::new();
    let now = h.now();
    let mut pn = 0;
    while h.context.can_send_pending_frames() && pn < 1_000 {
        let filler = h.frame(data_header(FILLER), None);
        h.insert(pn, vec![filler], now);
        pn += 1;
    }
    assert!(
        !h.context.can_send_pending_frames(),
        "the congestion window is full"
    );

    let data = h.frame(data_header(1), None);
    h.context.push_back_frame(data);
    let ack = pending_ack(&h.entry, h.clock.get_time());
    h.context.pending_acks.push_back(ack);
    let next_pn = h.context.next_packet_number.as_u64();
    let newest = h.context.inflight.get_range().end();
    let window_bytes = h.context.cca.bytes_in_flight();

    assert!(h.assemble_once(), "the assembler sends the ACK");
    assert_eq!(h.ack_completions.len(), 1);
    assert!(!h.context.has_pending_acks());
    // The ACK took a packet number of its own.
    assert_eq!(h.context.next_packet_number.as_u64(), next_pn + 1);
    // The data frame still waits for the congestion window, and nothing else was sent.
    assert!(h.context.has_pending_data(), "the data frame waits");
    assert_eq!(h.context.inflight.get_range().end(), newest);
    assert_eq!(h.context.cca.bytes_in_flight(), window_bytes);
}
