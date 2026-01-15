// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Peer registry for managing remote endpoints in a mesh network.
//!
//! This module provides functionality similar to libfabric's Address Vector (AV),
//! but simplified for socket addresses. Peers are registered and assigned opaque
//! handles that can be used for efficient lookups and to establish QUIC connections
//! for credential exchange and capability discovery.

use dashmap::DashMap;
use s2n_quic::connection::Error;
use std::{hash::Hash, net::SocketAddr, sync::Arc};

/// An opaque handle representing a registered peer.
///
/// Peer handles are used instead of socket addresses when initiating
/// requests to avoid repeated lookups and to efficiently access peer
/// metadata (credentials, capabilities, etc.).
///
/// Handles are cheap to copy and can be shared across threads.
#[derive(Debug, Clone)]
pub struct Handle(Arc<Entry>);

impl PartialEq for Handle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Handle {}

impl Hash for Handle {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.id.hash(state)
    }
}

/// Registry for managing peer connections in the mesh.
///
/// The peer registry maintains a mapping between socket addresses and
/// peer handles, and stores metadata about each peer including their
/// credentials and connection capabilities.
///
/// The registry can be cloned cheaply and shared across threads.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<InnerRegistry>,
}

struct InnerRegistry {
    /// Map from socket address to peer entry
    by_addr: DashMap<SocketAddr, Handle>,
    by_id: DashMap<u64, Handle>,
    // TODO add a backend to lookup the raw ID
}

/// Information about a registered peer.
#[derive(Debug)]
struct Entry {
    /// The unique ID for this peer
    id: u64,
    /// The handshake address of this peer
    addr: SocketAddr,
}

impl Registry {
    /// Creates a new peer registry.
    pub fn new() -> Self {
        todo!()
    }

    /// Registers a peer by socket address and returns its handle.
    ///
    /// If the peer is already registered, returns the existing handle.
    /// Otherwise, creates a new entry and assigns a fresh handle.
    ///
    /// # Arguments
    ///
    /// * `addr` - The socket address of the peer to register
    ///
    /// # Returns
    ///
    /// The peer handle that can be used to reference this peer.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let registry = PeerRegistry::new();
    /// let handle = registry.register_peer("127.0.0.1:8080".parse().unwrap()).await.unwrap();
    /// ```
    pub async fn register(&self, addr: SocketAddr) -> Result<Handle, Error> {
        // Check if peer is already registered
        if let Some(entry) = self.inner.by_addr.get(&addr) {
            return Ok(entry.clone());
        }

        todo!("issue handshake")
    }

    /// Returns the number of registered peers.
    pub fn len(&self) -> usize {
        self.inner.by_addr.len()
    }

    /// Returns whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.by_addr.is_empty()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
