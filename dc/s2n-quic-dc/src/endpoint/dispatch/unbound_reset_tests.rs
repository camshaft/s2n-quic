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

//= rendered/0005-endpoint/03-binding.md#DC-0005-056
//= type=test
//# A client MUST NOT answer with a reset a reset that arrives for a stream it doesn't hold.
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

/// A frame the client sends on its queue to the server's queue, for `answer` to send.
#[derive(Clone, Copy, Debug)]
enum ClientFrame {
    /// A data frame that doesn't open a stream.
    Data,
    /// A message frame that doesn't open a stream, which takes the fast path.
    Message,
    /// A plain reset, one without init fields.
    Reset,
}

impl ClientFrame {
    const ALL: [Self; 3] = [Self::Data, Self::Message, Self::Reset];

    /// The frame's header, carrying `binding_id`, and its payload.
    fn header(self, binding_id: u8) -> (Header, &'static [u8]) {
        let binding_id = VarInt::from_u8(binding_id);
        match self {
            Self::Data => {
                let payload: &[u8] = b"queue-data-payload";
                let header = Header::QueueData {
                    queue_pair: client_queue_pair(),
                    binding_id,
                    offset: VarInt::ZERO,
                    largest_offset: VarInt::new(payload.len() as u64).unwrap(),
                    is_fin: false,
                    blocked: false,
                    dest_acceptor_id: None,
                    priority: crate::credit::Priority::default(),
                };
                (header, payload)
            }
            Self::Message => {
                let payload: &[u8] = b"queue-msg-payload";
                let payload_len = VarInt::new(payload.len() as u64).unwrap();
                let header = Header::QueueMsg {
                    queue_pair: client_queue_pair(),
                    binding_id,
                    msg_id: VarInt::ZERO,
                    stream_offset: VarInt::ZERO,
                    largest_offset: payload_len,
                    message_size: payload_len,
                    chunk_size: payload_len,
                    chunk_index: VarInt::ZERO,
                    is_fin: true,
                    is_wakeup: true,
                    blocked: false,
                    dest_acceptor_id: None,
                    priority: crate::credit::Priority::default(),
                };
                (header, payload)
            }
            Self::Reset => {
                let payload: &[u8] = &[];
                let header = Header::QueueReset {
                    queue_pair: client_queue_pair(),
                    binding_id,
                    reset_target: ResetTarget::Both,
                    error_code: error::STOP_SENDING,
                    init: None,
                };
                (header, payload)
            }
        }
    }
}

/// The server's slot for `SERVER_QUEUE_ID` before the client's frame reaches it.
#[derive(Clone, Copy, Debug)]
enum ServerSlot {
    /// A stream bound the slot with this binding, and the stream's receivers have freed it since.
    Freed(u8),
    /// A stream bound the slot with this binding and holds it.
    Held(u8),
}

/// Sets up the server's slot for `SERVER_QUEUE_ID` on the path of `credentials` as `slot` says: a
/// stream binds the slot, and for `ServerSlot::Freed` the stream's receivers are dropped, which
/// frees the slot and keeps the stream's binding as the slot's last binding.
fn set_up_slot(path_secret_map: &PathSecretMap, credentials: &Credentials, slot: ServerSlot) {
    let (binding_id, free) = match slot {
        ServerSlot::Freed(binding_id) => (binding_id, true),
        ServerSlot::Held(binding_id) => (binding_id, false),
    };
    let entry = path_secret_map
        .get_by_id(&credentials.id)
        .expect("the map must hold the server entry for the packet's credential");
    let crate::path::secret::map::entry::QueueState::Server(state) = entry.queue_state() else {
        panic!("a server entry must have a server queue state");
    };
    let mut view = recv::QueueView::Server(state.view());
    let (mut freed_batch_tx, freed_batch_rx) = crate::queue::freed_batch_channel();
    let server = view.as_server_mut().expect("server view");
    let result = server.bind_for_msg(
        VarInt::from_u8(SERVER_QUEUE_ID),
        VarInt::from_u8(binding_id),
        &entry,
        &mut freed_batch_tx,
    );
    let crate::queue::BindResult::NewBinding {
        stream, control, ..
    } = result.expect("the slot must bind for the test's setup")
    else {
        panic!("a slot that never held a stream must take a new binding");
    };
    if free {
        drop(stream);
        drop(control);
    } else {
        // The stream holds the slot while its receivers live: leak them for the short-lived test.
        std::mem::forget(stream);
        std::mem::forget(control);
    }
    std::mem::forget(freed_batch_rx);
    assert_eq!(
        slot_is_free(path_secret_map, credentials, SERVER_QUEUE_ID),
        free,
        "the setup must leave the slot {slot:?}"
    );
}

/// Sends `frame`, carrying `binding_id`, from the client's queue to the server's slot for
/// `SERVER_QUEUE_ID`, set up as `slot` says. Returns the frames the server sends back, and whether
/// the slot is free once the server has processed the frame.
fn answer(slot: ServerSlot, frame: ClientFrame, binding_id: u8) -> (Vec<Header>, bool) {
    let peer: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let path_secret_map = server_map(peer);
    let (mut harness, mut frame_rx) = FastPathHarness::with_frame_rx();
    let mut recv_cache = recv::Cache::new(crate::endpoint::id::RecvDispatchWorkerId::new(0));

    let (header, payload) = frame.header(binding_id);
    let (packet, credentials) = sealed_frame(peer, endpoint::Type::Client, 0, header, payload);
    set_up_slot(&path_secret_map, &credentials, slot);
    let _ = process_with(packet, &path_secret_map, &mut harness, &mut recv_cache);

    let headers = submitted_headers(&mut frame_rx);
    let free = slot_is_free(&path_secret_map, &credentials, SERVER_QUEUE_ID);
    (headers, free)
}

/// The resets among `headers`.
fn resets_in(headers: &[Header]) -> Vec<Header> {
    headers
        .iter()
        .copied()
        .filter(|header| matches!(header, Header::QueueReset { .. }))
        .collect()
}

//= rendered/0005-endpoint/03-binding.md#DC-0005-053
//= type=test
//# The reset that answers a frame under DC-0005-020 MUST carry the stream-not-found error code.
/// A data frame, a message frame and a plain reset for a free slot, each carrying a binding newer
/// than the slot's last binding, each get a reset back that carries the stream-not-found code.
#[test]
fn the_reset_for_an_unbound_stream_carries_the_stream_not_found_code() {
    for frame in ClientFrame::ALL {
        let (headers, free) = answer(ServerSlot::Freed(1), frame, 2);
        let resets = resets_in(&headers);
        assert_eq!(
            resets.len(),
            1,
            "{frame:?} for a free slot with a newer binding must get one reset back"
        );
        let Header::QueueReset { error_code, .. } = resets[0] else {
            unreachable!("resets_in keeps only resets");
        };
        assert_eq!(
            error_code,
            error::STREAM_NOT_FOUND,
            "the reset that answers {frame:?} must carry the stream-not-found code"
        );
        assert_eq!(
            error::Error::from(error_code),
            error::Error::StreamNotFound,
            "the code of the reset that answers {frame:?} must read as the stream-not-found error"
        );
        assert!(free, "{frame:?} for a free slot must leave the slot free");
    }
}

//= rendered/0005-endpoint/03-binding.md#DC-0005-054
//= type=test
//# The reset that answers a frame under DC-0005-020 MUST carry the binding identifier of the frame it answers.
/// The reset that answers a frame for a free slot carries the frame's binding, and not the slot's
/// last binding: the frames carry bindings 2 and 9 to a slot whose last binding was 1.
#[test]
fn the_reset_for_an_unbound_stream_carries_the_binding_of_the_frame_it_answers() {
    for frame in ClientFrame::ALL {
        for binding in [2, 9] {
            let (headers, _) = answer(ServerSlot::Freed(1), frame, binding);
            let resets = resets_in(&headers);
            assert_eq!(
                resets.len(),
                1,
                "{frame:?} with binding {binding} for a free slot must get one reset back"
            );
            let Header::QueueReset {
                queue_pair,
                binding_id,
                ..
            } = resets[0]
            else {
                unreachable!("resets_in keeps only resets");
            };
            assert_eq!(
                binding_id,
                VarInt::from_u8(binding),
                "the reset that answers {frame:?} must carry the frame's binding {binding}"
            );
            assert_eq!(
                queue_pair.dest_queue_id,
                VarInt::from_u8(CLIENT_QUEUE_ID),
                "the reset that answers {frame:?} must go to the client's queue that sent it"
            );
        }
    }
}

//= rendered/0005-endpoint/03-binding.md#DC-0005-055
//= type=test
//# A server MUST NOT answer with a reset a frame for a slot that a stream holds, or a frame whose binding identifier isn't newer than the last binding of its free slot.
/// A frame doesn't get a reset back where the slot check finds its binding stale on a free slot,
/// stale on a slot that a stream holds, or newer than the binding of the stream that holds the
/// slot, and a stream that holds the slot keeps it. A frame whose binding is newer than the free
/// slot's last binding gets the reset, so the frame's binding alone decides the answer.
#[test]
fn a_frame_for_a_held_slot_or_with_a_binding_that_isnt_newer_doesnt_get_a_reset() {
    let cases = [
        // A free slot whose last binding is the frame's: the slot check answers StaleBinding.
        (ServerSlot::Freed(2), 2),
        // A free slot whose last binding is newer than the frame's: StaleBinding.
        (ServerSlot::Freed(2), 1),
        // A slot that a stream with a newer binding holds: StaleBinding.
        (ServerSlot::Held(2), 1),
        // A slot that a stream with an older binding holds: FutureBinding.
        (ServerSlot::Held(1), 2),
    ];
    for frame in ClientFrame::ALL {
        for (slot, binding) in cases {
            let (headers, free) = answer(slot, frame, binding);
            assert!(
                resets_in(&headers).is_empty(),
                "{frame:?} with binding {binding} for {slot:?} must not get a reset"
            );
            assert_eq!(
                free,
                matches!(slot, ServerSlot::Freed(_)),
                "{frame:?} with binding {binding} must leave {slot:?} as it was"
            );
        }

        let (headers, _) = answer(ServerSlot::Freed(2), frame, 3);
        assert_eq!(
            resets_in(&headers).len(),
            1,
            "{frame:?} with a binding newer than the free slot's last binding must get a reset"
        );
    }
}

//= rendered/0005-endpoint/03-binding.md#DC-0005-057
//= type=test
//# An endpoint MUST NOT carry the stream-not-found error code on a reset other than one that answers a frame under DC-0005-020.
/// The stream-not-found code differs from every other error code, and the reset that the server
/// sends for an opening frame for an acceptor it doesn't have carries the acceptor-not-found code
/// and not the stream-not-found code.
#[test]
fn a_reset_for_another_reason_doesnt_carry_the_stream_not_found_code() {
    for code in [
        error::ACCEPTOR_NOT_FOUND,
        error::FRAME_DECODE_ERROR,
        error::ABNORMAL_TERMINATION,
        error::STOP_SENDING,
        error::RETRANSMISSIONS_EXHAUSTED,
        error::SERVER_BUSY,
        error::QUEUE_CONTROL_ERROR,
        error::IDLE_TIMEOUT,
        error::SENDER_CANCELLED,
    ] {
        assert_ne!(
            code,
            error::STREAM_NOT_FOUND,
            "the stream-not-found code must differ from the code {}",
            code.as_u64()
        );
        assert_ne!(
            error::Error::from(code),
            error::Error::StreamNotFound,
            "the code {} must not read as the stream-not-found error",
            code.as_u64()
        );
    }

    let peer: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let path_secret_map = server_map(peer);
    let (mut harness, mut frame_rx) = FastPathHarness::with_frame_rx();
    let mut recv_cache = recv::Cache::new(crate::endpoint::id::RecvDispatchWorkerId::new(0));

    let (opening, _) = opening_frame(peer, 0, SERVER_QUEUE_ID, false);
    let _ = process_with(opening, &path_secret_map, &mut harness, &mut recv_cache);
    let headers = submitted_headers(&mut frame_rx);
    let acceptor_not_found = headers
        .iter()
        .any(|header| is_reset(header, error::ACCEPTOR_NOT_FOUND));
    let stream_not_found = headers
        .iter()
        .any(|header| is_reset(header, error::STREAM_NOT_FOUND));
    assert!(
        acceptor_not_found,
        "an opening frame for an acceptor the server doesn't have must get a reset"
    );
    assert!(
        !stream_not_found,
        "a reset for another reason must not carry the stream-not-found code"
    );
}
