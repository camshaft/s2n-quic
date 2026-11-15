// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests of the server's binding of streams: which frames bind a stream, and when the server
//! acknowledges a packet whose frames it rejects for their routing.

use super::*;
use crate::{
    endpoint::frame::Init,
    packet::datagram::{self, QueuePair},
    path::secret::map::Entry as PathSecretEntry,
    socket::pool::{
        self,
        descriptor::{Filled, SyncRecycler},
    },
};
use s2n_codec::{DecoderBufferMut, Encoder as _};
use s2n_quic_core::endpoint;
use std::net::SocketAddr;

const TAG_LEN: usize = 16;

/// The acceptor the opening frames below are for.
const ACCEPTOR_ID: u8 = 5;

/// The server's queue the frames below are routed at.
const QUEUE_ID: u8 = 0;

/// The client's own queue the frames below are sent from.
const SOURCE_QUEUE_ID: u8 = 7;

/// The binding the frames below belong to.
const BINDING_ID: u8 = 1;

/// The client's sender id in the routing info of the packets below.
const SOURCE_SENDER_ID: u8 = 3;

/// The payload of each data and message frame below.
const PAYLOAD: &[u8] = b"binding-test-payload";

/// The payload of each reset below.
const EMPTY: &[u8] = &[];

/// The kinds of frame that can open a stream.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Data,
    Msg,
    Reset,
}

impl Kind {
    const ALL: [Self; 3] = [Self::Data, Self::Msg, Self::Reset];
}

/// A frame of `kind` routed at `QUEUE_ID` for `BINDING_ID`, with its payload. Where `opening`, it
/// names `ACCEPTOR_ID`, as the opening frames of a stream do.
fn frame(kind: Kind, opening: bool) -> (Header, &'static [u8]) {
    let queue_pair = QueuePair {
        source_queue_id: VarInt::from_u8(SOURCE_QUEUE_ID),
        dest_queue_id: VarInt::from_u8(QUEUE_ID),
    };
    let binding_id = VarInt::from_u8(BINDING_ID);
    let dest_acceptor_id = opening.then_some(VarInt::from_u8(ACCEPTOR_ID));
    let priority = crate::credit::Priority::default();
    let len = VarInt::new(PAYLOAD.len() as u64).unwrap();
    match kind {
        Kind::Data => (
            Header::QueueData {
                queue_pair,
                binding_id,
                offset: VarInt::ZERO,
                largest_offset: len,
                is_fin: true,
                blocked: false,
                dest_acceptor_id,
                priority,
            },
            PAYLOAD,
        ),
        Kind::Msg => (
            Header::QueueMsg {
                queue_pair,
                binding_id,
                msg_id: VarInt::ZERO,
                stream_offset: VarInt::ZERO,
                largest_offset: len,
                message_size: len,
                chunk_size: len,
                chunk_index: VarInt::ZERO,
                is_fin: true,
                is_wakeup: true,
                blocked: false,
                dest_acceptor_id,
                priority,
            },
            PAYLOAD,
        ),
        Kind::Reset => (
            Header::QueueReset {
                queue_pair,
                binding_id,
                reset_target: ResetTarget::Both,
                error_code: VarInt::from_u8(42),
                init: dest_acceptor_id.map(|dest_acceptor_id| Init {
                    dest_acceptor_id,
                    priority,
                }),
            },
            EMPTY,
        ),
    }
}

/// `header` and `payload` as the client for `peer` sends them, in one packet numbered
/// `packet_number`. Where `forged`, a byte of the sealed payload is changed afterwards, so that
/// the packet fails authentication, as a forged packet, or one corrupted on the way, does.
fn sealed(
    peer: SocketAddr,
    packet_number: u8,
    header: Header,
    payload: &[u8],
    forged: bool,
) -> (datagram::decoder::Packet<Filled>, Credentials) {
    let sealer_entry = PathSecretEntry::builder(peer)
        .endpoint_type(endpoint::Type::Client)
        .build(None);
    let key_id = VarInt::ZERO;
    let sealer = sealer_entry.secret().application_sealer(key_id);
    let credentials = Credentials {
        id: sealer_entry.secret().peer_id(),
        key_id,
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
            source_sender_id: VarInt::from_u8(SOURCE_SENDER_ID),
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
    let mut packet = meta.with_storage(filled).expect("with_storage");
    if forged {
        packet.payload_mut()[0] ^= 0xFF;
    }

    (packet, credentials)
}

/// The variant of `result`, for an assertion's message: `Error` doesn't implement `Debug`.
fn outcome(result: &Result<(), Error>) -> &'static str {
    match result {
        Ok(()) => "Ok",
        Err(Error::PeerStateLookup { .. }) => "PeerStateLookup",
        Err(Error::Decryption { .. }) => "Decryption",
        Err(Error::Duplicate { .. }) => "Duplicate",
        Err(Error::StaleKey { .. }) => "StaleKey",
        Err(Error::MissingSenderId) => "MissingSenderId",
    }
}

struct Discard;

impl<T> channel::UnboundedSender<T> for Discard {
    fn send(&mut self, _value: T) -> Result<(), T> {
        Ok(())
    }
}

/// A server that receives one client's packets, with `ACCEPTOR_ID` registered.
struct Server {
    peer: SocketAddr,
    accepted: acceptor::channel::Receiver<Stream>,
    _acceptors: acceptor::Registry<Stream>,
    recv_cache: recv::Cache,
    frame_rx: crate::endpoint::frame::SubmissionReceiver,
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
    path_secret_map: PathSecretMap,
}

impl Server {
    fn new() -> Self {
        let peer: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let path_secret_map = crate::path::secret::map::testing::new(16);
        path_secret_map.test_insert(peer, endpoint::Type::Server);

        let acceptors = acceptor::Registry::<Stream>::new();
        let mut accepted = acceptors
            .register(
                VarInt::from_u8(ACCEPTOR_ID),
                acceptor::channel::Config {
                    capacity: 4,
                    eviction: acceptor::channel::Eviction::Front,
                },
            )
            .expect("the acceptor id must be free");
        accepted.register();

        let registry = crate::counter::Registry::default();
        let (frame_tx, frame_rx) = crate::endpoint::frame::submission_channel(1);
        let (freed_batch_tx, _freed_batch_rx) = crate::queue::freed_batch_channel();
        Self {
            peer,
            accepted,
            acceptor_registry: acceptors.local(),
            _acceptors: acceptors,
            recv_cache: recv::Cache::new(crate::endpoint::id::RecvDispatchWorkerId::new(0)),
            frame_rx,
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
            path_secret_map,
        }
    }

    /// Runs `process` on a packet numbered `packet_number` that holds one frame of `kind`, an
    /// opening frame where `opening`, forged where `forged`.
    fn receive(
        &mut self,
        kind: Kind,
        opening: bool,
        packet_number: u8,
        forged: bool,
    ) -> Result<(), Error> {
        let (header, payload) = frame(kind, opening);
        let (packet, _) = sealed(self.peer, packet_number, header, payload, forged);
        let clock = crate::time::testing::Clock::new(Duration::from_secs(1));
        let route = <routing::ModuloRoute as routing::SenderRoute>::new(1);
        process(
            Entry::new(packet),
            &mut self.recv_cache,
            &mut Discard,
            &mut Discard,
            &self.path_secret_map,
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

    /// Whether the server's slot for `QUEUE_ID` is bound: allocated, or with a stream receiver
    /// attached.
    fn slot_is_bound(&self) -> bool {
        let (_, credentials) = sealed(self.peer, 0, frame(Kind::Data, false).0, PAYLOAD, false);
        let entry = self
            .path_secret_map
            .get_by_id(&credentials.id)
            .expect("the map must hold the server entry for the client's credential");
        let state = match entry.queue_state() {
            crate::path::secret::map::entry::QueueState::Server(state) => state,
            _ => panic!("a server entry must have a server queue state"),
        };
        let mut view = recv::QueueView::Server(state.view());
        view.slot_diag(VarInt::from_u8(QUEUE_ID))
            .is_some_and(|diag| !diag.unallocated || diag.stream_has_receiver)
    }

    /// The largest packet number the server has recorded for acknowledgement, if any.
    fn largest_acknowledged(&self) -> Option<u64> {
        let context = self.recv_cache.senders.values().next()?;
        let context = context.borrow();
        context
            .ack_ranges
            .packets_for_test()
            .max_value()
            .map(|packet_number| packet_number.as_u64())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        while self.accepted.try_recv().is_some() {}

        // Leak the frames the server submitted rather than drop them: they are never sent, and a
        // frame dropped while its writer still waits for its outcome fails a debug assertion.
        let waker = s2n_quic_core::task::waker::noop();
        let mut cx = core::task::Context::from_waker(&waker);
        loop {
            let mut batch = crate::endpoint::frame::PriorityStorage::default();
            match self.frame_rx.poll_swap(&mut cx, &mut batch) {
                core::task::Poll::Ready(Some(())) => core::mem::forget(batch),
                _ => break,
            }
        }
    }
}

/// An opening frame of each kind, arriving first, binds the stream's slot and hands the new
/// stream to its acceptor.
//= rendered/0005-endpoint/03-binding.md#DC-0005-017
//= type=test
//# A server MUST bind a new stream on the first opening frame for it that arrives, whether that frame is a data frame, a message frame or a reset.
#[test]
fn the_first_opening_frame_to_arrive_binds_the_stream() {
    for kind in Kind::ALL {
        let mut server = Server::new();
        assert!(
            !server.slot_is_bound(),
            "the slot must be free before any frame arrives"
        );

        let result = server.receive(kind, true, 0, false);
        let got = outcome(&result);
        assert!(
            result.is_ok(),
            "an authentic opening {kind:?} frame must be accepted, not {got}"
        );
        assert!(
            server.slot_is_bound(),
            "the first opening {kind:?} frame must bind the stream's slot"
        );
        assert_eq!(
            server.accepted.backlog(),
            1,
            "the first opening {kind:?} frame must hand the new stream to the acceptor"
        );
    }
}

/// A second opening frame for a stream the first one bound, of any kind, doesn't hand a second
/// stream to the acceptor.
//= rendered/0005-endpoint/03-binding.md#DC-0005-019
//= type=test
//# A server MUST NOT bind a stream again when an opening frame arrives for a stream it has already bound.
#[test]
fn an_opening_frame_for_a_bound_stream_doesnt_bind_it_again() {
    let pairs = [
        (Kind::Data, Kind::Data),
        (Kind::Data, Kind::Msg),
        (Kind::Data, Kind::Reset),
        (Kind::Msg, Kind::Data),
        (Kind::Msg, Kind::Msg),
        (Kind::Msg, Kind::Reset),
        (Kind::Reset, Kind::Data),
        (Kind::Reset, Kind::Msg),
        (Kind::Reset, Kind::Reset),
    ];
    for (first, second) in pairs {
        let mut server = Server::new();
        let result = server.receive(first, true, 0, false);
        let got = outcome(&result);
        assert!(
            result.is_ok(),
            "an authentic opening {first:?} frame must be accepted, not {got}"
        );
        assert_eq!(
            server.accepted.backlog(),
            1,
            "the first opening {first:?} frame must hand the new stream to the acceptor"
        );

        let result = server.receive(second, true, 1, false);
        let got = outcome(&result);
        assert!(
            result.is_ok(),
            "an authentic opening {second:?} frame after a {first:?} frame must be accepted, not \
             {got}"
        );
        assert_eq!(
            server.accepted.backlog(),
            1,
            "an opening {second:?} frame after an opening {first:?} frame must not bind the \
             stream again"
        );
        assert!(
            server.accepted.try_recv().is_some(),
            "the stream the first opening {first:?} frame bound must stay with the acceptor"
        );
        assert!(
            server.accepted.try_recv().is_none(),
            "an opening {second:?} frame after an opening {first:?} frame must not hand the \
             acceptor a second stream"
        );
    }
}

/// A frame that doesn't open a stream, routed at a queue the server hasn't bound, is rejected for
/// its routing. Its packet is acknowledged where it authenticates, and isn't where it fails
/// authentication, on the fast path (a message frame) and on the ordinary path (a data frame).
//= rendered/0005-endpoint/03-binding.md#DC-0005-021
//= type=test
//# A receiver MUST authenticate a packet whose frames it rejects for their routing before it acknowledges that packet.
#[test]
fn a_packet_rejected_for_its_routing_is_acknowledged_only_once_it_authenticates() {
    for kind in [Kind::Msg, Kind::Data] {
        let mut server = Server::new();

        let result = server.receive(kind, false, 0, false);
        let got = outcome(&result);
        assert!(
            result.is_ok(),
            "an authentic packet whose {kind:?} frame is routed at an unbound queue must be \
             accepted, not {got}"
        );
        assert_eq!(
            server.largest_acknowledged(),
            Some(0),
            "an authentic packet whose {kind:?} frame is rejected for its routing must be \
             acknowledged"
        );

        let result = server.receive(kind, false, 1, true);
        let got = outcome(&result);
        assert!(
            matches!(result, Err(Error::Decryption { .. })),
            "a forged packet whose {kind:?} frame is routed at an unbound queue must be rejected \
             as Decryption, not {got}"
        );
        assert_eq!(
            server.largest_acknowledged(),
            Some(0),
            "a forged packet whose {kind:?} frame is rejected for its routing must not be \
             acknowledged"
        );

        let result = server.receive(kind, false, 1, false);
        let got = outcome(&result);
        assert!(
            result.is_ok(),
            "the authentic packet with the forged packet's number must be accepted, not {got}"
        );
        assert_eq!(
            server.largest_acknowledged(),
            Some(1),
            "the authentic packet with the forged packet's number must be acknowledged"
        );
        assert!(
            !server.slot_is_bound(),
            "a {kind:?} frame that doesn't open a stream must not bind its slot"
        );
    }
}
