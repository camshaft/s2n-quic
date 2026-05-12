// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Helpers for building stream3 endpoint instances inside Bach simulations.
//!
//! [`setup_sim_endpoint`] creates a fully wired pipeline backed by `bach::net::UdpSocket`
//! sockets, using the Bach clock and a single combined worker so everything runs on the
//! same Bach thread. Handshakes are skipped: call [`connect`] to automatically inject
//! matching fake path-secret entries into the two endpoint maps on demand.

use super::{Budgets, Config, Endpoint, WorkerLayout, setup_endpoint};
use crate::{
    acceptor,
    clock::bach::Clock,
    path::secret::Map as PathSecretMap,
    socket::{pool::Pool, rate::Rate},
    stream3::Stream,
};
use core::net::SocketAddr;
use std::{
    cell::RefCell,
    collections::HashMap,
    sync::Arc,
};

// ── Thread-local endpoint registry ───────────────────────────────────────────
//
// When `setup_sim_endpoint` creates an endpoint it registers its path-secret map
// in this thread-local map, keyed by the endpoint's data socket address.
// [`connect`] looks the peer up here so callers don't have to thread the maps
// through their tests manually.
//
// Bach runs all tasks on a single OS thread, so a `thread_local!` is safe for
// inter-task communication.

thread_local! {
    static SIM_MAP_REGISTRY: RefCell<HashMap<SocketAddr, PathSecretMap>> =
        RefCell::new(HashMap::new());
}

fn register_endpoint_map(data_addr: SocketAddr, map: PathSecretMap) {
    SIM_MAP_REGISTRY.with(|r| {
        r.borrow_mut().insert(data_addr, map);
    });
}

// ── Bach random generator ─────────────────────────────────────────────────────

/// A [`random::Generator`] that draws bytes from the Bach / bolero random scope.
///
/// When running inside a production Bach simulation the scope is seeded
/// deterministically, making test outcomes reproducible.  In ordinary
/// `#[test]` runs it falls back to a randomly seeded `Xoshiro` RNG provided
/// by bolero's thread-local default scope.
struct BachGenerator;

impl crate::random::Generator for BachGenerator {
    #[inline]
    fn public_random_fill(&mut self, dest: &mut [u8]) {
        use bach::rand::AnySliceMutExt as _;
        dest.fill_any();
    }

    #[inline]
    fn private_random_fill(&mut self, dest: &mut [u8]) {
        use bach::rand::AnySliceMutExt as _;
        dest.fill_any();
    }
}

// ── SimEndpointConfig ─────────────────────────────────────────────────────────

/// Describes how to create a simulated endpoint.
///
/// All fields are public so callers can override individual values; the
/// [`Default`] implementation supplies sensible testing values.
pub struct SimEndpointConfig {
    /// Address to bind the send + recv sockets to.
    ///
    /// Use `0.0.0.0:0` (the default) to let Bach assign the group-local IP and
    /// pick an ephemeral port.  Avoid hard-coding `127.0.0.1` or `[::1]` here —
    /// Bach replaces those with the group-assigned IP anyway, and using an
    /// IPv4/IPv6-specific address forces a specific address family.
    pub bind_addr: SocketAddr,

    /// Number of send sockets.  Must be a power of two (≥ 1).
    pub num_send_sockets: usize,

    /// Number of submission shards for the frame channel.  Must be a power of two.
    pub submission_shards: usize,

    /// Overall send rate cap (Gbps).
    pub overall_send_rate: Rate,

    /// Per-socket send rate cap (Gbps).
    pub per_socket_send_rate: Rate,

    /// Per-poll budgets.
    pub budgets: Budgets,

    /// Peer idle timeout passed to the recv cache.
    pub idle_timeout: core::time::Duration,

    /// Maximum transfer unit for the send / recv buffer pools (bytes).
    pub mtu: u16,
}

impl Default for SimEndpointConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:0".parse().unwrap(),
            num_send_sockets: 1,
            submission_shards: 1,
            overall_send_rate: Rate::new(25.0),
            per_socket_send_rate: Rate::new(5.0),
            budgets: Budgets::default(),
            idle_timeout: core::time::Duration::from_secs(30),
            mtu: 1500,
        }
    }
}

// ── setup_sim_endpoint ────────────────────────────────────────────────────────

/// Builds a stream3 [`Endpoint`] wired to Bach simulated UDP sockets.
///
/// All pipeline tasks are pinned to worker 0 (Bach is single-threaded).
/// The returned `Endpoint` is ready for use inside a `testing::sim` closure.
///
/// The endpoint's data address is automatically registered in the thread-local
/// sim endpoint registry so that [`connect`] can find the peer's path-secret map
/// without the caller having to pass it explicitly.
pub fn setup_sim_endpoint(
    config: SimEndpointConfig,
    path_secret_map: PathSecretMap,
    acceptor_registry: acceptor::Registry<Stream>,
) -> Endpoint {
    let SimEndpointConfig {
        bind_addr,
        num_send_sockets,
        submission_shards,
        overall_send_rate,
        per_socket_send_rate,
        budgets,
        idle_timeout,
        mtu,
    } = config;

    assert!(
        num_send_sockets.is_power_of_two(),
        "num_send_sockets must be a power of two"
    );
    assert!(
        submission_shards.is_power_of_two(),
        "submission_shards must be a power of two"
    );

    // Bind send sockets using the synchronous constructor so this function can
    // be called before the Bach runtime drains async tasks.
    let bind_opts = {
        let mut o = bach::net::socket::Options::default();
        o.local_addr = bind_addr;
        o
    };

    let send_sockets: Vec<Arc<bach::net::UdpSocket>> = (0..num_send_sockets)
        .map(|_| {
            let sock =
                bach::net::UdpSocket::new(&bind_opts).expect("failed to bind send socket");
            Arc::new(sock)
        })
        .collect();

    // Bind a single recv socket.
    let recv_socket =
        bach::net::UdpSocket::new(&bind_opts).expect("failed to bind recv socket");
    let recv_sockets = vec![recv_socket];

    let send_pool = Pool::new(mtu);
    let recv_pool = Pool::new(mtu);

    // Update path secret map so it knows how many sender slots to allocate.
    path_secret_map.set_socket_sender_count(num_send_sockets);

    // All workers on index 0 — Bach is single-threaded.
    let layout = WorkerLayout {
        frame_dispatch: 0,
        send: vec![0],
        recv_io: vec![0],
        recv_dispatch: vec![0],
    };

    let spawner = crate::stream2::spawner::bach::Runtime::new(1);
    let clock = Clock::default();
    let gso = s2n_quic_platform::features::Gso::default();

    let endpoint_config = Config {
        spawner,
        layout,
        send_pool,
        recv_pool,
        path_secret_map: path_secret_map.clone(),
        gso,
        acceptor_registry,
        idle_timeout,
        clock,
        overall_send_rate,
        per_socket_send_rate,
        budgets,
        submission_shards,
    };

    let endpoint = setup_endpoint(endpoint_config, send_sockets, recv_sockets, || {
        BachGenerator
    });

    // Register in the thread-local registry so `connect` can find it.
    register_endpoint_map(endpoint.data_addr, path_secret_map);

    endpoint
}

// ── connect ───────────────────────────────────────────────────────────────────

/// Ensures a fake path-secret pair exists between `local_endpoint` and the
/// peer at `peer_addr`, then returns the local path-secret entry for `peer_addr`.
///
/// Path secrets are inserted the first time; if they already exist (e.g. because
/// the test called this function twice) the existing entries are reused.
///
/// Both the local and peer endpoints must have been created via
/// [`setup_sim_endpoint`] in the same sim run so that both maps are registered.
///
/// # Panics
///
/// Panics if `peer_addr` has not been registered (i.e. no endpoint with that
/// data address was created via [`setup_sim_endpoint`] in this sim run).
pub fn connect(
    local_endpoint: &Endpoint,
    peer_addr: SocketAddr,
) -> Arc<crate::path::secret::map::Entry> {
    let local_addr = local_endpoint.data_addr;
    let local_map = &local_endpoint.path_secret_map;

    // Fast path: already connected.
    if let Some(entry) = local_map.get_raw(peer_addr) {
        return entry;
    }

    // Look up the peer's map from the registry.
    let peer_map = SIM_MAP_REGISTRY
        .with(|r| r.borrow().get(&peer_addr).cloned())
        .unwrap_or_else(|| {
            panic!(
                "no sim endpoint registered at {peer_addr}; \
                 call setup_sim_endpoint before connect"
            )
        });

    insert_fake_path_pair(local_map, local_addr, &peer_map, peer_addr);

    local_map
        .get_raw(peer_addr)
        .expect("path-secret entry just inserted by insert_fake_path_pair")
}

// ── insert_fake_path_pair ─────────────────────────────────────────────────────

/// Inserts a pair of matching fake path-secret entries so that two simulated
/// endpoints can exchange encrypted packets without a handshake.
///
/// `local_addr` is the address the peer (at `peer_map`) should send packets to.
/// `peer_addr` is the address the local endpoint (at `local_map`) should target.
///
/// Both maps receive reciprocal entries with the same shared secret. Returns the
/// common credential ID.
///
/// The socket sender count used for the new entries is read from each map; call
/// [`setup_sim_endpoint`] (which calls [`Map::set_socket_sender_count`]) before
/// this function so the entries are allocated with the correct number of sender
/// slots.
pub fn insert_fake_path_pair(
    local_map: &PathSecretMap,
    local_addr: SocketAddr,
    peer_map: &PathSecretMap,
    peer_addr: SocketAddr,
) -> crate::credentials::Id {
    local_map.test_insert_pair(local_addr, None, peer_map, peer_addr, None)
}
