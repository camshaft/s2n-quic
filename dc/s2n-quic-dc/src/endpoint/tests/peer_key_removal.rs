// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Streams open on a path secret whose peer's key is removed from the list of accepted keys.
//!
//! Removing a key evicts each cached path secret of a peer with that key, and revokes it: each
//! stream open on it ends on the endpoint that removed the key, and that endpoint refuses each
//! later packet on the path secret as one on an unknown path secret. It also sends the peer an
//! unknown path secret packet for the path secret, which ends the peer's halves of those streams
//! at once, whether or not the peer sends anything, and whether or not the endpoint that removed
//! the key drops its own halves.

use crate::{
    endpoint::error::Error,
    path::secret::map::PeerKey,
    stream::endpoint::testing::sim::{self, SimEndpointConfig, SERVER_PORT},
    testing::ext::*,
};
use bach::time::timeout;
use bytes::{Bytes, BytesMut};
use s2n_quic_core::varint::VarInt;
use std::{io, time::Duration};

const ACCEPTOR_ID: VarInt = VarInt::from_u32(1);

/// How long the server holds its errored halves after it removes the client's key: longer than
/// each wait of the client, so each of the client's halves can end only on the server's unknown
/// path secret packet, and not on the resets of the server's dropped halves.
const SERVER_HOLD: Duration = Duration::from_secs(10);

/// How long the client waits for each of its halves to end after the removal.
const CLIENT_WAIT: Duration = Duration::from_secs(1);

fn client_key() -> PeerKey {
    PeerKey::from_spki_der(&b"client key"[..])
}

fn server_key() -> PeerKey {
    PeerKey::from_spki_der(&b"server key"[..])
}

/// The stream error an `io::Error` carries, where it carries one.
fn stream_error(err: &io::Error) -> Option<Error> {
    err.get_ref()?.downcast_ref::<Error>().copied()
}

#[track_caller]
fn assert_peer_key_removed(err: &io::Error) {
    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset, "{err}");
    assert_eq!(stream_error(err), Some(Error::PeerKeyRemoved), "{err}");
}

#[track_caller]
fn assert_path_secret_rejected(err: &io::Error) {
    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset, "{err}");
    assert_eq!(stream_error(err), Some(Error::PathSecretRejected), "{err}");
}

/// The client's writer ends on the reset of its queue, or, where its "hello" wasn't acknowledged
/// yet, on the failure of those frames, which it answers as "path secret rejected by peer".
#[track_caller]
fn assert_writer_rejected(err: &io::Error) {
    if err.kind() == io::ErrorKind::ConnectionRefused {
        assert!(err.to_string().contains("path secret rejected by peer"), "{err}");
        return;
    }
    assert_path_secret_rejected(err);
}

/// The server reads "hello", removes the client's key, checks that both of its halves end at once
/// with `PeerKeyRemoved`, and holds them for [`SERVER_HOLD`], as an application that doesn't
/// drop its halves on an error would.
async fn server() {
    let server = sim::Server::new();
    let mut acceptor = server
        .register_acceptor_channel(ACCEPTOR_ID, 8)
        .expect("acceptor registration");
    let server_map = sim::lookup_sim_map(server.data_addr()).expect("server map");

    let stream = acceptor.recv().await.expect("the client's stream");
    let (mut reader, mut writer) = stream.into_split();

    // The stream works while the client is paired.
    let mut buf = BytesMut::with_capacity(16);
    while buf.len() < 5 {
        let read = reader
            .read_into(&mut buf)
            .await
            .expect("a read before the removal");
        assert_ne!(read, 0, "the client didn't send FIN");
    }
    assert_eq!(&buf[..], b"hello");

    // Unpair the client: this evicts and revokes the stream's path secret.
    assert!(server_map.remove_peer_key(&client_key()));
    assert_eq!(server_map.secrets_len(), 0);

    // Both of the server's halves end at once, and no more data comes through the reader.
    let err = reader
        .read_into(&mut buf)
        .await
        .expect_err("a read after the removal");
    assert_peer_key_removed(&err);
    assert_eq!(&buf[..], b"hello");
    let err = timeout(Duration::from_secs(1), writer.peer_cancellation())
        .await
        .expect("the server's writer ends at once");
    assert_peer_key_removed(&err);

    SERVER_HOLD.sleep().await;
    drop(reader);
    drop(writer);
}

/// The client, on an endpoint whose map evicts a path secret on a validated unknown path secret
/// packet where `evict_on_unknown_path_secret` is true, writes "hello" and then sends nothing,
/// and both of its halves end within [`CLIENT_WAIT`] while the server still holds its own.
async fn client(evict_on_unknown_path_secret: bool) {
    let mut client = SimEndpointConfig::default()
        .evict_on_unknown_path_secret(evict_on_unknown_path_secret)
        .client();
    // Let the server group bind its endpoint, as `connect` does.
    bach::task::yield_now().await;

    // Pair the two endpoints: each lists the other's key, and the path secrets record them, as a
    // handshake between the two would.
    let server_addr = bach::net::try_lookup(("server", SERVER_PORT)).expect("server address");
    let client_map = sim::lookup_sim_map(client.data_addr()).expect("client map");
    let server_map = sim::lookup_sim_map(server_addr).expect("server map");
    client_map.set_peer_keys([server_key()]);
    server_map.set_peer_keys([client_key()]);
    sim::insert_fake_path_pair_with_peer_keys(
        &client_map,
        client.data_addr(),
        Some(server_key()),
        &server_map,
        server_addr,
        Some(client_key()),
    );
    assert_eq!(client_map.secrets_len(), 1);
    assert_eq!(server_map.secrets_len(), 1);

    let stream = client
        .connect("server:0", ACCEPTOR_ID)
        .await
        .expect("connect");
    let (mut reader, mut writer) = stream.into_split();

    let mut hello = Bytes::from_static(b"hello");
    writer
        .write_all_from(&mut hello)
        .await
        .expect("a write before the removal");

    // The client doesn't send anything more, so its reader can end only on the server's unknown
    // path secret packet.
    let mut buf = BytesMut::with_capacity(16);
    let err = timeout(CLIENT_WAIT, reader.read_into(&mut buf))
        .await
        .expect("the client's reader ends while the server holds its halves")
        .expect_err("a read after the removal");
    assert_path_secret_rejected(&err);
    assert!(buf.is_empty());

    let err = timeout(CLIENT_WAIT, writer.peer_cancellation())
        .await
        .expect("the client's writer ends while the server holds its halves");
    assert_writer_rejected(&err);

    // A map that evicts on an unknown path secret no longer holds the path secret; the streams
    // ended all the same.
    let secrets = if evict_on_unknown_path_secret { 0 } else { 1 };
    assert_eq!(client_map.secrets_len(), secrets);
}

fn run(evict_on_unknown_path_secret: bool) {
    crate::testing::sim(move || {
        server().group("server").spawn();
        client(evict_on_unknown_path_secret)
            .group("client")
            .primary()
            .spawn();
    });
}

/// Case 1: a peer that only reads, while the endpoint that removed its key keeps its errored
/// halves open.
#[test]
fn removing_a_peer_key_ends_the_peers_halves_while_the_remover_holds_its_own() {
    run(false);
}

/// Case 2: a peer whose map evicts the path secret as it validates the unknown path secret packet,
/// and so refuses the resets of the other endpoint's dropped halves.
#[test]
fn removing_a_peer_key_ends_the_halves_of_a_peer_that_evicts_on_unknown_path_secrets() {
    run(true);
}
