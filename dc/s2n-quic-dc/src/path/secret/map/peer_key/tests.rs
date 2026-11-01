// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::PeerKey;
use crate::{
    credentials::{Credentials, Id, KeyId},
    path::secret::{
        map::{
            store::{ControlResponse, Store as _},
            Entry,
        },
        stateless_reset::Signer,
        Map,
    },
    testing::NoopSubscriber,
};
use s2n_quic_core::{crypto::tls::testing::certificates, endpoint, time::StdClock};
use std::net::SocketAddr;

/// The DER encoding of the `id-ecPublicKey` algorithm identifier's object identifier.
const EC_PUBLIC_KEY_OID: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];

fn key(name: &str) -> PeerKey {
    PeerKey::from_spki_der(name.as_bytes())
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn new_map() -> Map {
    Map::builder()
        .with_signer(Signer::new(b"default"))
        .with_capacity(100)
        .with_clock(StdClock::default())
        .with_subscriber(NoopSubscriber {})
        .build()
        .unwrap()
}

/// Caches a path secret with `peer`, as a handshake of `endpoint_type` with a peer that presented
/// `key` would, and returns its ID.
fn insert(
    map: &Map,
    peer: SocketAddr,
    endpoint_type: endpoint::Type,
    generation: u64,
    key: Option<PeerKey>,
) -> Id {
    let entry = Entry::builder(peer)
        .endpoint_type(endpoint_type)
        .generation(generation)
        .peer_key(key)
        .build(None);
    let id = *entry.id();
    map.store.test_insert(entry);
    id
}

/// Opens a packet on `credentials`, as the receive path does, and returns whether the map opened
/// it and the control packet the map answered it with.
fn open(map: &Map, credentials: &Credentials) -> (bool, Vec<u8>) {
    let mut out = Vec::new();
    let control = ControlResponse::ReturnBuffer { out: &mut out };
    let opened = map
        .opener_for_credentials(credentials, None, control)
        .is_some();
    (opened, out)
}

#[test]
fn a_certificate_key_is_its_subject_public_key_info() {
    let key = PeerKey::from_certificate_der(certificates::CERT_DER).unwrap();
    let spki = key.as_spki_der();

    // The key is a DER SEQUENCE that the certificate holds, and cert.pem holds a P-256 key.
    assert_eq!(spki[0], 0x30);
    assert!(certificates::CERT_DER
        .windows(spki.len())
        .any(|window| window == spki));
    assert!(spki
        .windows(EC_PUBLIC_KEY_OID.len())
        .any(|window| window == EC_PUBLIC_KEY_OID));

    assert_eq!(
        PeerKey::from_certificate_der(certificates::CERT_DER),
        Some(key)
    );
}

#[test]
fn bytes_that_arent_one_certificate_have_no_key() {
    let der = certificates::CERT_DER;

    assert_eq!(PeerKey::from_certificate_der(&[]), None);

    // A truncated certificate.
    assert_eq!(PeerKey::from_certificate_der(&der[..der.len() - 1]), None);

    // A certificate followed by another byte.
    let mut trailing = der.to_vec();
    trailing.push(0);
    assert_eq!(PeerKey::from_certificate_der(&trailing), None);

    // A SET in place of the certificate's SEQUENCE.
    let mut set = der.to_vec();
    set[0] = 0x31;
    assert_eq!(PeerKey::from_certificate_der(&set), None);

    // A length in the long form where the short form holds it isn't DER.
    assert_eq!(
        PeerKey::from_certificate_der(&[0x30, 0x81, 0x01, 0x00]),
        None
    );

    // A key alone isn't a certificate.
    let key = PeerKey::from_certificate_der(der).unwrap();
    assert_eq!(PeerKey::from_certificate_der(key.as_spki_der()), None);
}

#[test]
fn removing_a_key_evicts_its_cached_path_secrets() {
    let map = new_map();
    let (a, b) = (key("a"), key("b"));
    map.set_peer_keys([a.clone(), b.clone()]);

    // Path secrets with the peer that has key `a`, one from a handshake as the client and one as
    // the server, and one with the peer that has key `b`.
    let client = insert(&map, addr(4001), endpoint::Type::Client, 0, Some(a.clone()));
    let server = insert(&map, addr(4002), endpoint::Type::Server, 0, Some(a.clone()));
    let other = insert(&map, addr(4003), endpoint::Type::Client, 0, Some(b.clone()));
    for id in [&client, &server, &other] {
        assert!(map.get_by_id(id).is_some());
    }

    assert!(map.remove_peer_key(&a));

    // Both path secrets of `a` are gone, by ID and by address, so a packet on either is refused
    // as one of an unknown path secret; the path secret of `b` stays.
    assert!(map.get_by_id(&client).is_none());
    assert!(map.get_by_id(&server).is_none());
    assert!(!map.contains(&addr(4001)));
    assert!(!map.contains(&addr(4002)));
    assert!(map.get_by_id(&other).is_some());
    assert!(map.contains(&addr(4003)));

    // The list doesn't hold `a` any longer.
    assert!(!map.remove_peer_key(&a));
}

#[test]
fn a_packet_on_a_removed_key_path_secret_is_refused() {
    let map = new_map();
    let a = key("a");
    map.set_peer_keys([a.clone()]);
    let id = insert(&map, addr(4001), endpoint::Type::Server, 0, Some(a.clone()));
    let credentials = Credentials {
        id,
        key_id: KeyId::ZERO,
    };

    // While the key is listed, the map opens a packet on the path secret.
    let (opened, control) = open(&map, &credentials);
    assert!(opened);
    assert!(control.is_empty());

    // Once the key is removed, the map refuses the packet and answers it with an unknown path
    // secret packet.
    assert!(map.remove_peer_key(&a));
    let (opened, control) = open(&map, &credentials);
    assert!(!opened);
    assert!(!control.is_empty());
}

#[test]
fn a_path_secret_of_a_key_the_list_doesnt_hold_isnt_cached() {
    let map = new_map();
    let a = key("a");
    map.set_peer_keys([a.clone()]);

    // A handshake that ends with a key the list doesn't hold, or with no key, isn't cached.
    let unlisted = insert(&map, addr(4001), endpoint::Type::Client, 0, Some(key("b")));
    let missing = insert(&map, addr(4002), endpoint::Type::Server, 0, None);
    assert!(map.get_by_id(&unlisted).is_none());
    assert!(map.get_by_id(&missing).is_none());
    assert!(!map.contains(&addr(4001)));
    assert!(!map.contains(&addr(4002)));

    // A handshake of a removed key that ends after its removal isn't cached either.
    assert!(map.remove_peer_key(&a));
    let removed = insert(&map, addr(4003), endpoint::Type::Client, 0, Some(a.clone()));
    assert!(map.get_by_id(&removed).is_none());

    // Once the key is listed again, the peer's next path secret is cached.
    assert!(map.insert_peer_key(a.clone()));
    assert!(!map.insert_peer_key(a.clone()));
    let paired = insert(&map, addr(4003), endpoint::Type::Client, 1, Some(a));
    assert!(map.get_by_id(&paired).is_some());
    assert!(map.contains(&addr(4003)));
}

#[test]
fn setting_the_list_evicts_the_path_secrets_it_doesnt_hold() {
    let map = new_map();

    // A map without a list caches every path secret, with a key or without one.
    let unkeyed = insert(&map, addr(4001), endpoint::Type::Client, 0, None);
    let a = insert(&map, addr(4002), endpoint::Type::Client, 0, Some(key("a")));
    let b = insert(&map, addr(4003), endpoint::Type::Server, 0, Some(key("b")));
    for id in [&unkeyed, &a, &b] {
        assert!(map.get_by_id(id).is_some());
    }

    map.set_peer_keys([key("a")]);
    assert!(map.get_by_id(&unkeyed).is_none());
    assert!(map.get_by_id(&a).is_some());
    assert!(map.get_by_id(&b).is_none());

    // An empty list doesn't accept any peer.
    map.set_peer_keys(Vec::<PeerKey>::new());
    assert!(map.get_by_id(&a).is_none());
    assert!(!map.contains(&addr(4002)));
}

#[test]
fn a_map_without_a_list_ignores_key_edits() {
    let map = new_map();

    assert!(!map.insert_peer_key(key("a")));
    assert!(!map.remove_peer_key(&key("a")));

    // The map still caches the path secret of any key.
    let id = insert(&map, addr(4001), endpoint::Type::Client, 0, Some(key("b")));
    assert!(map.get_by_id(&id).is_some());
}
