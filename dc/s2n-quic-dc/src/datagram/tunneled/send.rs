// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    control,
    credentials::Credentials,
    crypto::seal,
    packet::{
        self,
        datagram::{encoder, RoutingInfo},
    },
};
use core::sync::atomic::{AtomicU64, Ordering};
use s2n_codec::EncoderBuffer;
use s2n_quic_core::{ensure, varint::VarInt};

#[derive(Clone, Copy, Debug)]
pub enum Error {
    PayloadTooLarge,
    PacketBufferTooSmall,
    PacketNumberExhaustion,
}

pub struct Sender<E> {
    encrypt_key: E,
    credentials: Credentials,
    packet_number: AtomicU64,
}

impl<E> Sender<E>
where
    E: seal::Application,
{
    #[inline]
    pub fn new(encrypt_key: E, credentials: Credentials) -> Self {
        Self {
            encrypt_key,
            credentials,
            packet_number: AtomicU64::new(0),
        }
    }

    /// Sets the packet number of the next send, for tests that reach the end of the packet
    /// number space.
    #[cfg(test)]
    pub(super) fn set_next_packet_number(&self, packet_number: u64) {
        self.packet_number.store(packet_number, Ordering::Relaxed);
    }

    //= rendered/0009-tunneled-datagrams/01-sending.md#DC-0009-006
    //# A sender MUST answer a caller, before any send, the most bytes the packet of a payload of a
    //# given length can take, or that a payload of that length is too long to send.
    #[inline]
    pub fn estimated_send_size(&self, cleartext_payload_len: usize) -> Option<usize> {
        let payload_len = payload_len(cleartext_payload_len).ok()?;
        Some(encoder::estimate_len(
            VarInt::ZERO,
            RoutingInfo::None, // No routing info for tunneled datagrams
            VarInt::ZERO,
            payload_len,
            E::tag_len(&self.encrypt_key),
        ))
    }

    //= rendered/0009-tunneled-datagrams/05-delivery.md#DC-0009-025
    //# A sender MUST NOT send a tunneled datagram a second time after its first send.
    #[inline]
    pub fn send_into<C>(
        &self,
        control_port: &C,
        mut cleartext_payload: &[u8],
        encrypted_packet: &mut [u8],
    ) -> Result<usize, Error>
    where
        C: control::Controller,
    {
        //= rendered/0009-tunneled-datagrams/01-sending.md#DC-0009-001
        //# A sender MUST give each datagram it seals under a key a packet number that no earlier
        //# datagram under that key had, whichever of its callers sends it, and whether or not
        //# callers send at the same time.
        //= rendered/0009-tunneled-datagrams/01-sending.md#DC-0009-002
        //# A sender MUST take zero as the packet number of its first send, and one more than the
        //# number of the send before it as the number of each later send, whether or not the send
        //# before it failed.
        let packet_number = self.packet_number.fetch_add(1, Ordering::Relaxed);

        //= rendered/0009-tunneled-datagrams/01-sending.md#DC-0009-003
        //# A sender MUST NOT seal a datagram whose packet number is larger than a 62-bit
        //# variable-length integer holds, and fails that send instead.
        let packet_number =
            VarInt::new(packet_number).map_err(|_| Error::PacketNumberExhaustion)?;

        //= rendered/0009-tunneled-datagrams/01-sending.md#DC-0009-004
        //# A sender MUST fail a send whose payload is longer than the packet's payload length field
        //# can state.
        let payload_len = payload_len(cleartext_payload.len())?;

        let estimated_packet_len = self
            .estimated_send_size(cleartext_payload.len())
            .ok_or(Error::PayloadTooLarge)?;

        //= rendered/0009-tunneled-datagrams/01-sending.md#DC-0009-005
        //# A sender MUST fail a send, without writing to the caller's buffer, where that buffer is
        //# shorter than the packet the send would write.
        ensure!(
            encrypted_packet.len() >= estimated_packet_len,
            Err(Error::PacketBufferTooSmall)
        );

        //= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-008
        //# A tunneled datagram packet MUST NOT carry routing information, and its routing
        //# information bit is clear.
        //= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-009
        //# A tunneled datagram packet MUST carry its packet number, and its packet number bit is
        //# set.
        //= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-010
        //# A tunneled datagram packet MUST NOT carry an application header, and its application
        //# header bit is clear.
        let actual_packet_len = {
            //= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-013
            //# A sender MUST write as the packet's control port the source port of the control
            //# path its caller passes to the send.
            let source_control_port = control_port.source_port();

            let encoder = EncoderBuffer::new(encrypted_packet);

            encoder::encode(
                encoder,
                source_control_port,
                RoutingInfo::None, // No routing info for tunneled datagrams
                Some(packet_number),
                VarInt::ZERO,
                &mut &[][..],
                payload_len,
                &mut cleartext_payload,
                &self.encrypt_key,
                &self.credentials,
            )
        };

        Ok(actual_packet_len)
    }
}

/// The value of the packet's payload length field for a payload of `len` bytes.
#[inline]
pub(super) fn payload_len(len: usize) -> Result<packet::PayloadLen, Error> {
    packet::PayloadLen::try_from(len).map_err(|_| Error::PayloadTooLarge)
}
