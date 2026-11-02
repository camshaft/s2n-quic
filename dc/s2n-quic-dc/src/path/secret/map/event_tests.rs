// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    event,
    packet::{secret_control as control, WireVersion},
    path::secret::{map::Entry, stateless_reset, Map},
};
use s2n_codec::{DecoderBufferMut, EncoderBuffer};
use s2n_quic_core::time::NoopClock;
use std::sync::{Arc, Mutex};

type Subscriber = (
    event::testing::Subscriber,
    event::metrics::aggregate::Subscriber<(
        event::metrics::aggregate::probe::Registry,
        event::metrics::aggregate::probe::dynamic::Registry,
    )>,
);

#[track_caller]
fn sub() -> Arc<Subscriber> {
    crate::testing::init_tracing();

    Arc::new((event::testing::Subscriber::snapshot(), Default::default()))
}

#[track_caller]
fn map(capacity: usize) -> Map {
    map_sub(capacity, sub())
}

fn map_sub<S: event::Subscriber>(capacity: usize, sub: S) -> Map {
    let signer = stateless_reset::Signer::random();

    let map = Map::builder()
        .with_signer(signer)
        .with_capacity(capacity)
        .with_clock(NoopClock)
        .with_subscriber(sub)
        .build()
        .unwrap();

    // stop the cleaner thread to avoid any out of order events
    map.test_stop_cleaner();

    map
}

#[test]
fn init_uninit() {
    let _ = map(10);
}

#[test]
fn insert_one() {
    let map = map(10);
    map.test_insert(
        "127.0.0.1:4567".parse().unwrap(),
        s2n_quic_core::endpoint::Type::Client,
    );
}

#[test]
fn control_packets() {
    let sub = sub();
    let client = map_sub(10, sub.clone());
    let server = map_sub(10, sub);

    let client_addr = "127.0.0.1:1234".parse().unwrap();
    let server_addr = "127.0.0.1:5678".parse().unwrap();

    let ids = client.test_insert_pair(
        Entry::builder(server_addr).local(client_addr),
        &server,
        Entry::builder(client_addr).local(server_addr),
    );

    let mut out = [0; 128];

    macro_rules! packet {
        ($expr:expr, $crypto:expr) => {
            let v = $expr;
            let len = v.encode(EncoderBuffer::new(&mut out), $crypto);
            let buf = &mut out[..len];
            let (pkt, _) = control::Packet::decode(DecoderBufferMut::new(buf)).unwrap();
            client.handle_control_packet(&pkt, &server_addr);
        };
    }

    let server_entry = server.store.get_by_id_untracked(&ids.peer).unwrap().clone();
    let client_entry = client
        .store
        .get_by_id_untracked(&ids.local)
        .unwrap()
        .clone();

    let fake_secret =
        crate::path::secret::seal::control::Secret::new(&[0; 32], &aws_lc_rs::hmac::HMAC_SHA256);
    let fake_srt = [0; 16];
    let fake_id = [0; 16].into();

    // make sure control packets can't be reflected back
    let client_secret = client_entry.control_sealer();
    let client_srt = client_entry.sender().stateless_reset;

    let real_secret = server_entry.control_sealer();
    let real_id = *server_entry.id();
    let real_srt = server_entry.sender().stateless_reset;

    // try to send a series of packet types from the server to the client
    let attempts = [
        (&fake_secret, fake_srt, fake_id),
        (&fake_secret, fake_srt, real_id),
        (&client_secret, client_srt, real_id),
        (&real_secret, real_srt, real_id),
    ];

    for (secret, stateless_reset, credential_id) in attempts {
        // The client accepts an UnknownPathSecret tagged with its own entry's stateless-reset
        // token and evicts that entry: each later packet would be dropped, and the eviction
        // events' `age` would depend on the real clock that sets the entry's creation time.
        // `unknown_path_secret_evicts_client_entry_events` below checks that case's events.
        if stateless_reset != client_srt {
            packet!(
                control::UnknownPathSecret {
                    wire_version: WireVersion::ZERO,
                    credential_id,
                    queue_id: None,
                },
                &stateless_reset
            );
        }

        packet!(
            control::StaleKey {
                wire_version: WireVersion::ZERO,
                credential_id,
                min_key_id: 123u16.into(),
                sender_id: None,
            },
            secret
        );

        packet!(
            control::ReplayDetected {
                wire_version: WireVersion::ZERO,
                credential_id,
                rejected_key_id: 123u16.into(),
                sender_id: None,
            },
            secret
        );
    }
}

#[test]
fn advertised_peer_info_from_builder() {
    let payload = bytes::Bytes::from_static(b"local-peer-info");
    let map = Map::builder()
        .with_signer(stateless_reset::Signer::random())
        .with_capacity(10)
        .with_advertised_peer_info(payload.clone())
        .with_clock(NoopClock)
        .with_subscriber(crate::event::tracing::Subscriber::default())
        .build()
        .unwrap();

    map.test_stop_cleaner();

    assert_eq!(map.advertised_peer_info(), Some(payload));
}

/// An event of an UnknownPathSecret packet, or of the eviction it causes: the event's name
/// and the credential id it carries, empty for an event that doesn't carry one.
type UnknownPathSecretEvent = (&'static str, Vec<u8>);

/// Records, in the order a map emits them, the events of each UnknownPathSecret packet the
/// map handles and of the eviction it causes. It leaves out the eviction events' `age`,
/// which reads the real clock that set the entry's creation time.
#[derive(Default)]
struct UnknownPathSecretEvents(Mutex<Vec<UnknownPathSecretEvent>>);

impl UnknownPathSecretEvents {
    fn push(&self, name: &'static str, credential_id: &[u8]) {
        self.0.lock().unwrap().push((name, credential_id.to_vec()));
    }

    fn take(&self) -> Vec<UnknownPathSecretEvent> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

impl event::Subscriber for UnknownPathSecretEvents {
    type ConnectionContext = ();

    fn create_connection_context(
        &self,
        _meta: &event::api::ConnectionMeta,
        _info: &event::api::ConnectionInfo,
    ) -> Self::ConnectionContext {
    }

    fn on_unknown_path_secret_packet_received(
        &self,
        _meta: &event::api::EndpointMeta,
        packet: &event::api::UnknownPathSecretPacketReceived,
    ) {
        self.push("UnknownPathSecretPacketReceived", packet.credential_id);
    }

    fn on_unknown_path_secret_packet_accepted(
        &self,
        _meta: &event::api::EndpointMeta,
        packet: &event::api::UnknownPathSecretPacketAccepted,
    ) {
        self.push("UnknownPathSecretPacketAccepted", packet.credential_id);
    }

    fn on_unknown_path_secret_packet_rejected(
        &self,
        _meta: &event::api::EndpointMeta,
        packet: &event::api::UnknownPathSecretPacketRejected,
    ) {
        self.push("UnknownPathSecretPacketRejected", packet.credential_id);
    }

    fn on_unknown_path_secret_packet_dropped(
        &self,
        _meta: &event::api::EndpointMeta,
        packet: &event::api::UnknownPathSecretPacketDropped,
    ) {
        self.push("UnknownPathSecretPacketDropped", packet.credential_id);
    }

    fn on_path_secret_map_background_handshake_requested(
        &self,
        _meta: &event::api::EndpointMeta,
        _request: &event::api::PathSecretMapBackgroundHandshakeRequested,
    ) {
        self.push("PathSecretMapBackgroundHandshakeRequested", &[]);
    }

    fn on_path_secret_map_id_entry_evicted(
        &self,
        _meta: &event::api::EndpointMeta,
        evicted: &event::api::PathSecretMapIdEntryEvicted,
    ) {
        self.push("PathSecretMapIdEntryEvicted", evicted.credential_id);
    }

    fn on_path_secret_map_address_entry_evicted(
        &self,
        _meta: &event::api::EndpointMeta,
        evicted: &event::api::PathSecretMapAddressEntryEvicted,
    ) {
        self.push("PathSecretMapAddressEntryEvicted", evicted.credential_id);
    }
}

/// A client map with `with_evict_on_unknown_path_secret` off accepts the server's
/// UnknownPathSecret for its entry and evicts the entry: it emits the packet's events,
/// requests a handshake, and then emits the eviction events for the entry's id and address.
#[test]
fn unknown_path_secret_evicts_client_entry_events() {
    let events = Arc::new(UnknownPathSecretEvents::default());
    let client = map_sub(10, events.clone());
    let server = map_sub(10, event::testing::Subscriber::no_snapshot());

    let client_addr = "127.0.0.1:1234".parse().unwrap();
    let server_addr = "127.0.0.1:5678".parse().unwrap();

    let ids = client.test_insert_pair(
        Entry::builder(server_addr).local(client_addr),
        &server,
        Entry::builder(client_addr).local(server_addr),
    );

    // `map_sub` keeps the builder's default for the setting, and `test_insert_pair` makes the
    // client map's entry a client entry
    assert!(!client.store.should_evict_on_unknown_path_secret());
    assert!(ids.local.endpoint_type().is_client());
    let client_srt = client
        .store
        .get_by_id_untracked(&ids.local)
        .unwrap()
        .sender()
        .stateless_reset;
    assert!(events.take().is_empty());

    // the server's UnknownPathSecret for the client's entry, tagged with that entry's
    // stateless-reset token, as the server sends it once it no longer holds the entry
    let unknown = control::UnknownPathSecret {
        wire_version: WireVersion::ZERO,
        credential_id: ids.peer,
        queue_id: None,
    };
    let mut out = [0; 128];
    let len = unknown.encode(EncoderBuffer::new(&mut out), &client_srt);
    let buf = &mut out[..len];
    let (packet, _) = control::Packet::decode(DecoderBufferMut::new(buf)).unwrap();
    client.handle_control_packet(&packet, &server_addr);

    // the packet's events carry the server's credential id, the eviction events the client's
    let server_id = ids.peer.to_vec();
    let client_id = ids.local.to_vec();
    assert_eq!(
        events.take(),
        [
            ("UnknownPathSecretPacketReceived", server_id.clone()),
            ("UnknownPathSecretPacketAccepted", server_id),
            ("PathSecretMapBackgroundHandshakeRequested", Vec::new()),
            ("PathSecretMapIdEntryEvicted", client_id.clone()),
            ("PathSecretMapAddressEntryEvicted", client_id),
        ]
    );
    assert!(client.store.get_by_id_untracked(&ids.local).is_none());
}
