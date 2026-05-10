// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Decode multi-frame packets produced by the stream3 encoder.
//!
//! The stream3 assembler packs multiple frames into one encrypted datagram:
//!
//! ```text
//! [packet header: tag, credentials, wire_version, src_ctrl_port, pkt_number]
//! [RoutingInfo::SenderId { source_sender_id }]
//! [payload_len varint]
//! [header_len varint][frame metadata: per-frame Header + optional payload_len]
//! --- encrypted ---
//! [frame payloads concatenated]
//! [auth tag]
//! ```
//!
//! After the outer packet has been decrypted in place, the decrypted buffer holds:
//!   `[application_header bytes][frame payload bytes]`
//!
//! This module decodes the application header (frame metadata region) and pairs each
//! frame header with its corresponding slice of the payload bytes.

use crate::stream3::frame::Header;
use s2n_codec::{decoder_invariant, DecoderBuffer, DecoderError};
use s2n_quic_core::varint::VarInt;

/// A frame extracted from the application header / payload regions of a decoded packet.
#[derive(Debug)]
pub(crate) struct DecodedFrame<'a> {
    pub header: Header,
    /// Slice into the decrypted payload corresponding to this frame's data.
    ///
    /// The slice is zero-length for frame types where `has_payload_length()` is false
    /// (FlowReset, FlowInitValidate, FlowValidateRequest).
    pub payload: &'a [u8],
}

/// Decode all frames from the application header and payload regions.
///
/// `application_header` contains the per-frame metadata (header type + optional payload_len).
/// `payload` contains the concatenated, already-decrypted frame payloads.
///
/// Returns the decoded frames on success. Returns a `DecoderError` if the metadata or
/// payload bytes are malformed or inconsistent (e.g., lengths exceed the available data).
pub(crate) fn decode_frames<'a>(
    application_header: &[u8],
    payload: &'a [u8],
) -> Result<Vec<DecodedFrame<'a>>, DecoderError> {
    let mut metadata = DecoderBuffer::new(application_header);
    let mut frames = Vec::new();
    let mut payload_offset = 0usize;

    while !metadata.is_empty() {
        let (header, rest) = metadata.decode::<Header>()?;
        metadata = rest;

        let payload_len = if header.has_payload_length() {
            let (len, rest) = metadata.decode::<VarInt>()?;
            metadata = rest;
            len.as_u64() as usize
        } else {
            0
        };

        let next_offset = payload_offset + payload_len;
        decoder_invariant!(
            next_offset <= payload.len(),
            "frame payload length exceeds available payload bytes"
        );

        let frame_payload = &payload[payload_offset..next_offset];
        payload_offset = next_offset;

        frames.push(DecodedFrame {
            header,
            payload: frame_payload,
        });
    }

    decoder_invariant!(
        payload_offset == payload.len(),
        "payload bytes not fully consumed by frame metadata"
    );

    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::datagram::{QueuePair, ResetTarget};
    use crate::stream3::frame::Header;
    use s2n_codec::{Encoder, EncoderBuffer, EncoderValue};
    use s2n_quic_core::varint::VarInt;

    /// Encode a single frame entry into the application header buffer, mirroring
    /// the format written by `assemble::push_frame_metadata`.
    fn push_frame_metadata(buf: &mut Vec<u8>, header: &Header, payload_len: usize) {
        let payload_len_varint = VarInt::try_from(payload_len as u64).unwrap_or(VarInt::ZERO);
        let entry_size = if header.has_payload_length() {
            header.encoding_size() + payload_len_varint.encoding_size()
        } else {
            debug_assert_eq!(payload_len, 0);
            header.encoding_size()
        };
        let start = buf.len();
        buf.resize(start + entry_size, 0);
        let mut enc = EncoderBuffer::new(&mut buf[start..]);
        enc.encode(header);
        if header.has_payload_length() {
            enc.encode(&payload_len_varint);
        }
    }

    struct FrameSpec {
        header: Header,
        payload: Vec<u8>,
    }

    fn encode_frames(specs: &[FrameSpec]) -> (Vec<u8>, Vec<u8>) {
        let mut app_header = Vec::new();
        let mut payload = Vec::new();
        for spec in specs {
            let plen = if spec.header.has_payload_length() {
                spec.payload.len()
            } else {
                0
            };
            push_frame_metadata(&mut app_header, &spec.header, plen);
            if spec.header.has_payload_length() {
                payload.extend_from_slice(&spec.payload);
            }
        }
        (app_header, payload)
    }

    #[test]
    fn round_trip_single_flow_data() {
        let header = Header::FlowData {
            queue_pair: QueuePair {
                source_queue_id: VarInt::from_u8(1),
                dest_queue_id: VarInt::from_u8(2),
            },
            stream_id: VarInt::from_u8(42),
            offset: VarInt::ZERO,
            is_fin: false,
        };
        let data = b"hello world";
        let specs = [FrameSpec {
            header,
            payload: data.to_vec(),
        }];
        let (app_header, payload) = encode_frames(&specs);
        let frames = decode_frames(&app_header, &payload).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].header, header);
        assert_eq!(frames[0].payload, data);
    }

    #[test]
    fn round_trip_multiple_frames() {
        let specs = vec![
            FrameSpec {
                header: Header::FlowData {
                    queue_pair: QueuePair {
                        source_queue_id: VarInt::from_u8(1),
                        dest_queue_id: VarInt::from_u8(2),
                    },
                    stream_id: VarInt::from_u8(10),
                    offset: VarInt::ZERO,
                    is_fin: false,
                },
                payload: b"stream10".to_vec(),
            },
            FrameSpec {
                header: Header::FlowReset {
                    dest_queue_id: VarInt::from_u8(3),
                    stream_id: VarInt::from_u8(20),
                    reset_target: ResetTarget::Both,
                    error_code: VarInt::from_u8(1),
                },
                payload: vec![],
            },
            FrameSpec {
                header: Header::FlowData {
                    queue_pair: QueuePair {
                        source_queue_id: VarInt::from_u8(4),
                        dest_queue_id: VarInt::from_u8(5),
                    },
                    stream_id: VarInt::from_u8(30),
                    offset: VarInt::from_u8(8),
                    is_fin: true,
                },
                payload: b"fin data".to_vec(),
            },
        ];

        let (app_header, payload) = encode_frames(&specs);
        let frames = decode_frames(&app_header, &payload).unwrap();

        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].header, specs[0].header);
        assert_eq!(frames[0].payload, b"stream10");
        assert_eq!(frames[1].header, specs[1].header);
        assert_eq!(frames[1].payload, b"");
        assert_eq!(frames[2].header, specs[2].header);
        assert_eq!(frames[2].payload, b"fin data");
    }

    #[test]
    fn empty_application_header_empty_payload() {
        let frames = decode_frames(&[], &[]).unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    fn payload_length_mismatch_returns_error() {
        let header = Header::FlowData {
            queue_pair: QueuePair {
                source_queue_id: VarInt::ZERO,
                dest_queue_id: VarInt::ZERO,
            },
            stream_id: VarInt::ZERO,
            offset: VarInt::ZERO,
            is_fin: false,
        };
        // Claim 100-byte payload but supply only 5
        let mut app_header = Vec::new();
        push_frame_metadata(&mut app_header, &header, 100);
        let payload = b"short".to_vec();
        let result = decode_frames(&app_header, &payload);
        assert!(result.is_err(), "expected error for payload underflow");
    }

    #[test]
    fn leftover_payload_bytes_returns_error() {
        let header = Header::FlowData {
            queue_pair: QueuePair {
                source_queue_id: VarInt::ZERO,
                dest_queue_id: VarInt::ZERO,
            },
            stream_id: VarInt::ZERO,
            offset: VarInt::ZERO,
            is_fin: false,
        };
        // Claim 5-byte payload but supply 10
        let mut app_header = Vec::new();
        push_frame_metadata(&mut app_header, &header, 5);
        let payload = b"0123456789".to_vec();
        let result = decode_frames(&app_header, &payload);
        assert!(result.is_err(), "expected error for leftover payload bytes");
    }
}

