// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Stream3 Endpoint: shared infrastructure for the process.

pub(crate) mod ack;
pub(crate) mod assemble;
pub(crate) mod counters;
pub(crate) mod decode;
pub(crate) mod dispatch;
pub(crate) mod inflight;
pub(crate) mod msg;
pub(crate) mod recv;
pub(crate) mod reset_error;
pub(crate) mod routing;
pub(crate) mod send;
pub(crate) mod socket;
pub(crate) mod tasks;
pub(crate) mod worker;

use crate::{
    acceptor,
    stream3::{frame::SubmissionSender, Stream},
};
use std::sync::atomic::AtomicU64;

pub struct Endpoint {
    /// Frame submission channel (writers submit Queue<Frame> here)
    pub frame_tx: SubmissionSender,
    /// Path secret map (shared with PSK providers)
    pub path_secret_map: crate::path::secret::Map,
    /// Queue allocator for flow queues
    pub queue_allocator: msg::queue::Allocator,
    /// Acceptor registry for server-side stream dispatch
    pub acceptor_registry: acceptor::Registry<Stream>,
    /// Endpoint-wide stream ID counter
    pub next_stream_id: AtomicU64,
    /// The port that recv sockets are bound to
    pub data_port: u16,
}

// ── Pipeline Setup ────────────────────────────────────────────────────────

/// Configuration for the stream3 pipeline.
pub struct EndpointConfig<S, C> {
    /// Worker pool spawner.
    pub spawner: S,
    /// Buffer pool for outbound (send) packets.
    pub send_pool: crate::socket::pool::Pool,
    /// Buffer pool for inbound (recv) packets.
    pub recv_pool: crate::socket::pool::Pool,
    /// Path-secret map shared with PSK providers.
    pub path_secret_map: crate::path::secret::Map,
    /// GSO capability probed for the local host.
    pub gso: s2n_quic_platform::features::Gso,
    /// Server-side acceptor registry.
    pub acceptor_registry: acceptor::Registry<Stream>,
    /// Peer idle timeout — controls when [`recv::Cache`] entries expire.
    ///
    /// [`recv::Cache`]: recv::Cache
    pub idle_timeout: core::time::Duration,
    /// Wall-clock source used for RTT estimation and timeouts.
    pub clock: C,
}

/// Assembles the stream3 pipeline from pre-opened sockets and spawns worker tasks.
///
/// This is the top-level composition function. It creates all inter-task channels, spawns one task
/// per pipeline stage, and returns a ready-to-use [`Endpoint`]. No pipeline logic lives here —
/// every stage is implemented in the task functions in [`tasks`].
///
/// # Worker distribution
///
/// Workers are assigned by simple modulo:
/// * Worker `0` runs the frame-dispatch task (routes batches to send sockets).
/// * Send workers handle per-socket assembly and transmission.
/// * Remaining workers pair a socket-recv task with a packet-dispatch task.
///
/// When the worker count exceeds the number of sockets, extra workers are idle. When the socket
/// count exceeds workers, multiple sockets share a worker.
pub fn setup_endpoint<SendSocket, RecvSocket, G, S, C>(
    config: EndpointConfig<S, C>,
    send_sockets: Vec<SendSocket>,
    recv_sockets: Vec<RecvSocket>,
    create_rand: impl Fn() -> G,
) -> Endpoint
where
    SendSocket: crate::socket::send::Socket + Send + 'static,
    RecvSocket: crate::socket::recv::Socket + Send + 'static,
    G: crate::random::Generator,
    S: crate::stream2::Spawner,
    C: s2n_quic_core::time::Clock
        + crate::clock::precision::Clock
        + Clone
        + Send
        + 'static,
{
    let num_send = send_sockets.len();

    // Choose the routing implementation that best fits the socket count.
    if num_send.is_power_of_two() {
        setup_endpoint_inner::<_, _, _, _, _, routing::PowerOfTwoRoute>(
            config,
            send_sockets,
            recv_sockets,
            create_rand,
        )
    } else {
        setup_endpoint_inner::<_, _, _, _, _, routing::ModuloRoute>(
            config,
            send_sockets,
            recv_sockets,
            create_rand,
        )
    }
}

fn setup_endpoint_inner<SendSocket, RecvSocket, G, S, C, SenderRoute>(
    config: EndpointConfig<S, C>,
    send_sockets: Vec<SendSocket>,
    recv_sockets: Vec<RecvSocket>,
    create_rand: impl Fn() -> G,
) -> Endpoint
where
    SendSocket: crate::socket::send::Socket + Send + 'static,
    RecvSocket: crate::socket::recv::Socket + Send + 'static,
    G: crate::random::Generator,
    S: crate::stream2::Spawner,
    C: s2n_quic_core::time::Clock
        + crate::clock::precision::Clock
        + Clone
        + Send
        + 'static,
    SenderRoute: routing::SenderRoute,
{
    use crate::{
        counter::Registry as CounterRegistry,
        socket::channel::{cell, intrusive_queue},
        stream2::spawner::LocalSpawner as _,
        stream3::frame,
    };

    let EndpointConfig {
        spawner,
        send_pool,
        recv_pool,
        path_secret_map,
        gso,
        acceptor_registry,
        idle_timeout,
        clock,
    } = config;

    let num_workers = spawner.worker_count().max(1);
    let num_send = send_sockets.len();
    let num_recv = recv_sockets.len();

    // The port our recv sockets listen on — embedded in outbound packets so peers can ACK back.
    let source_control_port = recv_sockets
        .first()
        .and_then(|s| s.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(0);

    // Frame submission channel: all writers share one sharded sender; one dispatch task drains it.
    let shard_count = (num_workers * 4).next_power_of_two();
    let (frame_tx, frame_rx) = frame::submission_channel(shard_count);

    // Per-send-socket channels ---------------------------------------------------
    // batch channel: pick_two routes FrameBatch items; the send task drains them.
    // ack channel:   dispatch tasks route ACK messages; the send task processes them.
    let (socket_batch_txs, socket_batch_rxs): (Vec<_>, Vec<_>) = (0..num_send)
        .map(|_| cell::sync::new::<tasks::FrameBatch>())
        .unzip();
    let (socket_ack_txs, socket_ack_rxs): (Vec<_>, Vec<_>) = (0..num_send)
        .map(|_| intrusive_queue::sync::new::<msg::Sender>())
        .unzip();

    // Shared flow-queue allocator and dispatch counters -------------------------
    let queue_allocator = msg::queue::Allocator::new();
    let queue_dispatcher = queue_allocator.dispatcher();
    let counter_registry = CounterRegistry::default();
    let counters = counters::Dispatch::new(&counter_registry);
    let decode_error_counter = counters.rx_none.clone();

    // Worker 0: frame-dispatch --------------------------------------------------
    // Drains the sharded submission channel, batches by path secret, and routes to sockets.
    {
        let socket_batch_txs = socket_batch_txs;
        let random = std::sync::Mutex::new(create_rand());
        spawner.spawn_local(0, move |mut local| {
            let random_fn = move |n: usize| {
                let mut bytes = [0u8; 8];
                random.lock().unwrap().public_random_fill(&mut bytes);
                usize::from_le_bytes(bytes) % n.max(1)
            };
            local.spawn(tasks::frame_dispatch(frame_rx, socket_batch_txs, random_fn));
        });
    }

    // Per-send-socket workers ---------------------------------------------------
    for (sender_idx, (socket, (batch_rx, ack_rx))) in send_sockets
        .into_iter()
        .zip(socket_batch_rxs.into_iter().zip(socket_ack_rxs.into_iter()))
        .enumerate()
    {
        let worker_id = (1 + sender_idx) % num_workers;
        let gso = gso.clone();
        let pool = send_pool.clone();

        spawner.spawn_local(worker_id, move |mut local| {
            local.spawn(tasks::socket_send_task(
                socket,
                batch_rx,
                ack_rx,
                sender_idx,
                source_control_port,
                gso,
                pool,
            ));
        });
    }

    // Per-recv-socket workers ---------------------------------------------------
    // Each recv socket is paired with a packet-dispatch task on the same worker.
    for (recv_idx, socket) in recv_sockets.into_iter().enumerate() {
        let worker_id = (1 + num_send + recv_idx) % num_workers;
        let pool = recv_pool.clone();
        let path_secret_map = path_secret_map.clone();
        let acceptor_registry = acceptor_registry.clone();
        let frame_tx = frame_tx.clone();
        let ack_sender = routing::AckSender::new(socket_ack_txs.clone());
        let queue_dispatcher = queue_dispatcher.clone();
        let counters = counters.clone();
        let decode_error_counter = decode_error_counter.clone();
        let clock = clock.clone();

        let (packet_tx, packet_rx) = intrusive_queue::sync::new();

        spawner.spawn_local(worker_id, move |mut local| {
            local.spawn(tasks::socket_recv_task(
                socket,
                pool,
                packet_tx,
                decode_error_counter,
            ));
            local.spawn(tasks::packet_dispatch_task(
                packet_rx,
                worker_id,
                idle_timeout,
                path_secret_map,
                acceptor_registry,
                frame_tx,
                ack_sender,
                queue_dispatcher,
                counters,
                clock,
            ));
        });
    }

    Endpoint {
        frame_tx,
        path_secret_map,
        queue_allocator,
        acceptor_registry,
        next_stream_id: AtomicU64::new(0),
        data_port: source_control_port,
    }
}
