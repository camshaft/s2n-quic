// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use s2n_quic_core::{
    packet::number::{PacketNumberSpace, SlidingWindow, SlidingWindowError},
    varint::VarInt,
};

#[derive(Debug, Default)]
pub struct StreamFilter {
    window: SlidingWindow,
}

impl StreamFilter {
    #[inline]
    pub fn on_packet_number(&mut self, packet_number: VarInt) -> Result<(), SlidingWindowError> {
        let packet_number = PacketNumberSpace::Initial.new_packet_number(packet_number);
        self.window.insert(packet_number)
    }

    /// Checks `packet_number` against the packet numbers this filter accepted, without
    /// recording it.
    #[inline]
    pub fn check(&self, packet_number: VarInt) -> Result<(), SlidingWindowError> {
        let packet_number = PacketNumberSpace::Initial.new_packet_number(packet_number);
        self.window.check(packet_number)
    }
}
