// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    credentials,
    credentials::Credentials,
    crypto::seal,
    packet::{
        datagram::{RoutingInfo, Tag},
        WireVersion,
    },
};
use s2n_codec::{Encoder, EncoderBuffer};
use s2n_quic_core::{assume, buffer};

#[inline(always)]
pub fn estimate_len(
    _packet_number: super::PacketNumber,
    routing_info: RoutingInfo,
    app_header_len: super::HeaderLen,
    payload_len: super::PayloadLen,
    crypto_tag_len: usize,
) -> usize {
    let app_header_len_usize = *app_header_len as usize;
    let payload_len_usize = *payload_len as usize;

    let mut encoder = s2n_codec::EncoderLenEstimator::new(usize::MAX);

    let mut tag = Tag::default();
    tag.set_has_routing_info(!matches!(routing_info, RoutingInfo::None));
    encoder.encode(&tag);

    // credentials
    {
        encoder.write_zerocopy::<credentials::Id, _>(|_| {});
        encoder.write_repeated(8, 0);
    }
    // wire version
    encoder.encode(&WireVersion::ZERO);
    encoder.encode(&0u16); // source control port
    encoder.write_repeated(8, 0); // packet number

    // routing info
    if tag.has_routing_info() {
        encoder.encode(&routing_info);
    }

    encoder.write_repeated(8, 0); // payload len

    if app_header_len_usize > 0 {
        encoder.write_repeated(8, 0); // application header len
        encoder.write_repeated(app_header_len_usize, 0); // application data
    }

    encoder.write_repeated(payload_len_usize, 0);

    encoder.write_repeated(crypto_tag_len, 0);

    encoder.len()
}

#[inline(always)]
pub fn encode<H, P, C>(
    mut encoder: EncoderBuffer,
    source_control_port: u16,
    routing_info: RoutingInfo,
    packet_number: Option<super::PacketNumber>,
    header_len: super::HeaderLen,
    header: &mut H,
    payload_len: super::PayloadLen,
    payload: &mut P,
    crypto: &C,
    credentials: &Credentials,
) -> usize
where
    H: buffer::reader::Storage<Error = core::convert::Infallible>,
    P: buffer::reader::Storage<Error = core::convert::Infallible>,
    C: seal::Application,
{
    //= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-007
    //# The first byte of a tunneled datagram packet MUST hold 0100 in its high four bits, and in
    //# its low four bits, from the highest, the routing information bit, the packet number bit, the
    //# application header bit and the key phase bit.
    let mut tag = super::Tag::default();
    tag.set_has_routing_info(!matches!(routing_info, RoutingInfo::None));
    tag.set_has_packet_number(packet_number.is_some());
    tag.set_payload_encrypted(header_len != super::HeaderLen::ZERO);

    //= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-030
    //# A sender MUST write the phase of the key that seals a packet in the packet's key phase
    //# bit.
    tag.set_key_phase(crypto.key_phase());
    encoder.encode(&tag);

    let header_len_usize = *header_len as usize;
    let payload_len_usize = *payload_len as usize;
    let nonce = *packet_number.unwrap_or(super::PacketNumber::ZERO);

    //= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-011
    //# A tunneled datagram packet MUST carry, after its first byte and in this order, the
    //# credentials of the key that sealed it, the wire version, the sender's control port, the
    //# packet number, the payload's length, the payload and the authentication tag.

    // encode the credentials being used
    encoder.encode(credentials);

    //= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-012
    //# A sender MUST write wire version zero in each tunneled datagram packet it sends.

    // wire version - we only support `0` currently
    encoder.encode(&WireVersion::ZERO);

    encoder.encode(&source_control_port);

    if let Some(packet_number) = packet_number {
        unsafe {
            assume!(encoder.remaining_capacity() >= 8);
            encoder.encode(&packet_number);
        }
    }

    // Encode routing info if present
    if tag.has_routing_info() {
        encoder.encode(&routing_info);
    }

    unsafe {
        assume!(encoder.remaining_capacity() >= 8);
        encoder.encode(&payload_len);
    }

    if !header.buffer_is_empty() {
        unsafe {
            assume!(encoder.remaining_capacity() >= 8);
            encoder.encode(&header_len);
        }
        encoder.write_sized(header_len_usize, |mut dest| {
            let _: Result<(), core::convert::Infallible> = header.copy_into(&mut dest);
        });
    }

    let payload_offset = encoder.len();

    let mut last_chunk = buffer::reader::storage::Chunk::empty();
    encoder.write_sized(payload_len_usize, |mut dest| {
        let result: Result<buffer::reader::storage::Chunk, core::convert::Infallible> =
            payload.partial_copy_into(&mut dest);
        last_chunk = result.expect("copy is infallible");
    });

    let last_chunk = if last_chunk.is_empty() {
        None
    } else {
        Some(&last_chunk[..])
    };

    encoder.advance_position(crypto.tag_len());

    let packet_len = encoder.len();

    let slice = encoder.as_mut_slice();
    let (header, payload_and_tag) = unsafe {
        assume!(slice.len() >= payload_offset);
        slice.split_at_mut(payload_offset)
    };

    //= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-014
    //# A sender MUST encrypt the whole payload of each packet.
    //= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-028
    //# A sender MUST authenticate the whole header of each packet together with its payload.
    //= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-029
    //# A sender MUST use the packet's packet number as the nonce of its encryption.
    crypto.encrypt(nonce, header, last_chunk, payload_and_tag);

    packet_len
}
