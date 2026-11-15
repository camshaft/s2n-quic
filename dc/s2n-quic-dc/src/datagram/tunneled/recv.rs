// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    crypto::{open, UninitSlice},
    packet::datagram::{decoder, Tag},
};
use s2n_codec::{decoder_invariant, DecoderBufferMut, DecoderError};
use s2n_quic_core::packet::number::{PacketNumberSpace, SlidingWindow, SlidingWindowError};

pub use crate::crypto::open::Error;
pub use decoder::Packet;

#[derive(Default)]
pub struct Endpoint {}

impl Endpoint {
    //= rendered/0009-tunneled-datagrams/03-receiving.md#DC-0009-020
    //# A datagram MAY carry more than one tunneled datagram packet, each starting at the byte
    //# after the end of the one before it.
    pub fn parse<'a>(&self, payload: &'a mut [u8]) -> Option<(Packet<&'a mut [u8]>, &'a mut [u8])> {
        let buffer = DecoderBufferMut::new(payload);

        //= rendered/0009-tunneled-datagrams/03-receiving.md#DC-0009-018
        //# A receiver MUST read the last 16 bytes of each packet as its authentication tag.
        let (packet, buffer) = Packet::decode(buffer, TagValidator, 16).ok()?;
        let buffer = buffer.into_less_safe_slice();
        Some((packet, buffer))
    }
}

struct TagValidator;

impl decoder::Validator for TagValidator {
    #[inline]
    fn validate_tag(&mut self, tag: Tag) -> Result<(), DecoderError> {
        //= rendered/0009-tunneled-datagrams/03-receiving.md#DC-0009-016
        //# A receiver MUST reject a packet whose routing information bit is set.
        decoder_invariant!(!tag.has_routing_info(), "expected tunnelled datagram");

        //= rendered/0009-tunneled-datagrams/03-receiving.md#DC-0009-032
        //# A receiver MUST reject a packet whose packet number bit is clear.
        decoder_invariant!(tag.has_packet_number(), "expected a packet number");

        //= rendered/0009-tunneled-datagrams/03-receiving.md#DC-0009-017
        //# A receiver MUST reject a packet whose application header bit is set.
        decoder_invariant!(
            !tag.payload_encrypted(),
            "partial encryption currently unsupported"
        );
        Ok(())
    }
}

pub struct Receiver<K: open::Application> {
    key: K,
}

impl<K: open::Application> Receiver<K> {
    pub fn new(key: K) -> Self {
        Self { key }
    }

    //= rendered/0009-tunneled-datagrams/03-receiving.md#DC-0009-019
    //# A receiver MUST NOT hand a payload to the application unless the packet's authentication
    //# tag verifies over its header and its payload, under the receiver's key and with its packet
    //# number as the nonce.
    //= rendered/0009-tunneled-datagrams/05-delivery.md#DC-0009-027
    //# A receiver MUST hand the application each payload it accepts exactly as the sender's caller
    //# gave it.
    pub fn recv_into(
        &mut self,
        packet: &Packet<&mut [u8]>,
        payload_out: &mut UninitSlice,
    ) -> Result<(), Error> {
        debug_assert_eq!(packet.payload().len(), payload_out.len());

        self.key.decrypt(
            packet.tag().key_phase(),
            packet.crypto_nonce(),
            packet.header(),
            packet.payload(),
            packet.auth_tag(),
            payload_out,
        )?;

        Ok(())
    }

    /// Opens `packet` into `payload_out`, then checks its packet number against `seen` and
    /// records it there. The payload is the application's only where both succeed.
    pub fn recv_into_checked(
        &mut self,
        seen: &mut SeenFilter,
        packet: &Packet<&mut [u8]>,
        payload_out: &mut UninitSlice,
    ) -> Result<(), Error> {
        self.recv_into(packet, payload_out)?;

        //= rendered/0009-tunneled-datagrams/04-replay.md#DC-0009-021
        //# A receiver MUST, once a packet's authentication tag verifies, check its packet number
        //# against its replay window and record it there in one step, and reject the packet where
        //# that step finds the number accepted before.
        seen.on_packet(packet).map_err(|error| match error {
            SlidingWindowError::Duplicate => Error::ReplayDefinitelyDetected,
            SlidingWindowError::TooOld => Error::ReplayPotentiallyDetected { gap: None },
        })
    }
}

//= rendered/0009-tunneled-datagrams/04-replay.md#DC-0009-024
//# A receiver MUST keep one replay window for each sender's packet numbers, holding the numbers
//# accepted under every key phase it can still open.
#[derive(Default)]
pub struct SeenFilter {
    window: SlidingWindow,
}

impl SeenFilter {
    //= rendered/0009-tunneled-datagrams/04-replay.md#DC-0009-022
    //# A receiver MUST reject a packet whose packet number is older than the oldest number its
    //# replay window holds.
    //= rendered/0009-tunneled-datagrams/04-replay.md#DC-0009-023
    //# A receiver MUST accept, in its replay check, a packet whose packet number is newer than
    //# every number it accepted before.
    //= rendered/0009-tunneled-datagrams/05-delivery.md#DC-0009-026
    //# A receiver MAY hand payloads to the application in another order than the order in which
    //# they were sent.
    #[inline]
    pub fn on_packet(&mut self, packet: &Packet<&mut [u8]>) -> Result<(), SlidingWindowError> {
        // The window holds packet numbers alone, whatever the key phase of the packet.
        let packet_number =
            PacketNumberSpace::ApplicationData.new_packet_number(packet.packet_number());
        self.window.insert(packet_number)
    }
}
