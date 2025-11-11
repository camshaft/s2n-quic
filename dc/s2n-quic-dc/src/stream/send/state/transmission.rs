// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    packet::stream::PacketSpace,
    socket::{
        pool::descriptor,
        send::{self, completion},
    },
    stream::send::{shared, transmission::Type},
};
use core::ops::Bound;
use s2n_quic_core::{inet::ExplicitCongestionNotification, time::Timestamp, varint::VarInt};
use std::sync::Weak;

pub type Completion = Weak<dyn crate::stream::send::shared::AsShared>;

pub type Entry = send::transmission::Entry<PacketInfo, Meta, Completion>;

pub type Transmission = send::transmission::Transmission<PacketInfo, Meta, Completion>;

pub type Builder = send::transmission::Builder<PacketInfo, Meta, Completion>;

pub type Wheel<const GRANULARITY_US: u64> =
    send::wheel::Wheel<PacketInfo, Meta, Completion, GRANULARITY_US>;

pub type PacketInfo = (VarInt, Info);

pub struct Event {
    pub packet_number: VarInt,
    pub info: Info,
    pub meta: Meta,
}

pub type Queue = completion::Queue<PacketInfo, Meta, Weak<dyn shared::AsShared>>;
pub type CompleteTransmission<'a> = completion::CompleteTransmission<'a, PacketInfo, Meta>;

#[derive(Debug)]
pub struct Meta {
    pub packet_space: PacketSpace,
    pub has_more_app_data: bool,
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            packet_space: PacketSpace::Stream,
            has_more_app_data: false,
        }
    }
}

#[derive(Debug)]
pub struct Info {
    pub packet_len: u16,
    pub descriptor: Option<descriptor::Filled>,
    pub stream_offset: VarInt,
    pub payload_len: u16,
    pub included_fin: bool,
    pub time_sent: Timestamp,
    pub ecn: ExplicitCongestionNotification,
}

impl Info {
    #[inline]
    pub fn cca_len(&self) -> u16 {
        if self.payload_len == 0 {
            self.packet_len
        } else {
            self.payload_len
        }
    }

    pub fn is_probe(&self) -> bool {
        self.payload_len == 0
    }

    #[inline]
    pub fn range(&self) -> core::ops::Range<VarInt> {
        self.stream_offset..self.end_offset()
    }

    /// Similar to range but extends to [`VarInt::MAX`] if `included_fin` is true
    #[inline]
    pub fn tracking_range(&self) -> (Bound<VarInt>, Bound<VarInt>) {
        let start = Bound::Included(self.stream_offset);
        let end = if self.included_fin {
            Bound::Included(VarInt::MAX)
        } else {
            Bound::Excluded(self.end_offset())
        };
        (start, end)
    }

    /// Non-inclusive offset
    #[inline]
    pub fn end_offset(&self) -> VarInt {
        self.stream_offset + VarInt::from_u16(self.payload_len)
    }
}

impl Info {
    #[inline]
    pub fn try_retransmit(&mut self) -> Option<super::retransmission::Segment> {
        let descriptor = self.descriptor.take()?;

        let retransmission = super::retransmission::Segment {
            descriptor,
            stream_offset: self.stream_offset,
            payload_len: self.payload_len,
            ty: Type::Stream,
            included_fin: self.included_fin,
        };

        Some(retransmission)
    }
}
