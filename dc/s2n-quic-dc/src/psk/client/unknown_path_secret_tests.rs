// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::{
    packet::{secret_control as control, WireVersion},
    testing::{server_name, Pair},
};
use s2n_codec::{DecoderBufferMut, EncoderBuffer};

/// A client that accepts an UnknownPathSecret for its cached entry stops using that entry: its
/// next `handshake_with_entry` handshakes with the peer again, instead of returning the entry as
/// cached.
#[tokio::test]
async fn unknown_path_secret_makes_the_next_connect_handshake_again() {
    let (client, server) = Pair::default().build().await;
    let server_addr = server.local_addr();

    // A cached entry for the server that the server doesn't hold, as after the server restarted
    // or evicted it.
    client
        .map()
        .test_insert(server_addr, s2n_quic_core::endpoint::Type::Client);
    let (cached, kind) = client
        .handshake_with_entry(server_addr, server_name())
        .await
        .unwrap();
    assert!(matches!(kind, HandshakeKind::Cached));
    let cached = cached.into_raw();

    // The server's UnknownPathSecret for that entry, as the endpoint receives it: the entry's
    // peer credential id, tagged with the stateless-reset token the entry holds for the server.
    let mut buffer = [0; control::UnknownPathSecret::MAX_PACKET_SIZE];
    let len = control::UnknownPathSecret {
        wire_version: WireVersion::ZERO,
        credential_id: cached.id().for_peer(),
        queue_id: None,
    }
    .encode(
        EncoderBuffer::new(&mut buffer),
        &cached.sender().stateless_reset,
    );
    let encoded = DecoderBufferMut::new(&mut buffer[..len]);
    let (packet, _) = control::Packet::decode(encoded).unwrap();
    client.map().handle_control_packet(&packet, &server_addr);

    assert!(!client.map().contains(&server_addr));

    let (fresh, kind) = client
        .handshake_with_entry(server_addr, server_name())
        .await
        .unwrap();
    assert!(matches!(kind, HandshakeKind::Fresh));
    assert_ne!(fresh.id(), cached.id());
}
