// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Queue key: just a binding_id for dispatch validation.
//!
//! The queue system uses the binding_id as its Key type. When a packet is
//! dispatched to a queue slot, the slot's binding_id is compared against the
//! packet's binding_id via ordered comparison:
//!
//! - Equal: accept the frame
//! - Received < current: stale packet for a recycled slot, drop silently
//! - Received > current: protocol bug (client rebound before QueueFree), panic in debug
//!
//! No credential_id check is needed because dispatch is already namespaced
//! per-recv-context (which is per credential_id + sender_id).

use crate::flow::queue::ValidationError;
use s2n_quic_core::varint::VarInt;
use std::cmp::Ordering;

/// Queue key — just the binding_id assigned at stream creation.
///
/// Stored in each queue slot on allocation. Validated on every dispatch
/// to detect stale packets routed to recycled slots.
#[derive(Debug, Clone, Copy)]
pub struct Handle {
    pub binding_id: VarInt,
}

impl Handle {
    #[inline]
    pub fn new(binding_id: VarInt) -> Self {
        Self { binding_id }
    }
}

impl crate::flow::queue::Key for Handle {
    type Request = VarInt;

    #[inline]
    fn validate(&self, binding_id: &VarInt) -> Result<(), ValidationError> {
        match self.binding_id.as_u64().cmp(&binding_id.as_u64()) {
            Ordering::Equal => Ok(()),
            Ordering::Greater => Err(ValidationError::StaleBinding),
            Ordering::Less => {
                tracing::error!(
                    current = self.binding_id.as_u64(),
                    received = binding_id.as_u64(),
                    "BUG: received binding_id greater than current — client rebound before QueueFree"
                );
                debug_assert!(
                    false,
                    "received binding_id greater than current — client rebound before QueueFree"
                );
                Err(ValidationError::FutureBinding)
            }
        }
    }

    #[inline]
    fn binding_id(&self) -> Option<VarInt> {
        Some(self.binding_id)
    }
}
