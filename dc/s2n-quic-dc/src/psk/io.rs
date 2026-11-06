// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
use super::{client, server};
use crate::{
    path::secret::{self, map::HandshakingPath},
    tracing::*,
};
use cfg_if::cfg_if;
use rand::RngExt;
use s2n_quic::{
    provider::{
        dc::{ConfirmComplete, MtuConfirmComplete},
        event::Subscriber as Sub,
        tls::Provider as Prov,
    },
    server::Name,
};
use s2n_quic_core::{
    event::api::{self as events, Subscriber as CoreSub},
    inet::SocketAddress,
};
use std::{
    hash::BuildHasher,
    io,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::time::Instant as TokioInstant;

mod limit;

pub use crate::endpoint::DEFAULT_IDLE_TIMEOUT;
/// Connection-level flow-control window and the local send window. Bounds the peer's
/// connection-level `remote_max_data`; the effective send rate is gated by the peer's advertised
/// per-stream window and the endpoint send credit pool, so this stays generous.
pub const DEFAULT_MAX_DATA: u64 = 1u64 << 23;
/// Per-stream *recv* window we advertise (`local_recv_max_data`). This is also the unbacked initial
/// window a fresh stream may receive before any pool-backed `MAX_DATA` grant, so it is kept small:
/// a fan-out of N peers can each fill this much before drawing from the recv credit pool. Window
/// growth beyond this is pool-backed and capped per-stream by the recv pool's `max_single_acquire`.
pub const DEFAULT_RECV_WINDOW: u64 = 64 * 1024;
pub const DEFAULT_BASE_MTU: u16 = 1450;
#[cfg(target_os = "linux")]
pub const DEFAULT_MTU: u16 = 8940;
#[cfg(not(target_os = "linux"))]
pub const DEFAULT_MTU: u16 = DEFAULT_BASE_MTU;
/// Jitter PTO probes by 33% to prevent synchronized timeouts across multiple connections
pub const DEFAULT_PTO_JITTER_PERCENTAGE: u8 = 33;
const DEFAULT_INITIAL_RTT: Duration = Duration::from_millis(1);

const BUFFER_SIZE: usize = 16 * 1024;

struct DcPathCapture;

pub(crate) struct DcPathContext {
    path: Option<HandshakingPath>,
}

impl CoreSub for DcPathCapture {
    type ConnectionContext = DcPathContext;

    fn create_connection_context(
        &mut self,
        _meta: &events::ConnectionMeta,
        _info: &events::ConnectionInfo,
    ) -> Self::ConnectionContext {
        DcPathContext { path: None }
    }

    fn on_dc_path_created(
        &mut self,
        context: &mut Self::ConnectionContext,
        _meta: &events::ConnectionMeta,
        event: &events::DcPathCreated,
    ) {
        if let Some(path) = event.path.downcast_ref::<Option<HandshakingPath>>() {
            context.path = path.clone();
        }
    }
}

/// Records the lowest round-trip time each connection's recovery measured, for the handshake
/// queue's start limit to read a handshake's time against.
struct PathRtt;

struct PathRttContext {
    /// The `min_rtt` of the connection's newest recovery metrics, or `None` before the first.
    min_rtt: Option<Duration>,
}

impl CoreSub for PathRtt {
    type ConnectionContext = PathRttContext;

    fn create_connection_context(
        &mut self,
        _meta: &events::ConnectionMeta,
        _info: &events::ConnectionInfo,
    ) -> Self::ConnectionContext {
        PathRttContext { min_rtt: None }
    }

    fn on_recovery_metrics(
        &mut self,
        context: &mut Self::ConnectionContext,
        _meta: &events::ConnectionMeta,
        event: &events::RecoveryMetrics,
    ) {
        context.min_rtt = Some(event.min_rtt);
    }
}

/// The random of a ServerHello that is a HelloRetryRequest (RFC 8446, section 4.1.3).
const HELLO_RETRY_REQUEST_RANDOM: [u8; 32] = [
    0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8, 0x91,
    0xc2, 0xa2, 0x11, 0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8, 0x33, 0x9c,
];

/// Counts what each client connection's handshake waited on its peer past its two round trips,
/// for the handshake queue's start limit to read a handshake's time against.
struct HandshakeWaits;

struct HandshakeWaitsContext {
    waits: limit::Waits,
}

impl CoreSub for HandshakeWaits {
    type ConnectionContext = HandshakeWaitsContext;

    fn create_connection_context(
        &mut self,
        _meta: &events::ConnectionMeta,
        _info: &events::ConnectionInfo,
    ) -> Self::ConnectionContext {
        HandshakeWaitsContext {
            waits: limit::Waits::default(),
        }
    }

    /// A Retry that the client takes makes it send its first flight again.
    fn on_packet_received(
        &mut self,
        context: &mut Self::ConnectionContext,
        _meta: &events::ConnectionMeta,
        event: &events::PacketReceived,
    ) {
        if let events::PacketHeader::Retry { .. } = event.packet_header {
            context.waits.round_trips = context.waits.round_trips.saturating_add(1);
        }
    }

    /// The client publishes a Retry it receives before it checks it, and a Retry that it
    /// discards doesn't add a round trip.
    fn on_packet_dropped(
        &mut self,
        context: &mut Self::ConnectionContext,
        _meta: &events::ConnectionMeta,
        event: &events::PacketDropped,
    ) {
        if let events::PacketDropReason::RetryDiscarded { .. } = event.reason {
            context.waits.round_trips = context.waits.round_trips.saturating_sub(1);
        }
    }

    /// A HelloRetryRequest makes the client send a new ClientHello.
    fn on_tls_server_hello(
        &mut self,
        context: &mut Self::ConnectionContext,
        _meta: &events::ConnectionMeta,
        event: &events::TlsServerHello,
    ) {
        if is_hello_retry_request(event.payload) {
            context.waits.round_trips = context.waits.round_trips.saturating_add(1);
        }
    }

    /// A lost packet's data arrives again after a wait that isn't a whole number of round trips.
    /// A lost MTU probe doesn't hold up the handshake.
    fn on_packet_lost(
        &mut self,
        context: &mut Self::ConnectionContext,
        _meta: &events::ConnectionMeta,
        event: &events::PacketLost,
    ) {
        if !event.is_mtu_probe {
            context.waits.lost = true;
        }
    }
}

/// Whether a ServerHello's body, in `payload`'s parts, is a HelloRetryRequest: its random is the
/// 32 bytes after its 2-byte `legacy_version`.
fn is_hello_retry_request(payload: &[&[u8]]) -> bool {
    let mut random = payload.iter().copied().flatten().skip(2);
    HELLO_RETRY_REQUEST_RANDOM
        .iter()
        .all(|byte| random.next() == Some(byte))
}

/// The lowest round-trip time that a connection's recovery measured, or `None` before the
/// first.
fn connection_rtt(connection: &s2n_quic::Connection) -> Option<Duration> {
    connection
        .query_event_context(|context: &PathRttContext| context.min_rtt)
        .ok()
        .flatten()
}

/// What a connection's handshake waited on its peer past its two round trips, as its events
/// showed it so far.
fn connection_waits(connection: &s2n_quic::Connection) -> limit::Waits {
    connection
        .query_event_context(|context: &HandshakeWaitsContext| context.waits)
        .unwrap_or_default()
}

/// The path secret map entry that a client's connection made once the server's
/// `DC_STATELESS_RESET_TOKENS` frame arrived, or `None` before it.
fn connection_entry(connection: &s2n_quic::Connection) -> Option<Arc<secret::map::Entry>> {
    connection
        .query_event_context(|context: &DcPathContext| {
            context.path.as_ref().and_then(|path| path.entry())
        })
        .ok()
        .flatten()
}

/// Reads the start limit's answer for a handshake that reached dc.complete in `elapsed` from its
/// connect, as `Answer::for_handshake` reads it from `earlier_rtt`, the lowest round-trip time
/// that the earlier handshakes with the peer measured, and the lowest that this connection
/// measured, and what the connection's handshake waited past its two round trips. It lowers
/// the round-trip time of the connection's map entry to the lower of the two times, so that
/// the next handshake with the peer reads it as its `earlier_rtt`.
fn start_answer(
    connection: &s2n_quic::Connection,
    earlier_rtt: Option<Duration>,
    elapsed: Duration,
) -> limit::Answer {
    let rtt = connection_rtt(connection);
    let lowest = earlier_rtt.into_iter().chain(rtt).min();
    if let (Some(entry), Some(lowest)) = (connection_entry(connection), lowest) {
        entry.lower_path_rtt(lowest);
    }
    limit::Answer::for_handshake(elapsed, earlier_rtt, rtt, connection_waits(connection))
}

pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

pub type Result<T = (), E = Error> = core::result::Result<T, E>;

pub struct Server {
    server: s2n_quic::Server,
}

impl Server {
    pub fn bind<
        Provider: Prov + Send + Sync + 'static,
        Subscriber: Sub + Send + Sync + 'static,
        Event: s2n_quic::provider::event::Subscriber,
    >(
        addr: SocketAddr,
        map: secret::Map,
        tls_materials_provider: Provider,
        subscriber: Subscriber,
        builder: server::Builder<Event>,
    ) -> Result<Self, Error> {
        let io = s2n_quic::provider::io::default::Builder::default()
            .with_receive_address(addr)?
            .with_base_mtu(DEFAULT_BASE_MTU.min(builder.mtu))?
            .with_initial_mtu(builder.mtu)?
            .with_max_mtu(builder.mtu)?
            .with_internal_recv_buffer_size(BUFFER_SIZE)?
            .build()?;

        let server = s2n_quic::Server::builder().with_io(io)?;

        let connection_limits = s2n_quic::provider::limits::Limits::new()
            .with_max_idle_timeout(builder.max_idle_timeout)?
            .with_data_window(builder.data_window)?
            .with_bidirectional_local_data_window(builder.data_window)?
            .with_bidirectional_remote_data_window(builder.recv_window)?
            .with_initial_round_trip_time(DEFAULT_INITIAL_RTT)?;

        let event = (
            (ConfirmComplete, (MtuConfirmComplete, DcPathCapture)),
            subscriber,
        );

        cfg_if!(
            if #[cfg(any(test, feature = "testing"))] {
                let server = {
                    let server = server
                        .with_limits(connection_limits)?
                        .with_dc(map.clone())?
                        .with_event(event)?
                        .with_tls(tls_materials_provider)?;
                    if let Some(limiter) = builder.endpoint_limits {
                        server.with_endpoint_limits(limiter)?.start()?
                    } else {
                        server.start()?
                    }
                };
            } else {
                let server = server
                    .with_limits(connection_limits)?
                    .with_dc(map.clone())?
                    .with_event(event)?
                    .with_tls(tls_materials_provider)?
                    .start()?;
            }
        );

        Ok(Self { server })
    }

    #[allow(dead_code)]
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.server.local_addr()
    }
}

pub(super) async fn server<
    Provider: Prov + Send + Sync + 'static,
    Subscriber: Sub + Send + Sync + 'static,
    Event: s2n_quic::provider::event::Subscriber,
>(
    address: SocketAddr,
    map: secret::Map,
    builder: server::Builder<Event>,
    tls_materials_provider: Provider,
    subscriber: Subscriber,
    on_ready: tokio::sync::oneshot::Sender<Result<SocketAddr, Error>>,
) {
    let mut server = match Server::bind::<Provider, Subscriber, Event>(
        address,
        map.clone(),
        tls_materials_provider,
        subscriber,
        builder,
    ) {
        Ok(s) => s,
        Err(e) => {
            error!("failed to bind server to {:?}: {:?}", address, e);
            let _ = on_ready.send(Err(e));
            return;
        }
    };

    let _ = on_ready.send(Ok(server.local_addr().unwrap()));

    while let Some(mut connection) = server.server.accept().await {
        let map_clone = map.clone();
        tokio::spawn(async move {
            // The accepted connection must remain open until the client has finished inserting
            // the entry into its map. The client indicates this by sending a ConnectionClose
            // when it is done.
            //
            // A 10 second timeout is specified to avoid spawned tasks piling up when the
            // ConnectionClose from the client is lost. This timeout covers dc handshake
            // confirmation and MTU probing completion. Data addresses are exchanged via
            // transport parameters during the TLS handshake.
            let deadline = TokioInstant::now() + Duration::from_secs(10);

            match tokio::time::timeout_at(deadline, ConfirmComplete::wait_ready(&mut connection))
                .await
            {
                Ok(Ok(())) => {
                    let _ = tokio::time::timeout_at(
                        deadline,
                        MtuConfirmComplete::wait_ready(&mut connection),
                    )
                    .await;
                }
                Ok(Err(e)) => {
                    debug!(
                        "failed to establish DC connection with {:?}: {:?}",
                        address, e
                    );
                }
                Err(_) => {
                    if let Ok(peer_address) = connection.remote_addr() {
                        map_clone.on_dc_connection_timeout(&peer_address);
                    }
                }
            }
        });
    }
}

#[derive(Clone)]
pub struct Client {
    client: s2n_quic::Client,
    map: secret::Map,
    queue: Arc<HandshakeQueue>,
}

impl Client {
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.client.local_addr()
    }

    pub fn bind<
        Provider: Prov + Send + Sync + 'static,
        Subscriber: Sub + Send + Sync + 'static,
        Event: s2n_quic::provider::event::Subscriber,
    >(
        addr: SocketAddr,
        map: secret::Map,
        tls_materials_provider: Provider,
        subscriber: Subscriber,
        builder: client::Builder<Event>,
    ) -> Result<Self, Error> {
        let io = s2n_quic::provider::io::default::Builder::default()
            .with_receive_address(addr)?
            .with_base_mtu(DEFAULT_BASE_MTU.min(builder.mtu))?
            .with_initial_mtu(builder.mtu)?
            .with_max_mtu(builder.mtu)?
            .with_internal_recv_buffer_size(BUFFER_SIZE)?
            .build()?;

        let client = s2n_quic::Client::builder().with_io(io)?;

        let connection_limits = s2n_quic::provider::limits::Limits::new()
            .with_max_idle_timeout(builder.max_idle_timeout)?
            .with_data_window(builder.data_window)?
            .with_bidirectional_local_data_window(builder.data_window)?
            .with_bidirectional_remote_data_window(builder.recv_window)?
            .with_initial_round_trip_time(DEFAULT_INITIAL_RTT)?;

        let event = (
            (
                ConfirmComplete,
                (MtuConfirmComplete, (DcPathCapture, PathRtt)),
            ),
            (HandshakeWaits, subscriber),
        );

        let client = client
            .with_limits(connection_limits)?
            .with_dc(map.clone())?
            .with_event(event)?
            .with_tls(tls_materials_provider)?
            .start()?;

        Ok(Self {
            client,
            map: map.clone(),
            queue: Arc::new(HandshakeQueue::new(builder.success_jitter)),
        })
    }

    pub(super) async fn connect(
        &self,
        peer: SocketAddr,
        reason: HandshakeReason,
        server_name: Name,
    ) -> Result<(), HandshakeFailed> {
        self.queue
            .clone()
            .handshake(&self.client, &self.map, peer, reason, server_name)
            .await
    }

    /// Evicts `stale`, a cached entry for `peer` that the caller found dead and silent, and lets
    /// the next handshake with `peer` start at once where its last one succeeded, unless the map
    /// already holds a newer entry for `peer`, as `HandshakeQueue::remove_completed` says.
    pub(super) fn evict_and_forget_completed(
        &self,
        peer: SocketAddr,
        stale: &Arc<secret::map::Entry>,
    ) {
        let map = &self.map;
        map.evict_entry(stale);
        let has_newer_entry = || map.get_raw(peer).is_some();
        self.queue.remove_completed(peer, has_newer_entry);
    }
}

#[cfg(test)]
impl Client {
    /// Returns true if there's a pending handshake entry for this peer.
    /// This is only available in test builds.
    pub fn has_pending_entry(&self, peer: SocketAddr) -> bool {
        let peer: SocketAddress = peer.into();
        let peer_hash = self.queue.hasher.hash_one(peer);
        let guard = self.queue.inner.lock().unwrap();
        guard.table.find(peer_hash, |e| e.peer == peer).is_some()
    }
}

struct Entry {
    peer: SocketAddress,
    handshaker: tokio::sync::OnceCell<Result<(), HandshakeFailed>>,
    by_reason: [AtomicU16; REASON_COUNT],
}

#[derive(Default)]
struct HandshakeQueueInner {
    table: hashbrown::HashTable<Arc<Entry>>,
}
struct HandshakeQueue {
    inner: Mutex<HandshakeQueueInner>,
    limiter_start: limit::Limit,
    limiter_inflight: limit::Limit,
    success_jitter: Duration,
    hasher: std::collections::hash_map::RandomState,
}

impl HandshakeQueue {
    fn new(success_jitter: Duration) -> Self {
        HandshakeQueue {
            // The "start" limit bounds TLS handshake concurrency.
            //
            // TLS handshakes have high CPU cost (~1ms) which stalls out the endpoint, so too many
            // of them at once raise our baseline latency ~linearly. The limit reads each
            // handshake's time from its connect to dc.complete past the two round trips its path
            // accounts for, and halves where that time is more than twice the base, or where
            // handshakes with two peers timed out or were refused since the last clean answer.
            limiter_start: limit::Limit::new(),
            // The inflight limit bounds the total number of connections we have open. Keeping
            // that bounded helps avoid unbounded work ongoing in s2n-quic (which implies unbounded
            // packet transmit/receive work). The limit reads whether each connection finished its
            // MTU probing before its deadline, and halves where connections to two peers missed
            // it, or their handshakes timed out or were refused, since the last clean answer.
            limiter_inflight: limit::Limit::new(),
            success_jitter,
            inner: Default::default(),
            hasher: Default::default(),
        }
    }

    /// Allocate an entry that will let us wait for the handshake to complete.
    /// This entry also stores the result of the handshake (success or failure).
    fn allocate_entry(&self, peer: SocketAddr, reason: HandshakeReason) -> Arc<Entry> {
        let peer: SocketAddress = peer.into();
        // FIXME: Maybe limit the size of the map?
        // It's not clear what we'd do if we exceeded the limit -- at least today, we only track
        // actively pending handshakes, so that implies dropping handshake requests entirely. But
        // it's not clear that has any real value, we're near guaranteed to want to handshake with
        // them eventually.
        let peer_hash = self.hasher.hash_one(peer);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *guard;
        let entry = match inner.table.entry(
            peer_hash,
            |e| e.peer == peer,
            |e| self.hasher.hash_one(e.peer),
        ) {
            hashbrown::hash_table::Entry::Occupied(o) => o.get().clone(),
            hashbrown::hash_table::Entry::Vacant(v) => v
                .insert(Arc::new(Entry {
                    peer,
                    handshaker: tokio::sync::OnceCell::new(),
                    by_reason: [const { AtomicU16::new(0) }; REASON_COUNT],
                }))
                .get()
                .clone(),
        };
        entry.by_reason[reason as usize]
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_add(1))
            })
            .expect("Some means always OK");
        entry
    }

    /// Remove a specific entry from the map. This will *not* remove any newly inserted entry (even
    /// if for the same peer address).
    fn remove_entry(&self, entry: &Arc<Entry>) {
        let peer_hash = self.hasher.hash_one(entry.peer);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *guard;
        match inner.table.find_entry(peer_hash, |e| e.peer == entry.peer) {
            Ok(o) => {
                if Arc::ptr_eq(o.get(), entry) {
                    o.remove();
                }
            }
            Err(_) => {
                // no further action to take
            }
        }
    }

    /// Removes the peer's de-duplication entry where its handshake already finished with a
    /// success, so the next handshake with the peer starts at once instead of returning that
    /// success. An entry whose handshake is still running, or that holds a failure, stays: the
    /// peer still gets one handshake at a time, and a failure keeps its backoff.
    ///
    /// A success also stays where `has_newer_entry` returns true, so a caller that finds the map
    /// holding a newer entry for the peer, one a concurrent handshake inserted, doesn't drop that
    /// handshake's hold. It's read after the handshake's result, under this queue's lock: a
    /// handshake inserts its entry into the map before it records its success here, so a success
    /// read here already has its entry in the map.
    fn remove_completed(&self, peer: SocketAddr, has_newer_entry: impl FnOnce() -> bool) {
        let peer: SocketAddress = peer.into();
        let peer_hash = self.hasher.hash_one(peer);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *guard;
        match inner.table.find_entry(peer_hash, |e| e.peer == peer) {
            Ok(o) => {
                if matches!(o.get().handshaker.get(), Some(Ok(()))) && !has_newer_entry() {
                    o.remove();
                }
            }
            Err(_) => {
                // no further action to take
            }
        }
    }

    /// Handshake with a peer while rate limiting and de-duplicating handshakes.
    ///
    /// This ensures that the handshakes running at once are bounded by limits that the
    /// handshakes' answers set: they grow while handshakes finish in time, and halve where
    /// handshakes slow down or handshakes with two peers time out or are refused (avoiding
    /// unbounded latencies *within* the handshake itself, which causes timeouts and can cause
    /// congestive collapse under enough load).
    async fn handshake(
        self: Arc<Self>,
        client: &s2n_quic::Client,
        map: &secret::Map,
        peer: SocketAddr,
        reason: HandshakeReason,
        server_name: Name,
    ) -> Result<(), HandshakeFailed> {
        let entry = self.allocate_entry(peer, reason);
        let entry2 = entry.clone();
        let entry3 = entry.clone();

        let handshake = async {
            // We've de-duplicated above already so the handshaker is unique per SocketAddr, so
            // this permit will only be used for the current handshake.
            let start = std::time::Instant::now();
            let permit_inflight = self.limiter_inflight.acquire().await;
            let permit_start = self.limiter_start.acquire().await;
            let limiter_duration = start.elapsed();

            // The lowest round-trip time that the earlier handshakes with this peer measured,
            // read from the peer's map entry before this handshake's entry takes its place.
            let earlier_rtt = map.get_raw(peer).and_then(|entry| entry.path_rtt());

            let mut attempt =
                client.connect(s2n_quic::client::Connect::new(peer).with_server_name(server_name));

            // Note that this provides counts at the time of starting the connection attempt.
            // Technically, this omits counts that happen after this point while the deduplication
            // is still active.
            let mut reason_counts = [
                (HandshakeReason::User, 0),
                (HandshakeReason::Periodic, 0),
                (HandshakeReason::Remote, 0),
            ];
            for (reason, count) in reason_counts.iter_mut() {
                *count = entry.by_reason[*reason as usize].load(Ordering::Relaxed) as usize;
            }

            attempt.set_application_context(Box::new(ConnectionContext {
                limiter_latency: limiter_duration,
                reason_counts,
            }));

            let handshake_start = std::time::Instant::now();
            let handshake_result = async {
                let mut connection = attempt.await?;

                // A 10 second deadline is used to bound ConfirmComplete and
                // MtuConfirmComplete, avoiding unbounded waits if the peer is slow.
                // Data addresses are exchanged via transport parameters during the
                // TLS handshake.
                let deadline = TokioInstant::now() + Duration::from_secs(10);

                tokio::time::timeout_at(deadline, ConfirmComplete::wait_ready(&mut connection))
                    .await
                    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake timeout"))??;

                Ok::<_, io::Error>((connection, deadline))
            }
            .await;
            let (mut connection, deadline) = match handshake_result {
                Ok(connected) => connected,
                Err(error) => {
                    // A handshake that timed out or that the server refused is a failure of this
                    // peer, which halves both limits where another peer's handshake failed too
                    // since the last clean answer. Any other failure doesn't change them.
                    let answer = limit::Answer::for_error(&error, peer);
                    permit_start.release(answer);
                    permit_inflight.release(answer);
                    return Err(error);
                }
            };

            // Don't wait for the connection to fully close, just wait until dc.complete to
            // release the permit, with the handshake's time past what its path accounts for, from
            // the lowest round-trip time of this connection and the peer's earlier ones, as its
            // answer.
            let answer = start_answer(&connection, earlier_rtt, handshake_start.elapsed());
            permit_start.release(answer);

            // Spawn a task to leave the connection open for MTU probing to complete.
            // The 1-second wait for peers that don't support MtuProbingComplete
            // is handled inside wait_ready() when the connection closes gracefully.
            //
            // This task also owns pruning our de-duplication tracking.
            let this = self.clone();
            let map_clone = map.clone();
            tokio::spawn(async move {
                // Use the same deadline for MTU probing - any remaining time from the 10s budget
                let probed = tokio::time::timeout_at(
                    deadline,
                    MtuConfirmComplete::wait_ready(&mut connection),
                )
                .await;
                let answer = if probed.is_err() {
                    map_clone.on_dc_connection_timeout(&peer);
                    // A connection that didn't finish its MTU probing before the deadline is a
                    // failure of this peer for the inflight limit.
                    limit::Answer::Failed(peer)
                } else {
                    limit::Answer::Finished
                };

                drop(connection);
                permit_inflight.release(answer);

                // Delay deleting the entry by a random time, up to 1 minute.
                //
                // The specific duration is not chosen with any particular rationale, mostly
                // intended to be a relatively small amount while still significantly reducing
                // handshake volume if we're repeatedly handshaking in a short period of time
                // (e.g., due to replay protection packets repeatedly arriving). It's unlikely that
                // handshaking more than roughly once per minute with a given peer actually
                // produces meaningfully better results than allowing a more normal rate of
                // handshakes.
                //
                // Note that we've already dropped the connection and permit above, so we're not
                // blocking any other peer from handshaking.
                let duration = {
                    let mut rng = rand::rng();
                    rng.random_range(0..=(this.success_jitter.as_millis() as u64))
                };
                tokio::time::sleep(Duration::from_millis(duration)).await;

                this.remove_entry(&entry);
            });

            Ok::<_, io::Error>(())
        };

        entry2
            .handshaker
            .get_or_init(|| async {
                // This ensures we only log the error once, even if the handshake was de-duplicated
                // many times.
                if let Err(e) = handshake.await {
                    // We may want to remove this in favor of only relying on the service log
                    // eventually, but keeping it for parity for now.
                    error!("handshake with {peer} failed: {e}");

                    let this = self.clone();
                    tokio::spawn(async move {
                        // Delay deleting the entry by a random time, up to 2 minutes.
                        //
                        // This avoids aggressively reconnecting to a given peer if handshakes
                        // fail (instead we keep returning the cached error). This is good both for
                        // fast failure (e.g., certificate issues) and for slow errors (timeouts).
                        // In the first case, it's very unlikely the issue will be fixed within
                        // seconds, so backing off is natural to keep aggregate handshake volume
                        // more bounded. For the latter, backing off avoids generating undue load
                        // on the network or server. The specific duration is not chosen
                        // with any particular rationale, mostly intended to be a relatively small
                        // amount (to avoid significantly extending recovery times if the server
                        // was temporarily overloaded) while still significantly reducing handshake
                        // volume (>60x for fast-failing handshakes and >10x for timeouts).
                        let duration = {
                            let mut rng = rand::rng();
                            rng.random_range(1000..120_000)
                        };
                        tokio::time::sleep(Duration::from_millis(duration)).await;

                        // If the handshake fails, we also remove the entry from the map.
                        // This permits another handshake to start for the same peer.
                        this.remove_entry(&entry3);
                    });

                    Err(HandshakeFailed(e))
                } else {
                    Ok(())
                }
            })
            .await
            .as_ref()
            .map(|v| *v)
            .map_err(|e| e.duplicate())
    }
}

// This is only created if we've already logged a handshake error.
#[derive(Debug)]
pub struct HandshakeFailed(io::Error);

impl HandshakeFailed {
    fn duplicate(&self) -> Self {
        // Manually create a similar io::Error while preserving information about the inner error if present.
        if let Some(inner) = self.0.get_ref() {
            Self(io::Error::new(self.0.kind(), inner.to_string()))
        } else {
            Self(io::Error::from(self.0.kind()))
        }
    }
}

impl From<HandshakeFailed> for io::Error {
    fn from(e: HandshakeFailed) -> io::Error {
        e.0
    }
}

#[derive(Debug, Copy, Clone)]
pub enum HandshakeReason {
    /// An explicit request by the application owner
    User,
    /// Periodic re-handshaking
    Periodic,
    /// Rehandshaking driven by the remote peer: a packet it sends (e.g., unknown path secret), or
    /// its silence on a path the send idle wheel marks dead.
    Remote,
}

const REASON_COUNT: usize = 3;

pub struct ConnectionContext {
    pub limiter_latency: Duration,
    pub reason_counts: [(HandshakeReason, usize); REASON_COUNT],
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        path::secret::{stateless_reset::Signer, Map},
        testing::{init_tracing, NoopSubscriber, TestTlsProvider},
    };
    use s2n_quic::provider::{
        endpoint_limits::{ConnectionAttempt, Limiter, Outcome},
        tls::Provider,
    };
    use s2n_quic_core::{
        event::{builder, ConnectionPublisher as _, ConnectionPublisherSubscriber},
        time::StdClock,
    };
    use std::time::Instant;
    use tokio_util::sync::DropGuard;

    /// A test limiter that closes all incoming connections immediately
    #[derive(Default)]
    struct CloseAllConnectionsLimiter;

    impl Limiter for CloseAllConnectionsLimiter {
        fn on_connection_attempt(&mut self, _info: &ConnectionAttempt) -> Outcome {
            Outcome::close()
        }
    }

    /// Helper to set up a test client and server
    struct TestSetup {
        client: Client,
        server_addr: SocketAddr,
        _server_guard: DropGuard,
    }

    impl TestSetup {
        /// Creates a test setup with an optional endpoint limiter for the server
        async fn new<L>(endpoint_limits: Option<L>) -> Self
        where
            L: s2n_quic::provider::endpoint_limits::Limiter + Send + Sync + 'static,
        {
            init_tracing();

            let tls = TestTlsProvider {};
            let subscriber = NoopSubscriber {};

            let server_map = Map::builder()
                .with_signer(Signer::new(b"default"))
                .with_capacity(50_000)
                .with_clock(StdClock::default())
                .with_subscriber(subscriber.clone())
                .build()
                .unwrap();

            let server_builder = crate::psk::server::Builder::default();
            let (server_addr_rx, server_guard) = if let Some(limiter) = endpoint_limits {
                crate::psk::server::Provider::setup(
                    "127.0.0.1:0".parse().unwrap(),
                    server_map.clone(),
                    tls.clone(),
                    subscriber.clone(),
                    server_builder.with_endpoint_limits(limiter),
                )
            } else {
                crate::psk::server::Provider::setup(
                    "127.0.0.1:0".parse().unwrap(),
                    server_map.clone(),
                    tls.clone(),
                    subscriber.clone(),
                    server_builder,
                )
            };

            let client_map = Map::builder()
                .with_signer(Signer::new(b"default"))
                .with_capacity(50_000)
                .with_clock(StdClock::default())
                .with_subscriber(subscriber.clone())
                .build()
                .unwrap();

            let client = Client::bind::<
                <TestTlsProvider as Provider>::Client,
                NoopSubscriber,
                s2n_quic::provider::event::default::Subscriber,
            >(
                "0.0.0.0:0".parse().unwrap(),
                client_map,
                tls.start_client().unwrap(),
                subscriber,
                crate::psk::client::Builder::default().with_success_jitter(Duration::ZERO),
            )
            .unwrap();

            let server_addr = server_addr_rx.await.unwrap().unwrap();

            Self {
                client,
                server_addr,
                _server_guard: server_guard,
            }
        }
    }

    /// Verifies MtuProbingComplete works correctly (no 1-second fallback delay).
    ///
    /// After a handshake, a cleanup task runs in the background. If MtuProbingComplete
    /// is NOT working, this task sleeps for 1 second before removing the deduplication entry.
    ///
    /// We detect this by waiting 500ms then checking if the entry was removed:
    /// - If entry was removed (MtuProbingComplete works): cleanup completed quickly
    /// - If entry still exists (1-second delay active): cleanup is still sleeping
    #[tokio::test]
    async fn mtu_probing_complete_no_delay_test() {
        let setup = TestSetup::new::<CloseAllConnectionsLimiter>(None).await;
        let server_name: s2n_quic::server::Name = "localhost".into();

        // First handshake
        setup
            .client
            .connect(
                setup.server_addr,
                HandshakeReason::User,
                server_name.clone(),
            )
            .await
            .unwrap();

        // Wait 500ms - enough for cleanup if MtuProbingComplete works, but not if 1s delay triggered
        tokio::time::sleep(Duration::from_millis(500)).await;

        // If entry still exists after 500ms, the cleanup task hasn't finished yet,
        // which indicates the 1-second fallback delay is active.
        assert!(!setup.client.has_pending_entry(setup.server_addr));

        // Second handshake to same peer - should succeed since entry was removed
        let second_handshake_start = Instant::now();
        setup
            .client
            .connect(
                setup.server_addr,
                HandshakeReason::User,
                server_name.clone(),
            )
            .await
            .unwrap();
        let second_handshake_duration = second_handshake_start.elapsed();

        // Additional timing check: if entry was properly removed, the second handshake
        // should take at least 1ms (a fresh handshake). If it's <1ms, it was deduplicated.
        assert!(second_handshake_duration >= Duration::from_millis(1));
    }

    /// Verifies that when the server closes a connection immediately (via endpoint limits),
    /// the client connection closes without waiting.
    ///
    /// This test ensures that `MtuConfirmComplete::wait_ready` properly detects the
    /// connection close signal and returns immediately rather than blocking.
    #[tokio::test]
    async fn server_close_connection_no_delay_test() {
        let setup = TestSetup::new(Some(CloseAllConnectionsLimiter)).await;
        let server_name: s2n_quic::server::Name = "localhost".into();

        // Attempt to connect - the server should immediately close the connection
        let start = Instant::now();
        let result = setup
            .client
            .connect(setup.server_addr, HandshakeReason::User, server_name)
            .await;
        let duration = start.elapsed();

        // The connection should fail (server rejected it)
        assert!(result.is_err());

        // The failure should be fast - definitely less than the 10-second timeout
        // and less than the 1-second fallback delay
        assert!(
            duration < Duration::from_millis(500),
            "Connection took {:?}, expected < 500ms",
            duration
        );
    }

    /// Verifies that the handshake queue's limits start at one and that the handshakes' answers
    /// size them: a clean handshake that filled a limit grows it, one peer's refusal leaves the
    /// limits as they are, and a second peer's refusal since the last clean answer halves them.
    #[tokio::test]
    async fn handshake_answers_size_the_limits() {
        let setup = TestSetup::new::<CloseAllConnectionsLimiter>(None).await;
        let refusing_a = TestSetup::new(Some(CloseAllConnectionsLimiter)).await;
        let refusing_b = TestSetup::new(Some(CloseAllConnectionsLimiter)).await;
        let queue = &setup.client.queue;
        let server_name: s2n_quic::server::Name = "localhost".into();
        assert_eq!(queue.limiter_start.current(), 1);
        assert_eq!(queue.limiter_inflight.current(), 1);

        // The handshake filled the start limit of one and finished cleanly, so the limit grows.
        setup
            .client
            .connect(
                setup.server_addr,
                HandshakeReason::User,
                server_name.clone(),
            )
            .await
            .unwrap();
        assert_eq!(queue.limiter_start.current(), 2);

        // The first handshake's inflight permit comes back once its connection finished MTU
        // probing, which grows the inflight limit and lets this handshake start. The server
        // refuses it, and one peer's refusal doesn't change either limit.
        let error = setup
            .client
            .connect(
                refusing_a.server_addr,
                HandshakeReason::User,
                server_name.clone(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.0.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(queue.limiter_start.current(), 2);
        assert_eq!(queue.limiter_inflight.current(), 2);

        // A second peer's refusal since the last clean answer halves both limits.
        setup
            .client
            .connect(refusing_b.server_addr, HandshakeReason::User, server_name)
            .await
            .unwrap_err();
        assert_eq!(queue.limiter_start.current(), 1);
        assert_eq!(queue.limiter_inflight.current(), 1);
    }

    /// Verifies that a client's connection has measured its path's lowest round-trip time by
    /// dc.complete, that the start limit's answer for its handshake takes two round trips of the
    /// lower of that time and an earlier connection's out of the handshake's time, and that the
    /// connection's map entry keeps the lower time.
    #[tokio::test]
    async fn the_start_answer_takes_two_measured_round_trips_out_of_the_time() {
        let setup = TestSetup::new::<CloseAllConnectionsLimiter>(None).await;
        let server_name: s2n_quic::server::Name = "localhost".into();
        let connect =
            s2n_quic::client::Connect::new(setup.server_addr).with_server_name(server_name);
        let mut connection = setup.client.client.connect(connect).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            ConfirmComplete::wait_ready(&mut connection),
        )
        .await
        .unwrap()
        .unwrap();

        let min_rtt = || {
            connection
                .query_event_context(|context: &PathRttContext| context.min_rtt)
                .unwrap()
                .expect("the connection measured a round-trip time by dc.complete")
        };
        // Once measured, the lowest round-trip time only goes down unless the connection meets
        // persistent congestion, so the round trip the answer used lies between the ones read
        // before and after it.
        let before = min_rtt();
        let elapsed = Duration::from_secs(1);
        let answer = start_answer(&connection, None, elapsed);
        let after = min_rtt();
        assert!(after > Duration::ZERO);
        // On the loopback path, the handshake didn't take a Retry or a HelloRetryRequest, or lose
        // a packet, and the client counted what it waited.
        let waits = connection
            .query_event_context(|context: &HandshakeWaitsContext| context.waits)
            .expect("the client counts what each handshake waited");
        assert_eq!(waits, limit::Waits::default());
        match answer {
            limit::Answer::Latency(time) => {
                assert!(time >= elapsed - before * 2, "{time:?}, {before:?}");
                assert!(time <= elapsed - after * 2, "{time:?}, {after:?}");
            }
            other => panic!("expected a latency answer, got {other:?}"),
        }

        // The connection's map entry, which the map holds for the peer, keeps the round trip
        // the answer used.
        let entry = connection_entry(&connection).expect("its map entry");
        let peer_entry = setup.client.map.get_raw(setup.server_addr).unwrap();
        assert!(Arc::ptr_eq(&entry, &peer_entry));
        let kept = entry.path_rtt().expect("a kept round trip");
        assert!(
            kept <= before && kept >= after,
            "{kept:?}, {before:?}, {after:?}"
        );

        // An earlier connection's lower round trip counts instead of this connection's, and the
        // entry keeps it.
        let earlier = Duration::from_nanos(1);
        assert_eq!(
            start_answer(&connection, Some(earlier), elapsed),
            limit::Answer::Latency(elapsed - earlier * 2)
        );
        assert_eq!(entry.path_rtt(), Some(earlier));
    }

    /// Verifies that a handshake through the queue keeps its connection's round-trip time with
    /// the peer's map entry, and that the next handshake with the peer reads that time and keeps
    /// the lower of it and its own with the entry it makes.
    #[tokio::test]
    async fn a_handshake_carries_the_peers_lowest_round_trip_time_to_its_next_entry() {
        let setup = TestSetup::new::<CloseAllConnectionsLimiter>(None).await;
        let server_name: s2n_quic::server::Name = "localhost".into();
        let queue = &setup.client.queue;

        setup
            .client
            .connect(
                setup.server_addr,
                HandshakeReason::User,
                server_name.clone(),
            )
            .await
            .unwrap();
        let first = setup.client.map.get_raw(setup.server_addr).unwrap();
        let first_rtt = first.path_rtt().expect("a kept round trip");
        assert!(first_rtt > Duration::ZERO);

        // A time lower than any connection measures stands for an earlier connection's, which
        // the next handshake reads and keeps with its entry instead of its connection's time.
        let earlier = Duration::from_nanos(1);
        first.lower_path_rtt(earlier);
        queue.remove_completed(setup.server_addr, || false);
        setup
            .client
            .connect(setup.server_addr, HandshakeReason::User, server_name)
            .await
            .unwrap();
        let second = setup.client.map.get_raw(setup.server_addr).unwrap();
        assert!(!Arc::ptr_eq(&first, &second), "a new entry");
        assert_eq!(second.path_rtt(), Some(earlier));
    }

    #[test]
    fn alloc_entry_increments() {
        let queue = HandshakeQueue::new(Duration::ZERO);
        let peer_a = "127.0.0.1:3333".parse().unwrap();
        assert_eq!(
            queue
                .allocate_entry(peer_a, HandshakeReason::User)
                .by_reason[HandshakeReason::User as usize]
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            queue
                .allocate_entry(peer_a, HandshakeReason::User)
                .by_reason[HandshakeReason::User as usize]
                .load(Ordering::Relaxed),
            2
        );
        assert_eq!(
            queue
                .allocate_entry(peer_a, HandshakeReason::Periodic)
                .by_reason[HandshakeReason::Periodic as usize]
                .load(Ordering::Relaxed),
            1
        );
    }

    /// A successful handshake's de-duplication entry is removed, so the next handshake with the
    /// peer starts at once; a handshake still running, or a failed one, stays held, and so does
    /// a success where the map holds a newer entry for the peer.
    #[test]
    fn remove_completed_keeps_running_and_failed_handshakes() {
        let queue = HandshakeQueue::new(Duration::from_secs(60));
        let peer: SocketAddr = "127.0.0.1:3333".parse().unwrap();

        let running = queue.allocate_entry(peer, HandshakeReason::User);
        queue.remove_completed(peer, || false);
        assert!(
            Arc::ptr_eq(&running, &queue.allocate_entry(peer, HandshakeReason::User)),
            "a running handshake stays held"
        );

        let refused = HandshakeFailed(io::Error::from(io::ErrorKind::ConnectionRefused));
        running.handshaker.set(Err(refused)).unwrap();
        queue.remove_completed(peer, || false);
        assert!(
            Arc::ptr_eq(&running, &queue.allocate_entry(peer, HandshakeReason::User)),
            "a failed handshake stays held"
        );

        queue.remove_entry(&running);
        let succeeded = queue.allocate_entry(peer, HandshakeReason::User);
        succeeded.handshaker.set(Ok(())).unwrap();
        queue.remove_completed(peer, || true);
        assert!(
            Arc::ptr_eq(
                &succeeded,
                &queue.allocate_entry(peer, HandshakeReason::User)
            ),
            "a success stays held where the map holds a newer entry for the peer"
        );
        queue.remove_completed(peer, || false);
        assert!(
            !Arc::ptr_eq(
                &succeeded,
                &queue.allocate_entry(peer, HandshakeReason::User)
            ),
            "a successful handshake is no longer held"
        );
    }

    /// Publishes events to a client connection's count of what its handshake waited, and
    /// returns what it counted.
    fn waits_after(
        publish: impl FnOnce(&mut ConnectionPublisherSubscriber<'_, HandshakeWaits>),
    ) -> limit::Waits {
        let mut subscriber = HandshakeWaits;
        let mut context = HandshakeWaitsContext {
            waits: limit::Waits::default(),
        };
        let meta = builder::ConnectionMeta {
            endpoint_type: s2n_quic_core::endpoint::Type::Client,
            id: 0,
            timestamp: s2n_quic_core::time::clock::testing::now(),
        };
        {
            let mut publisher =
                ConnectionPublisherSubscriber::new(meta, 1, &mut subscriber, &mut context);
            publish(&mut publisher);
        }
        context.waits
    }

    /// A path for the events that carry one.
    fn path() -> builder::Path<'static> {
        builder::Path {
            local_addr: builder::SocketAddress::IpV4 {
                ip: &[127, 0, 0, 1],
                port: 1,
            },
            local_cid: builder::ConnectionId { bytes: &[1] },
            remote_addr: builder::SocketAddress::IpV4 {
                ip: &[127, 0, 0, 1],
                port: 2,
            },
            remote_cid: builder::ConnectionId { bytes: &[2] },
            id: 0,
            is_active: true,
        }
    }

    /// Verifies that a client's connection counts a Retry it takes as a round trip, and not a
    /// packet of another kind.
    #[test]
    fn a_retry_the_client_takes_adds_a_round_trip() {
        let waits = waits_after(|publisher| {
            publisher.on_packet_received(builder::PacketReceived {
                packet_header: builder::PacketHeader::Initial {
                    number: 0,
                    version: 1,
                },
            });
            publisher.on_packet_received(builder::PacketReceived {
                packet_header: builder::PacketHeader::Retry { version: 1 },
            });
        });
        assert_eq!((waits.round_trips, waits.lost), (1, false));
    }

    /// Verifies that a Retry the client receives and then discards doesn't count.
    #[test]
    fn a_retry_the_client_discards_doesnt_add_a_round_trip() {
        let waits = waits_after(|publisher| {
            publisher.on_packet_received(builder::PacketReceived {
                packet_header: builder::PacketHeader::Retry { version: 1 },
            });
            publisher.on_packet_dropped(builder::PacketDropped {
                reason: builder::PacketDropReason::RetryDiscarded {
                    reason: builder::RetryDiscardReason::InvalidIntegrityTag,
                    path: path(),
                },
            });
        });
        assert_eq!((waits.round_trips, waits.lost), (0, false));
    }

    /// Verifies that a HelloRetryRequest counts as a round trip, and that a ServerHello with
    /// another random, or a body too short to hold a random, doesn't.
    #[test]
    fn a_hello_retry_request_adds_a_round_trip() {
        // A ServerHello's body starts with its 2-byte legacy_version and its 32-byte random. The
        // event holds the body in parts.
        let mut hello_retry_request = vec![0x03, 0x03];
        hello_retry_request.extend_from_slice(&HELLO_RETRY_REQUEST_RANDOM);
        hello_retry_request.push(0);
        let (head, tail) = hello_retry_request.split_at(10);
        let payload: &[&[u8]] = &[head, tail];
        let waits = waits_after(|publisher| {
            publisher.on_tls_server_hello(builder::TlsServerHello { payload });
        });
        assert_eq!((waits.round_trips, waits.lost), (1, false));

        let mut server_hello = hello_retry_request.clone();
        server_hello[33] ^= 1;
        for body in [&server_hello[..], &hello_retry_request[..33]] {
            let payload: &[&[u8]] = &[body];
            let waits = waits_after(|publisher| {
                publisher.on_tls_server_hello(builder::TlsServerHello { payload });
            });
            assert_eq!((waits.round_trips, waits.lost), (0, false));
        }
    }

    /// Verifies that a lost packet marks the handshake, and that a lost MTU probe doesn't.
    #[test]
    fn a_lost_packet_other_than_an_mtu_probe_marks_the_handshake() {
        let lost = |is_mtu_probe| builder::PacketLost {
            packet_header: builder::PacketHeader::OneRtt { number: 0 },
            path: path(),
            bytes_lost: 1200,
            is_mtu_probe,
        };
        let waits = waits_after(|publisher| publisher.on_packet_lost(lost(true)));
        assert_eq!((waits.round_trips, waits.lost), (0, false));
        let waits = waits_after(|publisher| publisher.on_packet_lost(lost(false)));
        assert_eq!((waits.round_trips, waits.lost), (0, true));
    }

    /// A test limiter that asks each client for a Retry before its connection: the server takes
    /// the client's next Initial, which carries the Retry's token, without asking the limiter.
    #[derive(Default)]
    struct RetryLimiter;

    impl Limiter for RetryLimiter {
        fn on_connection_attempt(&mut self, _info: &ConnectionAttempt) -> Outcome {
            Outcome::retry()
        }
    }

    /// Verifies that a client's connection counts the Retry the server sent it, and that the
    /// start limit's answer for its handshake takes three measured round trips out of the
    /// handshake's time.
    #[tokio::test]
    async fn the_start_answer_takes_a_retrys_round_trip_out_of_the_time() {
        let setup = TestSetup::new(Some(RetryLimiter)).await;
        let server_name: s2n_quic::server::Name = "localhost".into();
        let connect =
            s2n_quic::client::Connect::new(setup.server_addr).with_server_name(server_name);
        let mut connection = setup.client.client.connect(connect).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            ConfirmComplete::wait_ready(&mut connection),
        )
        .await
        .unwrap()
        .unwrap();

        let waits = connection
            .query_event_context(|context: &HandshakeWaitsContext| context.waits)
            .expect("the client counts what each handshake waited");
        assert_eq!((waits.round_trips, waits.lost), (1, false));

        let min_rtt = || {
            connection
                .query_event_context(|context: &PathRttContext| context.min_rtt)
                .unwrap()
                .expect("the connection measured a round-trip time by dc.complete")
        };
        // The round trip the answer used lies between the ones read before and after it.
        let before = min_rtt();
        let elapsed = Duration::from_secs(1);
        let answer = start_answer(&connection, None, elapsed);
        let after = min_rtt();
        match answer {
            limit::Answer::Latency(time) => {
                assert!(time >= elapsed - before * 3, "{time:?}, {before:?}");
                assert!(time <= elapsed - after * 3, "{time:?}, {after:?}");
            }
            other => panic!("expected a latency answer, got {other:?}"),
        }
    }
}
