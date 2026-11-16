// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A server answers a frame for a stream it hasn't bound, and won't bind from that frame, with a
//! reset sent back to the frame's sender: a data frame and a message frame that don't open a
//! stream, a control frame, and a plain reset. The answer follows the packet's authentication,
//! and only a server answers, so a client doesn't answer the server's reset with another reset.

use super::*;
use s2n_codec::Encoder as _;

/// The client's queue the frames come from, which the server's reset is for.
const CLIENT_QUEUE_ID: u8 = 7;
/// The server's queue the frames are routed at, which the server hasn't bound.
const SERVER_QUEUE_ID: u8 = 0;
/// The binding the frames carry, which the server's slot never held.
const BINDING_ID: u8 = 1;

/// The queue pair of the client's frames: from the client's queue to the server's.
fn client_queue_pair() -> QueuePair {
    QueuePair {
        source_queue_id: VarInt::from_u8(CLIENT_QUEUE_ID),
        dest_queue_id: VarInt::from_u8(SERVER_QUEUE_ID),
    }
}

/// `header` and `payload`, sealed by the entry for `peer` of type `sealer_type` with
/// `packet_number` as the packet's number, into a pool-backed packet that the entry of the other
/// type opens. Returns the packet and its credentials.
fn sealed_frame(
    peer: SocketAddr,
    sealer_type: endpoint::Type,
    packet_number: u8,
    header: Header,
    payload: &[u8],
) -> (
    datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
    Credentials,
) {
    let sealer_entry = PathSecretEntry::builder(peer)
        .endpoint_type(sealer_type)
        .build(None);
    let sealer = sealer_entry.secret().application_sealer(VarInt::ZERO);
    let credentials = Credentials {
        id: sealer_entry.secret().peer_id(),
        key_id: VarInt::ZERO,
    };

    let payload_len = VarInt::new(payload.len() as u64).unwrap();
    let mut header_buf = vec![0u8; header.metadata_len(payload.len())];
    {
        let mut enc = s2n_codec::EncoderBuffer::new(&mut header_buf);
        enc.encode(&header);
        if header.has_payload_length() {
            enc.encode(&payload_len);
        }
    }

    let mut buf = vec![0u8; 65536];
    let mut header_reader = &header_buf[..];
    let mut payload_reader = payload;
    let encoded_len = datagram::encoder::encode(
        s2n_codec::EncoderBuffer::new(&mut buf),
        443,
        datagram::RoutingInfo::SenderId {
            source_sender_id: VarInt::from_u8(3),
        },
        Some(VarInt::from_u8(packet_number)),
        VarInt::try_from(header_buf.len() as u64).unwrap(),
        &mut header_reader,
        payload_len,
        &mut payload_reader,
        &sealer,
        &credentials,
    );
    assert!(encoded_len > 0);

    let pool = pool::Pool::new(u16::MAX);
    let unfilled = pool.alloc::<SyncRecycler>().expect("pool alloc");
    let segments = unfilled
        .fill_with(|addr, _cmsg, mut iov| {
            iov[..encoded_len].copy_from_slice(&buf[..encoded_len]);
            addr.set(peer.into());
            Ok::<_, core::convert::Infallible>(encoded_len)
        })
        .expect("fill_with");
    let mut filled = segments.take_filled();

    let meta = {
        let decode_buf = DecoderBufferMut::new(&mut filled[..]);
        datagram::decoder::Meta::decode(&decode_buf, (), TAG_LEN).expect("meta decode")
    };
    let packet = meta.with_storage(filled).expect("with_storage");

    (packet, credentials)
}

/// Asserts that `headers` holds one frame: the server's reset with `STREAM_NOT_FOUND`, sent to the
/// client's queue for the binding the client's frame carried. `frame` names the client's frame in
/// the assertions' messages.
fn assert_answered_with_reset(headers: &[Header], frame: &str) {
    assert_eq!(
        headers.len(),
        1,
        "{frame} for a stream the server hasn't bound must get one frame back, a reset"
    );
    let Header::QueueReset {
        queue_pair,
        binding_id,
        reset_target,
        error_code,
        init,
    } = headers[0]
    else {
        panic!("{frame} for a stream the server hasn't bound must be answered with a reset");
    };
    assert_eq!(
        queue_pair.source_queue_id,
        VarInt::from_u8(SERVER_QUEUE_ID),
        "the reset must come from the server's queue the frame was routed at"
    );
    assert_eq!(
        queue_pair.dest_queue_id,
        VarInt::from_u8(CLIENT_QUEUE_ID),
        "the reset must go to the client's queue the frame came from"
    );
    assert_eq!(
        binding_id,
        VarInt::from_u8(BINDING_ID),
        "the reset must carry the binding id of the frame it answers"
    );
    assert!(
        matches!(reset_target, ResetTarget::Both),
        "the reset must end both halves of the client's stream"
    );
    assert_eq!(
        error_code,
        error::STREAM_NOT_FOUND,
        "the reset must say that the server hasn't bound the stream"
    );
    assert!(init.is_none(), "the reset must not carry init fields");
}

//= rendered/0005-endpoint/03-binding.md#DC-0005-020
//= type=test
//# A server MUST answer a frame for a stream it hasn't bound, and won't bind from that frame, with a reset sent back to the frame's sender.
/// A data frame that doesn't open a stream, for a queue the server hasn't bound, gets a reset back
/// and doesn't bind the queue.
#[test]
fn a_data_frame_for_an_unbound_stream_gets_a_reset() {
    let peer: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let path_secret_map = server_map(peer);
    let (mut harness, mut frame_rx) = FastPathHarness::with_frame_rx();
    let mut recv_cache = recv::Cache::new(crate::endpoint::id::RecvDispatchWorkerId::new(0));

    let payload = b"queue-data-payload";
    let header = Header::QueueData {
        queue_pair: client_queue_pair(),
        binding_id: VarInt::from_u8(BINDING_ID),
        offset: VarInt::ZERO,
        largest_offset: VarInt::new(payload.len() as u64).unwrap(),
        is_fin: false,
        blocked: false,
        dest_acceptor_id: None,
        priority: crate::credit::Priority::default(),
    };
    let (packet, credentials) = sealed_frame(peer, endpoint::Type::Client, 0, header, payload);
    let _ = process_with(packet, &path_secret_map, &mut harness, &mut recv_cache);

    assert_answered_with_reset(&submitted_headers(&mut frame_rx), "a data frame");
    assert!(
        slot_is_free(&path_secret_map, &credentials, SERVER_QUEUE_ID),
        "a data frame for a stream the server hasn't bound must not bind its slot"
    );
}

//= rendered/0005-endpoint/03-binding.md#DC-0005-020
//= type=test
//# A server MUST answer a frame for a stream it hasn't bound, and won't bind from that frame, with a reset sent back to the frame's sender.
/// A message frame that doesn't open a stream, for a queue the server hasn't bound, takes the fast
/// path, which rejects it before it authenticates the packet. The frame gets a reset back only
/// where its packet authenticates, and doesn't bind the queue either way.
#[test]
fn a_message_frame_for_an_unbound_stream_gets_a_reset_once_its_packet_authenticates() {
    let peer: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let path_secret_map = server_map(peer);
    let (mut harness, mut frame_rx) = FastPathHarness::with_frame_rx();
    let mut recv_cache = recv::Cache::new(crate::endpoint::id::RecvDispatchWorkerId::new(0));

    let (mut forged, credentials) = sealed_client_frame(
        peer,
        VarInt::ZERO,
        VarInt::from_u8(3),
        VarInt::ZERO,
        VarInt::from_u8(SERVER_QUEUE_ID),
        None,
    );
    let decrypt_len = forged.decrypt_into_len();
    let header = decode::detect_single_queue_msg(forged.application_header(), decrypt_len);
    assert!(
        header.is_some(),
        "the message frame must be a single message frame, which takes the fast path"
    );
    forged.payload_mut()[0] ^= 0xFF;
    let result = process_with(forged, &path_secret_map, &mut harness, &mut recv_cache);
    let got = outcome(&result);
    assert!(
        matches!(result, Err(Error::Decryption { .. })),
        "a packet that fails authentication must be rejected as Decryption, not {got}"
    );
    assert!(
        submitted_headers(&mut frame_rx).is_empty(),
        "a message frame whose packet fails authentication must not get a reset"
    );

    let (authentic, _) = sealed_client_frame(
        peer,
        VarInt::ZERO,
        VarInt::from_u8(3),
        VarInt::from_u8(1),
        VarInt::from_u8(SERVER_QUEUE_ID),
        None,
    );
    let _ = process_with(authentic, &path_secret_map, &mut harness, &mut recv_cache);

    assert_answered_with_reset(&submitted_headers(&mut frame_rx), "a message frame");
    assert!(
        slot_is_free(&path_secret_map, &credentials, SERVER_QUEUE_ID),
        "a message frame for a stream the server hasn't bound must not bind its slot"
    );
}

//= rendered/0005-endpoint/03-binding.md#DC-0005-020
//= type=test
//# A server MUST answer a frame for a stream it hasn't bound, and won't bind from that frame, with a reset sent back to the frame's sender.
/// A plain reset, one without init fields, for a queue the server hasn't bound gets a reset back
/// and doesn't bind the queue.
#[test]
fn a_reset_for_an_unbound_stream_gets_a_reset() {
    let peer: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let path_secret_map = server_map(peer);
    let (mut harness, mut frame_rx) = FastPathHarness::with_frame_rx();
    let mut recv_cache = recv::Cache::new(crate::endpoint::id::RecvDispatchWorkerId::new(0));

    let header = Header::QueueReset {
        queue_pair: client_queue_pair(),
        binding_id: VarInt::from_u8(BINDING_ID),
        reset_target: ResetTarget::Both,
        error_code: error::STOP_SENDING,
        init: None,
    };
    let (packet, credentials) = sealed_frame(peer, endpoint::Type::Client, 0, header, &[]);
    let _ = process_with(packet, &path_secret_map, &mut harness, &mut recv_cache);

    assert_answered_with_reset(&submitted_headers(&mut frame_rx), "a reset");
    assert!(
        slot_is_free(&path_secret_map, &credentials, SERVER_QUEUE_ID),
        "a reset for a stream the server hasn't bound must not bind its slot"
    );
}

//= rendered/0005-endpoint/03-binding.md#DC-0005-020
//= type=test
//# A server MUST answer a frame for a stream it hasn't bound, and won't bind from that frame, with a reset sent back to the frame's sender.
/// A control frame for a queue the server hasn't bound gets a reset back and doesn't bind the
/// queue.
#[test]
fn a_control_frame_for_an_unbound_stream_gets_a_reset() {
    let peer: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let path_secret_map = server_map(peer);
    let (mut harness, mut frame_rx) = FastPathHarness::with_frame_rx();
    let mut recv_cache = recv::Cache::new(crate::endpoint::id::RecvDispatchWorkerId::new(0));

    let payload = b"queue-control-payload";
    let header = Header::QueueControl {
        queue_pair: client_queue_pair(),
        binding_id: VarInt::from_u8(BINDING_ID),
    };
    let (packet, credentials) = sealed_frame(peer, endpoint::Type::Client, 0, header, payload);
    let _ = process_with(packet, &path_secret_map, &mut harness, &mut recv_cache);

    assert_answered_with_reset(&submitted_headers(&mut frame_rx), "a control frame");
    assert!(
        slot_is_free(&path_secret_map, &credentials, SERVER_QUEUE_ID),
        "a control frame for a stream the server hasn't bound must not bind its slot"
    );
}

/// The server's reset for a stream the client hasn't bound isn't answered: only a server answers,
/// so the two peers don't send resets back and forth.
#[test]
fn a_client_doesnt_answer_a_reset_for_an_unbound_stream() {
    let peer: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let path_secret_map = crate::path::secret::map::testing::new(16);
    path_secret_map.test_insert(peer, endpoint::Type::Client);
    let (mut harness, mut frame_rx) = FastPathHarness::with_frame_rx();
    let mut recv_cache = recv::Cache::new(crate::endpoint::id::RecvDispatchWorkerId::new(0));

    let header = Header::QueueReset {
        queue_pair: QueuePair {
            source_queue_id: VarInt::from_u8(SERVER_QUEUE_ID),
            dest_queue_id: VarInt::from_u8(CLIENT_QUEUE_ID),
        },
        binding_id: VarInt::from_u8(BINDING_ID),
        reset_target: ResetTarget::Both,
        error_code: error::STREAM_NOT_FOUND,
        init: None,
    };
    let (packet, credentials) = sealed_frame(peer, endpoint::Type::Server, 0, header, &[]);
    assert!(
        path_secret_map.get_by_id(&credentials.id).is_some(),
        "the map must hold the client entry for the packet's credential"
    );
    let _ = process_with(packet, &path_secret_map, &mut harness, &mut recv_cache);

    assert!(
        submitted_headers(&mut frame_rx).is_empty(),
        "a client must not answer the server's reset with another reset"
    );
}
