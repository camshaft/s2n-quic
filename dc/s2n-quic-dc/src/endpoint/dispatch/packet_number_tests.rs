// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests that `process` doesn't dispatch a single QueueMsg frame, which takes the fast path,
//! from a packet whose packet number the sender's receive state has accepted already, or whose
//! packet number is too old for that state to check.

use super::*;
use crate::{
    packet::datagram::{self, QueuePair},
    path::secret::map::Entry as PathSecretEntry,
    socket::pool::{self, descriptor::SyncRecycler},
};
use s2n_codec::{DecoderBufferMut, Encoder as _};
use s2n_quic_core::endpoint;
use std::net::SocketAddr;

const TAG_LEN: usize = 16;

/// The peer's address: its client entry seals each packet, and the receiver's path secret map
/// holds its server entry.
const PEER: &str = "127.0.0.1:8080";

/// The sender id the peer sends each packet from.
const SOURCE_SENDER_ID: u8 = 3;

/// The queue and the binding of the stream each packet's frame is for.
const QUEUE_ID: u8 = 0;
const BINDING_ID: u8 = 1;

/// The payload of each message, which is a whole message of one chunk.
const PAYLOAD: &[u8] = b"queue-msg-payload";

/// A packet that holds a single QueueMsg frame with the whole message `msg_id`, sealed under key
/// id 0 by the client entry for [`PEER`], with `packet_number`. Message `msg_id` starts at stream
/// offset `msg_id * PAYLOAD.len()`. Returns the packet and its credentials.
fn sealed_packet(
    packet_number: u64,
    msg_id: u64,
) -> (
    datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
    Credentials,
) {
    let peer: SocketAddr = PEER.parse().unwrap();
    let key_id = VarInt::ZERO;
    let sealer_entry = PathSecretEntry::builder(peer)
        .endpoint_type(endpoint::Type::Client)
        .build(None);
    let sealer = sealer_entry.secret().application_sealer(key_id);
    let credentials = Credentials {
        id: sealer_entry.secret().peer_id(),
        key_id,
    };

    let payload_len = VarInt::new(PAYLOAD.len() as u64).unwrap();
    let header = Header::QueueMsg {
        queue_pair: QueuePair {
            source_queue_id: VarInt::from_u8(7),
            dest_queue_id: VarInt::from_u8(QUEUE_ID),
        },
        binding_id: VarInt::from_u8(BINDING_ID),
        msg_id: VarInt::new(msg_id).unwrap(),
        stream_offset: VarInt::new(msg_id * PAYLOAD.len() as u64).unwrap(),
        largest_offset: VarInt::new((msg_id + 1) * PAYLOAD.len() as u64).unwrap(),
        message_size: payload_len,
        chunk_size: payload_len,
        chunk_index: VarInt::ZERO,
        // The stream's later messages follow this one, so the slot takes them.
        is_fin: false,
        is_wakeup: true,
        blocked: false,
        dest_acceptor_id: None,
        priority: crate::credit::Priority::default(),
    };

    let mut header_buf = vec![0u8; header.metadata_len(PAYLOAD.len())];
    {
        let mut enc = s2n_codec::EncoderBuffer::new(&mut header_buf);
        enc.encode(&header);
        enc.encode(&payload_len);
    }

    let mut buf = vec![0u8; 65536];
    let mut header_reader = &header_buf[..];
    let mut payload_reader = PAYLOAD;
    let encoded_len = datagram::encoder::encode(
        s2n_codec::EncoderBuffer::new(&mut buf),
        443,
        datagram::RoutingInfo::SenderId {
            source_sender_id: VarInt::from_u8(SOURCE_SENDER_ID),
        },
        Some(VarInt::new(packet_number).unwrap()),
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

/// Binds the stream each packet's frame is for, on the path secret map's server entry for
/// `credentials`, and returns a function that reads the ids of the messages the stream received
/// since that function's last call.
fn bind_stream(
    path_secret_map: &PathSecretMap,
    credentials: &Credentials,
) -> impl FnMut() -> Vec<u64> {
    let entry = path_secret_map
        .get_by_id(&credentials.id)
        .expect("the map must hold the server entry for the packets' credential");
    let mut view = match entry.queue_state() {
        crate::path::secret::map::entry::QueueState::Server(state) => {
            recv::QueueView::Server(state.view())
        }
        _ => unreachable!("server entry must have server queue state"),
    };

    let (freed_batch_tx, freed_batch_rx) = crate::queue::freed_batch_channel();
    let mut freed_batch_tx = freed_batch_tx;
    let queue_id = VarInt::from_u8(QUEUE_ID);
    let binding_id = VarInt::from_u8(BINDING_ID);
    let server = view.as_server_mut().expect("server view");
    let result = server.bind_for_msg(queue_id, binding_id, &entry, &mut freed_batch_tx);
    let crate::queue::BindResult::NewBinding {
        stream, control, ..
    } = result.expect("slot must bind for the test setup")
    else {
        panic!("expected NewBinding for a fresh slot");
    };

    // Dropping the receivers frees the binding, and the entry holds the shared queue state that
    // the receive cache's view reads, so the function keeps them alive.
    move || {
        let _ = (&control, &freed_batch_rx, &entry);
        let mut msg_ids = Vec::new();
        while let Ok(Some(received)) = stream.try_recv() {
            if let msg::Stream::Data { offset, .. } = received.into_inner() {
                msg_ids.push(offset.as_u64() / PAYLOAD.len() as u64);
            }
        }
        msg_ids
    }
}

/// Drops each value it's sent.
struct Discard;

impl<T> channel::UnboundedSender<T> for Discard {
    fn send(&mut self, _value: T) -> Result<(), T> {
        Ok(())
    }
}

/// The receiving endpoint's state across packets: the receive cache, which holds the packet
/// numbers each sender's receive state has accepted, and the other arguments `process` takes.
struct Receiver {
    recv_cache: recv::Cache,
    acceptor_registry: acceptor::LocalRegistry<Stream>,
    frame_tx: SubmissionSender,
    freed_batch_tx: crate::queue::FreedBatchTx,
    _freed_batch_rx: crate::queue::FreedBatchRx,
    counters: Arc<counters::Dispatch>,
    stream_clock: crate::time::DefaultClock,
    reader_metrics: Arc<crate::stream::metrics::ReaderMetrics>,
    writer_metrics: Arc<crate::stream::metrics::WriterMetrics>,
    send_credit_pool: crate::sync::Arc<crate::credit::Pool>,
    recv_credit_pool: crate::sync::Arc<crate::credit::Pool>,
}

impl Receiver {
    fn new() -> Self {
        let registry = crate::counter::Registry::default();
        let (frame_tx, _frame_rx) = crate::endpoint::frame::submission_channel(1);
        let (freed_batch_tx, _freed_batch_rx) = crate::queue::freed_batch_channel();
        Self {
            recv_cache: recv::Cache::new(crate::endpoint::id::RecvDispatchWorkerId::new(0)),
            acceptor_registry: acceptor::Registry::<Stream>::new().local(),
            frame_tx,
            freed_batch_tx,
            _freed_batch_rx,
            counters: counters::Dispatch::new(&registry),
            stream_clock: crate::time::DefaultClock::default(),
            reader_metrics: Arc::new(crate::stream::metrics::ReaderMetrics::new(&registry, "rx")),
            writer_metrics: Arc::new(crate::stream::metrics::WriterMetrics::new(&registry, "tx")),
            send_credit_pool: crate::sync::Arc::new(crate::credit::Pool::new(
                crate::credit::Config::default(),
            )),
            recv_credit_pool: crate::sync::Arc::new(crate::credit::Pool::new(
                crate::credit::Config::default(),
            )),
        }
    }

    /// Runs `process` on `packet` with this receiver's receive cache.
    fn receive(
        &mut self,
        packet: datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
        path_secret_map: &PathSecretMap,
    ) -> Result<(), Error> {
        let clock = crate::time::testing::Clock::new(Duration::from_secs(1));
        let route = <routing::ModuloRoute as routing::SenderRoute>::new(1);
        process(
            Entry::new(packet),
            &mut self.recv_cache,
            &mut Discard,
            &mut Discard,
            path_secret_map,
            &mut self.acceptor_registry,
            &mut self.frame_tx,
            &mut Discard,
            &mut self.freed_batch_tx,
            &clock,
            &self.counters,
            &route,
            &mut Discard,
            &self.stream_clock,
            &self.reader_metrics,
            &self.writer_metrics,
            &self.send_credit_pool,
            &self.recv_credit_pool,
        )
    }
}

/// Asserts that `process` refused the packet of `case` as a duplicate.
fn assert_duplicate(result: Result<(), Error>, case: &str) {
    assert!(
        matches!(result, Err(Error::Duplicate { .. })),
        "process must refuse {case} as a duplicate"
    );
}

/// A second copy of an accepted packet doesn't reach the stream: the stream receives the
/// packet's message once.
//= rendered/0005-endpoint/02-receive.md#DC-0005-010
//= type=test
//# A receiver MUST NOT dispatch the frames of a packet whose packet number it has already accepted from the same sender, or whose packet number is too old for it to tell.
#[test]
fn a_copy_of_an_accepted_packet_doesnt_reach_the_stream() {
    let path_secret_map = crate::path::secret::map::testing::new(16);
    path_secret_map.test_insert(PEER.parse().unwrap(), endpoint::Type::Server);
    let mut receiver = Receiver::new();

    let (first, credentials) = sealed_packet(0, 0);
    let (copy, _) = sealed_packet(0, 0);
    let mut received = bind_stream(&path_secret_map, &credentials);

    assert!(receiver.receive(first, &path_secret_map).is_ok(), "process must accept packet 0");
    assert_eq!(received(), [0], "the stream must receive message 0");

    assert_duplicate(receiver.receive(copy, &path_secret_map), "a copy of packet 0");
    assert_eq!(received(), Vec::<u64>::new(), "the stream must receive message 0 once");
}

/// A packet whose packet number the sender's receive state accepted already doesn't reach the
/// stream, though it carries a message the stream hasn't received. A later packet number that
/// carries the same message reaches it, so the packet number alone keeps the first one out.
//= rendered/0005-endpoint/02-receive.md#DC-0005-010
//= type=test
//# A receiver MUST NOT dispatch the frames of a packet whose packet number it has already accepted from the same sender, or whose packet number is too old for it to tell.
#[test]
fn a_new_message_under_an_accepted_packet_number_doesnt_reach_the_stream() {
    let path_secret_map = crate::path::secret::map::testing::new(16);
    path_secret_map.test_insert(PEER.parse().unwrap(), endpoint::Type::Server);
    let mut receiver = Receiver::new();

    let (first, credentials) = sealed_packet(0, 0);
    let (repeat, _) = sealed_packet(0, 1);
    let (next, _) = sealed_packet(1, 1);
    let mut received = bind_stream(&path_secret_map, &credentials);

    assert!(receiver.receive(first, &path_secret_map).is_ok(), "process must accept packet 0");
    assert_eq!(received(), [0], "the stream must receive message 0");

    assert_duplicate(
        receiver.receive(repeat, &path_secret_map),
        "a second packet 0 that carries message 1",
    );
    assert_eq!(
        received(),
        Vec::<u64>::new(),
        "the stream must not receive message 1 from a second packet 0"
    );

    assert!(receiver.receive(next, &path_secret_map).is_ok(), "process must accept packet 1");
    assert_eq!(received(), [1], "the stream must receive message 1 from packet 1");
}

/// A packet whose packet number is too old for the sender's receive state to check doesn't reach
/// the stream: packet 200 moves the window past packet 1, which has never arrived.
//= rendered/0005-endpoint/02-receive.md#DC-0005-010
//= type=test
//# A receiver MUST NOT dispatch the frames of a packet whose packet number it has already accepted from the same sender, or whose packet number is too old for it to tell.
#[test]
fn a_packet_number_too_old_to_check_doesnt_reach_the_stream() {
    let path_secret_map = crate::path::secret::map::testing::new(16);
    path_secret_map.test_insert(PEER.parse().unwrap(), endpoint::Type::Server);
    let mut receiver = Receiver::new();

    let (first, credentials) = sealed_packet(0, 0);
    let (newest, _) = sealed_packet(200, 1);
    let (too_old, _) = sealed_packet(1, 2);
    let (next, _) = sealed_packet(201, 2);
    let mut received = bind_stream(&path_secret_map, &credentials);

    assert!(receiver.receive(first, &path_secret_map).is_ok(), "process must accept packet 0");
    assert!(receiver.receive(newest, &path_secret_map).is_ok(), "process must accept packet 200");
    assert_eq!(received(), [0, 1], "the stream must receive messages 0 and 1");

    assert_duplicate(
        receiver.receive(too_old, &path_secret_map),
        "packet 1, which is too old for the window after packet 200",
    );
    assert_eq!(
        received(),
        Vec::<u64>::new(),
        "the stream must not receive message 2 from packet 1"
    );

    assert!(receiver.receive(next, &path_secret_map).is_ok(), "process must accept packet 201");
    assert_eq!(received(), [2], "the stream must receive message 2 from packet 201");
}

/// A packet whose packet number the sender's receive state accepted already, below the newest
/// number it accepted, doesn't reach the stream, though it carries a message the stream hasn't
/// received: packet 0 after packet 1 falls inside the window, on a number the window holds.
//= rendered/0005-endpoint/02-receive.md#DC-0005-010
//= type=test
//# A receiver MUST NOT dispatch the frames of a packet whose packet number it has already accepted from the same sender, or whose packet number is too old for it to tell.
#[test]
fn a_new_message_under_an_accepted_packet_number_below_the_newest_doesnt_reach_the_stream() {
    let path_secret_map = crate::path::secret::map::testing::new(16);
    path_secret_map.test_insert(PEER.parse().unwrap(), endpoint::Type::Server);
    let mut receiver = Receiver::new();

    let (first, credentials) = sealed_packet(0, 0);
    let (newest, _) = sealed_packet(1, 1);
    let (repeat, _) = sealed_packet(0, 2);
    let (next, _) = sealed_packet(2, 2);
    let mut received = bind_stream(&path_secret_map, &credentials);

    assert!(receiver.receive(first, &path_secret_map).is_ok(), "process must accept packet 0");
    assert!(receiver.receive(newest, &path_secret_map).is_ok(), "process must accept packet 1");
    assert_eq!(received(), [0, 1], "the stream must receive messages 0 and 1");

    assert_duplicate(
        receiver.receive(repeat, &path_secret_map),
        "a second packet 0, below the newest packet 1, that carries message 2",
    );
    assert_eq!(
        received(),
        Vec::<u64>::new(),
        "the stream must not receive message 2 from a second packet 0"
    );

    assert!(receiver.receive(next, &path_secret_map).is_ok(), "process must accept packet 2");
    assert_eq!(received(), [2], "the stream must receive message 2 from packet 2");
}
