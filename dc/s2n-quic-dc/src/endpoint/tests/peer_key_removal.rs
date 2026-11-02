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
//!
//! Each reader and writer on the revoked path secret also sends the peer a reset with
//! `PEER_KEY_REMOVED` as it hands the error to its application, which ends the peer's halves
//! where that packet is lost. The endpoint never sees those resets acknowledged, since it refuses
//! the peer's acknowledgements, and stops sending them again once the peer answers after them.

use crate::{
    endpoint::error::Error,
    packet::secret_control,
    path::secret::map::PeerKey,
    stream::endpoint::testing::sim::{self, MonitorHostAddr, SimEndpointConfig, SERVER_PORT},
    testing::ext::*,
};
use bach::{net::monitor, time::timeout};
use bytes::{Bytes, BytesMut};
use s2n_codec::DecoderBufferMut;
use s2n_quic_core::varint::VarInt;
use std::{
    io,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

const ACCEPTOR_ID: VarInt = VarInt::from_u32(1);

/// How long the server holds its errored halves after it removes the client's key: longer than
/// each wait of the client, so each of the client's halves ends on the server's unknown path
/// secret packet, or on the resets the server's halves send as they hand the server their errors,
/// and not on the server's drop.
const SERVER_HOLD: Duration = Duration::from_secs(10);

/// How long the client waits for each of its halves to end after the removal.
const CLIENT_WAIT: Duration = Duration::from_secs(1);

/// How long the client waits, once its halves ended, before it counts the server's packets: the
/// server's next probe, which drops its resets, comes well within it.
const RESENDS_SETTLE: Duration = Duration::from_secs(1);

/// How long the client counts the server's packets: it ends before the server drops its halves.
const RESENDS_WINDOW: Duration = Duration::from_secs(6);

fn client_key() -> PeerKey {
    PeerKey::from_spki_der(&b"client key"[..])
}

fn server_key() -> PeerKey {
    PeerKey::from_spki_der(&b"server key"[..])
}

/// One run of the test: what the client's map does with a validated unknown path secret packet,
/// what the network does with unknown path secret packets, and what the client checks.
#[derive(Clone, Copy, Default)]
struct Case {
    /// Whether the client's map evicts a path secret on a validated unknown path secret packet.
    evict_on_unknown_path_secret: bool,
    /// Whether the network drops each unknown path secret packet, the server's revoke packet
    /// among them, so that only the resets of the server's halves can end the client's halves.
    drop_unknown_path_secrets: bool,
    /// Whether the client counts the server's packets once its halves ended, and checks that the
    /// server doesn't send its resets again.
    count_resends: bool,
}

/// The server's packets the client counts once its halves ended (see [`Case::count_resends`]).
#[derive(Default)]
struct Resends {
    counting: AtomicBool,
    packets: AtomicUsize,
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

/// The client's reader ends on the server's unknown path secret packet, with
/// `PathSecretRejected`, or on the reset of the server's writer, with `PeerKeyRemoved`: on the
/// reset alone where the network drops the unknown path secret packets.
#[track_caller]
fn assert_reader_ended(err: &io::Error, case: Case) {
    if case.drop_unknown_path_secrets {
        assert_peer_key_removed(err);
        return;
    }
    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset, "{err}");
    let ended = [Error::PathSecretRejected, Error::PeerKeyRemoved];
    let error = stream_error(err);
    assert!(error.is_some_and(|error| ended.contains(&error)), "{err}");
}

/// The client's writer ends as its reader does, or, where the unknown path secret packet came
/// through while its "hello" wasn't acknowledged yet, on the failure of those frames, which it
/// answers as "path secret rejected by peer".
#[track_caller]
fn assert_writer_ended(err: &io::Error, case: Case) {
    if !case.drop_unknown_path_secrets && err.kind() == io::ErrorKind::ConnectionRefused {
        let message = err.to_string();
        assert!(message.contains("path secret rejected by peer"), "{err}");
        return;
    }
    assert_reader_ended(err, case);
}

/// Whether the packet is an unknown path secret packet, such as the one the server sends the
/// client as it revokes the path secret.
fn is_unknown_path_secret(packet: &monitor::Packet) -> bool {
    let mut payload = packet.transport.payload().to_vec();
    let buffer = DecoderBufferMut::new(&mut payload);
    let Ok((decoded, _)) = secret_control::Packet::decode(buffer) else {
        return false;
    };
    matches!(decoded, secret_control::Packet::UnknownPathSecret(_))
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
/// packet where `case` says so, writes "hello" and then sends nothing, and both of its halves end
/// within [`CLIENT_WAIT`] while the server still holds its own. Where `case` counts the server's
/// resends, it then checks that the server sends nothing for [`RESENDS_WINDOW`].
async fn client(case: Case, resends: Arc<Resends>) {
    let mut client = SimEndpointConfig::default()
        .evict_on_unknown_path_secret(case.evict_on_unknown_path_secret)
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

    // The client doesn't send anything more, so its reader can end only on what the server sends.
    let mut buf = BytesMut::with_capacity(16);
    let err = timeout(CLIENT_WAIT, reader.read_into(&mut buf))
        .await
        .expect("the client's reader ends while the server holds its halves")
        .expect_err("a read after the removal");
    assert_reader_ended(&err, case);
    assert!(buf.is_empty());

    let err = timeout(CLIENT_WAIT, writer.peer_cancellation())
        .await
        .expect("the client's writer ends while the server holds its halves");
    assert_writer_ended(&err, case);

    // A map that evicts on an unknown path secret no longer holds the path secret; the streams
    // ended all the same.
    let secrets = if case.evict_on_unknown_path_secret {
        0
    } else {
        1
    };
    assert_eq!(client_map.secrets_len(), secrets);

    if case.count_resends {
        // The client acknowledged the server's resets, and the server refused that
        // acknowledgement, as it refuses each packet on the revoked path secret. The refusal
        // stops the server from sending the resets again, so it sends nothing more while it holds
        // its halves.
        RESENDS_SETTLE.sleep().await;
        resends.counting.store(true, Ordering::Relaxed);
        RESENDS_WINDOW.sleep().await;
        let packets = resends.packets.load(Ordering::Relaxed);
        assert_eq!(packets, 0, "the server sent its resets again");
    }
}

fn run(case: Case) {
    crate::testing::sim(move || {
        let resends = Arc::new(Resends::default());
        if case.drop_unknown_path_secrets {
            let resends = resends.clone();
            let mut server_addr = MonitorHostAddr::new("server");
            monitor::on_packet_sent(move |packet| {
                if is_unknown_path_secret(packet) {
                    return monitor::Command::Drop;
                }
                let counting = resends.counting.load(Ordering::Relaxed);
                if counting && server_addr.is_packet_source(packet) {
                    resends.packets.fetch_add(1, Ordering::Relaxed);
                }
                monitor::Command::Pass
            });
        }
        server().group("server").spawn();
        client(case, resends).group("client").primary().spawn();
    });
}

/// Case 1: a peer that only reads, while the endpoint that removed its key keeps its errored
/// halves open.
#[test]
fn removing_a_peer_key_ends_the_peers_halves_while_the_remover_holds_its_own() {
    run(Case::default());
}

/// Case 2: a peer whose map evicts the path secret as it validates the unknown path secret packet,
/// and so refuses the resets of the other endpoint's halves.
#[test]
fn removing_a_peer_key_ends_the_halves_of_a_peer_that_evicts_on_unknown_path_secrets() {
    run(Case {
        evict_on_unknown_path_secret: true,
        ..Case::default()
    });
}

/// A peer that only reads, where the network drops each unknown path secret packet, the revoke's
/// among them: the resets the other endpoint's halves send as they hand it their errors end the
/// peer's halves while that endpoint holds its own.
#[test]
fn removing_a_peer_key_ends_a_quiet_peer_whose_unknown_path_secret_packets_are_lost() {
    run(Case {
        drop_unknown_path_secrets: true,
        ..Case::default()
    });
}

/// As above, and the endpoint that removed the key doesn't send its resets again once it refused
/// the peer's acknowledgement of them, where it would otherwise send them until it declares the
/// peer dead.
#[test]
fn the_remover_stops_sending_its_resets_once_the_peer_answers() {
    run(Case {
        drop_unknown_path_secrets: true,
        count_resends: true,
        ..Case::default()
    });
}
