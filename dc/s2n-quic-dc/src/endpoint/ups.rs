// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! UnknownPathSecret (UPS) response sending.
//!
//! When a recv_dispatch worker receives a packet whose credentials are not in the path
//! secret map, the pre-authentication step encodes an UnknownPathSecret control packet.
//! That packet is pushed into a shared sync queue and drained by a single background task
//! that applies per-credential dedup and token-bucket rate limiting before sending.
//!
//! The same queue carries the ReplayDetected and StaleKey packets a recv_dispatch worker
//! encodes for a packet whose key id it has already seen, or that falls outside its replay
//! window. A peer that keeps sending on such a key gets each of its packets rejected, far
//! faster than the background task's pacer sends, so [`Pending`] keeps at most one response
//! in the queue for each credential and peer sender id.

use crate::{
    counter::{Counter, Registry},
    credentials,
    endpoint::routing,
    intrusive::Entry,
    msg::addr::Addr,
    socket::channel::{self, Budget, ByteCost, Sendable, UnboundedSender},
    time::precision::Timestamp,
};
use core::task::{self, Poll};
use crossbeam_utils::CachePadded;
use hashbrown::{HashMap, HashSet};
use s2n_quic_core::{inet::ExplicitCongestionNotification, ready, varint::VarInt};
use std::{
    io::IoSlice,
    sync::{Arc, Mutex, MutexGuard},
};

/// A pre-encoded UnknownPathSecret, ReplayDetected or StaleKey response ready to be sent.
pub struct Response {
    pub dest_addr: Addr,
    pub packet: Vec<u8>,
    /// The credential and peer sender id of the packet the response answers.
    pub key: Key,
}

impl ByteCost for Response {
    fn byte_cost(&self) -> u64 {
        self.packet.len() as u64
    }
}

impl Sendable for Response {
    fn send<S: crate::socket::send::Socket>(&mut self, socket: &S) -> std::io::Result<()> {
        socket.send_msg(
            &self.dest_addr,
            &[IoSlice::new(&self.packet)],
            0,
            ExplicitCongestionNotification::NotEct,
        )?;
        Ok(())
    }
}

/// The credential and the peer's sender id of a packet a recv_dispatch worker rejected.
///
/// The peer recovers the send contexts of one sender id from each ReplayDetected or StaleKey
/// response, which names that sender id as its queue id. [`Pending`] keeps a response for each
/// sender id of a credential, so a burst on one of them doesn't hold back the response for
/// another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub credential_id: credentials::Id,
    pub sender_id: VarInt,
}

/// The keys of the responses waiting in the UPS queue.
///
/// [`PendingSender`] queues a response only where no response with its key waits in the
/// queue, and [`PendingReceiver`] takes the key out as the background task takes the response
/// off the queue. The queue then holds at most one response for each key: the credentials and
/// sender ids with a rejected packet bound its length, and the rejections don't. The first
/// rejection after a response leaves the queue queues a new one, so the peer still receives a
/// response for each key it keeps sending on.
///
/// The keys are split into one shard for each recv_dispatch worker, each shard behind its own
/// lock in its own cache line. A key's shard is the worker the recv router hands the key's
/// packets to, since both pick it from `routing::hash_id_and_sender` of the credential id and
/// the sender id. The lock of a shard is then taken by one recv_dispatch worker, for each
/// packet it rejects, and by the background task, for each response it takes off the queue,
/// so a burst of rejections on one worker doesn't wait on the rejections of another.
#[derive(Clone)]
pub struct Pending {
    shards: Arc<[CachePadded<Mutex<HashSet<Key>>>]>,
    suppressed: Counter,
}

impl Pending {
    /// `workers` is the number of recv_dispatch workers the recv router spreads the keys over,
    /// and `suppressed` counts the responses the sender drops because one with their key waits
    /// in the queue.
    pub fn new(workers: usize, suppressed: Counter) -> Self {
        Self {
            shards: (0..workers.max(1))
                .map(|_| CachePadded::new(Mutex::new(HashSet::new())))
                .collect(),
            suppressed,
        }
    }

    /// Wraps the sending end of the queue.
    pub fn sender<S>(&self, inner: S) -> PendingSender<S> {
        PendingSender {
            inner,
            pending: self.clone(),
        }
    }

    /// Wraps the receiving end of the queue.
    pub fn receiver<R>(&self, inner: R) -> PendingReceiver<R> {
        PendingReceiver {
            inner,
            pending: self.clone(),
        }
    }

    /// The number of keys with a response waiting in the queue.
    #[cfg(test)]
    pub fn queued(&self) -> usize {
        self.shards.iter().map(|shard| lock(shard).len()).sum()
    }

    /// The shard that holds `key`: the recv_dispatch worker the recv router hands the key's
    /// packets to, as `routing::SenderRoute::worker_id_for_recv` picks it from the same hash
    /// for a power-of-two or any other number of workers.
    pub(crate) fn shard(&self, key: &Key) -> usize {
        let hash = routing::hash_id_and_sender(&key.credential_id, key.sender_id);
        (hash % self.shards.len() as u64) as usize
    }

    fn keys(&self, key: &Key) -> MutexGuard<'_, HashSet<Key>> {
        lock(&self.shards[self.shard(key)])
    }
}

fn lock(keys: &Mutex<HashSet<Key>>) -> MutexGuard<'_, HashSet<Key>> {
    // Each holder makes one insert or one remove, so the set is whole after a panic.
    keys.lock().unwrap_or_else(|err| err.into_inner())
}

/// The sending end of the UPS queue, which drops a response whose key waits in the queue.
#[derive(Clone)]
pub struct PendingSender<S> {
    inner: S,
    pending: Pending,
}

impl<S> UnboundedSender<Entry<Response>> for PendingSender<S>
where
    S: UnboundedSender<Entry<Response>>,
{
    fn send(&mut self, value: Entry<Response>) -> Result<(), Entry<Response>> {
        let key = value.key;
        if !self.pending.keys(&key).insert(key) {
            // The response waiting in the queue tells the peer the same thing.
            self.pending.suppressed.add(1);
            return Ok(());
        }
        let result = self.inner.send(value);
        if result.is_err() {
            // The queue didn't take the response, so its key doesn't wait there.
            self.pending.keys(&key).remove(&key);
        }
        result
    }
}

/// The receiving end of the UPS queue, which takes the key of each response it yields out of
/// [`Pending`], so the next rejection with that key queues a new response.
pub struct PendingReceiver<R> {
    inner: R,
    pending: Pending,
}

impl<R> channel::Receiver<Entry<Response>> for PendingReceiver<R>
where
    R: channel::Receiver<Entry<Response>>,
{
    fn poll_recv(
        &mut self,
        cx: &mut task::Context<'_>,
        budget: &mut Budget,
    ) -> Poll<Option<Entry<Response>>> {
        let response = ready!(self.inner.poll_recv(cx, budget));
        if let Some(response) = &response {
            self.pending.keys(&response.key).remove(&response.key);
        }
        Poll::Ready(response)
    }

    fn on_consumed(&mut self, bytes: u64) {
        self.inner.on_consumed(bytes);
    }
}

// TODO: dedup filter needs to consider both UPS and stale key messages, not just UPS
const CREDENTIAL_ID_OFFSET: usize = 1;
const CREDENTIAL_ID_LEN: usize = 16;

/// Extracts the credential_id from an encoded UnknownPathSecret packet.
///
/// The packet layout is: [tag: 1 byte][credential_id: 16 bytes][...].
#[expect(dead_code)]
fn extract_credential_id(packet: &[u8]) -> Option<credentials::Id> {
    let end = CREDENTIAL_ID_OFFSET + CREDENTIAL_ID_LEN;
    if packet.len() < end {
        return None;
    }
    let mut id = [0u8; CREDENTIAL_ID_LEN];
    id.copy_from_slice(&packet[CREDENTIAL_ID_OFFSET..end]);
    Some(credentials::Id::from(id))
}

/// Per-credential dedup filter for the UPS drain pipeline.
///
/// Maintains a fixed-capacity map of credential_id → last-sent timestamp.
/// When the map exceeds capacity, it is cleared entirely (acceptable loss of
/// dedup accuracy for simplicity).
#[expect(dead_code)]
pub struct DedupFilter {
    seen: HashMap<credentials::Id, Timestamp>,
    capacity: usize,
    window: core::time::Duration,
    pub counters: DedupCounters,
}

pub struct DedupCounters {
    #[expect(dead_code)]
    pub suppressed: Counter,
}

impl DedupFilter {
    pub fn new(capacity: usize, window: core::time::Duration, counters: DedupCounters) -> Self {
        Self {
            seen: HashMap::with_capacity(capacity),
            capacity,
            window,
            counters,
        }
    }

    /// Returns `true` if the response should be sent, `false` if suppressed.
    pub fn check(&mut self, _response: &Response, _now: Timestamp) -> bool {
        // Dedup disabled: stale key and replay control packets must always be sent
        // to ensure the peer learns about invalidated flows promptly.
        true
    }
}

/// Counters for the UPS background send task.
pub struct Counters {
    pub sent: Counter,
    pub send_error: Counter,
    pub dedup_suppressed: Counter,
}

impl Counters {
    pub fn new(registry: &Registry) -> Self {
        Self {
            sent: registry.register("ups.sent"),
            send_error: registry.register("!ups.send_error"),
            dedup_suppressed: registry.register("ups.dedup_suppressed"),
        }
    }
}
