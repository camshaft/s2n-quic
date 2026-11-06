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

mod limit;
mod rt;

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

/// The lowest round-trip time that a connection's recovery measured, or `None` before the
/// first.
fn connection_rtt(connection: &s2n_quic::Connection) -> Option<Duration> {
    connection
        .query_event_context(|context: &PathRttContext| context.min_rtt)
        .ok()
        .flatten()
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
/// measured. It lowers the round-trip time of the connection's map entry to the lower of the
/// two, so that the next handshake with the peer reads it as its `earlier_rtt`.
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
    limit::Answer::for_handshake(elapsed, earlier_rtt, rtt)
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

        Self::bind_with_io(io, map, tls_materials_provider, subscriber, builder)
    }

    /// Binds the server over `io`, an s2n-quic IO provider, such as the bach-backed provider that
    /// the tests build; [`Self::bind`] builds the OS-socket provider from `builder`'s MTU.
    fn bind_with_io<
        Io: s2n_quic::provider::io::Provider,
        Provider: Prov + Send + Sync + 'static,
        Subscriber: Sub + Send + Sync + 'static,
        Event: s2n_quic::provider::event::Subscriber,
    >(
        io: Io,
        map: secret::Map,
        tls_materials_provider: Provider,
        subscriber: Subscriber,
        builder: server::Builder<Event>,
    ) -> Result<Self, Error> {
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
    let server = match Server::bind::<Provider, Subscriber, Event>(
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

    serve(server, map, address).await;
}

/// Accepts `server`'s connections, each in a task of its own that keeps the connection open until
/// its client closes it, or for at most 10 seconds. `address` is the address the server was bound
/// to, for the logs.
async fn serve(mut server: Server, map: secret::Map, address: SocketAddr) {
    while let Some(mut connection) = server.server.accept().await {
        let map_clone = map.clone();
        rt::spawn(async move {
            // The accepted connection must remain open until the client has finished inserting
            // the entry into its map. The client indicates this by sending a ConnectionClose
            // when it is done.
            //
            // A 10 second timeout is specified to avoid spawned tasks piling up when the
            // ConnectionClose from the client is lost. This timeout covers dc handshake
            // confirmation and MTU probing completion. Data addresses are exchanged via
            // transport parameters during the TLS handshake.
            let deadline = rt::Instant::now() + Duration::from_secs(10);

            match rt::timeout_at(deadline, ConfirmComplete::wait_ready(&mut connection)).await {
                Ok(Ok(())) => {
                    let _ =
                        rt::timeout_at(deadline, MtuConfirmComplete::wait_ready(&mut connection))
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

        Self::bind_with_io(io, map, tls_materials_provider, subscriber, builder)
    }

    /// Binds the client over `io`, an s2n-quic IO provider, such as the bach-backed provider that
    /// the tests build; [`Self::bind`] builds the OS-socket provider from `builder`'s MTU.
    fn bind_with_io<
        Io: s2n_quic::provider::io::Provider,
        Provider: Prov + Send + Sync + 'static,
        Subscriber: Sub + Send + Sync + 'static,
        Event: s2n_quic::provider::event::Subscriber,
    >(
        io: Io,
        map: secret::Map,
        tls_materials_provider: Provider,
        subscriber: Subscriber,
        builder: client::Builder<Event>,
    ) -> Result<Self, Error> {
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
            subscriber,
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
            let start = rt::Instant::now();
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

            let handshake_start = rt::Instant::now();
            let handshake_result = async {
                let mut connection = attempt.await?;

                // A 10 second deadline is used to bound ConfirmComplete and
                // MtuConfirmComplete, avoiding unbounded waits if the peer is slow.
                // Data addresses are exchanged via transport parameters during the
                // TLS handshake.
                let deadline = rt::Instant::now() + Duration::from_secs(10);

                rt::timeout_at(deadline, ConfirmComplete::wait_ready(&mut connection))
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
            rt::spawn(async move {
                // Use the same deadline for MTU probing - any remaining time from the 10s budget
                let answer = mtu_probing_answer(
                    MtuConfirmComplete::wait_ready(&mut connection),
                    deadline,
                    &map_clone,
                    peer,
                )
                .await;

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
                rt::sleep(Duration::from_millis(duration)).await;

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
                    rt::spawn(async move {
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
                        rt::sleep(Duration::from_millis(duration)).await;

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

/// Waits for `probing`, a connection's MTU probing, until `deadline`, and returns what it says
/// about the inflight limit: `Answer::Finished` where the probing finished in time, and
/// `Answer::Failed` of `peer` where it didn't, which `map` also reports as a dc connection
/// timeout.
async fn mtu_probing_answer(
    probing: impl std::future::Future<Output = ()>,
    deadline: rt::Instant,
    map: &secret::Map,
    peer: SocketAddr,
) -> limit::Answer {
    if rt::timeout_at(deadline, probing).await.is_err() {
        map.on_dc_connection_timeout(&peer);
        // A connection that didn't finish its MTU probing before the deadline is a failure of
        // this peer for the inflight limit.
        limit::Answer::Failed(peer)
    } else {
        limit::Answer::Finished
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
        testing::{
            ext::{GroupExt as _, PrimaryExt as _, SpawnExt as _},
            init_tracing, sim, without_snapshots, NoopSubscriber, TestTlsProvider,
        },
    };
    use s2n_quic::provider::{
        endpoint_limits::{ConnectionAttempt, Limiter, Outcome},
        tls::Provider,
    };
    use s2n_quic_core::time::StdClock;
    use std::time::Instant;
    use tokio::time::Instant as TokioInstant;
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

    /// Verifies that a connection that doesn't finish its MTU probing before its deadline is a
    /// failure of its peer for the inflight limit, which the map reports as a dc connection
    /// timeout: one peer's missed deadline leaves the limit as it is, and a second peer's missed
    /// deadline since the last clean answer halves it. The test runs on tokio's paused clock, so
    /// each wait ends at its deadline without waiting in real time.
    #[tokio::test(start_paused = true)]
    async fn missed_mtu_deadlines_of_two_peers_halve_the_inflight_limit() {
        let subscriber = Arc::new(crate::event::testing::Subscriber::no_snapshot());
        let map = Map::builder()
            .with_signer(Signer::new(b"default"))
            .with_capacity(50_000)
            .with_clock(StdClock::default())
            .with_subscriber(subscriber.clone())
            .build()
            .unwrap();
        let timeouts = || subscriber.dc_connection_timeout.load(Ordering::Relaxed);
        let queue = HandshakeQueue::new(Duration::ZERO);
        let inflight = &queue.limiter_inflight;
        let peer_a: SocketAddr = "127.0.0.1:4433".parse().unwrap();
        let peer_b: SocketAddr = "127.0.0.1:4434".parse().unwrap();

        // A connection that finished its MTU probing before its deadline filled the limit of
        // one, so the limit grows to 2.
        let permit = inflight.acquire().await;
        let deadline = TokioInstant::now() + Duration::from_secs(10);
        let probing = tokio::time::sleep(Duration::from_secs(1));
        let answer = mtu_probing_answer(probing, rt::Instant::Tokio(deadline), &map, peer_a).await;
        assert_eq!(answer, limit::Answer::Finished);
        assert_eq!(timeouts(), 0);
        permit.release(answer);
        assert_eq!(inflight.current(), 2);

        // Connections to two peers hold the limit's two permits, and neither finishes its MTU
        // probing before the deadline.
        let permit_a = inflight.acquire().await;
        let permit_b = inflight.acquire().await;
        let deadline = TokioInstant::now() + Duration::from_secs(10);

        let answer = mtu_probing_answer(
            std::future::pending(),
            rt::Instant::Tokio(deadline),
            &map,
            peer_a,
        )
        .await;
        assert!(
            TokioInstant::now() >= deadline,
            "the wait lasts until the deadline"
        );
        assert_eq!(answer, limit::Answer::Failed(peer_a));
        assert_eq!(timeouts(), 1);
        // One peer's missed deadline leaves the limit as it is.
        permit_a.release(answer);
        assert_eq!(inflight.current(), 2);

        let answer = mtu_probing_answer(
            std::future::pending(),
            rt::Instant::Tokio(deadline),
            &map,
            peer_b,
        )
        .await;
        assert_eq!(answer, limit::Answer::Failed(peer_b));
        assert_eq!(timeouts(), 2);
        // A second peer's missed deadline since the last clean answer halves the limit.
        permit_b.release(answer);
        assert_eq!(inflight.current(), 1);
    }

    /// The largest MTU the simulated endpoints probe for, so that each connection's MTU probing
    /// sends probes past the base MTU and finishes only once the peer answers them.
    const SIM_MAX_MTU: u16 = 8940;
    /// The port each simulated server listens on, in a bach group of its own.
    const SIM_SERVER_PORT: u16 = 4433;

    /// Returns the bach-backed IO provider for an endpoint on `port`, whose connections start at
    /// the base MTU and probe up to [`SIM_MAX_MTU`].
    fn sim_io(port: u16) -> s2n_quic::provider::io::bach::Provider {
        s2n_quic::provider::io::bach::Provider::builder()
            .with_address((std::net::Ipv4Addr::UNSPECIFIED, port).into())
            .unwrap()
            .with_base_mtu(DEFAULT_BASE_MTU)
            .unwrap()
            .with_initial_mtu(DEFAULT_BASE_MTU)
            .unwrap()
            .with_max_mtu(SIM_MAX_MTU)
            .unwrap()
            .build()
            .unwrap()
    }

    /// Returns a map for an endpoint of the current bach simulation, on the simulation's clock.
    fn sim_map<S: crate::event::Subscriber>(subscriber: S) -> Map {
        Map::builder()
            .with_signer(Signer::new(b"default"))
            .with_capacity(50_000)
            .with_clock(crate::time::DefaultClock::default())
            .with_subscriber(subscriber)
            .build()
            .unwrap()
    }

    /// Runs a PSK server in the current bach group, on [`SIM_SERVER_PORT`], until the simulation
    /// ends.
    async fn sim_server() {
        let map = sim_map(NoopSubscriber {});
        let server = Server::bind_with_io(
            sim_io(SIM_SERVER_PORT),
            map.clone(),
            TestTlsProvider {},
            NoopSubscriber {},
            crate::psk::server::Builder::default(),
        )
        .unwrap();
        let address = server.local_addr().unwrap();
        serve(server, map, address).await;
    }

    /// A map subscriber that sends a message for each dc connection timeout the map reports, so
    /// that a test waits on that event.
    struct DcConnectionTimeouts(tokio::sync::mpsc::UnboundedSender<()>);

    impl crate::event::Subscriber for DcConnectionTimeouts {
        type ConnectionContext = ();

        fn create_connection_context(
            &self,
            _meta: &crate::event::api::ConnectionMeta,
            _info: &crate::event::api::ConnectionInfo,
        ) -> Self::ConnectionContext {
        }

        fn on_dc_connection_timeout(
            &self,
            _meta: &crate::event::api::EndpointMeta,
            _event: &crate::event::api::DcConnectionTimeout,
        ) {
            let _ = self.0.send(());
        }
    }

    /// Verifies the MTU deadline through `HandshakeQueue::handshake`, with the PSK client and its
    /// servers in a bach simulation on the bach-backed IO provider. Once the dc handshake of a
    /// connection to a slow peer completes, the network drops every packet to and from that peer,
    /// so the server's MTU probing doesn't finish, and the connection misses its 10 second
    /// deadline, which the client's map reports as a dc connection timeout. One peer's missed
    /// deadline leaves the inflight limit as it is, and a second peer's missed deadline since the
    /// last clean answer halves it.
    #[test]
    fn missed_mtu_deadlines_through_the_handshake_queue_halve_the_inflight_limit() {
        // The handshakes run real TLS, whose record sizes, and so the times of its events, differ
        // across platforms, so the test doesn't compare its logs with a snapshot.
        let _no_snapshots = without_snapshots();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_check = done.clone();

        sim(|| {
            for peer in ["clean", "slow_a", "slow_b"] {
                sim_server().group(peer).spawn();
            }

            async move {
                // The network drops each packet from or to an IP address in `cut`.
                let cut = Arc::new(Mutex::new(Vec::<std::net::IpAddr>::new()));
                let monitor_cut = cut.clone();
                bach::net::monitor::on_packet_sent(move |packet| {
                    let cut = monitor_cut.lock().unwrap();
                    if cut.contains(&packet.source().ip())
                        || cut.contains(&packet.destination().ip())
                    {
                        bach::net::monitor::Command::Drop
                    } else {
                        bach::net::monitor::Command::Pass
                    }
                });

                let (timeouts_sender, mut timeouts) = tokio::sync::mpsc::unbounded_channel();
                let client = Client::bind_with_io(
                    sim_io(0),
                    sim_map(DcConnectionTimeouts(timeouts_sender)),
                    TestTlsProvider {}.start_client().unwrap(),
                    NoopSubscriber {},
                    crate::psk::client::Builder::default().with_success_jitter(Duration::ZERO),
                )
                .unwrap();
                let inflight = &client.queue.limiter_inflight;
                let server_name: s2n_quic::server::Name = "localhost".into();
                let peer = |group: &str| bach::net::try_lookup((group, SIM_SERVER_PORT)).unwrap();
                let (clean, slow_a, slow_b) = (peer("clean"), peer("slow_a"), peer("slow_b"));
                assert_eq!(inflight.current(), 1);

                // The clean connection finishes its MTU probing before its deadline, with the
                // limit of one filled, so the limit grows to 2 once its permit comes back.
                client
                    .connect(clean, HandshakeReason::User, server_name.clone())
                    .await
                    .unwrap();

                // This handshake starts once the clean connection's permit came back. Its dc
                // handshake completes before the server's MTU probing can finish, since that
                // probing waits for the client's acknowledgments of the server's probes, and from
                // then on the network drops the connection's packets.
                let slow_a_start = bach::time::Instant::now();
                client
                    .connect(slow_a, HandshakeReason::User, server_name.clone())
                    .await
                    .unwrap();
                cut.lock().unwrap().push(slow_a.ip());
                assert_eq!(inflight.current(), 2);

                client
                    .connect(slow_b, HandshakeReason::User, server_name)
                    .await
                    .unwrap();
                cut.lock().unwrap().push(slow_b.ip());

                // Each wait below ends at a dc connection timeout of the client's map, which a
                // connection's task reports just before it releases the connection's inflight
                // permit, with no await between the two, so the limit has its answer by then.
                timeouts.recv().await.unwrap();
                assert!(
                    slow_a_start.elapsed() >= Duration::from_secs(10),
                    "the connection waits for its MTU probing until its deadline"
                );
                // One peer's missed deadline leaves the limit as it is.
                assert_eq!(inflight.current(), 2);

                timeouts.recv().await.unwrap();
                // A second peer's missed deadline since the last clean answer halves the limit.
                assert_eq!(inflight.current(), 1);

                done.store(true, Ordering::SeqCst);
            }
            .group("client")
            .primary()
            .spawn();
        });

        assert!(
            done_check.load(Ordering::SeqCst),
            "the client task didn't finish"
        );
    }

    /// The time the network adds to each packet from or to the far peer as the packet arrives,
    /// beside the 500 microseconds of each simulated path, so that the far peer's round trip takes
    /// 51 ms and the near peer's 1 ms.
    const FAR_PATH_DELAY: Duration = Duration::from_millis(25);

    /// A client subscriber that sends a message as each connection opens, and keeps the time from
    /// each connection's opening to its dc.complete, in the order the connections reach it.
    struct HandshakeTimes {
        opened: tokio::sync::mpsc::UnboundedSender<()>,
        times: Arc<Mutex<Vec<Duration>>>,
    }

    impl CoreSub for HandshakeTimes {
        /// The time of the connection's opening, from the endpoint's start.
        type ConnectionContext = Duration;

        fn create_connection_context(
            &mut self,
            meta: &events::ConnectionMeta,
            _info: &events::ConnectionInfo,
        ) -> Self::ConnectionContext {
            let _ = self.opened.send(());
            meta.timestamp.duration_since_start()
        }

        fn on_dc_state_changed(
            &mut self,
            context: &mut Self::ConnectionContext,
            meta: &events::ConnectionMeta,
            event: &events::DcStateChanged,
        ) {
            if let events::DcState::Complete { .. } = event.state {
                let now = meta.timestamp.duration_since_start();
                let elapsed = now.saturating_sub(*context);
                self.times.lock().unwrap().push(elapsed);
            }
        }
    }

    /// Verifies that handshakes with a near and a far peer at once through
    /// `HandshakeQueue::handshake` don't lower the start limit, with the PSK client and its
    /// servers in a bach simulation on the bach-backed IO provider. The network delays each packet
    /// from or to the far peer, so the far peer's handshake takes about 50 times as long as the
    /// near peer's, though the endpoint doesn't add that time. The first handshake with each peer
    /// reads its own connection's round trips, which in the simulation are its path's. A limit
    /// that the test answers with each handshake's whole time halves on the far peer's handshake,
    /// as the start limit does where `handshake` releases its start permit with the whole time.
    #[test]
    fn near_and_far_handshakes_at_once_through_the_queue_dont_lower_the_start_limit() {
        // The handshakes run real TLS, whose record sizes, and so the times of its events, differ
        // across platforms, so the test doesn't compare its logs with a snapshot.
        let _no_snapshots = without_snapshots();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_check = done.clone();

        sim(|| {
            for peer in ["first", "near", "far"] {
                sim_server().group(peer).spawn();
            }

            async move {
                let peer = |group: &str| bach::net::try_lookup((group, SIM_SERVER_PORT)).unwrap();
                let (first, near, far) = (peer("first"), peer("near"), peer("far"));
                // The network delays each packet from or to the far peer as the packet arrives.
                let far_ip = far.ip();
                bach::net::monitor::on_packet_received(move |packet| {
                    if packet.source().ip() == far_ip || packet.destination().ip() == far_ip {
                        bach::net::monitor::delay(FAR_PATH_DELAY).into()
                    } else {
                        bach::net::monitor::Command::Pass
                    }
                });

                let (opened_sender, mut opened) = tokio::sync::mpsc::unbounded_channel();
                let times = Arc::new(Mutex::new(Vec::new()));
                let client = Client::bind_with_io(
                    sim_io(0),
                    sim_map(NoopSubscriber {}),
                    TestTlsProvider {}.start_client().unwrap(),
                    HandshakeTimes {
                        opened: opened_sender,
                        times: times.clone(),
                    },
                    crate::psk::client::Builder::default().with_success_jitter(Duration::ZERO),
                )
                .unwrap();
                let start_limit = &client.queue.limiter_start;
                let server_name: s2n_quic::server::Name = "localhost".into();
                assert_eq!(start_limit.current(), 1);

                // The first handshake runs alone and fills the limit of one, which grows to 2.
                client
                    .connect(first, HandshakeReason::User, server_name.clone())
                    .await
                    .unwrap();
                opened.recv().await.unwrap();
                assert_eq!(start_limit.current(), 2);

                // The far peer's handshake starts once the near peer's connection opened, which
                // holds a start permit from then on, so the two handshakes run at once and fill
                // the limit of 2.
                let near_handshake =
                    client.connect(near, HandshakeReason::User, server_name.clone());
                let far_handshake = async {
                    opened.recv().await.unwrap();
                    client
                        .connect(far, HandshakeReason::User, server_name.clone())
                        .await
                };
                let (near_result, far_result) = tokio::join!(near_handshake, far_handshake);
                near_result.unwrap();
                far_result.unwrap();
                // Neither handshake's time past its path's two round trips reads as congestion,
                // so the limit grows with each answer.
                assert_eq!(start_limit.current(), 4);

                let times = times.lock().unwrap().clone();
                assert_eq!(times.len(), 3, "each connection reaches dc.complete once");
                // The connections reach dc.complete in the order first, near and far.
                let (first_time, near_time, far_time) = (times[0], times[1], times[2]);
                // The far peer's handshake takes more than twice as long as the first one, which
                // ran alone, so its whole time reads as congestion.
                assert!(far_time > first_time.saturating_mul(2), "{times:?}");

                // A limit answered with each handshake's whole time halves on the far peer's.
                let whole = limit::Limit::new();
                let first_permit = whole.acquire().await;
                first_permit.release(limit::Answer::Latency(first_time));
                assert_eq!(whole.current(), 2);
                let near_permit = whole.acquire().await;
                let far_permit = whole.acquire().await;
                near_permit.release(limit::Answer::Latency(near_time));
                assert_eq!(whole.current(), 3);
                far_permit.release(limit::Answer::Latency(far_time));
                assert_eq!(whole.current(), 1);

                done.store(true, Ordering::SeqCst);
            }
            .group("client")
            .primary()
            .spawn();
        });

        assert!(
            done_check.load(Ordering::SeqCst),
            "the client task didn't finish"
        );
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
}
