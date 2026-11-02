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
//!
//! The sender of a packet whose credential the map doesn't hold chooses the credential and
//! sender id of its UnknownPathSecret response, so [`Pending`] also keeps those responses in
//! the queue within the bytes the pacer sends in one burst.

use crate::{
    counter::{Counter, Registry},
    credentials,
    intrusive::Entry,
    msg::addr::Addr,
    socket::channel::{self, Budget, ByteCost, Sendable, UnboundedSender},
    time::precision::Timestamp,
};
use core::task::{self, Poll};
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
    /// Whether the path secret map holds the credential of the packet the response answers.
    pub kind: Kind,
}

impl Response {
    /// The bytes the response holds toward the bound on UnknownPathSecret responses in the
    /// queue: its packet's for an UnknownPathSecret response, and none for another.
    fn unknown_bytes(&self) -> u64 {
        match self.kind {
            Kind::UnknownPathSecret => self.packet.len() as u64,
            Kind::Rejection => 0,
        }
    }
}

/// What the packet a response answers carried, which decides what bounds the responses of its
/// kind in the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An UnknownPathSecret response, to a packet whose credential the path secret map doesn't
    /// hold. The packet's sender chooses its credential and sender id, so [`Pending`] bounds the
    /// bytes of these responses in the queue by the pacer's burst.
    UnknownPathSecret,
    /// A ReplayDetected or StaleKey response, to a packet whose credential the map holds. The
    /// map's entries and the peer's sender ids bound the keys of these responses.
    Rejection,
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

/// The keys of the responses waiting in the UPS queue, and the bytes of the UnknownPathSecret
/// responses among them.
///
/// [`PendingSender`] queues a response only where no response with its key waits in the
/// queue, and [`PendingReceiver`] takes the key out as the background task takes the response
/// off the queue. The queue then holds at most one response for each key: the credentials and
/// sender ids with a rejected packet bound its length, and the rejections don't. The first
/// rejection after a response leaves the queue queues a new one, so the peer still receives a
/// response for each key it keeps sending on.
///
/// The sender of a packet whose credential the path secret map doesn't hold chooses the key of
/// its UnknownPathSecret response, so a sender of many distinct credential ids would still
/// queue one response for each. [`PendingSender`] also drops an UnknownPathSecret response
/// where the UnknownPathSecret responses waiting in the queue would hold more bytes with it
/// than the pacer sends in one burst, and queues one whatever its size where none waits. A
/// peer that keeps sending on a credential the map doesn't hold queues a new response once the
/// queue has room for it.
#[derive(Clone)]
pub struct Pending {
    state: Arc<Mutex<State>>,
    suppressed: Counter,
    unknown_dropped: Counter,
}

impl Pending {
    /// `suppressed` counts the responses the sender drops because one with their key waits in
    /// the queue, and `unknown_dropped` the UnknownPathSecret responses it drops because the
    /// ones waiting would hold more than `unknown_capacity` bytes with it: the bytes the pacer
    /// that sends the queue sends in one burst,
    /// [`Rate::burst_bytes`](crate::socket::rate::Rate::burst_bytes).
    pub fn new(suppressed: Counter, unknown_dropped: Counter, unknown_capacity: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                keys: Default::default(),
                unknown_bytes: 0,
                unknown_capacity,
            })),
            suppressed,
            unknown_dropped,
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
        self.state().keys.len()
    }

    /// The bytes of the UnknownPathSecret responses waiting in the queue.
    #[cfg(test)]
    pub fn queued_unknown_bytes(&self) -> u64 {
        self.state().unknown_bytes
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Each holder makes one insert or one remove of a key, with its bytes, so the state is
        // whole after a panic.
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }
}

/// The state behind the lock of [`Pending`].
struct State {
    /// The key of each response waiting in the queue.
    keys: HashSet<Key>,
    /// The bytes of the UnknownPathSecret responses waiting in the queue.
    unknown_bytes: u64,
    /// The most bytes of UnknownPathSecret responses the queue holds, unless one response alone
    /// holds more.
    unknown_capacity: u64,
}

/// Why [`PendingSender`] drops a response.
enum Dropped {
    /// A response with its key waits in the queue.
    Waiting,
    /// It is an UnknownPathSecret response, and the ones waiting in the queue would hold more
    /// than `unknown_capacity` bytes with it.
    Full,
}

impl State {
    /// Takes in the key and the UnknownPathSecret bytes of a response the sender queues, or
    /// answers why the sender drops it.
    fn admit(&mut self, key: Key, unknown_bytes: u64) -> Result<(), Dropped> {
        if self.keys.contains(&key) {
            return Err(Dropped::Waiting);
        }
        // With no UnknownPathSecret response waiting, the queue takes one whatever its size, so
        // a burst smaller than one response doesn't keep each of them out.
        if unknown_bytes > 0
            && self.unknown_bytes > 0
            && self.unknown_bytes + unknown_bytes > self.unknown_capacity
        {
            return Err(Dropped::Full);
        }
        self.keys.insert(key);
        self.unknown_bytes += unknown_bytes;
        Ok(())
    }

    /// Takes out the key and the UnknownPathSecret bytes of a response that left the queue, or
    /// that the queue refused.
    fn release(&mut self, key: &Key, unknown_bytes: u64) {
        self.keys.remove(key);
        self.unknown_bytes = self.unknown_bytes.saturating_sub(unknown_bytes);
    }
}

/// The sending end of the UPS queue, which drops a response whose key waits in the queue, and
/// an UnknownPathSecret response past the pacer's burst.
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
        let unknown_bytes = value.unknown_bytes();
        let admitted = self.pending.state().admit(key, unknown_bytes);
        match admitted {
            Ok(()) => {}
            Err(Dropped::Waiting) => {
                // The response waiting in the queue tells the peer the same thing.
                self.pending.suppressed.add(1);
                return Ok(());
            }
            Err(Dropped::Full) => {
                // The UnknownPathSecret responses waiting in the queue go out first, and the
                // peer's next packet on the credential queues a new one once there is room.
                self.pending.unknown_dropped.add(1);
                return Ok(());
            }
        }
        let result = self.inner.send(value);
        if result.is_err() {
            // The queue didn't take the response, so neither its key nor its bytes wait there.
            self.pending.state().release(&key, unknown_bytes);
        }
        result
    }
}

/// The receiving end of the UPS queue, which takes the key of each response it yields, and its
/// UnknownPathSecret bytes, out of [`Pending`], so the next rejection with that key queues a new
/// response.
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
            self.pending
                .state()
                .release(&response.key, response.unknown_bytes());
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
