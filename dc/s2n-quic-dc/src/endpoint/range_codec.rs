// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Range encoder/decoder — zero-allocation, ACK-style format.
//!
//! Reusable for any VarInt range set (QueueFree, ACKs, etc). Encodes ranges as
//! (gap, range_length) VarInt pairs working downward from the largest value.
//! No range_count prefix — payload length determines end.
//!
//! Format:
//!   first_range: VarInt  — largest - smallest_in_first_range
//!   [gap: VarInt, range_length: VarInt]*
//!
//! Gap = previous_smallest - current_end - 2 (same as RFC 9000 §19.3.1)
//! Range = end - start (number of values beyond the first in this range)

use s2n_codec::{DecoderBuffer, DecoderValue, Encoder, EncoderValue};
use s2n_quic_core::varint::VarInt;

/// Encode freed queue_id ranges directly into an encoder. Zero allocations.
///
/// `largest` is the max queue_id (goes in frame header).
/// `ranges` must yield ranges in DESCENDING order (largest first).
#[inline]
pub fn encode<E: Encoder>(
    largest: VarInt,
    ranges: impl Iterator<Item = core::ops::RangeInclusive<VarInt>>,
    buffer: &mut E,
) {
    let mut prev_smallest: Option<VarInt> = None;

    for range in ranges {
        let (start, end) = range.into_inner();

        if prev_smallest.is_none() {
            // First range: distance from largest to start
            let first_range = largest - start;
            buffer.encode(&first_range);
        } else {
            // Subsequent: gap then range_length (same formula as QUIC ACK)
            let gap = prev_smallest.unwrap() - end - VarInt::from_u8(2);
            let range_len = end - start;
            buffer.encode(&gap);
            buffer.encode(&range_len);
        }

        prev_smallest = Some(start);
    }
}

/// Lazy decoder: iterates over ranges without allocating.
///
/// Yields ranges in DESCENDING order (largest first), matching the encoding.
#[derive(Clone, Copy)]
pub struct RangeDecoder<'a> {
    largest: VarInt,
    payload: DecoderBuffer<'a>,
    first: bool,
    prev_smallest: VarInt,
}

impl<'a> RangeDecoder<'a> {
    #[inline]
    pub fn new(largest: VarInt, payload: &'a [u8]) -> Self {
        Self {
            largest,
            payload: DecoderBuffer::new(payload),
            first: true,
            prev_smallest: VarInt::ZERO,
        }
    }
}

impl Iterator for RangeDecoder<'_> {
    type Item = core::ops::RangeInclusive<VarInt>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.first {
            self.first = false;

            if self.payload.is_empty() {
                // No payload — single value (largest only)
                return Some(self.largest..=self.largest);
            }

            let (first_range, buffer) = self.payload.decode::<VarInt>().ok()?;
            self.payload = buffer;

            let start = self.largest.checked_sub(first_range)?;
            self.prev_smallest = start;
            return Some(start..=self.largest);
        }

        if self.payload.is_empty() {
            return None;
        }

        let (gap, buffer) = self.payload.decode::<VarInt>().ok()?;
        let (range_len, buffer) = buffer.decode::<VarInt>().ok()?;
        self.payload = buffer;

        // largest = previous_smallest - gap - 2
        let end = self.prev_smallest.checked_sub(gap + VarInt::from_u8(2))?;
        let start = end.checked_sub(range_len)?;
        self.prev_smallest = start;

        Some(start..=end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use s2n_quic_core::interval_set::IntervalSet;

    fn encode_to_vec(largest: VarInt, ranges: impl Iterator<Item = core::ops::RangeInclusive<VarInt>>) -> Vec<u8> {
        let mut buf = vec![0u8; 256];
        let len = {
            let mut encoder = s2n_codec::EncoderBuffer::new(&mut buf);
            encode(largest, ranges, &mut encoder);
            Encoder::len(&encoder)
        };
        buf.truncate(len);
        buf
    }

    #[test]
    fn single_value_no_payload() {
        let decoder = RangeDecoder::new(VarInt::from_u8(42), &[]);
        let ranges: Vec<_> = decoder.collect();
        assert_eq!(ranges.len(), 1);
        assert_eq!(*ranges[0].start(), VarInt::from_u8(42));
        assert_eq!(*ranges[0].end(), VarInt::from_u8(42));
    }

    #[test]
    fn encode_decode_round_trip() {
        let mut set = IntervalSet::new();
        let _ = set.insert(VarInt::from_u8(3)..=VarInt::from_u8(5));
        let _ = set.insert(VarInt::from_u8(8)..=VarInt::from_u8(10));

        let largest = set.max_value().unwrap();
        // IntervalSet iterates ascending; collect and reverse for encode
        let ranges_desc: Vec<_> = set.inclusive_ranges()
            .map(|r| *r.start()..=*r.end())
            .collect::<Vec<_>>();

        let buf = encode_to_vec(largest, ranges_desc.into_iter().rev());

        let decoder = RangeDecoder::new(largest, &buf);
        let decoded: Vec<_> = decoder.collect();

        assert_eq!(decoded.len(), 2);
        assert_eq!(*decoded[0].start(), VarInt::from_u8(8));
        assert_eq!(*decoded[0].end(), VarInt::from_u8(10));
        assert_eq!(*decoded[1].start(), VarInt::from_u8(3));
        assert_eq!(*decoded[1].end(), VarInt::from_u8(5));
    }

    #[test]
    fn contiguous_range() {
        let mut set = IntervalSet::new();
        let _ = set.insert(VarInt::from_u8(0)..=VarInt::from_u8(7));

        let largest = VarInt::from_u8(7);
        let buf = encode_to_vec(largest, core::iter::once(VarInt::from_u8(0)..=VarInt::from_u8(7)));

        let decoder = RangeDecoder::new(largest, &buf);
        let decoded: Vec<_> = decoder.collect();

        assert_eq!(decoded.len(), 1);
        assert_eq!(*decoded[0].start(), VarInt::from_u8(0));
        assert_eq!(*decoded[0].end(), VarInt::from_u8(7));
    }
}
