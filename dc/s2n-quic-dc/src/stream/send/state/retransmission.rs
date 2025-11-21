// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{socket::pool::descriptor, stream::send::state::transmission::Flags};
use core::cmp::Ordering;
use s2n_quic_core::varint::VarInt;

#[derive(Debug)]
pub struct Segment {
    pub descriptor: descriptor::Filled,
    pub stream_offset: VarInt,
    pub payload_len: u16,
    pub flags: Flags,
}

impl Segment {
    pub fn range(&self) -> core::ops::Range<VarInt> {
        let start = self.stream_offset;
        let end = start + VarInt::from_u16(self.payload_len);
        start..end
    }
}

impl PartialEq for Segment {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Segment {}

impl PartialOrd for Segment {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Segment {
    #[inline]
    fn cmp(&self, rhs: &Self) -> Ordering {
        self.stream_offset
            .cmp(&rhs.stream_offset)
            .then(self.payload_len.cmp(&rhs.payload_len))
            .reverse()
    }
}
