// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Queue IDs are plain slot indices. No encoding, no generation bits.
//! Anti-ABA protection is provided by the binding_id field on the wire.

use s2n_quic_core::varint::VarInt;

/// Convert a queue_id VarInt to a slot index.
///
/// Queue IDs are just slot indices — this is a simple cast.
#[inline]
pub fn slot_index(queue_id: VarInt) -> usize {
    queue_id.as_u64() as usize
}
