// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests that cite the packet assembly requirements of RFC 0005
//! (`rendered/0005-endpoint/05-assembly.md`).
//!
//! Each test drives `assemble` the way a sender does, then reads the packets it produced back
//! with the datagram decoder, so the assertions are about the bytes a peer receives.

use super::*;
use crate::stream::endpoint::send::ProbeState;

const SOURCE_CONTROL_PORT: u16 = 443;

/// The assembler state one sender keeps across rounds, beside each peer's `Context`.
struct Harness<Rx> {
    clock: Clock,
    pool: pool::Pool,
    header_buf: Vec<u8>,
    cancelled: crate::intrusive::Queue<Frame>,
    ack_completions: crate::intrusive::Queue<crate::endpoint::msg::Sender>,
    freed_batch_tx: FreedBatchTx,
    _freed_batch_rx: Rx,
    counters: AssemblerCounters,
    send_counters: crate::endpoint::counters::Send,
    credit_pool: crate::credit::Pool,
}

fn harness() -> Harness<impl Sized> {
    let (freed_batch_tx, freed_batch_rx) = crate::queue::freed_batch_channel();
    Harness {
        clock: Clock::new(Duration::from_micros(1)),
        pool: pool::Pool::new(u16::MAX),
        header_buf: Vec::new(),
        cancelled: crate::intrusive::Queue::new(),
        ack_completions: crate::intrusive::Queue::new(),
        freed_batch_tx,
        _freed_batch_rx: freed_batch_rx,
        counters: AssemblerCounters::new(&Registry::new()),
        send_counters: crate::endpoint::counters::Send::new(
            &Registry::default(),
            crate::endpoint::id::LocalSenderId::from_index(0),
        ),
        credit_pool: unused_credit_pool(),
    }
}

impl<Rx> Harness<Rx> {
    /// Runs one assembly round and returns each segment it put on the wire, in order.
    fn run(
        &mut self,
        context: &mut Context,
        status: ImmediateQueueStatus,
        source_sender_id: crate::endpoint::id::LocalSenderId,
        gso: &Gso,
    ) -> Vec<Vec<u8>> {
        let segments = assemble::<SyncRecycler, _>(
            context,
            status,
            &self.clock,
            source_sender_id,
            SOURCE_CONTROL_PORT,
            gso,
            self.pool.alloc::<SyncRecycler>().expect("pool alloc failed"),
            &mut self.header_buf,
            &mut self.cancelled,
            &mut self.ack_completions,
            &mut self.freed_batch_tx,
            &self.counters,
            &self.send_counters,
            &self.credit_pool,
        );

        let Some(segments) = segments else {
            return Vec::new();
        };
        segments.send_with(|_addr, _ecn, slices| {
            slices.iter().map(|slice| slice.to_vec()).collect()
        })
    }

    /// Runs one round for sender identifier 1 with an empty immediate queue.
    fn round(&mut self, context: &mut Context, gso: &Gso) -> Vec<Vec<u8>> {
        self.run(context, ImmediateQueueStatus::Empty, sender_id(1), gso)
    }
}

fn sender_id(id: u8) -> crate::endpoint::id::LocalSenderId {
    crate::endpoint::id::LocalSenderId::new(VarInt::from_u8(id))
}

fn packet_number(number: u8) -> PacketNumber {
    PacketNumberSpace::Initial.new_packet_number(VarInt::from_u8(number))
}

fn crypto_tag_len(context: &Context) -> usize {
    crate::crypto::seal::Application::tag_len(&context.sealer)
}

/// A QueueData frame at `offset` that carries `payload`.
fn data_frame(
    entry: &Arc<PathSecretEntry>,
    offset: u64,
    payload: &[u8],
) -> crate::intrusive::Entry<Frame> {
    Frame {
        header: Header::QueueData {
            queue_pair: crate::packet::datagram::QueuePair {
                source_queue_id: VarInt::from_u8(1),
                dest_queue_id: VarInt::from_u8(2),
            },
            binding_id: VarInt::from_u8(1),
            offset: VarInt::new(offset).unwrap(),
            largest_offset: VarInt::new(offset).unwrap(),
            is_fin: false,
            blocked: false,
            dest_acceptor_id: None,
            priority: crate::credit::Priority::default(),
        },
        payload: payload_vec(payload),
        path_secret_entry: entry.clone(),
        completion: None,
        status: TransmissionStatus::default(),
        ttl: DEFAULT_TTL,
        enqueued_at: None,
        flow_credits: 0,
    }
    .into()
}

/// A sealing entry, and the peer's opening entry for the same path secret.
fn paired_entries(mtu: u16) -> (Arc<PathSecretEntry>, Arc<PathSecretEntry>) {
    let peer: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let sealer = PathSecretEntry::builder(peer)
        .endpoint_type(s2n_quic_core::endpoint::Type::Client)
        .build(None);
    sealer.update_max_datagram_size(mtu);
    let opener = PathSecretEntry::builder(peer)
        .endpoint_type(s2n_quic_core::endpoint::Type::Server)
        .build(None);
    (sealer, opener)
}

fn context_for(
    entry: &Arc<PathSecretEntry>,
    sender_idx: crate::endpoint::id::LocalSenderId,
) -> Context {
    let registry = Registry::new();
    Context::new(
        entry,
        registry.register_queue_gauge("test.inflight"),
        registry.register_queue_gauge("test.ack"),
        registry.register_queue_gauge("test.pending"),
        sender_idx,
        &crate::time::bach::Clock::default(),
    )
    .unwrap()
}

/// A packet as the peer reads it off the wire.
#[derive(Debug)]
struct WirePacket {
    sender_id: VarInt,
    packet_number: u64,
    /// The packet's encoded length, without the GSO padding that may follow it.
    len: usize,
    frames: Vec<Header>,
}

fn read_packet(segment: &[u8], tag_len: usize) -> WirePacket {
    let mut bytes = segment.to_vec();
    let total_len = bytes.len();
    let (packet, rest) = crate::packet::datagram::decoder::Packet::decode(
        s2n_codec::DecoderBufferMut::new(&mut bytes),
        (),
        tag_len,
    )
    .expect("an assembled packet decodes");
    let RoutingInfo::SenderId { source_sender_id } = packet.routing_info() else {
        panic!("an assembled packet carries its sender identifier");
    };
    let mut frames = Vec::new();
    for item in crate::stream::endpoint::decode::decode_frames(packet.application_header()) {
        let (header, _payload_len) = item.expect("frame metadata decodes");
        frames.push(header);
    }
    WirePacket {
        sender_id: source_sender_id,
        packet_number: packet.packet_number().as_u64(),
        len: total_len - rest.len(),
        frames,
    }
}

fn read_all(segments: &[Vec<u8>], tag_len: usize) -> Vec<WirePacket> {
    segments
        .iter()
        .map(|segment| read_packet(segment, tag_len))
        .collect()
}

/// Opens a packet as the peer does, and returns its decrypted payload.
fn open_packet<O: crate::crypto::open::Application>(
    bytes: &mut [u8],
    tag_len: usize,
    opener: &O,
) -> Option<Vec<u8>> {
    let (mut packet, _rest) = crate::packet::datagram::decoder::Packet::decode(
        s2n_codec::DecoderBufferMut::new(bytes),
        (),
        tag_len,
    )
    .ok()?;
    packet.decrypt_in_place(opener).ok()?;
    Some(packet.payload().to_vec())
}

fn data_offsets(frames: &[Header]) -> Vec<u64> {
    frames
        .iter()
        .filter_map(|header| match header {
            Header::QueueData { offset, .. } => Some(offset.as_u64()),
            _ => None,
        })
        .collect()
}

fn queued_offsets(frames: &crate::intrusive::Queue<Frame>) -> Vec<u64> {
    let headers: Vec<Header> = frames.iter().map(|frame| frame.header).collect();
    data_offsets(&headers)
}

/// Every packet stays within the path's maximum packet size, across MTUs, with full data
/// frames, a small frame, an acknowledgement and probes in the mix.
#[test]
fn assembled_packets_fit_the_path_max_packet_size() {
    //= rendered/0005-endpoint/05-assembly.md#DC-0005-032
    //= type=test
    //# A sender MUST NOT send a packet larger than the maximum packet size of the path.
    for mtu in [1_200, 1_400, 1_500, 4_000, 8_950] {
        let registry = Registry::new();
        let (mut context, entry) = make_context(mtu, &registry);
        let mut harness = harness();
        let gso = make_gso(segment::MAX_COUNT);
        let path_mtu = context.path_info(&gso).max_datagram_size as usize;
        let tag_len = crypto_tag_len(&context);

        let mut offset = 0u64;
        for _ in 0..8 {
            let frame = full_data_frame(&entry, mtu, offset);
            offset += frame.payload_len() as u64;
            context.push_back_frame(frame);
        }
        context.push_back_frame(data_frame(&entry, offset, b"tail"));

        let mut segments = Vec::new();
        for _ in 0..4 {
            segments.extend(harness.round(&mut context, &gso));
        }
        let now = harness.clock.get_time();
        context.pending_acks.push_back(make_pending_ack(&entry, now));
        segments.extend(harness.round(&mut context, &gso));
        context.pto.probe_state = ProbeState::ProbeTwice;
        for _ in 0..2 {
            segments.extend(harness.round(&mut context, &gso));
        }

        assert!(!segments.is_empty(), "mtu {mtu}: the sender must send packets");
        for segment in &segments {
            assert!(
                segment.len() <= path_mtu,
                "mtu {mtu}: a {}-byte segment exceeds the path's {path_mtu}-byte maximum",
                segment.len()
            );
            let packet = read_packet(segment, tag_len);
            assert!(
                packet.len <= path_mtu,
                "mtu {mtu}: a {}-byte packet exceeds the path's {path_mtu}-byte maximum",
                packet.len
            );
        }
    }
}

/// Data packets, an acknowledgement-only packet and probes sent under one sender identifier
/// carry packet numbers that rise with each packet.
#[test]
fn packet_numbers_rise_under_one_sender_identifier() {
    //= rendered/0005-endpoint/05-assembly.md#DC-0005-033
    //= type=test
    //# A sender MUST number each packet it sends under one sender identifier higher than every packet it sent before under that identifier.
    let mtu = 1_500;
    let registry = Registry::new();
    let (mut context, entry) = make_context(mtu, &registry);
    let mut harness = harness();
    let gso = make_gso(4);
    let tag_len = crypto_tag_len(&context);
    // Start part way into the packet number space, as a sender does after its first packets.
    context.next_packet_number = VarInt::from_u8(60);

    let mut segments = Vec::new();
    let mut offset = 0u64;
    for _ in 0..3 {
        let frame = full_data_frame(&entry, mtu, offset);
        offset += frame.payload_len() as u64;
        context.push_back_frame(frame);
    }
    segments.extend(harness.round(&mut context, &gso));
    let now = harness.clock.get_time();
    context.pending_acks.push_back(make_pending_ack(&entry, now));
    segments.extend(harness.round(&mut context, &gso));
    context.push_back_frame(data_frame(&entry, offset, b"after the acknowledgement"));
    segments.extend(harness.round(&mut context, &gso));
    context.pto.probe_state = ProbeState::ProbeTwice;
    for _ in 0..3 {
        segments.extend(harness.round(&mut context, &gso));
    }

    let packets = read_all(&segments, tag_len);
    assert!(packets.len() >= 4, "the rounds must send packets, got {packets:?}");
    assert_eq!(packets[0].packet_number, 60);
    for packet in &packets {
        assert_eq!(
            packet.sender_id,
            VarInt::from_u8(1),
            "every packet carries the one sender identifier"
        );
    }
    for pair in packets.windows(2) {
        assert!(
            pair[1].packet_number > pair[0].packet_number,
            "packet number {} follows {} under the same sender identifier",
            pair[1].packet_number,
            pair[0].packet_number
        );
    }
}

/// The frame payload is not on the wire unencrypted, and the peer's opener recovers it.
#[test]
fn frame_payloads_are_encrypted() {
    //= rendered/0005-endpoint/05-assembly.md#DC-0005-034
    //= type=test
    //# A sender MUST encrypt the frame payloads of each packet.
    let (sealer_entry, opener_entry) = paired_entries(1_500);
    let mut context = context_for(&sealer_entry, crate::endpoint::id::LocalSenderId::from_index(0));
    let opener = opener_entry
        .secret()
        .application_opener(context.credentials.key_id);
    let tag_len = crypto_tag_len(&context);
    let mut harness = harness();

    let marker: &[u8] = b"plaintext-034:";
    let plaintext = marker.repeat(20);
    context.push_back_frame(data_frame(&sealer_entry, 0, &plaintext));
    let segments = harness.round(&mut context, &make_gso(1));
    assert_eq!(segments.len(), 1, "one packet carries the frame");

    let mut wire = segments[0].clone();
    assert!(
        !wire.windows(marker.len()).any(|window| window == marker),
        "the frame payload must not appear on the wire unencrypted"
    );
    let decrypted = open_packet(&mut wire, tag_len, &opener).expect("the peer opens the packet");
    assert_eq!(decrypted, plaintext, "the peer recovers the frame payload");
}

/// Changing any bit of a packet that carries frames, in its header, its frame metadata, its
/// payload or its tag, makes the peer refuse it.
#[test]
fn every_byte_of_a_packet_with_frames_is_authenticated() {
    //= rendered/0005-endpoint/05-assembly.md#DC-0005-052
    //= type=test
    //# A sender MUST authenticate every byte of each packet that carries frames, its unencrypted header and frame metadata among them.
    let (sealer_entry, opener_entry) = paired_entries(1_500);
    let mut context = context_for(&sealer_entry, crate::endpoint::id::LocalSenderId::from_index(0));
    let opener = opener_entry
        .secret()
        .application_opener(context.credentials.key_id);
    let tag_len = crypto_tag_len(&context);
    let mut harness = harness();

    context.push_back_frame(data_frame(&sealer_entry, 0, b"authenticated"));
    context.push_back_frame(data_frame(&sealer_entry, 13, b"metadata"));
    let segments = harness.round(&mut context, &make_gso(1));
    assert_eq!(segments.len(), 1, "one packet carries both frames");
    let packet = read_packet(&segments[0], tag_len);
    assert_eq!(packet.frames.len(), 2, "the packet carries frame metadata");
    let wire = &segments[0][..packet.len];

    let mut unmodified = wire.to_vec();
    assert!(
        open_packet(&mut unmodified, tag_len, &opener).is_some(),
        "the peer opens the packet as sent"
    );
    for index in 0..wire.len() {
        for mask in [0x01u8, 0x80] {
            let mut modified = wire.to_vec();
            modified[index] ^= mask;
            assert!(
                open_packet(&mut modified, tag_len, &opener).is_none(),
                "the peer opened the packet with bit {mask:#04x} of byte {index} of {} changed",
                wire.len()
            );
        }
    }
}

/// Each sent packet keeps the frames it carried: an acknowledgement returns exactly the
/// acknowledged packet's frames, and every other frame stays with its packet or is declared
/// lost, once.
#[test]
fn sent_frames_are_kept_until_acknowledged_or_lost() {
    //= rendered/0005-endpoint/05-assembly.md#DC-0005-035
    //= type=test
    //# A sender MUST record which frames each packet carried until the packet is acknowledged or declared lost.
    let registry = Registry::new();
    let (mut context, entry) = make_context(1_500, &registry);
    let mut harness = harness();
    let gso = make_gso(1);
    let tag_len = crypto_tag_len(&context);

    let offsets = [0u64, 100, 200, 300, 400, 500];
    for (number, offset) in offsets.iter().enumerate() {
        context.push_back_frame(data_frame(&entry, *offset, b"recorded"));
        let segments = harness.round(&mut context, &gso);
        assert_eq!(segments.len(), 1, "one packet carries the frame");
        let packet = read_packet(&segments[0], tag_len);
        assert_eq!(packet.packet_number, number as u64);
        assert_eq!(data_offsets(&packet.frames), [*offset]);
    }

    // The peer acknowledges the newest packet, number 5, alone.
    let mut completed = crate::intrusive::Queue::<Frame>::new();
    let mut lost = crate::intrusive::Queue::<Frame>::new();
    let mut cancelled = crate::intrusive::Queue::<Frame>::new();
    let now: s2n_quic_core::time::Timestamp = harness.clock.get_time().into();
    crate::endpoint::ack::process_ack(
        VarInt::from_u8(5),
        VarInt::from_u8(5),
        &[],
        s2n_quic_core::frame::ack::EcnCounts::default(),
        Duration::ZERO,
        &mut context,
        &harness.send_counters,
        &mut completed,
        &mut lost,
        &mut cancelled,
        &now,
        &mut crate::xorshift::Rng::with_seed(1),
        &mut Vec::new(),
    );

    assert_eq!(
        queued_offsets(&completed),
        [500],
        "the acknowledgement returns the frame packet 5 carried"
    );
    assert!(completed
        .iter()
        .all(|frame| matches!(frame.status, TransmissionStatus::Acknowledged)));

    let mut accounted = queued_offsets(&lost);
    accounted.extend(queued_offsets(&cancelled));
    for number in 0..=5u8 {
        let Some(packet) = context.inflight.remove(packet_number(number)) else {
            continue;
        };
        assert_ne!(number, 5, "the acknowledged packet leaves the record");
        let carried = queued_offsets(&packet.frames);
        assert_eq!(
            carried,
            [offsets[number as usize]],
            "packet {number} still records the frame it carried"
        );
        accounted.extend(carried);
    }
    accounted.sort_unstable();
    assert_eq!(
        accounted,
        &offsets[..5],
        "each unacknowledged frame is recorded with its packet or declared lost, once"
    );
}

/// With one, two or three packets outstanding, a probe timeout sends exactly two packets, and
/// each one is ack-eliciting.
#[test]
fn probe_timeout_sends_two_ack_eliciting_packets() {
    //= rendered/0005-endpoint/05-assembly.md#DC-0005-036
    //= type=test
    //# When its probe timeout fires, a sender MUST send two ack-eliciting packets to the peer.
    for outstanding in 1..=3u64 {
        let registry = Registry::new();
        let (mut context, entry) = make_context(1_500, &registry);
        let mut harness = harness();
        let tag_len = crypto_tag_len(&context);
        for index in 0..outstanding {
            context.push_back_frame(data_frame(&entry, index * 100, b"outstanding"));
            assert_eq!(harness.round(&mut context, &make_gso(1)).len(), 1);
        }

        context.pto.probe_state = ProbeState::ProbeTwice;
        let gso = make_gso(2);
        let mut probes = Vec::new();
        for _ in 0..4 {
            if !context.pto.probe_state.is_requested() {
                break;
            }
            probes.extend(harness.round(&mut context, &gso));
        }

        assert!(
            !context.pto.probe_state.is_requested(),
            "{outstanding} outstanding: the probes must be sent"
        );
        assert_eq!(
            probes.len(),
            2,
            "{outstanding} outstanding: a probe timeout sends two packets"
        );
        for packet in read_all(&probes, tag_len) {
            assert!(
                packet.frames.iter().any(Header::is_ack_eliciting),
                "{outstanding} outstanding: probe packet {} must be ack-eliciting",
                packet.packet_number
            );
        }
    }
}

/// With nothing pending, a probe carries the frames of the oldest packet that is sent and not
/// yet acknowledged, under a new packet number.
#[test]
fn probe_resends_the_oldest_unacknowledged_packet() {
    //= rendered/0005-endpoint/05-assembly.md#DC-0005-037
    //= type=test
    //# When a probe is due and no frames are pending for the peer, the sender MUST make the probe from frames of the oldest packet still awaiting acknowledgement, sent again under a new packet number.
    let registry = Registry::new();
    let (mut context, entry) = make_context(1_500, &registry);
    let mut harness = harness();
    let gso = make_gso(1);
    let tag_len = crypto_tag_len(&context);

    context.push_back_frame(data_frame(&entry, 0, b"oldest"));
    assert_eq!(harness.round(&mut context, &gso).len(), 1);
    context.push_back_frame(data_frame(&entry, 100, b"newer"));
    assert_eq!(harness.round(&mut context, &gso).len(), 1);
    assert!(!context.has_pending_data(), "no frames are pending for the peer");

    context.pto.probe_state = ProbeState::ProbeTwice;
    let probe = read_all(&harness.round(&mut context, &gso), tag_len);
    assert_eq!(probe.len(), 1, "the probe is one packet");
    assert_eq!(
        probe[0].packet_number, 2,
        "the probe is sent under a new packet number"
    );
    assert_eq!(
        data_offsets(&probe[0].frames),
        [0],
        "the probe carries the frame of packet 0, the oldest awaiting acknowledgement"
    );
    assert_eq!(
        context.inflight.oldest_non_shell_pn(),
        Some(packet_number(1)),
        "packet 1 is now the oldest packet whose frames await acknowledgement"
    );
}

/// While data frames are held back, a waiting acknowledgement goes out alone, and a probe goes
/// out though the data is held.
#[test]
fn acknowledgements_and_probes_are_not_held_behind_data() {
    //= rendered/0005-endpoint/05-assembly.md#DC-0005-038
    //= type=test
    //# A sender SHOULD send waiting acknowledgements and probes without holding them behind paced data frames.

    // A waiting acknowledgement.
    {
        let registry = Registry::new();
        let (mut context, entry) = make_context(1_500, &registry);
        let mut harness = harness();
        let tag_len = crypto_tag_len(&context);
        for index in 0..4u64 {
            context.push_back_frame(data_frame(&entry, index * 100, b"held"));
        }
        let now = harness.clock.get_time();
        context.pending_acks.push_back(make_pending_ack(&entry, now));

        let segments = harness.run(
            &mut context,
            ImmediateQueueStatus::HasMore,
            sender_id(1),
            &make_gso(4),
        );
        let frames: Vec<Header> = read_all(&segments, tag_len)
            .into_iter()
            .flat_map(|packet| packet.frames)
            .collect();
        assert!(
            frames.iter().any(|header| matches!(header, Header::Ack { .. })),
            "the waiting acknowledgement is sent, got {frames:?}"
        );
        assert!(
            data_offsets(&frames).is_empty(),
            "the acknowledgement does not wait for the held data frames"
        );
        assert_eq!(context.pending_count(), 4, "the data frames stay held");
    }

    // A due probe.
    {
        let registry = Registry::new();
        let (mut context, entry) = make_context(1_500, &registry);
        let mut harness = harness();
        let gso = make_gso(1);
        let tag_len = crypto_tag_len(&context);
        context.push_back_frame(data_frame(&entry, 0, b"outstanding"));
        assert_eq!(harness.round(&mut context, &gso).len(), 1);
        for index in 1..=2u64 {
            context.push_back_frame(data_frame(&entry, index * 100, b"held"));
        }

        let held = harness.run(&mut context, ImmediateQueueStatus::HasMore, sender_id(1), &gso);
        assert!(held.is_empty(), "without a probe, the data frames are held");

        context.pto.probe_state = ProbeState::ProbeTwice;
        let probe = harness.run(&mut context, ImmediateQueueStatus::HasMore, sender_id(1), &gso);
        let packets = read_all(&probe, tag_len);
        assert_eq!(packets.len(), 1, "the probe goes out while data is held");
        assert!(
            packets[0].frames.iter().any(Header::is_ack_eliciting),
            "the probe packet is ack-eliciting"
        );
        assert_ne!(
            context.pto.probe_state,
            ProbeState::ProbeTwice,
            "the probe was sent"
        );
    }
}

/// Two senders send one peer's frames, each under its own sender identifier and with its own
/// packet number space.
#[test]
fn one_peer_frames_spread_across_senders() {
    //= rendered/0005-endpoint/05-assembly.md#DC-0005-039
    //= type=test
    //# A sender MAY spread one peer's frames across several of its senders, each with its own sender identifier and packet number space.
    let registry = Registry::new();
    let (mut first, entry) = make_context(1_500, &registry);
    let mut second = context_for(&entry, crate::endpoint::id::LocalSenderId::from_index(1));
    assert!(Arc::ptr_eq(
        &first.path_secret_entry,
        &second.path_secret_entry
    ));
    let mut first_harness = harness();
    let mut second_harness = harness();
    let gso = make_gso(1);
    let tag_len = crypto_tag_len(&first);

    let mut first_segments = Vec::new();
    let mut second_segments = Vec::new();
    for index in 0..3u64 {
        first.push_back_frame(data_frame(&entry, index * 200, b"first sender"));
        first_segments.extend(first_harness.run(
            &mut first,
            ImmediateQueueStatus::Empty,
            sender_id(1),
            &gso,
        ));
        second.push_back_frame(data_frame(&entry, index * 200 + 100, b"second sender"));
        second_segments.extend(second_harness.run(
            &mut second,
            ImmediateQueueStatus::Empty,
            sender_id(2),
            &gso,
        ));
    }

    for (segments, id) in [(&first_segments, 1u8), (&second_segments, 2)] {
        let packets = read_all(segments, tag_len);
        for packet in &packets {
            assert_eq!(
                packet.sender_id,
                VarInt::from_u8(id),
                "sender {id} sends under its own sender identifier"
            );
        }
        let numbers: Vec<u64> = packets.iter().map(|packet| packet.packet_number).collect();
        assert_eq!(
            numbers,
            [0, 1, 2],
            "sender {id} numbers its packets in its own packet number space"
        );
    }
}
