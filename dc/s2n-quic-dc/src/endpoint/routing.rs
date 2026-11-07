// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::msg;
use crate::{
    credentials::{self, Credentials},
    endpoint::id::{Id, IdMap, LocalSenderId, RemoteSenderId, SendWorkerId},
    intrusive::Entry,
    socket::channel::UnboundedSender,
};
use s2n_quic_core::varint::VarInt;

#[inline]
pub fn hash_credentials_and_sender(credentials: &Credentials, source_sender_id: VarInt) -> u64 {
    hash_id_and_sender(&credentials.id, source_sender_id)
}

/// Combines a credentials ID with a sender ID to produce a hash for routing decisions.
///
/// The credentials ID already has good entropy (it's a random 128-bit value hashed down to
/// 64 bits). The sender ID is a small sequential value (socket index). We mix them using a
/// three-step process:
///
/// 1. XOR the hash with sender_id scaled by a large odd constant (phi, the golden ratio
///    as a fixed-point 64-bit integer: 2^64 / phi ≈ 0x9e3779b97f4a7c15). Multiplying by
///    phi spreads even closely-spaced sender IDs across the full 64-bit range.
///
/// 2. Rotate and XOR to break any remaining linear correlation between input bits and
///    output bits.
///
/// 3. Multiply by another large odd constant to provide final avalanche — small changes
///    in any input bit propagate to all output bits.
///
/// The result is that (credentials_id=X, sender_id=0) and (credentials_id=X, sender_id=1)
/// map to completely different hash buckets, which is what we need for distributing packets
/// from the same peer across different workers.
#[inline]
pub fn hash_id_and_sender(id: &credentials::Id, sender_id: VarInt) -> u64 {
    let mut hash = id.to_hash();

    let sender_id = sender_id.as_u64();
    hash ^= sender_id.wrapping_mul(0x9e3779b97f4a7c15);
    hash = hash.rotate_left(32) ^ sender_id;
    hash = hash.wrapping_mul(0x517cc1b727220a95);

    hash
}

pub(crate) trait SenderRoute: Clone + Copy + Send + 'static {
    fn new(count: usize) -> Self;
    fn route(&self, hash: u64) -> usize;

    /// Returns the local sender_id that should be used to send ACK packets back to the peer.
    ///
    /// Uses `source_sender_id % num_send` so the ACK goes from the same shared port that
    /// received the data, achieving symmetric 5-tuples for conntrack compatibility.
    #[inline]
    fn sender_id_for_ack(&self, source_sender_id: RemoteSenderId) -> super::id::LocalSenderId {
        super::id::LocalSenderId::new(unsafe {
            VarInt::new_unchecked(self.route(source_sender_id.as_varint().as_u64()) as u64)
        })
    }

    /// Returns the local worker_id that is responsible for decoding/decrypting a packet
    #[inline]
    fn worker_id_for_recv(
        &self,
        credentials: &Credentials,
        source_sender_id: RemoteSenderId,
    ) -> super::id::RecvDispatchWorkerId {
        let hash = hash_credentials_and_sender(credentials, source_sender_id.as_varint());
        super::id::RecvDispatchWorkerId::new(self.route(hash))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PowerOfTwoRoute {
    mask: u64,
}

impl SenderRoute for PowerOfTwoRoute {
    fn new(count: usize) -> Self {
        debug_assert!(count.is_power_of_two());
        Self {
            mask: (count - 1) as u64,
        }
    }

    #[inline]
    fn route(&self, hash: u64) -> usize {
        (hash & self.mask) as usize
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ModuloRoute {
    divisor: u64,
}

impl SenderRoute for ModuloRoute {
    fn new(count: usize) -> Self {
        Self {
            divisor: count as u64,
        }
    }

    #[inline]
    fn route(&self, hash: u64) -> usize {
        (hash % self.divisor) as usize
    }
}

// ── ACK Routing ───────────────────────────────────────────────────────────

/// Routes `msg::Sender` messages to the correct per-worker ACK channel.
///
/// Indexed directly by `sender_idx` — each slot is a clone of the owning worker's
/// channel sender. Single lookup, no indirection.
#[derive(Clone)]
pub(crate) struct AckSender<T> {
    senders: IdMap<LocalSenderId, T>,
    /// The senders of the endpoint's highest-priority traffic, which a peer's acknowledgements
    /// move to first where the sender they're routed to is marked lost.
    priority_senders: Option<Vec<LocalSenderId>>,
}

impl<T: Clone> AckSender<T> {
    /// Build a flat sender array indexed by sender_idx.
    ///
    /// `worker_senders` has one entry per send worker. `sender_id_to_worker` maps
    /// each sender_idx to its owning worker. The result has one entry per sender_idx,
    /// each cloned from the appropriate worker's sender.
    pub fn new(
        worker_senders: IdMap<SendWorkerId, T>,
        sender_id_to_worker: &IdMap<LocalSenderId, SendWorkerId>,
    ) -> Self {
        let senders = sender_id_to_worker
            .iter()
            .map(|(sender_id, &worker_idx)| (sender_id, worker_senders[worker_idx].clone()))
            .collect();
        Self {
            senders,
            priority_senders: None,
        }
    }

    /// Sets the senders of the endpoint's highest-priority traffic, which a peer's
    /// acknowledgements move to first where the sender they're routed to is marked lost.
    pub fn with_priority_senders(mut self, priority_senders: Option<Vec<LocalSenderId>>) -> Self {
        self.priority_senders = priority_senders;
        self
    }
}

impl<T> UnboundedSender<Entry<msg::Sender>> for AckSender<T>
where
    T: UnboundedSender<Entry<msg::Sender>>,
{
    fn send(&mut self, mut msg: Entry<msg::Sender>) -> Result<(), Entry<msg::Sender>> {
        // A peer's acknowledgements leave a sender whose interface pair is marked lost, while
        // another sender of the path delivers.
        if let msg::Sender::PendingAck(ref mut submission) = *msg {
            let path = &submission.path_secret_entry;
            let chosen = delivering_ack_sender_id(
                submission.local_sender_id,
                submission.remote_sender_id,
                self.priority_senders.as_deref(),
                self.senders.len(),
                |sender| path.is_sender_lost(sender),
            );
            submission.local_sender_id = chosen;
        }
        #[cfg(test)]
        observe::record(&msg, self.senders.len());
        let sender_idx = msg.sender_idx();
        debug_assert!(
            sender_idx.as_usize() < self.senders.len(),
            "sender_idx {sender_idx} out of bounds (len {})",
            self.senders.len()
        );
        let tx = &mut self.senders[sender_idx];
        tx.send(msg)
    }
}

// ── ACK Completion Routing ───────────────────────────────────────────────

use crate::intrusive::Queue;

/// Routes ACK completion entries back to the recv dispatch worker that submitted them.
///
/// Accepts a `Queue<msg::Sender>` (batch from the assembler), partitions entries by
/// `recv_worker_id`, and sends one queue per worker — one lock acquisition per
/// destination worker per batch.
pub(crate) struct AckCompletionSender<T> {
    senders: IdMap<super::id::RecvDispatchWorkerId, T>,
    /// Staging queues for partitioning, one per recv worker. Reused across sends
    /// to avoid repeated allocation.
    staging: IdMap<super::id::RecvDispatchWorkerId, Queue<msg::Sender>>,
}

impl<T: Clone> Clone for AckCompletionSender<T> {
    fn clone(&self) -> Self {
        Self {
            senders: self.senders.clone(),
            staging: super::id::RecvDispatchWorkerId::range(self.staging.len())
                .map(|id| (id, Queue::new()))
                .collect(),
        }
    }
}

impl<T: Clone> AckCompletionSender<T> {
    pub fn new(senders: IdMap<super::id::RecvDispatchWorkerId, T>) -> Self {
        let len = senders.len();
        Self {
            staging: super::id::RecvDispatchWorkerId::range(len)
                .map(|id| (id, Queue::new()))
                .collect(),
            senders,
        }
    }
}

impl<T> UnboundedSender<Queue<msg::Sender>> for AckCompletionSender<T>
where
    T: UnboundedSender<Queue<msg::Sender>>,
{
    fn send(&mut self, mut queue: Queue<msg::Sender>) -> Result<(), Queue<msg::Sender>> {
        while let Some(entry) = queue.pop_front() {
            let worker_id = entry
                .recv_worker_id()
                .expect("completion entry must have a recv_worker_id");
            debug_assert!(
                worker_id.as_usize() < self.staging.len(),
                "recv_worker_id {worker_id} out of bounds (len {})",
                self.staging.len()
            );
            self.staging[worker_id].push_back(entry);
        }

        for ((_, tx), (_, staging)) in self.senders.iter_mut().zip(self.staging.iter_mut()) {
            if !staging.is_empty() {
                let batch = core::mem::take(staging);
                let _ = tx.send(batch);
            }
        }

        Ok(())
    }
}

/// Returns the sender that a peer sender's acknowledgements go to.
///
/// `routed` is the sender the route picks for `source_sender_id`. Where the endpoint has senders
/// for its highest-priority traffic (`priority_senders`) and `routed` isn't one of them, the
/// acknowledgements go to one of those senders, picked by `source_sender_id`, so that each peer
/// sender's acknowledgements keep to one sender.
#[inline]
pub(crate) fn ack_sender_id(
    routed: LocalSenderId,
    source_sender_id: RemoteSenderId,
    priority_senders: Option<&[LocalSenderId]>,
) -> LocalSenderId {
    let Some(senders) = priority_senders.filter(|senders| !senders.is_empty()) else {
        return routed;
    };
    if senders.contains(&routed) {
        return routed;
    }
    let position = source_sender_id.as_varint().as_u64() % senders.len() as u64;
    senders[position as usize]
}

/// Returns the senders that the routes choose from while some of a path's interface pairs are
/// marked lost: of the first `socket_count` senders, the ones that `is_lost` doesn't mark. Where
/// `priority_senders`, the senders of the endpoint's highest-priority traffic, holds some of
/// them, it returns those alone. It returns `None` where `is_lost` marks none of the first
/// `socket_count` senders, or every one of them, so that the routes keep the choice they make
/// without the marks.
pub(crate) fn delivering_senders(
    priority_senders: Option<&[LocalSenderId]>,
    socket_count: usize,
    is_lost: impl Fn(LocalSenderId) -> bool,
) -> Option<Vec<LocalSenderId>> {
    let senders = || (0..socket_count).map(LocalSenderId::from_index);
    // A path whose pairs all deliver, the usual case, doesn't allocate.
    if !senders().any(&is_lost) {
        return None;
    }
    let delivering: Vec<LocalSenderId> = senders().filter(|&sender| !is_lost(sender)).collect();
    if delivering.is_empty() {
        return None;
    }
    let priority: Vec<LocalSenderId> = priority_senders
        .unwrap_or_default()
        .iter()
        .copied()
        .filter(|sender| delivering.contains(sender))
        .collect();
    if priority.is_empty() {
        Some(delivering)
    } else {
        Some(priority)
    }
}

/// Returns the sender that a peer sender's acknowledgements go to while some of the path's
/// interface pairs are marked lost: `routed`, the sender that `ack_sender_id` picked, where
/// `is_lost` doesn't mark it, and otherwise one of the `delivering_senders`, picked by
/// `source_sender_id` as `ack_sender_id` picks one. Where `is_lost` marks every sender, it
/// returns `routed`.
pub(crate) fn delivering_ack_sender_id(
    routed: LocalSenderId,
    source_sender_id: RemoteSenderId,
    priority_senders: Option<&[LocalSenderId]>,
    socket_count: usize,
    is_lost: impl Fn(LocalSenderId) -> bool,
) -> LocalSenderId {
    if !is_lost(routed) {
        return routed;
    }
    let Some(senders) = delivering_senders(priority_senders, socket_count, is_lost) else {
        return routed;
    };
    let position = source_sender_id.as_varint().as_u64() % senders.len() as u64;
    senders[position as usize]
}

/// Returns the sender that a freed batch goes to: of two different senders drawn at random, the
/// one with the lower `score`, or the first where the scores are equal.
///
/// The senders are `priority_senders`, the senders of the endpoint's highest-priority traffic,
/// where it has some, and otherwise the first `socket_count` senders. While `is_lost` marks
/// some of the first `socket_count` senders and not all of them, they are the
/// `delivering_senders` instead. Where there is one sender, or none, it returns the first
/// without a draw.
pub(crate) fn freed_batch_sender_id<S: PartialOrd>(
    priority_senders: Option<&[LocalSenderId]>,
    socket_count: usize,
    rng: &mut crate::xorshift::Rng,
    is_lost: impl Fn(LocalSenderId) -> bool,
    mut score: impl FnMut(LocalSenderId) -> S,
) -> LocalSenderId {
    let delivering = delivering_senders(priority_senders, socket_count, is_lost);
    let priority_senders = delivering
        .as_deref()
        .or(priority_senders)
        .filter(|senders| !senders.is_empty());
    let count = priority_senders.map_or(socket_count, |senders| senders.len());
    let sender = |position: usize| match priority_senders {
        Some(senders) => senders[position],
        None => LocalSenderId::from_index(position),
    };
    if count <= 1 {
        return sender(0);
    }
    let first = rng.next_usize(count);
    let mut second = rng.next_usize(count - 1);
    if second >= first {
        second += 1;
    }
    let (first, second) = (sender(first), sender(second));
    if score(first) <= score(second) {
        first
    } else {
        second
    }
}

/// Records each acknowledgement and freed batch that the endpoint routes to a send worker, for
/// the tests that check which sender each goes to.
#[cfg(test)]
pub(crate) mod observe {
    use super::{msg, Id, LocalSenderId};
    use core::cell::RefCell;
    use std::{
        net::SocketAddr,
        sync::{Arc, Mutex},
    };

    /// What a routed message carries.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Kind {
        /// A peer's pending acknowledgements.
        PendingAck,
        /// A freed batch, which returns the `QueueFree` tokens of a peer's queues.
        PendingFreed,
    }

    /// One message routed to a send worker: what it carries, its sender, its path's peer, and
    /// the marks of the path's senders when it was routed.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Routed {
        pub(crate) kind: Kind,
        pub(crate) sender: LocalSenderId,
        pub(crate) peer: SocketAddr,
        /// Whether the sender's interface pair was marked lost.
        pub(crate) lost: bool,
        /// Whether a sender of the path was marked lost.
        pub(crate) path_has_lost: bool,
        /// Whether a sender of the path wasn't marked lost.
        pub(crate) path_delivers: bool,
    }

    thread_local! {
        static SINK: RefCell<Option<Arc<Mutex<Vec<Routed>>>>> = const { RefCell::new(None) };
    }

    /// Records each message that this thread routes from now on in `sink`.
    pub(crate) fn record_into(sink: Arc<Mutex<Vec<Routed>>>) {
        SINK.with(|current| *current.borrow_mut() = Some(sink));
    }

    /// Records `message`, which the endpoint routed among its first `senders` senders, where this
    /// thread records the messages it routes.
    pub(crate) fn record(message: &msg::Sender, senders: usize) {
        let kind = match message {
            msg::Sender::PendingAck(_) => Kind::PendingAck,
            msg::Sender::PendingFreed { .. } => Kind::PendingFreed,
            msg::Sender::ReceivedAck { .. } => return,
        };
        SINK.with(|sink| {
            if let Some(sink) = sink.borrow().as_ref() {
                let path = message.path_secret_entry();
                let lost = |index: usize| path.is_sender_lost(LocalSenderId::from_index(index));
                sink.lock().unwrap().push(Routed {
                    kind,
                    sender: message.sender_idx(),
                    peer: *path.peer(),
                    lost: path.is_sender_lost(message.sender_idx()),
                    path_has_lost: (0..senders).any(&lost),
                    path_delivers: !(0..senders).all(&lost),
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Rng;

    fn sender(position: usize) -> LocalSenderId {
        LocalSenderId::from_index(position)
    }

    fn remote(id: u32) -> RemoteSenderId {
        RemoteSenderId::new(VarInt::from_u32(id))
    }

    #[test]
    fn acknowledgements_keep_the_routed_sender_without_priority_senders() {
        assert_eq!(ack_sender_id(sender(2), remote(5), None), sender(2));
        assert_eq!(
            ack_sender_id(sender(2), remote(5), Some(&[][..])),
            sender(2)
        );
    }

    #[test]
    fn acknowledgements_go_to_a_sender_of_the_highest_priority_interface() {
        let priority = [sender(1), sender(3)];
        // The route's sender is on that interface already.
        assert_eq!(
            ack_sender_id(sender(3), remote(4), Some(&priority[..])),
            sender(3)
        );
        // Otherwise the peer's sender picks one of the interface's senders.
        assert_eq!(
            ack_sender_id(sender(2), remote(4), Some(&priority[..])),
            sender(1)
        );
        assert_eq!(
            ack_sender_id(sender(2), remote(5), Some(&priority[..])),
            sender(3)
        );
    }

    #[test]
    fn freed_batches_go_to_the_lower_scored_of_two_different_senders() {
        let mut rng = Rng::with_seed(7);
        // Scored by index, the higher of two different senders never wins, so sender 7 doesn't.
        for _ in 0..256 {
            let chosen = freed_batch_sender_id(None, 8, &mut rng, |_| false, |id| id.as_usize());
            assert!(
                chosen.as_usize() < 7,
                "{chosen} won over a sender of a lower score"
            );
        }
        // One sender, or none, takes every freed batch.
        for socket_count in [0, 1] {
            let chosen = freed_batch_sender_id(None, socket_count, &mut rng, |_| false, |_| 0);
            assert_eq!(chosen, sender(0));
        }
        // Where the two scores are equal, the first draw wins.
        for _ in 0..256 {
            let first = sender(rng.clone().next_usize(8));
            let chosen = freed_batch_sender_id(None, 8, &mut rng, |_| false, |_| 0);
            assert_eq!(chosen, first);
        }
    }

    #[test]
    fn freed_batches_go_to_a_sender_of_the_highest_priority_interface() {
        let mut rng = Rng::with_seed(7);
        let priority = [sender(1), sender(3)];
        for _ in 0..256 {
            // Both draws are of the interface's senders, and sender 3 scores lower.
            let chosen = freed_batch_sender_id(
                Some(&priority[..]),
                8,
                &mut rng,
                |_| false,
                |id| usize::MAX - id.as_usize(),
            );
            assert_eq!(chosen, sender(3));
        }
        let chosen = freed_batch_sender_id(Some(&[sender(5)][..]), 8, &mut rng, |_| false, |_| 0);
        assert_eq!(chosen, sender(5));
        // An empty list keeps the choice among every sender.
        let chosen = freed_batch_sender_id(Some(&[][..]), 1, &mut rng, |_| false, |_| 0);
        assert_eq!(chosen, sender(0));
    }

    #[test]
    fn acknowledgements_leave_a_sender_marked_lost() {
        let priority = [sender(1), sender(3)];
        let three = |id: LocalSenderId| id == sender(3);
        let odd = |id: LocalSenderId| id.as_usize() % 2 == 1;
        // A sender that isn't marked keeps the peer's acknowledgements.
        assert_eq!(
            delivering_ack_sender_id(sender(2), remote(5), Some(&priority[..]), 8, three),
            sender(2)
        );
        // Otherwise they go to a priority sender that delivers.
        assert_eq!(
            delivering_ack_sender_id(sender(3), remote(4), Some(&priority[..]), 8, three),
            sender(1)
        );
        assert_eq!(
            delivering_ack_sender_id(sender(3), remote(5), Some(&priority[..]), 8, three),
            sender(1)
        );
        // Where each priority sender is marked, the peer's sender picks one of the senders that
        // deliver.
        assert_eq!(
            delivering_ack_sender_id(sender(3), remote(4), Some(&priority[..]), 8, odd),
            sender(0)
        );
        assert_eq!(
            delivering_ack_sender_id(sender(3), remote(5), Some(&priority[..]), 8, odd),
            sender(2)
        );
        assert_eq!(
            delivering_ack_sender_id(sender(7), remote(5), None, 8, odd),
            sender(2)
        );
        // Where every sender is marked, the route's sender keeps them.
        assert_eq!(
            delivering_ack_sender_id(sender(3), remote(5), Some(&priority[..]), 8, |_| true),
            sender(3)
        );
    }

    #[test]
    fn freed_batches_leave_senders_marked_lost() {
        let mut rng = Rng::with_seed(7);
        let priority = [sender(1), sender(3)];
        let three = |id: LocalSenderId| id == sender(3);
        let odd = |id: LocalSenderId| id.as_usize() % 2 == 1;
        let seven = |id: LocalSenderId| id == sender(7);
        for _ in 0..256 {
            // The priority sender that delivers takes each batch, though sender 3 scores lower.
            let chosen = freed_batch_sender_id(Some(&priority[..]), 8, &mut rng, three, |id| {
                usize::MAX - id.as_usize()
            });
            assert_eq!(chosen, sender(1));
            // Where each priority sender is marked, the batches go to the senders that deliver.
            let chosen = freed_batch_sender_id(Some(&priority[..]), 8, &mut rng, odd, |_| 0);
            assert_eq!(chosen.as_usize() % 2, 0, "{chosen} is marked lost");
            // Without priority senders, sender 7 scores lowest, but its mark keeps it from a
            // batch.
            let chosen = freed_batch_sender_id(None, 8, &mut rng, seven, |id| 7 - id.as_usize());
            assert_ne!(chosen, sender(7));
        }
        // Where every sender is marked, the draws are the ones without marks.
        let (mut marked, mut unmarked) = (Rng::with_seed(11), Rng::with_seed(11));
        for _ in 0..64 {
            assert_eq!(
                freed_batch_sender_id(None, 8, &mut marked, |_| true, |id| id.as_usize()),
                freed_batch_sender_id(None, 8, &mut unmarked, |_| false, |id| id.as_usize())
            );
        }
    }
}
