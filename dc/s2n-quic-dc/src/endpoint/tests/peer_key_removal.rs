// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Streams open on a path secret whose peer's key is removed from the list of accepted keys.
//!
//! Removing a key evicts each cached path secret of a peer with that key, and revokes it: each
//! stream open on it ends on the endpoint that removed the key, that endpoint refuses each later
//! packet on the path secret as one on an unknown path secret, and the halves it drops reset the
//! peer's halves, so the stream ends on both sides.

use crate::{
    endpoint::error::Error,
    path::secret::map::PeerKey,
    stream::endpoint::testing::sim::{self, SERVER_PORT},
    testing::ext::*,
};
use bach::time::timeout;
use bytes::{Bytes, BytesMut};
use s2n_quic_core::varint::VarInt;
use std::{io, time::Duration};

const ACCEPTOR_ID: VarInt = VarInt::from_u32(1);

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

/// A stream opened between two paired endpoints ends on both sides once the server removes the
/// client's key, and nothing the client writes after that goes through it.
///
/// 1. The client writes "hello", which the server reads.
/// 2. The server removes the client's key. Its reader and its writer end at once with
///    `PeerKeyRemoved`, and it holds both for a second.
/// 3. The client writes "again" after the removal. The server refuses the packet as one on an
///    unknown path secret, so the client's writer ends with "path secret rejected by peer" while
///    the server still holds its halves.
/// 4. The server drops its halves, whose resets end the client's reader with `PeerKeyRemoved`.
#[test]
fn removing_a_peer_key_ends_its_open_streams_on_both_sides() {
    crate::testing::sim(|| {
        async move {
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

            // Hold both halves while the client writes on the revoked path secret, so the
            // client's writer can only end on the server's refusal of that packet.
            Duration::from_secs(1).sleep().await;

            // Dropping the halves resets the client's.
            drop(reader);
            drop(writer);
        }
        .group("server")
        .spawn();

        async move {
            let mut client = sim::Client::new();
            // Let the server group bind its endpoint, as `connect` does.
            bach::task::yield_now().await;

            // Pair the two endpoints: each lists the other's key, and the path secrets record
            // them, as a handshake between the two would.
            let server_addr =
                bach::net::try_lookup(("server", SERVER_PORT)).expect("server address");
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

            // Write again once the server has removed the client's key.
            Duration::from_millis(100).sleep().await;
            let mut again = Bytes::from_static(b"again");
            writer
                .write_all_from(&mut again)
                .await
                .expect("the write is queued");

            // The server refuses the packet, so the write fails while the server still holds
            // its halves.
            let err = timeout(Duration::from_millis(500), writer.peer_cancellation())
                .await
                .expect("the client's writer ends before the server drops its halves");
            assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused, "{err}");

            // The server's dropped writer resets the client's reader, which got no data.
            let mut buf = BytesMut::with_capacity(16);
            let err = timeout(Duration::from_secs(5), reader.read_into(&mut buf))
                .await
                .expect("the client's reader ends")
                .expect_err("a read after the removal");
            assert_peer_key_removed(&err);
            assert!(buf.is_empty());
        }
        .group("client")
        .primary()
        .spawn();
    });
}
