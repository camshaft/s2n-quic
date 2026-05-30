// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    endpoint::{assemble, combinator::AssemblerCounters, counters, frame, id::Id, inflight, send},
    intrusive::{Entry, Queue},
    path::secret::map::Entry as PathSecretEntry,
    socket::{
        channel::ImmediateQueueStatus,
        pool::{self, descriptor::SyncRecycler},
    },
    time::bach::Clock,
    xorshift::Rng,
};
use bytes::BytesMut;
use core::time::Duration;
use s2n_codec::EncoderValue as _;
use s2n_quic_core::{
    ack, frame as quic_frame, packet::number::PacketNumberSpace, time::Clock as _, varint::VarInt,
};
use s2n_quic_platform::features::{gso::MaxSegments, Gso};
use std::sync::Arc;

const TEST_MTU: u16 = 1400;
const SOURCE_CONTROL_PORT: u16 = 4433;

pub fn run_assemble_benchmark(packets: usize, frames_per_packet: usize, payload_len: usize) -> u64 {
    let registry = crate::counter::Registry::default();
    let clock = Clock::default();
    let entry = test_path_secret_entry();
    let mut context = make_context(&entry, &registry, &clock);
    let send_counters =
        counters::Send::new(&registry, crate::endpoint::id::LocalSenderId::from_index(0));
    let assembler_counters = AssemblerCounters::new(&registry);
    let pool = pool::Pool::new(u16::MAX);
    let mut header_buf = Vec::new();
    let mut cancelled = Queue::new();
    let mut ack_completions = Queue::new();
    let (mut freed_batch_tx, _freed_batch_rx) = crate::queue::freed_batch_channel();
    let gso: Gso = MaxSegments::try_from(1usize).unwrap().into();

    for packet_idx in 0..packets {
        for frame_idx in 0..frames_per_packet {
            context.push_back_frame(benchmark_frame(
                &entry,
                packet_idx * frames_per_packet + frame_idx,
                payload_len,
            ));
        }

        let Some(unfilled) = pool.alloc::<SyncRecycler>() else {
            break;
        };
        let _ = assemble::assemble::<SyncRecycler, _>(
            &mut context,
            ImmediateQueueStatus::Empty,
            &clock,
            crate::endpoint::id::LocalSenderId::from_index(0),
            SOURCE_CONTROL_PORT,
            &gso,
            unfilled,
            &mut header_buf,
            &mut cancelled,
            &mut ack_completions,
            &mut freed_batch_tx,
            &assembler_counters,
            &send_counters,
        );
    }

    let mut total_segments = 0u64;
    loop {
        let Some(unfilled) = pool.alloc::<SyncRecycler>() else {
            break;
        };
        let Some(segments) = assemble::assemble::<SyncRecycler, _>(
            &mut context,
            ImmediateQueueStatus::Empty,
            &clock,
            crate::endpoint::id::LocalSenderId::from_index(0),
            SOURCE_CONTROL_PORT,
            &gso,
            unfilled,
            &mut header_buf,
            &mut cancelled,
            &mut ack_completions,
            &mut freed_batch_tx,
            &assembler_counters,
            &send_counters,
        ) else {
            break;
        };
        total_segments += segments.segment_count() as u64;
    }

    total_segments
        .saturating_add(cancelled.len() as u64)
        .saturating_add(ack_completions.len() as u64)
        .saturating_add(context.inflight.has_inflight() as u64)
}

pub fn run_ack_processing_benchmark(
    packets: usize,
    frames_per_packet: usize,
    payload_len: usize,
    ack_frames: usize,
) -> u64 {
    let registry = crate::counter::Registry::default();
    let clock = Clock::default();
    let entry = test_path_secret_entry();
    let mut context = make_context(&entry, &registry, &clock);
    let send_counters =
        counters::Send::new(&registry, crate::endpoint::id::LocalSenderId::from_index(0));
    let mut completed = Queue::new();
    let mut lost = Queue::new();
    let mut cancelled = Queue::new();
    let mut rng = Rng::new();

    seed_inflight_packets(
        &mut context,
        &entry,
        &clock,
        packets,
        frames_per_packet,
        payload_len,
    );

    let mut payload = encode_ack_payload(packets, ack_frames);
    let mut deferred = Vec::new();
    let _ = context.process_ack_payload(
        &mut payload,
        Duration::ZERO,
        &send_counters,
        &mut completed,
        &mut lost,
        &mut cancelled,
        &clock,
        &mut rng,
        &mut deferred,
    );

    completed
        .len()
        .saturating_add(lost.len())
        .saturating_add(cancelled.len())
        .saturating_add(deferred.len())
        .saturating_add(context.inflight.has_inflight() as usize) as u64
}

fn test_path_secret_entry() -> Arc<PathSecretEntry> {
    let peer: std::net::SocketAddr = "127.0.0.1:4433".parse().unwrap();
    let entry = PathSecretEntry::builder(peer)
        .socket_sender_count(1)
        .build();
    entry.set_peer_data_addrs(&[peer]);
    entry.update_max_datagram_size(TEST_MTU);
    entry
}

fn make_context(
    entry: &Arc<PathSecretEntry>,
    registry: &crate::counter::Registry,
    clock: &Clock,
) -> send::Context {
    let inflight_gauge = registry.register_queue_gauge("bench.inflight");
    let ack_gauge = registry.register_queue_gauge("bench.ack");
    let pending_gauge = registry.register_queue_gauge("bench.pending");
    send::Context::new(
        entry,
        inflight_gauge,
        ack_gauge,
        pending_gauge,
        crate::endpoint::id::LocalSenderId::from_index(0),
        clock,
    )
    .unwrap()
}

fn benchmark_frame(
    entry: &Arc<PathSecretEntry>,
    idx: usize,
    payload_len: usize,
) -> Entry<frame::Frame> {
    let queue_id = VarInt::new((idx as u64 % 1024) + 1).unwrap();
    Entry::new(frame::Frame {
        header: frame::Header::QueueData {
            queue_pair: crate::packet::datagram::QueuePair {
                source_queue_id: queue_id,
                dest_queue_id: queue_id,
            },
            binding_id: VarInt::from_u8(1),
            offset: VarInt::ZERO,
            is_fin: false,
            dest_acceptor_id: None,
        },
        payload: BytesMut::zeroed(payload_len).into(),
        path_secret_entry: entry.clone(),
        completion: None,
        status: frame::TransmissionStatus::Pending,
        ttl: frame::DEFAULT_TTL,
        enqueued_at: None,
    })
}

fn seed_inflight_packets(
    context: &mut send::Context,
    entry: &Arc<PathSecretEntry>,
    clock: &Clock,
    packets: usize,
    frames_per_packet: usize,
    payload_len: usize,
) {
    let now = clock.get_time();
    for packet_idx in 0..packets {
        let rtt = context.rtt_estimator;
        let sent_bytes = ((payload_len.max(1) * frames_per_packet.max(1)) + 64) as u16;
        let cc_info = context.cca.on_packet_sent(now, sent_bytes, false, &rtt);

        let mut frames = Queue::new();
        for frame_idx in 0..frames_per_packet {
            frames.push_back(benchmark_frame(
                entry,
                packet_idx * frames_per_packet + frame_idx,
                payload_len,
            ));
        }

        let pn =
            PacketNumberSpace::Initial.new_packet_number(VarInt::new(packet_idx as u64).unwrap());
        context.inflight.insert(
            pn,
            inflight::Packet::new(
                frames,
                inflight::TransmissionInfo {
                    cc_info,
                    time_sent: now,
                    sent_bytes,
                },
            ),
        );
    }

    context.next_packet_number = VarInt::new(packets as u64).unwrap_or(VarInt::MAX);
}

fn encode_ack_payload(total_packets: usize, ack_frames: usize) -> BytesMut {
    if total_packets == 0 {
        return BytesMut::new();
    }

    let frame_count = ack_frames.max(1).min(total_packets);
    let mut payload = Vec::with_capacity(frame_count * 32);

    let base = total_packets / frame_count;
    let remainder = total_packets % frame_count;
    let mut start = 0usize;

    for idx in 0..frame_count {
        let mut count = base;
        if idx < remainder {
            count += 1;
        }
        let end = start + count - 1;
        let mut ranges = ack::Ranges::new(count.max(1) + 1);
        for packet in start..=end {
            let packet_number =
                PacketNumberSpace::Initial.new_packet_number(VarInt::new(packet as u64).unwrap());
            let _ = ranges.insert_packet_number(packet_number);
        }
        let frame = quic_frame::Ack {
            ack_delay: VarInt::ZERO,
            ack_ranges: &ranges,
            ecn_counts: None,
        };
        payload.extend_from_slice(&frame.encode_to_vec());
        start = end + 1;
    }

    BytesMut::from(payload.as_slice())
}
