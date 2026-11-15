// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Each field with a `#[snapshot("[HIDDEN]")]` attribute is written as `"[HIDDEN]"` in each
//! textual form of its struct: the `Debug` form of the event and of its builder, and the fields
//! that the tracing integration logs.

use crate::event::{api, builder, IntoEvent, Subscriber as _};
use s2n_quic_core::{
    inet,
    time::{Clock as _, NoopClock},
};
use std::{
    io,
    sync::{Arc, Mutex},
};

const CREDENTIAL_ID: [u8; 16] = [0xab; 16];

/// The `Debug` form of the credential's bytes, which no textual form may hold
fn credential_bytes() -> String {
    let bytes = &CREDENTIAL_ID[..];
    format!("{bytes:?}")
}

fn peer_address() -> builder::SocketAddress<'static> {
    builder::SocketAddress::IpV4 {
        ip: &[127, 0, 0, 1],
        port: 4433,
    }
}

/// Checks that `output` writes each of `fields` as `"[HIDDEN]"` and doesn't hold the
/// credential's bytes
#[track_caller]
fn assert_hidden(output: &str, fields: &[&str]) {
    for field in fields {
        let hidden = format!("{field}: \"[HIDDEN]\"");
        assert!(output.contains(&hidden), "{hidden} isn't in {output}");
    }
    let bytes = credential_bytes();
    assert!(!output.contains(&bytes), "{bytes} is in {output}");
}

#[test]
fn event_debug_hides_snapshot_fields() {
    let event = builder::PathSecretMapEntryReplaced {
        peer_address: peer_address(),
        new_credential_id: &CREDENTIAL_ID,
        previous_credential_id: &CREDENTIAL_ID,
    };
    let fields = ["new_credential_id", "previous_credential_id"];

    let output = format!("{event:?}");
    assert_hidden(&output, &fields);
    assert!(output.contains("4433"), "{output}");

    let event: api::PathSecretMapEntryReplaced = event.into_event();
    let output = format!("{event:?}");
    assert_hidden(&output, &fields);
    assert!(output.contains("4433"), "{output}");
}

#[test]
fn connection_info_debug_hides_credential_id() {
    let remote_address =
        inet::SocketAddress::IpV4(inet::SocketAddressV4::new([127u8, 0, 0, 1], 4433u16));
    let info = builder::ConnectionInfo {
        credential_id: &CREDENTIAL_ID,
        key_id: 7,
        remote_address: &remote_address,
        is_server: true,
    };

    let output = format!("{info:?}");
    assert_hidden(&output, &["credential_id"]);
    assert!(output.contains("key_id: 7"), "{output}");

    let info: api::ConnectionInfo = info.into_event();
    let output = format!("{info:?}");
    assert_hidden(&output, &["credential_id"]);
    assert!(output.contains("key_id: 7"), "{output}");
}

/// Keeps what the `fmt` subscriber writes
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn tracing_hides_snapshot_fields() {
    let capture = Capture::default();
    let writer = capture.clone();
    let log = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .finish();

    tracing::subscriber::with_default(log, || {
        let subscriber = crate::event::tracing::Subscriber::default();
        let meta: api::EndpointMeta = builder::EndpointMeta {
            timestamp: NoopClock.get_time().into_event(),
        }
        .into_event();
        let event: api::PathSecretMapEntryInserted = builder::PathSecretMapEntryInserted {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        }
        .into_event();
        subscriber.on_path_secret_map_entry_inserted(&meta, &event);
    });

    let written = capture.0.lock().unwrap();
    let output = String::from_utf8_lossy(&written);
    let line = output
        .lines()
        .find(|line| line.contains("path_secret_map_entry_inserted"))
        .unwrap_or_else(|| panic!("the event isn't in {output}"));
    assert!(line.contains("DEBUG"), "{line}");
    assert!(line.contains("credential_id=\"[HIDDEN]\""), "{line}");
    let bytes = credential_bytes();
    assert!(!output.contains(&bytes), "{bytes} is in {output}");
}

/// Checks each textual form of `event` that this module's doc comment lists: the `Debug` form of
/// the builder and of the api event it builds, and the line the tracing integration logs for it
/// once `publish` passes the api event to its `on_*` method. Each form writes each of `fields` as
/// `"[HIDDEN]"` and doesn't hold the credential's bytes.
#[track_caller]
fn assert_event_hidden<B, E>(
    target: &str,
    fields: &[&str],
    event: B,
    publish: impl FnOnce(&crate::event::tracing::Subscriber, &api::EndpointMeta, &E),
) where
    B: core::fmt::Debug + IntoEvent<E>,
    E: core::fmt::Debug,
{
    assert_hidden(&format!("{event:?}"), fields);
    let event: E = event.into_event();
    assert_hidden(&format!("{event:?}"), fields);

    let capture = Capture::default();
    let writer = capture.clone();
    let log = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .finish();

    tracing::subscriber::with_default(log, || {
        let subscriber = crate::event::tracing::Subscriber::default();
        let meta: api::EndpointMeta = builder::EndpointMeta {
            timestamp: NoopClock.get_time().into_event(),
        }
        .into_event();
        publish(&subscriber, &meta, &event);
    });

    let written = capture.0.lock().unwrap();
    let output = String::from_utf8_lossy(&written);
    let line = output
        .lines()
        .find(|line| line.contains(target))
        .unwrap_or_else(|| panic!("{target} isn't in {output}"));
    assert!(line.contains("DEBUG"), "{line}");
    for field in fields {
        let hidden = format!("{field}=\"[HIDDEN]\"");
        assert!(line.contains(&hidden), "{hidden} isn't in {line}");
    }
    let bytes = credential_bytes();
    assert!(!output.contains(&bytes), "{bytes} is in {output}");
}

/// One row for each event that has a `#[snapshot("[HIDDEN]")]` field. `ConnectionInfo` has one
/// too, but it isn't an event and the tracing integration doesn't log it, so
/// `connection_info_debug_hides_credential_id` checks its two `Debug` forms.
#[test]
fn every_event_hides_its_snapshot_fields() {
    let age = core::time::Duration::from_secs(1);

    assert_event_hidden(
        "path_secret_map_entry_inserted",
        &["credential_id"],
        builder::PathSecretMapEntryInserted {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_path_secret_map_entry_inserted(meta, event),
    );
    assert_event_hidden(
        "path_secret_map_entry_ready",
        &["credential_id"],
        builder::PathSecretMapEntryReady {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_path_secret_map_entry_ready(meta, event),
    );
    assert_event_hidden(
        "path_secret_map_entry_replaced",
        &["new_credential_id", "previous_credential_id"],
        builder::PathSecretMapEntryReplaced {
            peer_address: peer_address(),
            new_credential_id: &CREDENTIAL_ID,
            previous_credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_path_secret_map_entry_replaced(meta, event),
    );
    assert_event_hidden(
        "path_secret_map_id_entry_evicted",
        &["credential_id"],
        builder::PathSecretMapIdEntryEvicted {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
            age,
        },
        |s, meta, event| s.on_path_secret_map_id_entry_evicted(meta, event),
    );
    assert_event_hidden(
        "path_secret_map_address_entry_evicted",
        &["credential_id"],
        builder::PathSecretMapAddressEntryEvicted {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
            age,
        },
        |s, meta, event| s.on_path_secret_map_address_entry_evicted(meta, event),
    );
    assert_event_hidden(
        "unknown_path_secret_packet_sent",
        &["credential_id"],
        builder::UnknownPathSecretPacketSent {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_unknown_path_secret_packet_sent(meta, event),
    );
    assert_event_hidden(
        "unknown_path_secret_packet_received",
        &["credential_id"],
        builder::UnknownPathSecretPacketReceived {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_unknown_path_secret_packet_received(meta, event),
    );
    assert_event_hidden(
        "unknown_path_secret_packet_accepted",
        &["credential_id"],
        builder::UnknownPathSecretPacketAccepted {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_unknown_path_secret_packet_accepted(meta, event),
    );
    assert_event_hidden(
        "unknown_path_secret_packet_rejected",
        &["credential_id"],
        builder::UnknownPathSecretPacketRejected {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_unknown_path_secret_packet_rejected(meta, event),
    );
    assert_event_hidden(
        "unknown_path_secret_packet_dropped",
        &["credential_id"],
        builder::UnknownPathSecretPacketDropped {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_unknown_path_secret_packet_dropped(meta, event),
    );
    assert_event_hidden(
        "key_accepted",
        &["credential_id"],
        builder::KeyAccepted {
            credential_id: &CREDENTIAL_ID,
            key_id: 7,
            gap: 2,
            forward_shift: 1,
        },
        |s, meta, event| s.on_key_accepted(meta, event),
    );
    assert_event_hidden(
        "replay_definitely_detected",
        &["credential_id"],
        builder::ReplayDefinitelyDetected {
            credential_id: &CREDENTIAL_ID,
            key_id: 7,
        },
        |s, meta, event| s.on_replay_definitely_detected(meta, event),
    );
    assert_event_hidden(
        "replay_potentially_detected",
        &["credential_id"],
        builder::ReplayPotentiallyDetected {
            credential_id: &CREDENTIAL_ID,
            key_id: 7,
            gap: 2,
        },
        |s, meta, event| s.on_replay_potentially_detected(meta, event),
    );
    assert_event_hidden(
        "replay_detected_packet_sent",
        &["credential_id"],
        builder::ReplayDetectedPacketSent {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_replay_detected_packet_sent(meta, event),
    );
    assert_event_hidden(
        "replay_detected_packet_received",
        &["credential_id"],
        builder::ReplayDetectedPacketReceived {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_replay_detected_packet_received(meta, event),
    );
    assert_event_hidden(
        "replay_detected_packet_accepted",
        &["credential_id"],
        builder::ReplayDetectedPacketAccepted {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
            key_id: 7,
        },
        |s, meta, event| s.on_replay_detected_packet_accepted(meta, event),
    );
    assert_event_hidden(
        "replay_detected_packet_rejected",
        &["credential_id"],
        builder::ReplayDetectedPacketRejected {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_replay_detected_packet_rejected(meta, event),
    );
    assert_event_hidden(
        "replay_detected_packet_dropped",
        &["credential_id"],
        builder::ReplayDetectedPacketDropped {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_replay_detected_packet_dropped(meta, event),
    );
    assert_event_hidden(
        "stale_key_packet_sent",
        &["credential_id"],
        builder::StaleKeyPacketSent {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_stale_key_packet_sent(meta, event),
    );
    assert_event_hidden(
        "stale_key_packet_received",
        &["credential_id"],
        builder::StaleKeyPacketReceived {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_stale_key_packet_received(meta, event),
    );
    assert_event_hidden(
        "stale_key_packet_accepted",
        &["credential_id"],
        builder::StaleKeyPacketAccepted {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_stale_key_packet_accepted(meta, event),
    );
    assert_event_hidden(
        "stale_key_packet_rejected",
        &["credential_id"],
        builder::StaleKeyPacketRejected {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_stale_key_packet_rejected(meta, event),
    );
    assert_event_hidden(
        "stale_key_packet_dropped",
        &["credential_id"],
        builder::StaleKeyPacketDropped {
            peer_address: peer_address(),
            credential_id: &CREDENTIAL_ID,
        },
        |s, meta, event| s.on_stale_key_packet_dropped(meta, event),
    );
    assert_event_hidden(
        "path_secret_map_id_cache_accessed",
        &["credential_id"],
        builder::PathSecretMapIdCacheAccessed {
            credential_id: &CREDENTIAL_ID,
            hit: true,
        },
        |s, meta, event| s.on_path_secret_map_id_cache_accessed(meta, event),
    );
    assert_event_hidden(
        "path_secret_map_id_cache_accessed_hit",
        &["credential_id"],
        builder::PathSecretMapIdCacheAccessedHit {
            credential_id: &CREDENTIAL_ID,
            age,
        },
        |s, meta, event| s.on_path_secret_map_id_cache_accessed_hit(meta, event),
    );
}
