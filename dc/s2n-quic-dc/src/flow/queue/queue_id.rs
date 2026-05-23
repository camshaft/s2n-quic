// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use s2n_quic_core::varint::VarInt;

/// Maximum number of queue slots supported.
///
/// Queue IDs are plain slot indices — anti-ABA protection is provided by the
/// binding_id field carried alongside the queue_id on the wire.
pub const MAX_SLOTS: usize = 1usize << 25;

/// Encode a slot index as a queue ID.
///
/// In the new protocol, queue_id IS the slot index. No generation interleaving.
#[inline]
pub fn encode(index: usize, _generation: u64) -> VarInt {
    debug_assert!(index < MAX_SLOTS);
    // SAFETY: index < MAX_SLOTS = 2^25, which fits in a 62-bit VarInt.
    unsafe { VarInt::new_unchecked(index as u64) }
}

/// Extract the slot index from a queue ID.
#[inline]
pub fn index(queue_id: VarInt) -> usize {
    queue_id.as_u64() as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use bolero::check;

    #[test]
    fn round_trip() {
        check!().with_type::<u32>().for_each(|raw_index| {
            let slot = (*raw_index as usize) % MAX_SLOTS;
            let queue_id = encode(slot, 0);
            assert_eq!(index(queue_id), slot);
        });
    }

    #[test]
    fn generation_ignored() {
        let queue_id_a = encode(42, 0);
        let queue_id_b = encode(42, 999);
        assert_eq!(queue_id_a, queue_id_b);
        assert_eq!(index(queue_id_a), 42);
    }
}
