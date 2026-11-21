// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::event::{builder, IntoEvent as _, Subscriber as _};
use s2n_quic_core::{
    connection,
    event::{api::Subscriber as _, builder as core_builder},
    inet,
    time::{Clock as _, NoopClock},
};
use std::sync::Arc;
use tracing_subscriber::layer::SubscriberExt as _;

const INTERVAL: Duration = Duration::from_secs(60);

/// A line one of the subscribers logged
#[derive(Debug)]
struct Line {
    level: ::tracing::Level,
    fields: Vec<(&'static str, String)>,
}

impl Line {
    fn field(&self, name: &str) -> &str {
        self.fields
            .iter()
            .find(|(field, _)| *field == name)
            .map(|(_, value)| value.as_str())
            .unwrap_or_else(|| panic!("{name} isn't in {self:?}"))
    }
}

/// Collects the lines the subscribers log on the thread that calls [`Capture::run`]
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Line>>>);

impl Capture {
    fn run(f: impl FnOnce()) -> Vec<Line> {
        let capture = Self::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        ::tracing::subscriber::with_default(subscriber, f);
        let mut lines = capture.0.lock().unwrap();
        core::mem::take(&mut *lines)
    }
}

impl<S: ::tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(
        &self,
        event: &::tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() != TARGET {
            return;
        }
        let mut fields = Fields(Vec::new());
        event.record(&mut fields);
        let line = Line {
            level: *event.metadata().level(),
            fields: fields.0,
        };
        self.0.lock().unwrap().push(line);
    }
}

struct Fields(Vec<(&'static str, String)>);

impl ::tracing::field::Visit for Fields {
    fn record_str(&mut self, field: &::tracing::field::Field, value: &str) {
        self.0.push((field.name(), value.to_string()));
    }

    fn record_debug(&mut self, field: &::tracing::field::Field, value: &dyn fmt::Debug) {
        self.0.push((field.name(), format!("{value:?}")));
    }
}

fn address(port: u16) -> inet::SocketAddress {
    SocketAddr::from(([127, 0, 0, 1], port)).into()
}

/// The path secret map's event metadata `secs` seconds after its clock starts
fn at(secs: u64) -> api::EndpointMeta {
    let timestamp = NoopClock.get_time() + Duration::from_secs(secs);
    builder::EndpointMeta {
        timestamp: timestamp.into_event(),
    }
    .into_event()
}

fn evicted(sub: &Subscriber, secs: u64, address: &inet::SocketAddress, id: &[u8]) {
    let meta = at(secs);
    let event = builder::PathSecretMapIdEntryEvicted {
        peer_address: address.into_event(),
        credential_id: id,
        age: Duration::from_secs(1),
    };
    let event: api::PathSecretMapIdEntryEvicted = event.into_event();
    sub.on_path_secret_map_id_entry_evicted(&meta, &event);
}

fn stale_key_sent(sub: &Subscriber, secs: u64, address: &inet::SocketAddress, id: &[u8]) {
    let meta = at(secs);
    let event = builder::StaleKeyPacketSent {
        peer_address: address.into_event(),
        credential_id: id,
    };
    let event: api::StaleKeyPacketSent = event.into_event();
    sub.on_stale_key_packet_sent(&meta, &event);
}

fn replayed(sub: &Subscriber, secs: u64, id: &[u8]) {
    let meta = at(secs);
    let event = builder::ReplayDefinitelyDetected {
        credential_id: id,
        key_id: 7,
    };
    let event: api::ReplayDefinitelyDetected = event.into_event();
    sub.on_replay_definitely_detected(&meta, &event);
}

#[test]
fn a_burst_for_one_peer_logs_its_first_event_and_then_a_held_back_count() {
    let sub = Subscriber::new(INTERVAL);
    let peer = address(4433);
    let lines = Capture::run(|| {
        for secs in 0..5 {
            evicted(&sub, secs, &peer, &[1; 16]);
        }
        // The interval since the first event has passed, so the next one logs, with the count of
        // the four the throttle held back.
        evicted(&sub, 60, &peer, &[1; 16]);
    });

    assert_eq!(lines.len(), 2, "{lines:?}");
    let first = &lines[0];
    assert_eq!(first.level, ::tracing::Level::WARN);
    assert_eq!(first.field("kind"), "path_secret_map:id_entry_evicted");
    assert_eq!(first.field("peer"), "127.0.0.1:4433");
    assert_eq!(first.field("credential_id"), "01".repeat(16));
    assert_eq!(first.field("held_back"), "0");
    assert_eq!(first.field("message"), "age=1s");
    assert_eq!(lines[1].field("held_back"), "4");
}

#[test]
fn a_burst_for_each_of_two_peers_logs_each_peer() {
    let sub = Subscriber::new(INTERVAL);
    let lines = Capture::run(|| {
        for secs in 0..3 {
            evicted(&sub, secs, &address(1), &[1; 16]);
            evicted(&sub, secs, &address(2), &[2; 16]);
        }
    });

    let peers: Vec<_> = lines.iter().map(|line| line.field("peer")).collect();
    assert_eq!(peers, ["127.0.0.1:1", "127.0.0.1:2"]);
}

#[test]
fn each_kind_of_event_for_a_peer_is_throttled_on_its_own() {
    let sub = Subscriber::new(INTERVAL);
    let peer = address(4433);
    let lines = Capture::run(|| {
        for secs in 0..3 {
            evicted(&sub, secs, &peer, &[1; 16]);
            stale_key_sent(&sub, secs, &peer, &[1; 16]);
        }
    });

    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0].field("kind"), "path_secret_map:id_entry_evicted");
    assert_eq!(
        lines[1].field("kind"),
        "path_secret_map:stale_key_packet_sent"
    );
}

#[test]
fn a_replay_is_throttled_for_its_credential_id() {
    let sub = Subscriber::new(INTERVAL);
    let lines = Capture::run(|| {
        replayed(&sub, 0, &[1; 16]);
        replayed(&sub, 0, &[2; 16]);
        replayed(&sub, 1, &[1; 16]);
    });

    assert_eq!(lines.len(), 2, "{lines:?}");
    for (line, id) in lines.iter().zip(["01", "02"]) {
        assert_eq!(
            line.field("kind"),
            "path_secret_map:replay_definitely_detected"
        );
        assert_eq!(line.field("peer"), "unknown");
        assert_eq!(line.field("credential_id"), id.repeat(16));
        assert_eq!(line.field("message"), "key_id=7");
    }
}

#[test]
fn a_zero_interval_logs_every_event() {
    let sub = Subscriber::new(Duration::ZERO);
    let peer = address(4433);
    let lines = Capture::run(|| {
        for _ in 0..3 {
            evicted(&sub, 0, &peer, &[1; 16]);
        }
    });

    let held_back: Vec<_> = lines.iter().map(|line| line.field("held_back")).collect();
    assert_eq!(held_back, ["0", "0", "0"]);
}

fn check(
    throttle: &Throttle,
    subject: Subject,
    secs: u64,
    dropped: &mut Vec<Dropped>,
) -> Option<u64> {
    let now = Duration::from_secs(secs);
    throttle.check(subject, "kind", Level::Warn, now, dropped)
}

fn peer_subject(port: u16) -> Subject {
    Subject::Peer(SocketAddr::from(([127, 0, 0, 1], port)))
}

#[test]
fn an_entry_past_its_interval_is_dropped_with_its_held_back_count() {
    let throttle = Throttle::new(INTERVAL);
    let mut dropped = vec![];
    let held = Subject::credential(&[1; 16]);
    assert_eq!(check(&throttle, held, 0, &mut dropped), Some(0));
    assert_eq!(check(&throttle, held, 1, &mut dropped), None);

    // Fill the throttle up to the count of entries at which it looks for entries to drop.
    for port in 1..MIN_PRUNE_LEN as u16 {
        let peer = peer_subject(port);
        assert_eq!(check(&throttle, peer, 0, &mut dropped), Some(0));
    }
    assert!(dropped.is_empty(), "{dropped:?}");

    // Each entry's interval has passed by now, so the next event drops them all, and returns the
    // count the first one held back.
    let next = Subject::credential(&[2; 16]);
    assert_eq!(check(&throttle, next, 60, &mut dropped), Some(0));
    let expected = Dropped {
        subject: held,
        kind: "kind",
        level: Level::Warn,
        held_back: 1,
    };
    assert_eq!(dropped, [expected]);
    let state = throttle.state.lock().unwrap();
    assert_eq!(state.entries.len(), 1, "only the new entry is left");
    assert_eq!(state.prune_len, MIN_PRUNE_LEN);
}

fn log(throttle: &Throttle, subject: Subject, secs: u64) {
    Throttle::log(
        throttle,
        Duration::from_secs(secs),
        Level::Warn,
        "kind",
        subject,
        None,
        format_args!(""),
    );
}

#[test]
fn a_dropped_entry_logs_its_held_back_count() {
    let throttle = Throttle::new(INTERVAL);
    let held = Subject::credential(&[1; 16]);
    let lines = Capture::run(|| {
        log(&throttle, held, 0);
        log(&throttle, held, 1);
        for port in 1..MIN_PRUNE_LEN as u16 {
            log(&throttle, peer_subject(port), 0);
        }
        log(&throttle, Subject::credential(&[2; 16]), 60);
    });

    // The first event of each entry, then the dropped entry's count, then the last event.
    assert_eq!(lines.len(), MIN_PRUNE_LEN + 2);
    let dropped = &lines[MIN_PRUNE_LEN];
    assert_eq!(dropped.level, ::tracing::Level::WARN);
    assert_eq!(dropped.field("peer"), "unknown");
    assert_eq!(dropped.field("credential_id"), "01".repeat(16));
    assert_eq!(dropped.field("held_back"), "1");
    assert_eq!(
        dropped.field("message"),
        "no later event of this kind for this peer"
    );
    let last = &lines[MIN_PRUNE_LEN + 1];
    assert_eq!(last.field("credential_id"), "02".repeat(16));
    assert_eq!(last.field("held_back"), "0");
}

fn connection_meta(secs: u64) -> core_api::ConnectionMeta {
    core_builder::ConnectionMeta {
        endpoint_type: s2n_quic_core::endpoint::Type::Client,
        id: 0,
        timestamp: NoopClock.get_time() + Duration::from_secs(secs),
    }
    .into_event()
}

/// Runs a handshake with `address` that starts at `secs`, and completes or fails a second later
fn handshake(
    sub: &mut HandshakeSubscriber,
    secs: u64,
    address: &inet::SocketAddress,
    completes: bool,
) {
    let meta = connection_meta(secs);
    let info = core_builder::ConnectionInfo { application: None };
    let info: core_api::ConnectionInfo = info.into_event();
    let mut context = sub.create_connection_context(&meta, &info);

    let cid = core_builder::ConnectionId { bytes: &[] };
    let path = core_builder::Path {
        local_addr: address.into_event(),
        local_cid: cid,
        remote_addr: address.into_event(),
        remote_cid: cid,
        id: 0,
        is_active: true,
    };
    let started = core_builder::ConnectionStarted { path };
    let started: core_api::ConnectionStarted = started.into_event();
    sub.on_connection_started(&mut context, &meta, &started);

    let meta = connection_meta(secs + 1);
    if completes {
        let state = core_builder::DcStateChanged {
            state: core_builder::DcState::Complete,
        };
        let state: core_api::DcStateChanged = state.into_event();
        sub.on_dc_state_changed(&mut context, &meta, &state);
    }
    let closed = core_builder::ConnectionClosed {
        error: connection::Error::idle_timer_expired(),
    };
    let closed: core_api::ConnectionClosed = closed.into_event();
    sub.on_connection_closed(&mut context, &meta, &closed);
}

#[test]
fn a_handshake_logs_its_start_and_its_completion_or_failure() {
    let mut sub = HandshakeSubscriber::new(INTERVAL);
    let lines = Capture::run(|| {
        handshake(&mut sub, 0, &address(1), true);
        handshake(&mut sub, 0, &address(2), false);
    });

    let logged: Vec<_> = lines
        .iter()
        .map(|line| (line.field("kind"), line.field("peer")))
        .collect();
    let expected = [
        (HANDSHAKE_STARTED, "127.0.0.1:1"),
        (HANDSHAKE_COMPLETED, "127.0.0.1:1"),
        (HANDSHAKE_STARTED, "127.0.0.1:2"),
        (HANDSHAKE_FAILED, "127.0.0.1:2"),
    ];
    assert_eq!(logged, expected);
    assert_eq!(lines[0].field("message"), "endpoint=Client");
    assert_eq!(lines[1].level, ::tracing::Level::INFO);
    assert_eq!(lines[3].level, ::tracing::Level::ERROR);
    let error = connection::Error::idle_timer_expired().to_string();
    assert_eq!(lines[3].field("message"), error);
}

#[test]
fn repeated_failed_handshakes_with_one_peer_log_once_an_interval() {
    let mut sub = HandshakeSubscriber::new(INTERVAL);
    let peer = address(4433);
    let lines = Capture::run(|| {
        for secs in [0, 10, 20, 60] {
            handshake(&mut sub, secs, &peer, false);
        }
    });

    let logged: Vec<_> = lines
        .iter()
        .map(|line| (line.field("kind"), line.field("held_back")))
        .collect();
    let expected = [
        (HANDSHAKE_STARTED, "0"),
        (HANDSHAKE_FAILED, "0"),
        (HANDSHAKE_STARTED, "2"),
        (HANDSHAKE_FAILED, "2"),
    ];
    assert_eq!(logged, expected);
}
