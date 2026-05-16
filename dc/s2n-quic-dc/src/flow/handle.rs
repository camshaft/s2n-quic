// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Flow handle with validation support

use crate::credentials;
use rustc_hash::{FxHashMap, FxHashSet};
use s2n_quic_core::varint::VarInt;
use std::{
    collections::{hash_map, VecDeque},
    sync::Arc,
};

/// Flow handle that validates credentials and stream_id
///
/// This is used as the Key type in the queue system to ensure that
/// packets routed to a flow actually belong to that flow.
#[derive(Debug, Clone)]
pub struct Handle {
    /// Global stream identifier (client-wide)
    stream_id: VarInt,
    /// Inner state (server-side with drop channel, or client-side with path entry)
    inner: HandleInner,
}

#[derive(Clone)]
enum HandleInner {
    /// Server-side handle — sends stream_id to drop channel on drop
    Server {
        credential_id: credentials::Id,
        drop_channel: Arc<DropChannel>,
    },
    /// Client-side handle with path secret entry
    Client {
        path_entry: Arc<crate::path::secret::map::Entry>,
    },
}

impl core::fmt::Debug for HandleInner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Server { .. } => f.debug_struct("Server").finish_non_exhaustive(),
            Self::Client { .. } => f.debug_struct("Client").finish_non_exhaustive(),
        }
    }
}

impl HandleInner {
    fn credential_id(&self) -> &credentials::Id {
        match self {
            Self::Server { credential_id, .. } => credential_id,
            Self::Client { path_entry } => path_entry.id(),
        }
    }
}

impl Handle {
    /// Create a client-side handle with path secret entry
    pub fn client(stream_id: VarInt, path_entry: Arc<crate::path::secret::map::Entry>) -> Self {
        Self {
            stream_id,
            inner: HandleInner::Client { path_entry },
        }
    }

    pub fn credential_id(&self) -> &credentials::Id {
        self.inner.credential_id()
    }

    pub fn stream_id(&self) -> VarInt {
        self.stream_id
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if let HandleInner::Server { drop_channel, .. } = &self.inner {
            drop_channel.push(self.stream_id);
        }
    }
}

/// Thread-safe drop notification channel. Handle::drop pushes stream_ids here;
/// the owning dispatch worker drains them.
pub struct DropChannel {
    pending: parking_lot::Mutex<VecDeque<VarInt>>,
}

impl DropChannel {
    pub fn new() -> Self {
        Self {
            pending: parking_lot::Mutex::new(VecDeque::new()),
        }
    }

    fn push(&self, stream_id: VarInt) {
        self.pending.lock().push_back(stream_id);
    }

    fn drain_into(&self, buf: &mut VecDeque<VarInt>) {
        let mut pending = self.pending.lock();
        if pending.is_empty() {
            return;
        }
        // Swap: buf (empty from last drain) goes into pending for allocation reuse,
        // pending (with items) comes out into buf for processing.
        core::mem::swap(&mut *pending, buf);
    }
}

/// Request parameters for flow validation
#[derive(Debug, Clone, Copy)]
pub struct Request {
    pub credential_id: credentials::Id,
    pub stream_id: VarInt,
}

impl crate::flow::queue::Key for Handle {
    type Request = Request;

    #[inline]
    fn validate(&self, params: &Self::Request) -> Result<(), crate::flow::queue::ValidationError> {
        use crate::flow::queue::ValidationError;
        if self.credential_id() != &params.credential_id {
            return Err(ValidationError::CredentialMismatch);
        }
        if self.stream_id != params.stream_id {
            return Err(ValidationError::StreamIdMismatch);
        }
        Ok(())
    }
}

/// Sentinel value returned by `Tracker::try_register` when the stream_id is
/// recognised as a recently-completed flow.  Using `VarInt::MAX` is safe because
/// real queue IDs are assigned by a monotonically-incrementing counter starting
/// from 0 and can never reach this value in practice.
pub const COMPLETED_SENTINEL: VarInt = VarInt::MAX;

/// Maximum number of recently-completed stream_ids retained by the Tracker.
///
/// Any TooOld FlowInit retransmission can only arrive if its attempt_id fell
/// outside the AttemptDedup window (capacity = 2048).  In the worst case the
/// right_edge can advance by at most (total streams) since the stream was
/// completed.  We keep 4× the dedup window as a generous safety margin for
/// typical workloads; for the 10 000-stream fuzz scenario this covers all
/// possible TooOld completions.
const COMPLETED_RING_CAPACITY: usize = 8192;

/// Tracker for managing flow lifecycle on a single dispatch worker thread.
///
/// The map is thread-local (Rc + RefCell). Cross-thread Handle drops are
/// received via a shared DropChannel and applied during `drain_drops`.
#[derive(Clone)]
pub struct Tracker {
    map: std::rc::Rc<std::cell::RefCell<FxHashMap<VarInt, VarInt>>>,
    drop_channel: Arc<DropChannel>,
    drain_buf: std::rc::Rc<std::cell::RefCell<VecDeque<VarInt>>>,
    credentials: credentials::Id,
    /// Ring buffer of recently-completed stream_ids (capped at
    /// [`COMPLETED_RING_CAPACITY`]).  Used by `try_register` to detect TooOld
    /// FlowInit retransmissions for streams that already finished, preventing
    /// them from being dispatched to the acceptor as spurious new streams.
    completed_ring: std::rc::Rc<std::cell::RefCell<VecDeque<VarInt>>>,
    /// Hash-set mirror of `completed_ring` for O(1) membership tests.
    completed_set: std::rc::Rc<std::cell::RefCell<FxHashSet<VarInt>>>,
}

impl core::fmt::Debug for Tracker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Tracker")
            .field("credentials", &self.credentials)
            .finish_non_exhaustive()
    }
}

impl Tracker {
    #[inline]
    pub fn new(credentials: credentials::Id) -> Self {
        Self {
            map: Default::default(),
            drop_channel: Arc::new(DropChannel::new()),
            drain_buf: Default::default(),
            credentials,
            completed_ring: Default::default(),
            completed_set: Default::default(),
        }
    }

    /// Drain pending Handle drops and remove them from the local map.
    #[inline]
    fn drain_drops(&self) {
        let mut buf = self.drain_buf.borrow_mut();
        self.drop_channel.drain_into(&mut buf);
        if buf.is_empty() {
            return;
        }
        let mut map = self.map.borrow_mut();
        let mut ring = self.completed_ring.borrow_mut();
        let mut set = self.completed_set.borrow_mut();
        for stream_id in buf.drain(..) {
            map.remove(&stream_id);
            // Record as recently completed so that TooOld retransmissions of
            // this stream_id are recognised and discarded in try_register.
            if ring.len() >= COMPLETED_RING_CAPACITY {
                if let Some(evicted) = ring.pop_front() {
                    set.remove(&evicted);
                }
            }
            ring.push_back(stream_id);
            set.insert(stream_id);
        }
    }

    #[inline]
    pub fn try_register<Q>(
        &self,
        stream_id: VarInt,
        create_queue: impl FnOnce(Handle) -> (VarInt, Q),
    ) -> Result<Q, VarInt> {
        self.drain_drops();

        // Reject retransmissions for recently-completed streams before checking
        // the active map.  These arrive on the TooOld path when a PTO probe
        // for a long-gone stream makes it past the attempt-dedup window.
        if self.completed_set.borrow().contains(&stream_id) {
            tracing::trace!(
                stream_id = stream_id.as_u64(),
                "TooOld FlowInit for recently-completed stream — stale retransmission"
            );
            return Err(COMPLETED_SENTINEL);
        }

        match self.map.borrow_mut().entry(stream_id) {
            hash_map::Entry::Occupied(entry) => {
                return Err(*entry.get());
            }
            hash_map::Entry::Vacant(entry) => {
                let handle = Handle {
                    stream_id,
                    inner: HandleInner::Server {
                        credential_id: self.credentials,
                        drop_channel: self.drop_channel.clone(),
                    },
                };
                let (queue_id, queue) = create_queue(handle);
                entry.insert(queue_id);
                Ok(queue)
            }
        }
    }
}
