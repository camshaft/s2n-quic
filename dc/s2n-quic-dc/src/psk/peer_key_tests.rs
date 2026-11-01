// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Handshakes between endpoints whose path secret maps accept only the peer keys they list.

use crate::{
    path::secret::{map::PeerKey, stateless_reset::Signer, HandshakeKind, Map},
    psk::{client, server},
    testing::{init_tracing, server_name, NoopSubscriber},
};
use s2n_quic::provider::tls::{default as tls, Provider};
use s2n_quic_core::{crypto::tls::testing::certificates, time::StdClock};
use s2n_tls::callbacks::VerifyHostNameCallback;
use std::time::Duration;

/// The certificate and private key an endpoint presents.
#[derive(Clone, Copy, Debug)]
enum Identity {
    /// cert.pem, with a P-256 key.
    A,
    /// cert_pkcs1.pem, with an RSA key.
    B,
}

impl Identity {
    fn certificate(self) -> &'static str {
        match self {
            Self::A => certificates::CERT_PEM,
            Self::B => certificates::CERT_PKCS1_PEM,
        }
    }

    fn private_key(self) -> &'static str {
        match self {
            Self::A => certificates::KEY_PEM,
            Self::B => certificates::KEY_PKCS1_PEM,
        }
    }
}

/// The key that `Identity::A` presents.
fn key_a() -> PeerKey {
    PeerKey::from_certificate_der(certificates::CERT_DER).unwrap()
}

struct AnyHostName;

impl VerifyHostNameCallback for AnyHostName {
    fn verify_host_name(&self, _host_name: &str) -> bool {
        true
    }
}

/// A TLS provider whose server asks for the client's certificate, and whose endpoints trust both
/// identities' certificates, so that only a map's list of keys tells the two identities apart.
#[derive(Clone, Copy)]
struct Tls(Identity);

impl Provider for Tls {
    type Server = tls::Server;
    type Client = tls::Client;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn start_server(self) -> Result<Self::Server, Self::Error> {
        let server = tls::Server::builder()
            .with_application_protocols(["h3"].iter())?
            .with_certificate(self.0.certificate(), self.0.private_key())?
            .with_empty_trust_store()?
            .with_trusted_certificate(certificates::CERT_PEM)?
            .with_trusted_certificate(certificates::CERT_PKCS1_PEM)?
            .with_client_authentication()?
            .with_verify_host_name_callback(AnyHostName)?
            .build()?;
        Ok(server)
    }

    fn start_client(self) -> Result<Self::Client, Self::Error> {
        let client = tls::Client::builder()
            .with_application_protocols(["h3"].iter())?
            .with_certificate(certificates::CERT_PEM)?
            .with_certificate(certificates::CERT_PKCS1_PEM)?
            .with_client_identity(self.0.certificate(), self.0.private_key())?
            .build()?;
        Ok(client)
    }
}

/// A map without a list of keys, which accepts each peer its TLS provider accepts.
fn new_map() -> Map {
    Map::builder()
        .with_signer(Signer::new(b"default"))
        .with_capacity(1_000)
        .with_clock(StdClock::default())
        .with_subscriber(NoopSubscriber {})
        .build()
        .unwrap()
}

/// A map that accepts only the peers whose key `keys` lists.
fn map(keys: impl IntoIterator<Item = PeerKey>) -> Map {
    let map = new_map();
    map.set_peer_keys(keys);
    map
}

async fn start_server(map: &Map, identity: Identity) -> server::Provider {
    server::Provider::builder()
        .start(
            "127.0.0.1:0".parse().unwrap(),
            Tls(identity),
            NoopSubscriber {},
            map.clone(),
        )
        .await
        .unwrap()
}

/// Each client has a handshake queue of its own, so a handshake it makes is never one that
/// another client's earlier handshake with the same server answers.
fn start_client(map: &Map, identity: Identity) -> client::Provider {
    client::Provider::builder()
        .with_success_jitter(Duration::ZERO)
        .start(
            "0.0.0.0:0".parse().unwrap(),
            map.clone(),
            Tls(identity),
            NoopSubscriber {},
            server_name(),
        )
        .unwrap()
}

async fn handshake(
    client: &client::Provider,
    server: &server::Provider,
) -> std::io::Result<HandshakeKind> {
    client
        .handshake_with(server.local_addr(), server_name())
        .await
}

#[tokio::test]
async fn a_paired_key_connects() {
    init_tracing();
    let (server_map, client_map) = (map([key_a()]), map([key_a()]));
    let server = start_server(&server_map, Identity::A).await;
    let client = start_client(&client_map, Identity::A);

    assert_eq!(
        handshake(&client, &server).await.unwrap(),
        HandshakeKind::Fresh
    );

    // The client caches the path secret with the key the server presented.
    let entry = client_map.get_raw(server.local_addr()).unwrap();
    assert_eq!(entry.peer_key(), Some(&key_a()));
    assert_eq!(
        handshake(&client, &server).await.unwrap(),
        HandshakeKind::Cached
    );
}

#[tokio::test]
async fn an_unpaired_client_key_is_refused_by_the_server() {
    init_tracing();
    let client_map = map([key_a()]);

    // A server without a list accepts the client's key B, which its TLS provider trusts.
    let open_server = start_server(&new_map(), Identity::A).await;
    let open_client = start_client(&client_map, Identity::B);
    handshake(&open_client, &open_server).await.unwrap();

    // A server whose list doesn't hold B refuses it, and caches nothing.
    let server_map = map([key_a()]);
    let server = start_server(&server_map, Identity::A).await;
    let client = start_client(&client_map, Identity::B);
    assert!(handshake(&client, &server).await.is_err());
    assert!(!client_map.contains(&server.local_addr()));
    assert_eq!(server_map.secrets_len(), 0);
}

#[tokio::test]
async fn an_unpaired_server_key_is_refused_by_the_client() {
    init_tracing();
    let server = start_server(&map([key_a()]), Identity::B).await;

    // A client without a list accepts the server's key B, which its TLS provider trusts.
    let open_client = start_client(&new_map(), Identity::A);
    handshake(&open_client, &server).await.unwrap();

    // A client whose list doesn't hold B refuses it, and caches nothing.
    let client_map = map([key_a()]);
    let client = start_client(&client_map, Identity::A);
    assert!(handshake(&client, &server).await.is_err());
    assert!(!client_map.contains(&server.local_addr()));
    assert_eq!(client_map.secrets_len(), 0);
}

#[tokio::test]
async fn a_changed_key_is_refused_on_both_sides() {
    init_tracing();
    let (server_map, client_map) = (map([key_a()]), map([key_a()]));
    let server = start_server(&server_map, Identity::A).await;
    let paired = start_client(&client_map, Identity::A);
    handshake(&paired, &server).await.unwrap();

    // The server refuses the client once the client's key changes from A to B.
    let changed_client = start_client(&client_map, Identity::B);
    assert!(handshake(&changed_client, &server).await.is_err());

    // The client refuses the server once the server's key changes from A to B.
    let changed_server = start_server(&server_map, Identity::B).await;
    let client = start_client(&client_map, Identity::A);
    assert!(handshake(&client, &changed_server).await.is_err());
    assert!(!client_map.contains(&changed_server.local_addr()));
}

#[tokio::test]
async fn a_removed_key_is_refused_by_the_client() {
    init_tracing();
    let (server_map, client_map) = (map([key_a()]), map([key_a()]));
    let server = start_server(&server_map, Identity::A).await;
    let peer = server.local_addr();

    let paired = start_client(&client_map, Identity::A);
    handshake(&paired, &server).await.unwrap();
    assert!(client_map.contains(&peer));

    // Unpairing the server evicts the client's cached path secret with it ...
    assert!(client_map.remove_peer_key(&key_a()));
    assert!(!client_map.contains(&peer));
    assert_eq!(client_map.secrets_len(), 0);

    // ... and the client refuses the server's next handshake.
    let unpaired = start_client(&client_map, Identity::A);
    assert!(handshake(&unpaired, &server).await.is_err());
    assert!(!client_map.contains(&peer));

    // Pairing the server again accepts it again.
    assert!(client_map.insert_peer_key(key_a()));
    let repaired = start_client(&client_map, Identity::A);
    assert_eq!(
        handshake(&repaired, &server).await.unwrap(),
        HandshakeKind::Fresh
    );
}

#[tokio::test]
async fn a_removed_key_is_refused_by_the_server() {
    init_tracing();
    let (server_map, client_map) = (map([key_a()]), map([key_a()]));
    let server = start_server(&server_map, Identity::A).await;

    let paired = start_client(&client_map, Identity::A);
    handshake(&paired, &server).await.unwrap();

    // Unpairing the client evicts the server's cached path secrets with it, so the server answers
    // a packet on the client's cached path secret as a packet on an unknown path secret ...
    assert!(server_map.remove_peer_key(&key_a()));
    assert_eq!(server_map.secrets_len(), 0);
    assert_eq!(server_map.peers_len(), 0);

    // ... and the server refuses the client's next handshake.
    let unpaired = start_client(&map([key_a()]), Identity::A);
    assert!(handshake(&unpaired, &server).await.is_err());
    assert_eq!(server_map.secrets_len(), 0);
}
