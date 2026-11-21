// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Throttled logging of the events that show what happened to the path secret of each peer.
//!
//! [`Subscriber`] logs the path secret map's per-peer events, and [`HandshakeSubscriber`] logs the
//! start, completion and failure of each handshake. Both log through `tracing`, with the peer's
//! address and, where the event has one, the credential id. Neither records a metric, so a peer's
//! address never becomes a metric dimension.
//!
//! Each kind of event is throttled for each peer on its own. The first event of a kind for a peer
//! logs at once. Later events of that kind for that peer within the interval are held back and
//! counted, and the next one the throttle lets through logs that count as `held_back`. So a burst
//! of events for one peer logs one line for each interval, and doesn't hold back the events of
//! another peer, or another kind of event for the same peer.
//!
//! Add [`Subscriber`] to the subscriber of the path secret map, and [`HandshakeSubscriber`] to the
//! subscriber of the PSK client and server.

use crate::event::{self, api, Event};
use core::{fmt, time::Duration};
use s2n_quic_core::event::api as core_api;
use std::{
    collections::{hash_map, HashMap},
    net::SocketAddr,
    sync::Mutex,
};

/// The interval of each subscriber's [`Default`]: one event of each kind for each peer a minute.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// The `tracing` target of each line.
const TARGET: &str = module_path!();

/// The count of entries below which the throttle doesn't look for entries to drop.
const MIN_PRUNE_LEN: usize = 1024;

const HANDSHAKE_STARTED: &str = "handshake:started";
const HANDSHAKE_COMPLETED: &str = "handshake:completed";
const HANDSHAKE_FAILED: &str = "handshake:failed";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Info,
    Warn,
    Error,
}

/// What a throttled event is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Subject {
    /// The peer's address
    Peer(SocketAddr),
    /// The credential id of an event that doesn't carry the peer's address
    Credential([u8; 16]),
}

impl Subject {
    fn peer(address: &api::SocketAddress) -> Self {
        Self::Peer(address.into())
    }

    fn credential(id: &[u8]) -> Self {
        let mut bytes = [0; 16];
        let len = id.len().min(bytes.len());
        bytes[..len].copy_from_slice(&id[..len]);
        Self::Credential(bytes)
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Peer(address) => fmt::Display::fmt(address, f),
            // The line's credential id tells which peer it is: the map's `entry_ready` line for the
            // same credential id, and its control packet lines, give the peer's address.
            Self::Credential(_) => f.write_str("unknown"),
        }
    }
}

/// Writes a credential id in hexadecimal, or `-` where the line doesn't have one.
struct CredentialId<'a>(Option<&'a [u8]>);

impl fmt::Display for CredentialId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(id) = self.0 {
            for byte in id {
                write!(f, "{byte:02x}")?;
            }
            Ok(())
        } else {
            f.write_str("-")
        }
    }
}

/// Decides, for each kind of event and each subject, which events log.
#[derive(Debug)]
struct Throttle {
    interval: Duration,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    entries: HashMap<(Subject, &'static str), Held>,
    /// The count of entries at which the next event drops each entry whose interval has passed
    prune_len: usize,
}

#[derive(Debug)]
struct Held {
    level: Level,
    /// When the throttle last let an event of this kind for this subject through
    logged_at: Duration,
    /// The events of this kind for this subject held back since then
    count: u64,
}

/// The held-back count of an entry that the throttle dropped, which its caller logs.
#[derive(Debug, PartialEq, Eq)]
struct Dropped {
    subject: Subject,
    kind: &'static str,
    level: Level,
    held_back: u64,
}

impl Throttle {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            state: Mutex::new(State {
                entries: HashMap::new(),
                prune_len: MIN_PRUNE_LEN,
            }),
        }
    }

    /// Returns the count of the events held back since the last one that logged, where the
    /// event of `kind` about `subject` at `now` logs, and `None` where the throttle holds it back.
    ///
    /// Once the throttle holds `prune_len` entries, it drops each entry whose interval has passed
    /// before it reads `subject`'s, and pushes each dropped entry that held events back onto
    /// `dropped`, so that its count is logged and the entries don't outgrow the active peers.
    fn check(
        &self,
        subject: Subject,
        kind: &'static str,
        level: Level,
        now: Duration,
        dropped: &mut Vec<Dropped>,
    ) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.prune(now, self.interval, dropped);

        match state.entries.entry((subject, kind)) {
            hash_map::Entry::Occupied(mut entry) => {
                let held = entry.get_mut();
                if now.saturating_sub(held.logged_at) < self.interval {
                    held.count = held.count.saturating_add(1);
                    return None;
                }
                held.logged_at = now;
                Some(core::mem::take(&mut held.count))
            }
            hash_map::Entry::Vacant(entry) => {
                entry.insert(Held {
                    level,
                    logged_at: now,
                    count: 0,
                });
                Some(0)
            }
        }
    }

    /// Logs the event of `kind` about `subject` at `now`, unless the throttle holds it back.
    fn log(
        &self,
        now: Duration,
        level: Level,
        kind: &'static str,
        subject: Subject,
        credential_id: Option<&[u8]>,
        detail: fmt::Arguments,
    ) {
        let mut dropped = Vec::new();
        let held_back = self.check(subject, kind, level, now, &mut dropped);

        for entry in dropped {
            emit(
                entry.level,
                entry.kind,
                entry.subject,
                None,
                entry.held_back,
                format_args!("no later event of this kind for this peer"),
            );
        }

        if let Some(held_back) = held_back {
            emit(level, kind, subject, credential_id, held_back, detail);
        }
    }
}

impl State {
    fn prune(&mut self, now: Duration, interval: Duration, dropped: &mut Vec<Dropped>) {
        if self.entries.len() < self.prune_len {
            return;
        }

        self.entries.retain(|&(subject, kind), held| {
            if now.saturating_sub(held.logged_at) < interval {
                return true;
            }
            if held.count > 0 {
                dropped.push(Dropped {
                    subject,
                    kind,
                    level: held.level,
                    held_back: held.count,
                });
            }
            false
        });

        self.prune_len = self.entries.len().saturating_mul(2).max(MIN_PRUNE_LEN);
    }
}

fn emit(
    level: Level,
    kind: &'static str,
    subject: Subject,
    credential_id: Option<&[u8]>,
    held_back: u64,
    detail: fmt::Arguments,
) {
    let credential_id = match (credential_id, &subject) {
        (Some(id), _) => CredentialId(Some(id)),
        (None, Subject::Credential(id)) => CredentialId(Some(&id[..])),
        (None, Subject::Peer(_)) => CredentialId(None),
    };

    match level {
        Level::Info => ::tracing::info!(
            target: TARGET,
            kind,
            peer = %subject,
            credential_id = %credential_id,
            held_back,
            "{detail}"
        ),
        Level::Warn => ::tracing::warn!(
            target: TARGET,
            kind,
            peer = %subject,
            credential_id = %credential_id,
            held_back,
            "{detail}"
        ),
        Level::Error => ::tracing::error!(
            target: TARGET,
            kind,
            peer = %subject,
            credential_id = %credential_id,
            held_back,
            "{detail}"
        ),
    }
}

/// Logs the path secret map's per-peer events, throttled for each peer and each kind of event.
///
/// It logs, with the peer's address and the credential id:
/// - each entry evicted from the map, by credential id or by address, at `WARN`;
/// - each `UnknownPathSecret` packet sent, received, accepted (at `WARN`), rejected or dropped;
/// - each replay the map detects, and each packet whose key id is too old for the replay window
///   to tell (a stale key), at `WARN`. These events carry the credential id and not the peer's
///   address, so they are throttled for each credential id, and the `ReplayDetected` and
///   `StaleKey` packet lines for the same credential id give the peer;
/// - each `ReplayDetected` and `StaleKey` packet sent, received, accepted, rejected or dropped;
/// - each background handshake the map asks for, each entry replaced by a new one for the same
///   peer, and each entry ready for use;
/// - each handshake confirmation or MTU probe that timed out, at `WARN`.
#[derive(Debug)]
pub struct Subscriber {
    throttle: Throttle,
}

impl Default for Subscriber {
    fn default() -> Self {
        Self::new(DEFAULT_INTERVAL)
    }
}

impl Subscriber {
    /// Logs at most one event of each kind for each peer in each `interval`, with the count of the
    /// events it held back since the last one. A zero `interval` logs every event.
    pub fn new(interval: Duration) -> Self {
        Self {
            throttle: Throttle::new(interval),
        }
    }

    fn peer<E: Event>(
        &self,
        meta: &api::EndpointMeta,
        level: Level,
        _event: &E,
        peer: &api::SocketAddress,
        id: Option<&[u8]>,
        detail: fmt::Arguments,
    ) {
        let now = meta.timestamp.duration_since_start();
        let subject = Subject::peer(peer);
        self.throttle.log(now, level, E::NAME, subject, id, detail);
    }

    fn credential<E: Event>(
        &self,
        meta: &api::EndpointMeta,
        level: Level,
        _event: &E,
        credential_id: &[u8],
        detail: fmt::Arguments,
    ) {
        let now = meta.timestamp.duration_since_start();
        let subject = Subject::credential(credential_id);
        let id = Some(credential_id);
        self.throttle.log(now, level, E::NAME, subject, id, detail);
    }
}

impl event::Subscriber for Subscriber {
    type ConnectionContext = ();

    fn create_connection_context(
        &self,
        _meta: &api::ConnectionMeta,
        _info: &api::ConnectionInfo,
    ) -> Self::ConnectionContext {
    }

    fn on_dc_connection_timeout(&self, meta: &api::EndpointMeta, event: &api::DcConnectionTimeout) {
        let peer = &event.peer_address;
        self.peer(meta, Level::Warn, event, peer, None, format_args!(""));
    }

    fn on_path_secret_map_background_handshake_requested(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapBackgroundHandshakeRequested,
    ) {
        let peer = &event.peer_address;
        self.peer(meta, Level::Info, event, peer, None, format_args!(""));
    }

    fn on_path_secret_map_entry_ready(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapEntryReady,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Info, event, peer, id, format_args!(""));
    }

    fn on_path_secret_map_entry_replaced(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapEntryReplaced,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.new_credential_id);
        let previous = CredentialId(Some(event.previous_credential_id));
        self.peer(
            meta,
            Level::Info,
            event,
            peer,
            id,
            format_args!("previous_credential_id={previous}"),
        );
    }

    fn on_path_secret_map_id_entry_evicted(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapIdEntryEvicted,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(
            meta,
            Level::Warn,
            event,
            peer,
            id,
            format_args!("age={:?}", event.age),
        );
    }

    fn on_path_secret_map_address_entry_evicted(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapAddressEntryEvicted,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(
            meta,
            Level::Warn,
            event,
            peer,
            id,
            format_args!("age={:?}", event.age),
        );
    }

    fn on_unknown_path_secret_packet_sent(
        &self,
        meta: &api::EndpointMeta,
        event: &api::UnknownPathSecretPacketSent,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Info, event, peer, id, format_args!(""));
    }

    fn on_unknown_path_secret_packet_received(
        &self,
        meta: &api::EndpointMeta,
        event: &api::UnknownPathSecretPacketReceived,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Info, event, peer, id, format_args!(""));
    }

    fn on_unknown_path_secret_packet_accepted(
        &self,
        meta: &api::EndpointMeta,
        event: &api::UnknownPathSecretPacketAccepted,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Warn, event, peer, id, format_args!(""));
    }

    fn on_unknown_path_secret_packet_rejected(
        &self,
        meta: &api::EndpointMeta,
        event: &api::UnknownPathSecretPacketRejected,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Info, event, peer, id, format_args!(""));
    }

    fn on_unknown_path_secret_packet_dropped(
        &self,
        meta: &api::EndpointMeta,
        event: &api::UnknownPathSecretPacketDropped,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Info, event, peer, id, format_args!(""));
    }

    fn on_replay_definitely_detected(
        &self,
        meta: &api::EndpointMeta,
        event: &api::ReplayDefinitelyDetected,
    ) {
        self.credential(
            meta,
            Level::Warn,
            event,
            event.credential_id,
            format_args!("key_id={}", event.key_id),
        );
    }

    fn on_replay_potentially_detected(
        &self,
        meta: &api::EndpointMeta,
        event: &api::ReplayPotentiallyDetected,
    ) {
        self.credential(
            meta,
            Level::Warn,
            event,
            event.credential_id,
            format_args!("key_id={} gap={}", event.key_id, event.gap),
        );
    }

    fn on_replay_detected_packet_sent(
        &self,
        meta: &api::EndpointMeta,
        event: &api::ReplayDetectedPacketSent,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Info, event, peer, id, format_args!(""));
    }

    fn on_replay_detected_packet_received(
        &self,
        meta: &api::EndpointMeta,
        event: &api::ReplayDetectedPacketReceived,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Info, event, peer, id, format_args!(""));
    }

    fn on_replay_detected_packet_accepted(
        &self,
        meta: &api::EndpointMeta,
        event: &api::ReplayDetectedPacketAccepted,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(
            meta,
            Level::Info,
            event,
            peer,
            id,
            format_args!("key_id={}", event.key_id),
        );
    }

    fn on_replay_detected_packet_rejected(
        &self,
        meta: &api::EndpointMeta,
        event: &api::ReplayDetectedPacketRejected,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Info, event, peer, id, format_args!(""));
    }

    fn on_replay_detected_packet_dropped(
        &self,
        meta: &api::EndpointMeta,
        event: &api::ReplayDetectedPacketDropped,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Info, event, peer, id, format_args!(""));
    }

    fn on_stale_key_packet_sent(&self, meta: &api::EndpointMeta, event: &api::StaleKeyPacketSent) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Info, event, peer, id, format_args!(""));
    }

    fn on_stale_key_packet_received(
        &self,
        meta: &api::EndpointMeta,
        event: &api::StaleKeyPacketReceived,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Info, event, peer, id, format_args!(""));
    }

    fn on_stale_key_packet_accepted(
        &self,
        meta: &api::EndpointMeta,
        event: &api::StaleKeyPacketAccepted,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Info, event, peer, id, format_args!(""));
    }

    fn on_stale_key_packet_rejected(
        &self,
        meta: &api::EndpointMeta,
        event: &api::StaleKeyPacketRejected,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Info, event, peer, id, format_args!(""));
    }

    fn on_stale_key_packet_dropped(
        &self,
        meta: &api::EndpointMeta,
        event: &api::StaleKeyPacketDropped,
    ) {
        let peer = &event.peer_address;
        let id = Some(event.credential_id);
        self.peer(meta, Level::Info, event, peer, id, format_args!(""));
    }
}

/// Logs the start, completion and failure of each handshake, throttled for each peer and each kind
/// of event.
///
/// A handshake completes once its connection's dc state is complete, and fails where its
/// connection closes before that, at `ERROR`. A connection that closes after it completed doesn't
/// log.
#[derive(Debug)]
pub struct HandshakeSubscriber {
    throttle: Throttle,
}

impl Default for HandshakeSubscriber {
    fn default() -> Self {
        Self::new(DEFAULT_INTERVAL)
    }
}

impl HandshakeSubscriber {
    /// Logs at most one event of each kind for each peer in each `interval`, with the count of the
    /// events it held back since the last one. A zero `interval` logs every event.
    pub fn new(interval: Duration) -> Self {
        Self {
            throttle: Throttle::new(interval),
        }
    }

    fn log(
        &self,
        meta: &core_api::ConnectionMeta,
        level: Level,
        kind: &'static str,
        peer: SocketAddr,
        detail: fmt::Arguments,
    ) {
        let now = meta.timestamp.duration_since_start();
        let subject = Subject::Peer(peer);
        self.throttle.log(now, level, kind, subject, None, detail);
    }
}

/// The handshake state of one connection.
#[derive(Debug, Default)]
pub struct HandshakeContext {
    peer: Option<SocketAddr>,
    complete: bool,
}

impl core_api::Subscriber for HandshakeSubscriber {
    type ConnectionContext = HandshakeContext;

    fn create_connection_context(
        &mut self,
        _meta: &core_api::ConnectionMeta,
        _info: &core_api::ConnectionInfo,
    ) -> Self::ConnectionContext {
        HandshakeContext::default()
    }

    fn on_connection_started(
        &mut self,
        context: &mut Self::ConnectionContext,
        meta: &core_api::ConnectionMeta,
        event: &core_api::ConnectionStarted,
    ) {
        let peer = SocketAddr::from(&event.path.remote_addr);
        context.peer = Some(peer);
        self.log(
            meta,
            Level::Info,
            HANDSHAKE_STARTED,
            peer,
            format_args!("endpoint={:?}", meta.endpoint_type),
        );
    }

    fn on_dc_state_changed(
        &mut self,
        context: &mut Self::ConnectionContext,
        meta: &core_api::ConnectionMeta,
        event: &core_api::DcStateChanged,
    ) {
        if context.complete || !matches!(event.state, core_api::DcState::Complete { .. }) {
            return;
        }
        context.complete = true;
        if let Some(peer) = context.peer {
            self.log(
                meta,
                Level::Info,
                HANDSHAKE_COMPLETED,
                peer,
                format_args!(""),
            );
        }
    }

    fn on_connection_closed(
        &mut self,
        context: &mut Self::ConnectionContext,
        meta: &core_api::ConnectionMeta,
        event: &core_api::ConnectionClosed,
    ) {
        if context.complete {
            return;
        }
        if let Some(peer) = context.peer {
            self.log(
                meta,
                Level::Error,
                HANDSHAKE_FAILED,
                peer,
                format_args!("{}", event.error),
            );
        }
    }
}

#[cfg(test)]
mod tests;
