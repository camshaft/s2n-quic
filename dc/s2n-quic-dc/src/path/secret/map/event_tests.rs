// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    event,
    packet::{secret_control as control, WireVersion},
    path::secret::{map::Entry, stateless_reset, Map},
};
use s2n_codec::{DecoderBufferMut, EncoderBuffer};
use s2n_quic_core::time::NoopClock;
use std::sync::Arc;

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

fn map_sub(capacity: usize, sub: Arc<Subscriber>) -> Map {
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
        packet!(
            control::UnknownPathSecret {
                wire_version: WireVersion::ZERO,
                credential_id,
                queue_id: None,
            },
            &stateless_reset
        );

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
fn control_packets_at_the_key_id_limit() {
    use crate::path::secret::sender::MAX_KEY_ID;
    use s2n_quic_core::varint::{VarInt, MAX_VARINT_VALUE};
    use std::sync::atomic::{AtomicU64, Ordering};

    crate::testing::init_tracing();

    // The test counts the events, so the subscriber doesn't keep a snapshot.
    let subscriber = event::testing::Subscriber::no_snapshot();
    let sub: Arc<Subscriber> = Arc::new((subscriber, Default::default()));
    let client = map_sub(10, sub.clone());
    let server = map_sub(10, sub.clone());

    let client_addr = "127.0.0.1:1234".parse().unwrap();
    let server_addr = "127.0.0.1:5678".parse().unwrap();

    let ids = client.test_insert_pair(
        Entry::builder(server_addr).local(client_addr),
        &server,
        Entry::builder(client_addr).local(server_addr),
    );

    let server_entry = server.store.get_by_id_untracked(&ids.peer).unwrap().clone();
    let client_entry = client
        .store
        .get_by_id_untracked(&ids.local)
        .unwrap()
        .clone();

    // The server holds the path secret, so the client authenticates each packet it sends.
    let secret = server_entry.control_sealer();
    let credential_id = *server_entry.id();

    let mut out = [0; 128];

    macro_rules! packet {
        ($expr:expr) => {
            let v = $expr;
            let len = v.encode(EncoderBuffer::new(&mut out), &secret);
            let buf = &mut out[..len];
            let (pkt, _) = control::Packet::decode(DecoderBufferMut::new(buf)).unwrap();
            client.handle_control_packet(&pkt, &server_addr);
        };
    }

    let stale_key = |min_key_id: u64| control::StaleKey {
        wire_version: WireVersion::ZERO,
        credential_id,
        min_key_id: VarInt::new(min_key_id).unwrap(),
        sender_id: None,
    };
    let replay_detected = |rejected_key_id: u64| control::ReplayDetected {
        wire_version: WireVersion::ZERO,
        credential_id,
        rejected_key_id: VarInt::new(rejected_key_id).unwrap(),
        sender_id: None,
    };

    let events = &sub.0;
    let count = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    let sender = client_entry.sender();
    assert_eq!(*sender.next_key_id(), 0);

    // A stale key packet whose minimum key id is past the last one the sender can use is
    // refused, and the sender goes on from its current key id.
    for min_key_id in [MAX_KEY_ID + 1, MAX_VARINT_VALUE] {
        packet!(stale_key(min_key_id));
    }
    assert_eq!(count(&events.stale_key_packet_accepted), 0);
    assert_eq!(count(&events.stale_key_packet_rejected), 2);
    assert_eq!(*sender.next_key_id(), 1);

    // A replay detected packet that rejects the last key id, or one past it, is refused and
    // doesn't request a handshake.
    for rejected_key_id in [MAX_KEY_ID, MAX_KEY_ID + 1, MAX_VARINT_VALUE] {
        packet!(replay_detected(rejected_key_id));
    }
    assert_eq!(count(&events.replay_detected_packet_accepted), 0);
    assert_eq!(count(&events.replay_detected_packet_rejected), 3);
    assert_eq!(count(&events.path_secret_map_background_handshake_requested), 0);
    assert_eq!(*sender.next_key_id(), 2);

    // The last key id itself is accepted, and the sender sends at it.
    packet!(stale_key(MAX_KEY_ID));
    assert_eq!(count(&events.stale_key_packet_accepted), 1);
    assert_eq!(*sender.next_key_id(), MAX_KEY_ID);

    // A replay detected packet that rejects the key id before the last one moves the sender to
    // the last one, and requests a handshake.
    sender.reset_counter();
    packet!(replay_detected(MAX_KEY_ID - 1));
    assert_eq!(count(&events.replay_detected_packet_accepted), 1);
    assert_eq!(count(&events.path_secret_map_background_handshake_requested), 1);
    assert_eq!(*sender.next_key_id(), MAX_KEY_ID);
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
