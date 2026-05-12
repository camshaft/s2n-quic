// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Helpers for building stream3 endpoint instances inside Bach simulations.
//!
//! [`setup_sim_endpoint`] creates a fully wired pipeline backed by `bach::net::UdpSocket`
//! sockets, using the bach clock and a single combined worker so everything runs on the
//! same Bach thread. Handshakes are skipped: call [`insert_fake_path_pair`] to inject
//! matching fake path-secret entries into the two maps before sending packets.

use super::{Budgets, Config, Endpoint, WorkerLayout, setup_endpoint};
use crate::{
    acceptor,
    clock::bach::Clock,
    path::secret::Map as PathSecretMap,
    socket::{pool::Pool, rate::Rate},
    stream3::Stream,
};
use core::net::SocketAddr;
use std::sync::Arc;

/// Describes how to create a simulated endpoint.
///
/// All fields are public so callers can override individual values; the
/// [`Default`] implementation supplies sensible testing values.
pub struct SimEndpointConfig {
    /// Address to bind the send + recv sockets to.
    ///
    /// Port 0 lets the OS (or Bach) pick an ephemeral port.
    pub bind_addr: SocketAddr,

    /// Number of send sockets.  Must be a power of two (≥ 1).
    pub num_send_sockets: usize,

    /// Number of submission shards for the frame channel.  Must be a power of two.
    pub submission_shards: usize,

    /// Overall send rate cap.  `Rate::new(100.0)` gives 100 Gbps (effectively unlimited
    /// in simulation).
    pub overall_send_rate: Rate,

    /// Per-socket send rate cap.  Same default as `overall_send_rate`.
    pub per_socket_send_rate: Rate,

    /// Per-poll budgets.
    pub budgets: Budgets,

    /// Peer idle timeout passed to the recv cache.
    pub idle_timeout: core::time::Duration,
}

impl Default for SimEndpointConfig {
    fn default() -> Self {
        Self {
            bind_addr: "[::1]:0".parse().unwrap(),
            num_send_sockets: 1,
            submission_shards: 1,
            overall_send_rate: Rate::new(100.0),
            per_socket_send_rate: Rate::new(100.0),
            budgets: Budgets::default(),
            idle_timeout: core::time::Duration::from_secs(30),
        }
    }
}

/// Builds a stream3 [`Endpoint`] wired to Bach simulated UDP sockets.
///
/// All pipeline tasks are pinned to worker 0 (Bach is single-threaded).
/// The returned `Endpoint` is ready for use inside a `testing::sim` closure.
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

    // Use a small pool fitting one standard-MTU packet at a time.
    let mtu = 1500u16;
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
    let gso = {
        let g = s2n_quic_platform::features::Gso::default();
        g.disable();
        g
    };

    let endpoint_config = Config {
        spawner,
        layout,
        send_pool,
        recv_pool,
        path_secret_map,
        gso,
        acceptor_registry,
        idle_timeout,
        clock,
        overall_send_rate,
        per_socket_send_rate,
        budgets,
        submission_shards,
    };

    setup_endpoint(endpoint_config, send_sockets, recv_sockets, || {
        s2n_quic_core::random::testing::Generator::default()
    })
}

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
