// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Server-side freed-queue notification.
//!
//! When a server-side receiver is dropped the stream's slot index must be
//! recycled back to the client so it can reuse that queue slot.  This module
//! implements the per-peer accumulator and the batch submission channel.
//!
//! ## Flow
//!
//! 1. Stream completes → `StreamReceiver` / `ControlReceiver` dropped.
//! 2. `FreedInner::record(queue_id)` is called.
//! 3. The `queue_id` is inserted into the pending set under the inner lock.
//!    If no batch token is in-flight, the pre-allocated `Entry<msg::Sender>`
//!    is submitted to the intrusive freed channel and `in_flight` is set.
//! 4. The assembler calls `FreedInner::take` at serialisation time to drain
//!    the pending set.  Any IDs that accumulated between step 3 and step 4
//!    are included naturally — this is the "snapshot at transmission time"
//!    property that yields free batching.
//! 5. After encoding the assembler calls `FreedInner::check_and_resubmit`.
//!    If more IDs accumulated the token requeues itself; otherwise `in_flight`
//!    is cleared and the next `record` call will submit the token again.

use crate::{
    bitset::HierarchicalBitSet, byte_vec::ByteVec, endpoint::id::LocalSenderId, intrusive,
    path::secret::map::Entry as PathSecretEntry, socket::channel::UnboundedSender,
    stream::endpoint::msg,
};
use s2n_quic_core::varint::VarInt;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

// TODO: replace with direct AckSender submission (client-side LB) once the
// &mut/gauge ownership story is resolved. For now we use an intermediate
// intrusive channel with a lightweight routing task.
pub type FreedBatchTx = crate::socket::channel::intrusive::sync::Sender<msg::Sender>;
pub type FreedBatchRx = crate::socket::channel::intrusive::sync::Receiver<msg::Sender>;

pub fn freed_batch_channel() -> (FreedBatchTx, FreedBatchRx) {
    crate::socket::channel::intrusive::sync::new::<msg::Sender>()
}

// ── Per-peer accumulator ──────────────────────────────────────────────────────

pub struct FreedInner {
    state: Mutex<FreedState>,
}

/// A previously-transmitted QueueFree frame awaiting retransmission.
///
/// Stored without the `Arc<PathSecretEntry>` to avoid reference cycles.
/// The assembler reconstructs the full `Frame` using the context's own path_secret_entry.
pub struct RetryEntry {
    pub free_request_id: VarInt,
    pub smallest_queue_id: VarInt,
    pub payload: ByteVec,
}

struct FreedState {
    freed: HierarchicalBitSet,
    next_request_id: VarInt,
    in_flight: bool,
    /// QueueFree frames that were in-flight when the send context was invalidated.
    /// The assembler drains this first, retransmitting with the original request_id
    /// and encoding so the client deduplicates via seen_requests.
    retry_queue: VecDeque<RetryEntry>,
    /// The slots held for the resets that haven't left the priority router yet, of a rejected
    /// init or of the slot's stream, by queue id. A slot freed while it is held is recorded once
    /// `release` ends its hold.
    held: HashMap<u64, Held>,
}

/// The hold of a slot for the resets of `binding_id`, of a rejected init or of the slot's stream.
struct Held {
    binding_id: u64,
    /// The resets of `binding_id` that hold the slot and haven't left the priority router yet.
    resets: usize,
    /// Whether the slot was freed while it was held.
    freed: bool,
}

/// What the hold of a stream's reset did with it: [`FreedInner::hold_reset`] holds the slot or
/// refuses the reset, and a reset that doesn't hold a slot is `Unheld`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetHold {
    /// The reset holds the slot until it leaves the priority router, beside each other reset of
    /// its binding that holds the slot.
    Held,
    /// The reset is sent without a hold, as a reset that a producer waits on or a reset of a
    /// client's slot.
    Unheld,
    /// The slot is held for another binding's reset, so the reset isn't sent.
    Refused,
}

impl Default for FreedInner {
    fn default() -> Self {
        Self::new()
    }
}

impl FreedInner {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FreedState {
                freed: HierarchicalBitSet::new(1),
                next_request_id: VarInt::from_u8(0),
                in_flight: false,
                retry_queue: VecDeque::new(),
                held: HashMap::new(),
            }),
        }
    }

    /// Record that `queue_id` has been freed and, if no token is in-flight,
    /// allocate an entry and submit it to the freed channel.
    ///
    /// Where `hold` or `hold_reset` holds the slot, the record waits until `release` ends the
    /// hold.
    pub fn record(
        &self,
        queue_id: VarInt,
        path_entry: &Arc<PathSecretEntry>,
        endpoint_tx: &mut FreedBatchTx,
    ) {
        let mut state = self.state.lock().unwrap();

        if let Some(held) = state.held.get_mut(&queue_id.as_u64()) {
            held.freed = true;
            return;
        }

        let id = queue_id.as_u64() as u32;
        if id as u64 >= HierarchicalBitSet::MAX_CAPACITY as u64 {
            return;
        }
        let needed = id + 1;
        if needed > state.freed.capacity() {
            state.freed.grow(needed);
        }
        state.freed.insert(id);

        if state.in_flight {
            return;
        }

        state.in_flight = true;
        drop(state);

        let entry = intrusive::Entry::new(msg::Sender::PendingFreed {
            path_secret_entry: path_entry.clone(),
            local_sender_id: LocalSenderId::UNSPECIFIED,
        });

        if endpoint_tx.send(entry).is_err() {
            self.state.lock().unwrap().in_flight = false;
        }
    }

    /// Hold the slot `queue_id` for the rejected init of `binding_id`, so that a `record` of the
    /// slot waits until `release` ends the hold. Returns false where the slot is held already,
    /// for a reset that hasn't left yet, of a rejected init or of the slot's stream.
    pub fn hold(&self, queue_id: VarInt, binding_id: VarInt) -> bool {
        let mut state = self.state.lock().unwrap();
        match state.held.entry(queue_id.as_u64()) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(Held {
                    binding_id: binding_id.as_u64(),
                    resets: 1,
                    freed: false,
                });
                true
            }
        }
    }

    /// Hold the slot `queue_id` for a reset of the stream of `binding_id` that no producer waits
    /// on, so that a `record` of the slot waits until `release` ends the hold. A reset of the
    /// binding that holds the slot already adds to the hold, which lasts until each of its resets
    /// leaves, and a reset of another binding is refused while that hold lasts.
    pub fn hold_reset(&self, queue_id: VarInt, binding_id: VarInt) -> ResetHold {
        let mut state = self.state.lock().unwrap();
        if let Some(held) = state.held.get_mut(&queue_id.as_u64()) {
            if held.binding_id != binding_id.as_u64() {
                return ResetHold::Refused;
            }
            held.resets += 1;
            return ResetHold::Held;
        }
        let held = Held {
            binding_id: binding_id.as_u64(),
            resets: 1,
            freed: false,
        };
        state.held.insert(queue_id.as_u64(), held);
        ResetHold::Held
    }

    /// Release a reset's part of the hold of `queue_id` for `binding_id` once the reset leaves,
    /// and end the hold once each reset of the binding that holds the slot has left, recording the
    /// slot as freed where a `record` waited on the hold. A hold for another binding stays.
    pub fn release(
        &self,
        queue_id: VarInt,
        binding_id: VarInt,
        path_entry: &Arc<PathSecretEntry>,
        endpoint_tx: &mut FreedBatchTx,
    ) {
        let mut state = self.state.lock().unwrap();
        let freed = match state.held.get_mut(&queue_id.as_u64()) {
            Some(held) if held.binding_id == binding_id.as_u64() => {
                held.resets -= 1;
                if held.resets > 0 {
                    return;
                }
                held.freed
            }
            _ => return,
        };
        state.held.remove(&queue_id.as_u64());
        drop(state);

        if freed {
            self.record(queue_id, path_entry, endpoint_tx);
        }
    }

    /// Drain the accumulated freed IDs into `dest`, returning a request_id.
    /// Returns `None` if there is nothing to send.
    pub fn take(&self, dest: &mut HierarchicalBitSet) -> Option<VarInt> {
        let mut state = self.state.lock().unwrap();
        if state.freed.is_empty() {
            return None;
        }
        core::mem::swap(&mut state.freed, dest);
        Some(state.take_next_request_id())
    }

    /// Merge remaining IDs back and decide whether to resubmit in a single lock.
    pub fn finish_encoding(
        &self,
        remainder: &mut HierarchicalBitSet,
        entry: intrusive::Entry<msg::Sender>,
        tx: &mut FreedBatchTx,
    ) {
        let mut state = self.state.lock().unwrap();
        state.freed.union(remainder);

        if state.freed.is_empty() && state.retry_queue.is_empty() {
            state.in_flight = false;
        } else {
            drop(state);
            if tx.send(entry).is_err() {
                self.state.lock().unwrap().in_flight = false;
            }
        }
    }

    /// After encoding: if more IDs accumulated or retry frames exist, resubmit
    /// the entry to the freed channel; otherwise clear `in_flight`.
    pub fn check_and_resubmit(&self, entry: intrusive::Entry<msg::Sender>, tx: &mut FreedBatchTx) {
        let mut empty = HierarchicalBitSet::new(1);
        self.finish_encoding(&mut empty, entry, tx);
    }

    /// Clear the in_flight flag.
    /// Used when the entry is dropped without going through the full assembly path
    /// (e.g., AckProcessor can't create context, or context is invalidated).
    pub fn clear_in_flight(&self) {
        self.state.lock().unwrap().in_flight = false;
    }

    /// Push a previously-transmitted QueueFree frame onto the retry queue.
    ///
    /// Called from cancelled_drain when peer-dead invalidates a send context.
    /// The entry retains its original encoding and request_id so the client
    /// deduplicates on retransmission.
    ///
    /// If no token is currently in-flight, submits one to trigger assembly.
    pub fn push_retry(
        &self,
        entry: RetryEntry,
        path_entry: &Arc<PathSecretEntry>,
        endpoint_tx: &mut FreedBatchTx,
    ) {
        let mut state = self.state.lock().unwrap();
        state.retry_queue.push_back(entry);

        if state.in_flight {
            return;
        }

        state.in_flight = true;
        drop(state);

        let token = intrusive::Entry::new(msg::Sender::PendingFreed {
            path_secret_entry: path_entry.clone(),
            local_sender_id: LocalSenderId::UNSPECIFIED,
        });

        if endpoint_tx.send(token).is_err() {
            self.state.lock().unwrap().in_flight = false;
        }
    }

    /// Pop the next retry entry, if any. Called by the assembler before
    /// encoding fresh ranges.
    pub fn pop_retry(&self) -> Option<RetryEntry> {
        self.state.lock().unwrap().retry_queue.pop_front()
    }

    /// Returns true if there are retry frames or pending freed IDs.
    pub fn has_pending_work(&self) -> bool {
        let state = self.state.lock().unwrap();
        !state.retry_queue.is_empty() || !state.freed.is_empty()
    }
}

impl core::fmt::Debug for FreedInner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.state.try_lock() {
            Ok(s) => f
                .debug_struct("FreedInner")
                .field("freed_count", &s.freed.len())
                .field("in_flight", &s.in_flight)
                .field("held", &s.held.len())
                .finish(),
            Err(_) => write!(f, "FreedInner(<locked>)"),
        }
    }
}

impl FreedState {
    fn take_next_request_id(&mut self) -> VarInt {
        let id = self.next_request_id;
        if let Ok(next) = VarInt::new(id.as_u64().saturating_add(1)) {
            self.next_request_id = next;
        }
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        socket::channel::Budget,
        testing::{ext::*, sim},
    };
    use s2n_quic_core::varint::VarInt;

    fn test_setup() -> (Arc<PathSecretEntry>, FreedInner, FreedBatchTx, FreedBatchRx) {
        let (tx, rx) = freed_batch_channel();
        let path_entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap()).build(None);
        let freed = FreedInner::new();
        (path_entry, freed, tx, rx)
    }

    #[test]
    fn a_held_slot_is_recorded_once_its_reset_leaves() {
        sim(|| {
            let (path_entry, freed, mut tx, rx) = test_setup();

            async move {
                let _rx = rx;
                let slot = VarInt::from_u8(5);
                assert!(freed.hold(slot, VarInt::from_u8(1)));
                // A second rejected init of the held slot doesn't get a hold of its own.
                assert!(!freed.hold(slot, VarInt::from_u8(2)));

                // The slot is freed while its reset waits: the record waits on the hold.
                freed.record(slot, &path_entry, &mut tx);
                assert!(!freed.has_pending_work());

                // A release for another binding leaves the hold as it is.
                freed.release(slot, VarInt::from_u8(2), &path_entry, &mut tx);
                assert!(!freed.has_pending_work());

                // The held binding's reset leaves: the slot is recorded as freed.
                freed.release(slot, VarInt::from_u8(1), &path_entry, &mut tx);
                let mut dest = HierarchicalBitSet::new(1);
                freed.take(&mut dest).unwrap();
                assert!(dest.contains(5));

                // The slot can be held again for its next rejected init.
                assert!(freed.hold(slot, VarInt::from_u8(3)));
            }
            .primary()
            .spawn();
        });
    }

    #[test]
    fn a_stream_reset_holds_its_slot_and_refuses_another_bindings_reset() {
        sim(|| {
            let (path_entry, freed, mut tx, rx) = test_setup();

            async move {
                let _rx = rx;
                let slot = VarInt::from_u8(5);
                let (b1, b2) = (VarInt::from_u8(1), VarInt::from_u8(2));
                assert_eq!(freed.hold_reset(slot, b1), ResetHold::Held);
                // A reset of another binding isn't sent, and its rejected init doesn't get a hold.
                assert_eq!(freed.hold_reset(slot, b2), ResetHold::Refused);
                assert!(!freed.hold(slot, b2));

                // The slot is freed while the reset waits: the record waits on the hold.
                freed.record(slot, &path_entry, &mut tx);
                assert!(!freed.has_pending_work());

                // The reset leaves: the slot is recorded as freed, and can be held again.
                freed.release(slot, b1, &path_entry, &mut tx);
                let mut dest = HierarchicalBitSet::new(1);
                freed.take(&mut dest).unwrap();
                assert!(dest.contains(5));
                assert_eq!(freed.hold_reset(slot, b2), ResetHold::Held);

                // Where a rejected init holds the slot, a reset of its binding adds to the hold,
                // and a reset of another binding is refused.
                let slot = VarInt::from_u8(6);
                let (b3, b4) = (VarInt::from_u8(3), VarInt::from_u8(4));
                assert!(freed.hold(slot, b3));
                assert_eq!(freed.hold_reset(slot, b3), ResetHold::Held);
                assert_eq!(freed.hold_reset(slot, b4), ResetHold::Refused);
            }
            .primary()
            .spawn();
        });
    }

    #[test]
    fn a_slot_stays_held_until_each_reset_of_its_binding_leaves() {
        sim(|| {
            let (path_entry, freed, mut tx, rx) = test_setup();

            async move {
                let _rx = rx;
                let slot = VarInt::from_u8(5);
                let (b1, b2) = (VarInt::from_u8(1), VarInt::from_u8(2));
                // Each half of the stream sends a reset that holds the slot.
                assert_eq!(freed.hold_reset(slot, b1), ResetHold::Held);
                assert_eq!(freed.hold_reset(slot, b1), ResetHold::Held);
                freed.record(slot, &path_entry, &mut tx);

                // One reset leaves: the other still waits, so the slot isn't recorded as freed,
                // another binding's reset is refused, and its rejected init doesn't get a hold.
                freed.release(slot, b1, &path_entry, &mut tx);
                assert!(!freed.has_pending_work());
                assert_eq!(freed.hold_reset(slot, b2), ResetHold::Refused);
                assert!(!freed.hold(slot, b2));

                // The other reset leaves: the slot is recorded as freed, and can be held again.
                freed.release(slot, b1, &path_entry, &mut tx);
                let mut dest = HierarchicalBitSet::new(1);
                freed.take(&mut dest).unwrap();
                assert!(dest.contains(5));
                assert_eq!(freed.hold_reset(slot, b2), ResetHold::Held);

                // A stream's reset adds to the hold of its binding's rejected init, and the slot
                // is recorded as freed once both resets have left.
                let slot = VarInt::from_u8(6);
                let b3 = VarInt::from_u8(3);
                assert!(freed.hold(slot, b3));
                assert_eq!(freed.hold_reset(slot, b3), ResetHold::Held);
                freed.record(slot, &path_entry, &mut tx);
                freed.release(slot, b3, &path_entry, &mut tx);
                assert!(!freed.has_pending_work());
                freed.release(slot, b3, &path_entry, &mut tx);
                let mut dest = HierarchicalBitSet::new(1);
                freed.take(&mut dest).unwrap();
                assert!(dest.contains(6));
            }
            .primary()
            .spawn();
        });
    }

    #[test]
    fn record_submits_and_batches() {
        sim(|| {
            let (path_entry, freed, mut tx, mut rx) = test_setup();

            async move {
                let mut budget = Budget::new(usize::MAX);

                freed.record(VarInt::from_u8(5), &path_entry, &mut tx);
                freed.record(VarInt::from_u8(6), &path_entry, &mut tx);
                freed.record(VarInt::from_u8(7), &path_entry, &mut tx);

                // Only one token submitted despite three records
                let entry =
                    crate::socket::channel::Receiver::<intrusive::Entry<msg::Sender>>::recv(
                        &mut rx,
                        &mut budget,
                    )
                    .await
                    .unwrap();

                // take() drains all accumulated IDs
                let mut dest = HierarchicalBitSet::new(1);
                let request_id = freed.take(&mut dest).unwrap();
                assert_eq!(request_id, VarInt::from_u8(0));
                assert!(dest.contains(5));
                assert!(dest.contains(6));
                assert!(dest.contains(7));

                // check_and_resubmit clears in_flight when empty
                freed.check_and_resubmit(entry, &mut tx);

                // New record submits a fresh token
                freed.record(VarInt::from_u8(8), &path_entry, &mut tx);
                let entry =
                    crate::socket::channel::Receiver::<intrusive::Entry<msg::Sender>>::recv(
                        &mut rx,
                        &mut budget,
                    )
                    .await
                    .unwrap();

                let mut dest = HierarchicalBitSet::new(1);
                let request_id = freed.take(&mut dest).unwrap();
                assert_eq!(request_id, VarInt::from_u8(1));
                assert!(dest.contains(8));

                freed.check_and_resubmit(entry, &mut tx);
            }
            .primary()
            .spawn();
        });
    }

    #[test]
    fn check_and_resubmit_requeues_when_more() {
        sim(|| {
            let (path_entry, freed, mut tx, mut rx) = test_setup();

            async move {
                let mut budget = Budget::new(usize::MAX);

                freed.record(VarInt::from_u8(1), &path_entry, &mut tx);
                let entry =
                    crate::socket::channel::Receiver::<intrusive::Entry<msg::Sender>>::recv(
                        &mut rx,
                        &mut budget,
                    )
                    .await
                    .unwrap();

                let mut dest = HierarchicalBitSet::new(1);
                freed.take(&mut dest);

                // More IDs arrive while token is out
                freed.record(VarInt::from_u8(2), &path_entry, &mut tx);

                // Resubmit detects non-empty and requeues
                freed.check_and_resubmit(entry, &mut tx);

                // Token reappears
                let entry =
                    crate::socket::channel::Receiver::<intrusive::Entry<msg::Sender>>::recv(
                        &mut rx,
                        &mut budget,
                    )
                    .await
                    .unwrap();
                dest.clear();
                let id = freed.take(&mut dest).unwrap();
                assert!(dest.contains(2));
                assert_eq!(id, VarInt::from_u8(1));

                freed.check_and_resubmit(entry, &mut tx);
            }
            .primary()
            .spawn();
        });
    }

    #[test]
    fn put_back_merges_remainder() {
        sim(|| {
            let (path_entry, freed, mut tx, mut rx) = test_setup();

            async move {
                let mut budget = Budget::new(usize::MAX);

                freed.record(VarInt::from_u8(10), &path_entry, &mut tx);
                freed.record(VarInt::from_u8(20), &path_entry, &mut tx);
                let entry =
                    crate::socket::channel::Receiver::<intrusive::Entry<msg::Sender>>::recv(
                        &mut rx,
                        &mut budget,
                    )
                    .await
                    .unwrap();

                let mut dest = HierarchicalBitSet::new(1);
                freed.take(&mut dest);
                assert_eq!(dest.len(), 2);

                // Simulate partial encode: put back ID 20
                let mut remainder = HierarchicalBitSet::new(21);
                remainder.insert(20);
                freed.finish_encoding(&mut remainder, entry, &mut tx);

                // More IDs arrive
                freed.record(VarInt::from_u8(30), &path_entry, &mut tx);

                // Token was resubmitted since remainder was non-empty
                let entry =
                    crate::socket::channel::Receiver::<intrusive::Entry<msg::Sender>>::recv(
                        &mut rx,
                        &mut budget,
                    )
                    .await
                    .unwrap();

                // Next take includes put-back + new
                dest.clear();
                let id = freed.take(&mut dest).unwrap();
                assert!(id.as_u64() > 0);
                assert!(dest.contains(20));
                assert!(dest.contains(30));

                freed.check_and_resubmit(entry, &mut tx);
            }
            .primary()
            .spawn();
        });
    }

    #[test]
    fn clear_in_flight_restores_token() {
        sim(|| {
            let (path_entry, freed, mut tx, mut rx) = test_setup();

            async move {
                let mut budget = Budget::new(usize::MAX);

                freed.record(VarInt::from_u8(1), &path_entry, &mut tx);
                let entry =
                    crate::socket::channel::Receiver::<intrusive::Entry<msg::Sender>>::recv(
                        &mut rx,
                        &mut budget,
                    )
                    .await
                    .unwrap();

                // Simulate peer-dead: clear in_flight, drop the entry
                freed.clear_in_flight();
                drop(entry);

                // New record can submit again
                freed.record(VarInt::from_u8(2), &path_entry, &mut tx);
                let entry =
                    crate::socket::channel::Receiver::<intrusive::Entry<msg::Sender>>::recv(
                        &mut rx,
                        &mut budget,
                    )
                    .await
                    .unwrap();

                let mut dest = HierarchicalBitSet::new(1);
                let _id = freed.take(&mut dest).unwrap();
                // IDs 1 and 2 are both in there (1 was never taken before clear)
                assert!(dest.contains(1));
                assert!(dest.contains(2));

                freed.check_and_resubmit(entry, &mut tx);
            }
            .primary()
            .spawn();
        });
    }

    #[test]
    fn take_empty_returns_none() {
        sim(|| {
            let (path_entry, freed, mut tx, mut rx) = test_setup();

            async move {
                let mut budget = Budget::new(usize::MAX);

                freed.record(VarInt::from_u8(1), &path_entry, &mut tx);
                let entry =
                    crate::socket::channel::Receiver::<intrusive::Entry<msg::Sender>>::recv(
                        &mut rx,
                        &mut budget,
                    )
                    .await
                    .unwrap();

                let mut dest = HierarchicalBitSet::new(1);
                freed.take(&mut dest);

                // Second take on empty returns None
                let result = freed.take(&mut dest);
                assert!(result.is_none());

                freed.check_and_resubmit(entry, &mut tx);
            }
            .primary()
            .spawn();
        });
    }
}
