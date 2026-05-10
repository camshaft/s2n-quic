// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Packet assembly: packs pending frames into MTU-sized encrypted packets.
//!
//! Called by the Dispatcher when a send::Context fires from the local wheel.
//! Drains frames from the pending queue, packs them into segments respecting
//! MTU and CCA window constraints, encrypts, and registers in the inflight map.

use crate::{
    clock::precision,
    crypto::seal,
    intrusive_queue::Queue,
    msg::segment,
    packet::{self, datagram::RoutingInfo},
    socket::pool,
    stream3::{
        endpoint::{inflight, send::Context},
        frame::Frame,
    },
};
use s2n_codec::{Encoder, EncoderBuffer, EncoderValue};
use s2n_quic_core::{buffer, time::Clock, varint::VarInt};

/// Attempt to assemble pending frames into a full GSO datagram of encrypted packets.
///
/// Returns None if the CCA window is full, no transmittable frames exist, or pool
/// allocation fails. The caller is responsible for re-registering the context in the
/// timer wheel if frames remain after assembly.
///
/// `header_buf` is a caller-provided reusable allocation for encoding per-frame metadata
/// into the application header region. It is cleared before use and after return.
///
/// Cancelled frames (where `should_transmit()` returns false) are sent to `cancelled`
/// for completion notification.
pub(crate) fn assemble<Clk>(
    context: &mut Context,
    clock: &Clk,
    source_sender_id: VarInt,
    source_control_port: u16,
    pool: &pool::Pool,
    header_buf: &mut Vec<u8>,
    cancelled: &mut impl crate::socket::channel::UnboundedSender<Queue<Frame>>,
) -> Option<pool::descriptor::Segments>
where
    Clk: precision::Clock + Clock + ?Sized,
{
    let available_window = context
        .cca
        .congestion_window()
        .saturating_sub(context.cca.bytes_in_flight());

    if available_window == 0 {
        return None;
    }

    let mtu = context.path_secret_entry.max_datagram_size();
    let now = clock.now();
    let time_sent = clock.get_time();

    let unfilled = pool.alloc()?;

    let mut segment_size: u16 = 0;
    let mut segments_written: u32 = 0;

    let result = unfilled.fill_with(|addr, cmsg, mut payload| {
        addr.set(context.path_secret_entry.data_addr().into());

        let mut offset: usize = 0;
        let mut watermark: usize = 0;

        loop {
            if segments_written as usize >= segment::MAX_COUNT {
                break;
            }

            // Check if we have buffer capacity for another segment
            if offset + mtu as usize > payload.len() {
                break;
            }

            let max_segment_len = {
                let remaining_total = segment::MAX_TOTAL as usize - offset.min(segment::MAX_TOTAL as usize);
                if segment_size == 0 {
                    remaining_total.min(mtu as usize)
                } else {
                    remaining_total.min(segment_size as usize)
                }
            };

            if max_segment_len == 0 {
                break;
            }

            // Drain cancelled frames before collecting transmittable ones
            let mut cancelled_queue = Queue::new();
            let mut packet_frames = Queue::new();

            while let Some(frame) = context.pending.pop_front() {
                if !frame.should_transmit() {
                    cancelled_queue.push_back(frame);
                    continue;
                }

                packet_frames.push_back(frame);

                let estimated_len = estimate_segment_len(
                    source_sender_id,
                    context.next_packet_number,
                    context.flow_attempt_id_counter,
                    &packet_frames,
                    header_buf,
                    seal::Application::tag_len(&context.sealer),
                );

                if estimated_len > max_segment_len {
                    let frame = packet_frames
                        .pop_back()
                        .expect("packet_frames is not empty after push_back");
                    context.pending.push_front(frame);
                    break;
                }

                if estimated_len == max_segment_len {
                    break;
                }
            }

            // Send cancelled frames to the completion channel
            if !cancelled_queue.is_empty() {
                let _ = cancelled.send(cancelled_queue);
            }

            if packet_frames.is_empty() {
                break;
            }

            // Assign packet number
            let packet_number = context.next_packet_number;
            context.next_packet_number += 1;

            // Zero padding between segments for GSO alignment
            if offset > watermark {
                payload[watermark..offset].fill(0);
            }

            // Encode this segment
            header_buf.clear();
            let encoded_len = encode_segment(
                &mut payload[offset..],
                source_control_port,
                source_sender_id,
                packet_number,
                &context.sealer,
                &context.credentials,
                &mut context.flow_attempt_id_counter,
                &packet_frames,
                header_buf,
            );

            debug_assert!(encoded_len <= max_segment_len);

            watermark = offset + encoded_len;

            // First segment establishes GSO segment size
            if segment_size == 0 {
                segment_size = encoded_len as u16;
            }

            // Register in inflight map
            let has_more_app_data = context.has_pending();
            let cc_info = context.cca.on_packet_sent(
                time_sent,
                encoded_len as u16,
                has_more_app_data,
                &context.rtt_estimator,
            );
            let tx_info = inflight::TransmissionInfo {
                cc_info,
                time_sent,
                sent_bytes: encoded_len as u16,
            };
            let pn = s2n_quic_core::packet::number::PacketNumberSpace::Initial
                .new_packet_number(packet_number);
            context
                .inflight
                .insert(pn, inflight::Packet::new(packet_frames, tx_info));

            segments_written += 1;

            // Advance to next segment boundary
            offset += segment_size as usize;

            // Undersized segment must be last (GSO constraint)
            if (encoded_len as u16) < segment_size {
                break;
            }
        }

        if segments_written > 0 && segment_size > 0 {
            cmsg.set_segment_len(segment_size);
        }

        <Result<_, core::convert::Infallible>>::Ok(watermark)
    });

    let segments = result.expect("fill_with closure is infallible");

    if segments_written == 0 {
        return None;
    }

    // Update PTO
    context.pto.on_packet_sent(now);

    header_buf.clear();

    Some(pool::descriptor::Segments::new(
        segments.take_filled(),
        segment_size,
    ))
}

fn estimate_segment_len(
    source_sender_id: VarInt,
    packet_number: VarInt,
    initial_flow_attempt_id: VarInt,
    frames: &Queue<Frame>,
    header_buf: &mut Vec<u8>,
    crypto_tag_len: usize,
) -> usize {
    let mut flow_attempt_id = initial_flow_attempt_id;
    let total_payload_len = encode_frame_metadata(frames, &mut flow_attempt_id, header_buf);
    let header_len = VarInt::new(header_buf.len() as u64).expect("header length fits in VarInt");
    let payload_len =
        VarInt::new(total_payload_len as u64).expect("payload length fits in VarInt");

    datagram::encoder::estimate_len(
        packet_number,
        RoutingInfo::SenderId { source_sender_id },
        header_len,
        payload_len,
        crypto_tag_len,
    )
}

/// Encode a single segment containing one or more frames.
///
/// Wire layout:
///   [packet-level header: tag, credentials, wire_version, source_control_port, pn, SenderId routing]
///   [header_len varint][frame metadata: Header + payload_len per frame...]
///   [payload_len varint][frame payloads concatenated...]
///   [auth tag: 16 bytes]
///
/// The packet header through the frame metadata region is cleartext (authenticated as AAD).
/// The payload region is encrypted in place.
fn encode_segment<S: seal::Application>(
    buf: &mut [u8],
    source_control_port: u16,
    source_sender_id: VarInt,
    packet_number: VarInt,
    sealer: &S,
    credentials: &crate::credentials::Credentials,
    flow_attempt_id: &mut VarInt,
    frames: &Queue<Frame>,
    header_buf: &mut Vec<u8>,
) -> usize {
    let routing_info = RoutingInfo::SenderId { source_sender_id };

    // Build the application header: per-frame metadata entries
    let total_payload_len = encode_frame_metadata(frames, flow_attempt_id, header_buf);

    let header_len = VarInt::try_from(header_buf.len() as u64).unwrap_or(VarInt::ZERO);
    let payload_len_varint = VarInt::try_from(total_payload_len as u64).unwrap_or(VarInt::ZERO);

    // Build a concatenated payload reader over all frame payloads
    let mut payload_reader = FramePayloadReader::new(frames);

    let mut header_reader = &header_buf[..];

    datagram::encoder::encode(
        EncoderBuffer::new(buf),
        source_control_port,
        routing_info,
        Some(packet_number),
        header_len,
        &mut header_reader,
        payload_len_varint,
        &mut payload_reader,
        sealer,
        credentials,
    )
}

fn encode_frame_metadata(
    frames: &Queue<Frame>,
    flow_attempt_id: &mut VarInt,
    header_buf: &mut Vec<u8>,
) -> usize {
    header_buf.clear();
    let mut total_payload_len = 0usize;

    for frame in frames.iter() {
        let header = stamp_attempt_id(&frame.header, flow_attempt_id);
        let payload_len = VarInt::try_from(frame.payload_len() as u64).unwrap_or(VarInt::ZERO);
        let entry_size = header.encoding_size() + payload_len.encoding_size();
        let start = header_buf.len();
        header_buf.resize(start + entry_size, 0);
        let mut enc = EncoderBuffer::new(&mut header_buf[start..]);
        enc.encode(&header);
        enc.encode(&payload_len);
        debug_assert_eq!(enc.len(), entry_size);

        total_payload_len += frame.payload_len();
    }

    total_payload_len
}

/// Produce a Header with attempt_id stamped for FlowInit frames.
fn stamp_attempt_id(
    header: &crate::stream3::frame::Header,
    flow_attempt_id: &mut VarInt,
) -> crate::stream3::frame::Header {
    use crate::stream3::frame::Header;
    match header {
        Header::FlowInit {
            source_queue_id,
            dest_acceptor_id,
            attempt_id,
            stream_id,
            is_fin,
        } => {
            let attempt_id = if *attempt_id == VarInt::MAX {
                let id = *flow_attempt_id;
                *flow_attempt_id += 1;
                id
            } else {
                *attempt_id
            };
            Header::FlowInit {
                source_queue_id: *source_queue_id,
                dest_acceptor_id: *dest_acceptor_id,
                attempt_id,
                stream_id: *stream_id,
                is_fin: *is_fin,
            }
        }
        other => *other,
    }
}

/// A Storage reader that concatenates payloads from multiple frames.
///
/// The encoder calls `partial_copy_into` to drain payload bytes into the packet buffer.
/// This implementation iterates through each frame's ByteVec payload in order.
struct FramePayloadReader {
    /// Concatenated payload from all frames, built once at construction.
    inner: crate::byte_vec::ByteVec,
}

impl FramePayloadReader {
    fn new(frames: &Queue<Frame>) -> Self {
        let mut inner = crate::byte_vec::ByteVec::new();
        for frame in frames.iter() {
            if frame.payload_len() > 0 {
                inner.append(&mut frame.payload.clone());
            }
        }
        Self { inner }
    }
}

impl buffer::reader::Storage for FramePayloadReader {
    type Error = core::convert::Infallible;

    #[inline]
    fn buffered_len(&self) -> usize {
        self.inner.len()
    }

    #[inline]
    fn read_chunk(
        &mut self,
        watermark: usize,
    ) -> Result<buffer::reader::storage::Chunk<'_>, Self::Error> {
        self.inner.read_chunk(watermark)
    }

    #[inline]
    fn partial_copy_into<Dest>(
        &mut self,
        dest: &mut Dest,
    ) -> Result<buffer::reader::storage::Chunk<'_>, Self::Error>
    where
        Dest: buffer::writer::Storage + ?Sized,
    {
        self.inner.partial_copy_into(dest)
    }
}

use crate::packet::datagram;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        byte_vec::ByteVec,
        counter::Registry,
        packet::datagram::{QueuePair, ResetTarget},
        path::secret::map::Entry as PathSecretEntry,
        stream3::frame::{Frame, Header, TransmissionStatus, DEFAULT_TTL},
    };
    use bolero::{check, TypeGenerator};
    use bytes::Bytes;
    use core::time::Duration;
    use std::sync::Arc;

    #[derive(Clone, Copy)]
    struct FixedClock(precision::Timestamp);

    impl precision::Clock for FixedClock {
        type Timer = crate::clock::Timer;

        fn now(&self) -> precision::Timestamp {
            self.0
        }

        fn timer(&self) -> Self::Timer {
            unreachable!("tests do not construct timers from FixedClock")
        }
    }

    impl s2n_quic_core::time::Clock for FixedClock {
        fn get_time(&self) -> s2n_quic_core::time::Timestamp {
            self.0.into()
        }
    }

    struct CancelledSender(Vec<Queue<Frame>>);

    impl crate::socket::channel::UnboundedSender<Queue<Frame>> for CancelledSender {
        fn send(&mut self, value: Queue<Frame>) -> Result<(), Queue<Frame>> {
            self.0.push(value);
            Ok(())
        }
    }

    #[derive(Clone, Debug, TypeGenerator)]
    enum HeaderSpec {
        FlowInit {
            source_queue_id: u8,
            dest_acceptor_id: u8,
            fresh_attempt_id: bool,
            stream_id: u8,
            is_fin: bool,
        },
        FlowData {
            source_queue_id: u8,
            dest_queue_id: u8,
            stream_id: u8,
            offset: u16,
            is_fin: bool,
        },
        FlowControl {
            source_queue_id: u8,
            dest_queue_id: u8,
            stream_id: u8,
        },
        FlowReset {
            dest_queue_id: u8,
            stream_id: u8,
            reset_target: ResetTargetSpec,
            error_code: u8,
        },
        FlowInitValidate {
            source_queue_id: u8,
            dest_queue_id: u8,
            attempt_id: u8,
            stream_id: u8,
        },
        FlowValidateRequest {
            dest_sender_id: u8,
            source_queue_id: u8,
            dest_queue_id: u8,
            attempt_id: u8,
            stream_id: u8,
        },
        Control {
            dest_sender_id: u8,
        },
    }

    #[derive(Clone, Copy, Debug, TypeGenerator)]
    enum ResetTargetSpec {
        Both,
        Stream,
        Control,
    }

    #[derive(Clone, Debug, TypeGenerator)]
    struct FrameSpec {
        header: HeaderSpec,
        payload: Vec<u8>,
    }

    impl ResetTargetSpec {
        fn into_reset_target(self) -> ResetTarget {
            match self {
                Self::Both => ResetTarget::Both,
                Self::Stream => ResetTarget::Stream,
                Self::Control => ResetTarget::Control,
            }
        }
    }

    impl HeaderSpec {
        fn into_header(self) -> Header {
            match self {
                Self::FlowInit {
                    source_queue_id,
                    dest_acceptor_id,
                    fresh_attempt_id,
                    stream_id,
                    is_fin,
                } => Header::FlowInit {
                    source_queue_id: VarInt::from_u8(source_queue_id),
                    dest_acceptor_id: VarInt::from_u8(dest_acceptor_id),
                    attempt_id: if fresh_attempt_id {
                        VarInt::MAX
                    } else {
                        VarInt::from_u8(source_queue_id)
                    },
                    stream_id: VarInt::from_u8(stream_id),
                    is_fin,
                },
                Self::FlowData {
                    source_queue_id,
                    dest_queue_id,
                    stream_id,
                    offset,
                    is_fin,
                } => Header::FlowData {
                    queue_pair: QueuePair {
                        source_queue_id: VarInt::from_u8(source_queue_id),
                        dest_queue_id: VarInt::from_u8(dest_queue_id),
                    },
                    stream_id: VarInt::from_u8(stream_id),
                    offset: VarInt::from_u16(offset),
                    is_fin,
                },
                Self::FlowControl {
                    source_queue_id,
                    dest_queue_id,
                    stream_id,
                } => Header::FlowControl {
                    queue_pair: QueuePair {
                        source_queue_id: VarInt::from_u8(source_queue_id),
                        dest_queue_id: VarInt::from_u8(dest_queue_id),
                    },
                    stream_id: VarInt::from_u8(stream_id),
                },
                Self::FlowReset {
                    dest_queue_id,
                    stream_id,
                    reset_target,
                    error_code,
                } => Header::FlowReset {
                    dest_queue_id: VarInt::from_u8(dest_queue_id),
                    stream_id: VarInt::from_u8(stream_id),
                    reset_target: reset_target.into_reset_target(),
                    error_code: VarInt::from_u8(error_code),
                },
                Self::FlowInitValidate {
                    source_queue_id,
                    dest_queue_id,
                    attempt_id,
                    stream_id,
                } => Header::FlowInitValidate {
                    queue_pair: QueuePair {
                        source_queue_id: VarInt::from_u8(source_queue_id),
                        dest_queue_id: VarInt::from_u8(dest_queue_id),
                    },
                    attempt_id: VarInt::from_u8(attempt_id),
                    stream_id: VarInt::from_u8(stream_id),
                },
                Self::FlowValidateRequest {
                    dest_sender_id,
                    source_queue_id,
                    dest_queue_id,
                    attempt_id,
                    stream_id,
                } => Header::FlowValidateRequest {
                    dest_sender_id: VarInt::from_u8(dest_sender_id),
                    queue_pair: QueuePair {
                        source_queue_id: VarInt::from_u8(source_queue_id),
                        dest_queue_id: VarInt::from_u8(dest_queue_id),
                    },
                    attempt_id: VarInt::from_u8(attempt_id),
                    stream_id: VarInt::from_u8(stream_id),
                },
                Self::Control { dest_sender_id } => Header::Control {
                    dest_sender_id: VarInt::from_u8(dest_sender_id),
                },
            }
        }
    }

    fn make_context(mtu: u16) -> (Context, Arc<PathSecretEntry>) {
        let entry = PathSecretEntry::fake("127.0.0.1:8080".parse().unwrap(), None);
        entry.update_max_datagram_size(mtu);
        let registry = Registry::new();
        let gauge = registry.register_queue_gauge("test.inflight");
        (Context::new(&entry, gauge), entry)
    }

    fn to_frame(
        spec: FrameSpec,
        entry: &Arc<PathSecretEntry>,
        mtu: u16,
    ) -> crate::intrusive_queue::Entry<Frame> {
        const MAX_TEST_PAYLOAD_DIVISOR: usize = 4;

        let payload = spec
            .payload
            .into_iter()
            .take((mtu as usize / MAX_TEST_PAYLOAD_DIVISOR).max(1))
            .collect::<Vec<_>>();

        Frame {
            header: spec.header.into_header(),
            source_sender_id: VarInt::MAX,
            payload: payload_vec(&payload),
            path_secret_entry: entry.clone(),
            completion: None,
            status: TransmissionStatus::default(),
            ttl: DEFAULT_TTL,
            transmission_time: None,
        }
        .into()
    }

    fn payload_vec(bytes: &[u8]) -> ByteVec {
        let mut payload = ByteVec::new();
        if !bytes.is_empty() {
            payload.push_back(Bytes::copy_from_slice(bytes));
        }
        payload
    }

    fn assert_gso_invariants(segments: &pool::descriptor::Segments, mtu: u16) {
        let sizes = segments.sizes().collect::<Vec<_>>();
        assert!(!sizes.is_empty());
        assert!(sizes.len() <= segment::MAX_COUNT);
        assert!(segments.total_payload_len() <= segment::MAX_TOTAL);

        let segment_len = sizes[0];
        assert!(segment_len <= mtu);

        for size in sizes.iter().take(sizes.len().saturating_sub(1)) {
            assert_eq!(*size, segment_len);
        }

        assert!(sizes.last().copied().unwrap() <= segment_len);
        assert_eq!(
            sizes.iter().map(|size| *size as usize).sum::<usize>(),
            segments.total_payload_len() as usize
        );
    }

    #[test]
    fn assemble_accounts_for_header_overhead() {
        let mtu = 256;
        let (mut context, entry) = make_context(mtu);
        let clock = FixedClock(precision::Timestamp { nanos: 1_000 });
        let pool = pool::Pool::new(u16::MAX);
        let mut header_buf = Vec::new();
        let mut cancelled = CancelledSender(Vec::new());

        for _ in 0..128 {
            context.push_frame(
                Frame {
                    header: Header::FlowReset {
                        dest_queue_id: VarInt::from_u8(1),
                        stream_id: VarInt::from_u8(1),
                        reset_target: ResetTarget::Both,
                        error_code: VarInt::from_u8(1),
                    },
                    source_sender_id: VarInt::MAX,
                    payload: ByteVec::new(),
                    path_secret_entry: entry.clone(),
                    completion: None,
                    status: TransmissionStatus::default(),
                    ttl: DEFAULT_TTL,
                    transmission_time: None,
                }
                .into(),
            );
        }

        let segments = assemble(
            &mut context,
            &clock,
            VarInt::from_u8(1),
            443,
            &pool,
            &mut header_buf,
            &mut cancelled,
        )
        .expect("frames should assemble");

        assert_gso_invariants(&segments, mtu);
        assert!(context.has_pending(), "header-heavy frames should spill into another batch");
    }

    #[test]
    fn assemble_fuzz_respects_gso_invariants() {
        let mtu = 256;
        const MAX_TEST_FRAMES: usize = segment::MAX_COUNT * 2;
        const TEST_CRYPTO_TAG_LEN: usize = 16;

        check!()
            .with_type::<Vec<FrameSpec>>()
            .with_test_time(Duration::from_secs(10))
            .for_each(|specs| {
                let specs = specs
                    .iter()
                    .take(MAX_TEST_FRAMES)
                    .cloned()
                    .collect::<Vec<_>>();
                let (mut context, entry) = make_context(mtu);
                let clock = FixedClock(precision::Timestamp { nanos: 1_000 });
                let pool = pool::Pool::new(u16::MAX);
                let mut header_buf = Vec::new();
                let mut cancelled = CancelledSender(Vec::new());

                for spec in specs {
                    let frame = to_frame(spec, &entry, mtu);
                    let mut single = Queue::new();
                    single.push_back(frame);

                    if estimate_segment_len(
                        VarInt::from_u8(1),
                        VarInt::ZERO,
                        context.flow_attempt_id_counter,
                        &single,
                        &mut header_buf,
                        TEST_CRYPTO_TAG_LEN,
                    ) <= mtu as usize
                    {
                        context.push_frame(
                            single
                                .pop_front()
                                .expect("single-frame queue should contain one frame"),
                        );
                    }
                }

                let mut batches = 0usize;
                while context.has_pending() {
                    let segments = assemble(
                        &mut context,
                        &clock,
                        VarInt::from_u8(1),
                        443,
                        &pool,
                        &mut header_buf,
                        &mut cancelled,
                    )
                    .expect("assemble should make progress for bounded test inputs");

                    assert_gso_invariants(&segments, mtu);
                    context.cca = crate::congestion::Controller::new(mtu);
                    batches += 1;
                    assert!(batches <= MAX_TEST_FRAMES);
                }
            });
    }
}
