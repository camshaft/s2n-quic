// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use s2n_quic_core::varint::VarInt;

pub const INDEX_BITS: u32 = 25;
pub const INDEX_MASK: u64 = (1u64 << INDEX_BITS) - 1;
pub const GENERATION_BITS: u32 = 62 - INDEX_BITS;
pub const GENERATION_MASK: u64 = (1u64 << GENERATION_BITS) - 1;
pub const MAX_SLOTS: usize = 1usize << INDEX_BITS;

#[inline]
pub fn encode(index: usize, generation: u64) -> VarInt {
    debug_assert!(index < MAX_SLOTS);
    let value = ((generation & GENERATION_MASK) << INDEX_BITS) | index as u64;
    unsafe { VarInt::new_unchecked(value) }
}

#[inline]
pub fn index(queue_id: VarInt) -> usize {
    (queue_id.as_u64() & INDEX_MASK) as usize
}
