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
