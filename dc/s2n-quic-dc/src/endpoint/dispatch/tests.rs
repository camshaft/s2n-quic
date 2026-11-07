// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

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

/// A single non-init QueueMsg frame routed at `(dest_queue_id, binding_id)`, sealed into a
/// pool-backed `Filled` packet. Returns the decryptable packet plus the matching opener.
///
/// The sealer (Client) and opener (Server) are derived from `fake_deterministic` path-secret
/// entries, so they share the same application key and the opener authenticates the sealer's
/// output. The frame's `dest_queue_id`/`binding_id` are carried in the (cleartext, AEAD-AAD)
/// application header — exactly the routing fields the fast path reads to steer dispatch.
fn sealed_queue_msg_packet(
    dest_queue_id: VarInt,
    binding_id: VarInt,
) -> (
    datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
    crate::crypto::awslc::open::Application,
    usize,
) {
    // Geometry consistent with the 17-byte payload: a single-chunk message.
    sealed_queue_msg_packet_geometry(dest_queue_id, binding_id, 17, 17, 0)
}

/// Like [`sealed_queue_msg_packet`] but lets the caller override the message-geometry header
/// fields (`message_size`, `chunk_size`, `chunk_index`). These are cleartext routing/geometry
/// fields carried as AEAD associated data, so an in-flight bit-flip of any of them produces a
/// packet exactly like this — one whose header decodes fine but whose geometry the receiver's
/// `MsgTable::insert` rejects (e.g. `chunk_index` past the implied chunk count → `OffsetOverflow`).
fn sealed_queue_msg_packet_geometry(
    dest_queue_id: VarInt,
    binding_id: VarInt,
    message_size: u64,
    chunk_size: u64,
    chunk_index: u64,
) -> (
    datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
    crate::crypto::awslc::open::Application,
    usize,
) {
    sealed_queue_packet(
        dest_queue_id,
        binding_id,
        None,
        message_size,
        chunk_size,
        chunk_index,
    )
}

/// Like [`sealed_queue_msg_packet`] for an init: the frame carries `dest_acceptor_id`, so the
/// server binds the slot for that acceptor before the scatter-decrypt runs.
fn sealed_queue_init_packet(
    dest_queue_id: VarInt,
    binding_id: VarInt,
    dest_acceptor_id: VarInt,
) -> (
    datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
    crate::crypto::awslc::open::Application,
    usize,
) {
    sealed_queue_packet(dest_queue_id, binding_id, Some(dest_acceptor_id), 17, 17, 0)
}

/// Seals the single QueueMsg frame that [`sealed_queue_msg_packet_geometry`] and
/// [`sealed_queue_init_packet`] describe.
fn sealed_queue_packet(
    dest_queue_id: VarInt,
    binding_id: VarInt,
    dest_acceptor_id: Option<VarInt>,
    message_size: u64,
    chunk_size: u64,
    chunk_index: u64,
) -> (
    datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
    crate::crypto::awslc::open::Application,
    usize,
) {
    let peer: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let sealer_entry = PathSecretEntry::builder(peer)
        .endpoint_type(endpoint::Type::Client)
        .build(None);
    let opener_entry = PathSecretEntry::builder(peer)
        .endpoint_type(endpoint::Type::Server)
        .build(None);

    let key_id = VarInt::ZERO;
    let sealer = sealer_entry.secret().application_sealer(key_id);
    let opener = opener_entry.secret().application_opener(key_id);
    let credentials = crate::credentials::Credentials {
        id: *sealer_entry.secret().id(),
        key_id,
    };

    let payload = b"queue-msg-payload".to_vec();

    let header = Header::QueueMsg {
        queue_pair: QueuePair {
            source_queue_id: VarInt::from_u8(7),
            dest_queue_id,
        },
        binding_id,
        msg_id: VarInt::ZERO,
        stream_offset: VarInt::ZERO,
        largest_offset: VarInt::new(payload.len() as u64).unwrap(),
        message_size: VarInt::new(message_size).unwrap(),
        chunk_size: VarInt::new(chunk_size).unwrap(),
        chunk_index: VarInt::new(chunk_index).unwrap(),
        is_fin: true,
        is_wakeup: true,
        blocked: false,
        dest_acceptor_id,
        priority: crate::credit::Priority::default(),
    };

    // Encode the single-frame application header: [Header][payload_len varint].
    let mut header_buf = vec![0u8; header.metadata_len(payload.len())];
    {
        let mut enc = s2n_codec::EncoderBuffer::new(&mut header_buf);
        enc.encode(&header);
        let plen = VarInt::new(payload.len() as u64).unwrap();
        enc.encode(&plen);
    }

    // Seal the datagram into a scratch buffer.
    let mut buf = vec![0u8; 65536];
    let routing_info = datagram::RoutingInfo::SenderId {
        source_sender_id: VarInt::from_u8(7),
    };
    let mut header_reader = &header_buf[..];
    let mut payload_reader = &payload[..];
    let encoded_len = datagram::encoder::encode(
        s2n_codec::EncoderBuffer::new(&mut buf),
        443,
        routing_info,
        Some(VarInt::ZERO),
        VarInt::try_from(header_buf.len() as u64).unwrap(),
        &mut header_reader,
        VarInt::try_from(payload.len() as u64).unwrap(),
        &mut payload_reader,
        &sealer,
        &credentials,
    );
    assert!(encoded_len > 0);

    // Copy the sealed bytes into a pool-backed `Filled` descriptor, mirroring the recv path.
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

    // Decode metadata from the filled bytes, then re-attach the storage — same as the router.
    let meta = {
        let decode_buf = DecoderBufferMut::new(&mut filled[..]);
        datagram::decoder::Meta::decode(&decode_buf, (), TAG_LEN).expect("meta decode")
    };
    let packet = meta.with_storage(filled).expect("with_storage");
    let decrypt_len = packet.decrypt_into_len();

    (packet, opener, decrypt_len)
}

/// Build a fresh (no slots bound) Server `QueueView`. Any QueueMsg routed at it rejects with
/// `Unallocated` *before* the scatter-decrypt runs — the un-authenticated reject path.
fn fresh_server_view() -> recv::QueueView {
    let entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .endpoint_type(endpoint::Type::Server)
        .build(None);
    match entry.queue_state() {
        crate::path::secret::map::entry::QueueState::Server(state) => {
            recv::QueueView::Server(state.view())
        }
        _ => unreachable!("server entry must have server queue state"),
    }
}

/// A server `QueueView` with the slot at `(queue_id, binding_id)` already bound, so a QueueMsg
/// routed there passes `validate_msg_dispatch` (binding matches) and reaches `MsgTable::insert`
/// — the geometry-validation stage. Used to exercise the live-stream insert-reject path, as
/// opposed to `fresh_server_view`'s unallocated-binding reject.
fn bound_server_view(queue_id: VarInt, binding_id: VarInt) -> recv::QueueView {
    let entry = PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
        .endpoint_type(endpoint::Type::Server)
        .build(None);
    let mut view = match entry.queue_state() {
        crate::path::secret::map::entry::QueueState::Server(state) => {
            recv::QueueView::Server(state.view())
        }
        _ => unreachable!("server entry must have server queue state"),
    };

    let (freed_batch_tx, freed_batch_rx) = crate::queue::freed_batch_channel();
    let mut freed_batch_tx = freed_batch_tx;
    let server = view.as_server_mut().expect("server view");
    let result = server.bind_for_msg(queue_id, binding_id, &entry, &mut freed_batch_tx);
    let crate::queue::BindResult::NewBinding {
        stream, control, ..
    } = result.expect("slot must bind for the test setup")
    else {
        panic!("expected NewBinding for a fresh slot");
    };

    // The receivers (`stream`/`control`) MUST stay alive: dropping them clears the slot's
    // HAS_RECEIVER flag and frees the binding, which would un-bind the slot and make the
    // dispatch below hit the `Unallocated` reject instead of the `MsgTable::insert` reject we
    // want to exercise. The path-secret `entry` likewise keeps the shared `ServerState` (and its
    // page table) alive. Leak all three for the short-lived test rather than threading them out.
    std::mem::forget(stream);
    std::mem::forget(control);
    std::mem::forget(freed_batch_rx);
    std::mem::forget(entry);
    view
}

/// Assembles the auxiliary args `decrypt_fast_path` needs but that play no role in the
/// reject path under test.
struct FastPathHarness {
    acceptor_registry: acceptor::LocalRegistry<Stream>,
    frame_tx: SubmissionSender,
    frame_rx: crate::endpoint::frame::SubmissionReceiver,
    freed_batch_tx: crate::queue::FreedBatchTx,
    counters: Arc<counters::Dispatch>,
    stream_clock: crate::time::DefaultClock,
    reader_metrics: Arc<crate::stream::metrics::ReaderMetrics>,
    writer_metrics: Arc<crate::stream::metrics::WriterMetrics>,
    send_credit_pool: crate::sync::Arc<crate::credit::Pool>,
    recv_credit_pool: crate::sync::Arc<crate::credit::Pool>,
    path_entry: Arc<PathSecretEntry>,
    _freed_batch_rx: crate::queue::FreedBatchRx,
}

impl FastPathHarness {
    fn new() -> Self {
        let registry = crate::counter::Registry::default();
        let (frame_tx, frame_rx) = crate::endpoint::frame::submission_channel(1);
        let (freed_batch_tx, _freed_batch_rx) = crate::queue::freed_batch_channel();
        Self {
            acceptor_registry: acceptor::Registry::<Stream>::new().local(),
            frame_tx,
            frame_rx,
            freed_batch_tx,
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
            path_entry: PathSecretEntry::builder("127.0.0.1:4433".parse().unwrap())
                .endpoint_type(endpoint::Type::Server)
                .build(None),
            _freed_batch_rx,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn call(
        &mut self,
        header: Header,
        opener: &crate::crypto::awslc::open::Application,
        packet: &datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
        decrypt_len: usize,
        queue_view: &mut recv::QueueView,
    ) -> Result<AutoWake, FastPathError> {
        decrypt_fast_path(
            header,
            opener,
            packet,
            decrypt_len,
            queue_view,
            &mut self.acceptor_registry,
            &mut self.frame_tx,
            &mut self.freed_batch_tx,
            &self.counters,
            &self.path_entry,
            &self.stream_clock,
            &self.reader_metrics,
            &self.writer_metrics,
            &self.send_credit_pool,
            &self.recv_credit_pool,
        )
    }

    /// Authenticates `packet` in place and settles the init the fast path rejected, as `process`
    /// does once it reads whether the packet number is new, and returns whether the packet
    /// authenticated.
    fn authenticate(
        &mut self,
        rejected: RejectedInit,
        packet: &mut datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
        opener: &crate::crypto::awslc::open::Application,
        queue_view: &mut recv::QueueView,
        new_packet_number: bool,
    ) -> bool {
        let authenticated = packet.decrypt_in_place(opener).is_ok();
        settle_rejected_init(
            rejected,
            authenticated && new_packet_number,
            queue_view,
            &self.path_entry,
            &mut self.frame_tx,
            &mut self.freed_batch_tx,
        );
        authenticated
    }
}

fn single_queue_msg_header(
    packet: &datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
    decrypt_len: usize,
) -> Header {
    decode::detect_single_queue_msg(packet.application_header(), decrypt_len)
        .expect("packet must be a single QueueMsg frame")
}

/// A QueueMsg routed to a binding the receiver rejects (here: an unallocated queue) takes the
/// fast-path reject branch *before* the scatter-decrypt, so the packet is never authenticated.
/// The fix surfaces this as `FastPathError::AuthForDrop` so the caller authenticates before
/// ACKing — rather than the old behavior of returning `Ok(AutoWake::default())`, which ACKed
/// the packet without ever verifying its AEAD tag.
#[test]
fn fast_path_unallocated_binding_requires_auth_before_ack() {
    let dest_queue_id = VarInt::from_u8(0);
    let binding_id = VarInt::from_u8(1);
    let (packet, opener, decrypt_len) = sealed_queue_msg_packet(dest_queue_id, binding_id);
    let header = single_queue_msg_header(&packet, decrypt_len);

    let mut view = fresh_server_view();
    let mut harness = FastPathHarness::new();

    let result = harness.call(header, &opener, &packet, decrypt_len, &mut view);
    assert!(
        matches!(result, Err(FastPathError::AuthForDrop)),
        "an un-authenticated binding reject must demand authentication before ACK, \
         not report success"
    );
}

/// End-to-end of the closure's `AuthForDrop` recovery: an authentic packet that routes to a
/// reject authenticates in place (→ ACK is safe), while a packet whose AEAD-AAD routing field
/// was corrupted in flight fails authentication (→ no ACK → the genuine packet retransmits).
///
/// This is the behavioral split the bug turned on: pre-fix BOTH cases were ACKed (the corrupted
/// one silently, never authenticated), so a corrupted-routing packet for a *live* stream made
/// the sender free that packet number and stop retransmitting — a permanent stream hole.
#[test]
fn fast_path_auth_for_drop_distinguishes_tampered_packet() {
    // Authentic packet routed to an unallocated queue: in-place auth succeeds.
    {
        let (mut packet, opener, decrypt_len) =
            sealed_queue_msg_packet(VarInt::from_u8(0), VarInt::from_u8(1));
        let header = single_queue_msg_header(&packet, decrypt_len);
        let mut view = fresh_server_view();
        let mut harness = FastPathHarness::new();

        let result = harness.call(header, &opener, &packet, decrypt_len, &mut view);
        assert!(matches!(result, Err(FastPathError::AuthForDrop)));
        // The closure authenticates in place before ACKing; an authentic packet passes.
        assert!(
            packet.decrypt_in_place(&opener).is_ok(),
            "authentic packet must authenticate so the ACK is legitimate"
        );
    }

    // Same scenario, but a byte the AEAD tag authenticates is flipped in flight: the reject
    // path is still taken, yet in-place authentication now fails, so the caller sends no ACK.
    {
        let (mut packet, opener, decrypt_len) =
            sealed_queue_msg_packet(VarInt::from_u8(0), VarInt::from_u8(1));
        let header = single_queue_msg_header(&packet, decrypt_len);
        let mut view = fresh_server_view();
        let mut harness = FastPathHarness::new();

        let result = harness.call(header, &opener, &packet, decrypt_len, &mut view);
        assert!(matches!(result, Err(FastPathError::AuthForDrop)));

        // Corrupt an authenticated byte, then confirm in-place auth fails → ACK suppressed →
        // the genuine packet is retransmitted rather than freed by a spurious ACK.
        packet.payload_mut()[0] ^= 0xFF;
        assert!(
            packet.decrypt_in_place(&opener).is_err(),
            "a tampered packet must fail authentication so its ACK is suppressed and the \
             genuine packet is retransmitted"
        );
    }
}

/// REPRO (fails on HEAD): a QueueMsg whose AEAD-authenticated *geometry* fields were corrupted in
/// flight routes to a LIVE, bound slot, passes `validate_msg_dispatch`, then is rejected by
/// `MsgTable::insert` (here `chunk_index` past the implied chunk count → `OffsetOverflow`). The
/// fast-path scatter-decrypt — the ONLY place the AEAD tag is checked on this path — never runs,
/// because `Slot::push_msg` swallows the insert error as `Ok((AutoWake::default(), 0))` BEFORE
/// invoking the write callback.
///
/// `decrypt_fast_path` therefore returns `Ok(AutoWake::default())` and `process` ACKs an
/// un-authenticated packet. This is the same vulnerability class the `AuthForDrop` fix closed for
/// the binding-reject path (`MsgError::Queue`), but the `MsgTable::insert` rejections take a
/// different return (`Ok`, not `MsgError::Queue`) and were missed. The consequence is identical:
/// corrupting an in-flight chunk's geometry makes the sender free that packet number and stop
/// retransmitting → permanent stream hole on a live stream.
///
/// The fix should route insert-rejections that fired before authentication through
/// `FastPathError::AuthForDrop` as well, so the closure authenticates in place and suppresses the
/// ACK on a tampered packet.
#[test]
fn fast_path_insert_reject_requires_auth_before_ack() {
    let dest_queue_id = VarInt::from_u8(0);
    let binding_id = VarInt::from_u8(1);

    // message_size=17, chunk_size=17 → exactly 1 chunk (index 0). A corrupted chunk_index of 5
    // is past that count, so MsgTable::insert returns OffsetOverflow.
    let (packet, opener, decrypt_len) =
        sealed_queue_msg_packet_geometry(dest_queue_id, binding_id, 17, 17, 5);
    let header = single_queue_msg_header(&packet, decrypt_len);

    let mut view = bound_server_view(dest_queue_id, binding_id);
    // Sanity: confirm the slot is actually bound, so we exercise the `MsgTable::insert` reject
    // path and NOT the unallocated-binding reject (which returns AuthForDrop for a different
    // reason — without this guard the test could pass even if the binding silently failed).
    {
        let server = view.as_server_mut().unwrap();
        // Use a non-front msg_id (10) so the message completes but does NOT drain (the table
        // front is still the never-sent msg_id 0), leaving `base_id == 0`. That keeps msg_id 0
        // fresh for the corrupted packet below, so it hits `OffsetOverflow` (a fresh entry with
        // 1 chunk vs chunk_index 5) rather than `Stale`.
        let probe = server.send_msg::<core::convert::Infallible>(
            dest_queue_id,
            binding_id,
            10, // msg_id (non-front, stays buffered)
            0,  // stream_offset
            17, // peer_max_offset
            17, // message_size
            17, // chunk_size
            0,  // chunk_index (valid: 1-chunk message)
            17, // payload_len
            false,
            false,
            false,
            |ptr, len| {
                unsafe { core::ptr::write_bytes(ptr, 0, len as usize) };
                Ok(())
            },
        );
        assert!(
            probe.is_ok(),
            "setup: a VALID-geometry QueueMsg to this slot must succeed (Ok), proving the slot is \
             bound; if this were an Unallocated error the test below would pass for the wrong reason"
        );
    }
    let mut harness = FastPathHarness::new();

    let result = harness.call(header, &opener, &packet, decrypt_len, &mut view);
    assert!(
        matches!(result, Err(FastPathError::AuthForDrop)),
        "a QueueMsg rejected by MsgTable::insert BEFORE the scatter-decrypt was never \
         authenticated, so the fast path must demand auth-before-ACK (AuthForDrop) rather than \
         report success and let `process` ACK an un-verified packet — got {}",
        match result {
            Ok(_) => "Ok (packet would be ACKed un-authenticated — BUG)",
            Err(FastPathError::HeaderMismatch) => "HeaderMismatch",
            Err(FastPathError::WriteFailed) => "WriteFailed",
            Err(FastPathError::AuthForDrop) => "AuthForDrop",
            Err(FastPathError::RejectInit(_)) => "RejectInit",
        }
    );
}

/// `QueueView::slot_diag` reports the slot's real current state — the data the enriched QueueDbg
/// delivery-failure log relies on to show a binding divergence. A bound slot reports its stored
/// binding with both halves live; an unbound slot reports `unallocated` with no receivers.
#[test]
fn slot_diag_reports_bound_vs_unallocated_state() {
    let queue_id = VarInt::from_u8(7);
    let binding_id = VarInt::from_u8(42);

    // Bound slot: stored binding matches, both halves attached, queues empty.
    let mut bound = bound_server_view(queue_id, binding_id);
    let diag = bound
        .slot_diag(queue_id)
        .expect("bound slot must be present");
    assert!(!diag.unallocated, "a freshly bound slot is allocated");
    assert_eq!(
        diag.stored_binding,
        binding_id.as_u64(),
        "slot must report the binding it was bound with"
    );
    assert!(diag.stream_has_receiver && diag.control_has_receiver);
    assert_eq!(diag.stream_depth, 0);
    assert_eq!(diag.control_depth, 0);

    // A never-bound slot in the same view reads back as unallocated with no receiver attached —
    // exactly the divergence a stale/unallocated QueueDbg delivery surfaces.
    let other_queue_id = VarInt::from_u8(9);
    assert!(
        bound
            .slot_diag(other_queue_id)
            .is_none_or(|d| d.unallocated && !d.stream_has_receiver),
        "an unbound slot must read back as unallocated with no receiver"
    );
}

/// Drains the frames the dispatch submitted to `frame_rx`, and returns each reset among them by
/// the slot it resets and its binding id.
fn take_resets(frame_rx: &mut crate::endpoint::frame::SubmissionReceiver) -> Vec<(u64, u64)> {
    let waker = s2n_quic_core::task::waker::noop();
    let mut cx = core::task::Context::from_waker(&waker);
    let mut resets = Vec::new();
    loop {
        let mut storage = crate::endpoint::frame::PriorityStorage::default();
        let core::task::Poll::Ready(Some(())) = frame_rx.poll_swap(&mut cx, &mut storage) else {
            break;
        };
        for frame in storage.iter() {
            if let Header::QueueReset {
                queue_pair,
                binding_id,
                ..
            } = frame.header
            {
                resets.push((queue_pair.source_queue_id.as_u64(), binding_id.as_u64()));
            }
        }
    }
    resets
}

/// The inits a peer sends while the creditless window refuses frames can't add a reset for each
/// rejected init: each slot of the peer's queues gets the reset of its first rejected init alone,
/// whatever the binding ids of the inits routed to it, until that reset leaves the priority router
/// and releases the slot. A slot past the peer's queues doesn't get a reset.
#[test]
fn rejected_inits_leave_one_reset_for_each_slot_until_it_leaves() {
    let state = Arc::new(crate::queue::ServerState::new(VarInt::from_u8(4), 0));
    let mut view = recv::QueueView::Server(state.view());
    let mut harness = FastPathHarness::new();

    let init = |harness: &mut FastPathHarness, view: &mut recv::QueueView, slot: u8, binding: u8| {
        // The registry doesn't hold an acceptor, so the server rejects the init.
        let (mut packet, opener, decrypt_len) = sealed_queue_init_packet(
            VarInt::from_u8(slot),
            VarInt::from_u8(binding),
            VarInt::from_u8(9),
        );
        let header = single_queue_msg_header(&packet, decrypt_len);
        let result = harness.call(header, &opener, &packet, decrypt_len, view);
        let Err(FastPathError::RejectInit(rejected)) = result else {
            panic!("the server rejects an init without an acceptor");
        };
        assert!(
            harness.authenticate(rejected, &mut packet, &opener, view, true),
            "the init is sealed with its own header, so it authenticates"
        );
    };

    // Three bindings each send an init to eight slots, and none of the resets leaves.
    for binding in 1..=3 {
        for slot in 0..8 {
            init(&mut harness, &mut view, slot, binding);
        }
    }
    assert_eq!(
        take_resets(&mut harness.frame_rx),
        vec![(0, 1), (1, 1), (2, 1), (3, 1)],
        "each of the peer's 4 queues gets the reset of its first rejected init alone"
    );
    // The slots past the peer's queues are recorded as freed, and each held slot waits on its
    // reset.
    let mut freed = crate::bitset::HierarchicalBitSet::new(1);
    state.freed.take(&mut freed).expect("the slots past the peer's queues are freed");
    assert!((4..8).all(|slot| freed.contains(slot)));
    assert!((0..4).all(|slot| !freed.contains(slot)), "a held slot waits on its reset");

    // Slot 2's reset leaves the priority router: slot 2 gets the reset of its next rejected init,
    // and slot 3, whose reset still waits, doesn't.
    state.freed.release(
        VarInt::from_u8(2),
        VarInt::from_u8(1),
        &harness.path_entry,
        &mut harness.freed_batch_tx,
    );
    let mut freed = crate::bitset::HierarchicalBitSet::new(1);
    state.freed.take(&mut freed).expect("the release records slot 2 as freed");
    assert!(freed.contains(2) && !freed.contains(3));
    init(&mut harness, &mut view, 2, 4);
    init(&mut harness, &mut view, 3, 4);
    assert_eq!(take_resets(&mut harness.frame_rx), vec![(2, 4)]);
}

/// An init whose packet fails the AEAD check doesn't hold its slot, record it as freed or send a
/// reset: a tampered or forged init can't hold a slot for its own binding and so keep the reset of
/// the client's genuine rejected init on that slot from being sent.
#[test]
fn a_rejected_init_that_fails_authentication_leaves_the_slot_unheld() {
    let state = Arc::new(crate::queue::ServerState::new(VarInt::from_u8(4), 0));
    let mut view = recv::QueueView::Server(state.view());
    let mut harness = FastPathHarness::new();

    // An init on slot 1 for binding 2 whose payload is changed on the network.
    let (mut packet, opener, decrypt_len) =
        sealed_queue_init_packet(VarInt::from_u8(1), VarInt::from_u8(2), VarInt::from_u8(9));
    let header = single_queue_msg_header(&packet, decrypt_len);
    let result = harness.call(header, &opener, &packet, decrypt_len, &mut view);
    let Err(FastPathError::RejectInit(rejected)) = result else {
        panic!("the server rejects an init without an acceptor");
    };
    packet.payload_mut()[0] ^= 0xFF;
    assert!(
        !harness.authenticate(rejected, &mut packet, &opener, &mut view, true),
        "a tampered packet fails the AEAD check, so it isn't ACKed"
    );
    assert!(
        take_resets(&mut harness.frame_rx).is_empty(),
        "a packet that fails the AEAD check doesn't send a reset"
    );
    let mut freed = crate::bitset::HierarchicalBitSet::new(1);
    assert!(
        state.freed.take(&mut freed).is_none(),
        "a packet that fails the AEAD check doesn't record its slot as freed"
    );

    // The client's genuine init on slot 1 for binding 1 then holds the slot and gets its reset.
    let (mut packet, opener, decrypt_len) =
        sealed_queue_init_packet(VarInt::from_u8(1), VarInt::from_u8(1), VarInt::from_u8(9));
    let header = single_queue_msg_header(&packet, decrypt_len);
    let result = harness.call(header, &opener, &packet, decrypt_len, &mut view);
    let Err(FastPathError::RejectInit(rejected)) = result else {
        panic!("the server rejects an init without an acceptor");
    };
    assert!(harness.authenticate(rejected, &mut packet, &opener, &mut view, true));
    assert_eq!(take_resets(&mut harness.frame_rx), vec![(1, 1)]);
}

/// A copy of an authentic rejected init whose packet number the endpoint took already, as a
/// replayed packet, leaves the slot as the first copy left it: it doesn't hold the slot, record it
/// as freed or send a reset, so the genuine rejected init of the next binding on the slot gets its
/// reset.
#[test]
fn a_replayed_rejected_init_leaves_the_slot_as_the_first_copy_left_it() {
    let state = Arc::new(crate::queue::ServerState::new(VarInt::from_u8(4), 0));
    let mut view = recv::QueueView::Server(state.view());
    let mut harness = FastPathHarness::new();

    let init = |harness: &mut FastPathHarness,
                view: &mut recv::QueueView,
                binding: u8,
                new_packet_number: bool| {
        // The registry doesn't hold an acceptor, so the server rejects the init.
        let (mut packet, opener, decrypt_len) = sealed_queue_init_packet(
            VarInt::from_u8(1),
            VarInt::from_u8(binding),
            VarInt::from_u8(9),
        );
        let header = single_queue_msg_header(&packet, decrypt_len);
        let result = harness.call(header, &opener, &packet, decrypt_len, view);
        let Err(FastPathError::RejectInit(rejected)) = result else {
            panic!("the server rejects an init without an acceptor");
        };
        harness.authenticate(rejected, &mut packet, &opener, view, new_packet_number)
    };

    // Binding 1's init on slot 1 is rejected with a reset, and the reset leaves.
    assert!(init(&mut harness, &mut view, 1, true));
    assert_eq!(take_resets(&mut harness.frame_rx), vec![(1, 1)]);
    state.freed.release(
        VarInt::from_u8(1),
        VarInt::from_u8(1),
        &harness.path_entry,
        &mut harness.freed_batch_tx,
    );
    let mut freed = crate::bitset::HierarchicalBitSet::new(1);
    state.freed.take(&mut freed).expect("the release records slot 1 as freed");

    // A copy of that packet comes again: it authenticates, and its packet number was taken.
    assert!(init(&mut harness, &mut view, 1, false));
    assert!(
        take_resets(&mut harness.frame_rx).is_empty(),
        "a replayed init doesn't send a reset"
    );
    assert!(
        state.freed.take(&mut freed).is_none(),
        "a replayed init doesn't record its slot as freed"
    );

    // Binding 2's genuine init on slot 1 then holds the slot and gets its reset.
    assert!(init(&mut harness, &mut view, 2, true));
    assert_eq!(take_resets(&mut harness.frame_rx), vec![(1, 2)]);
}

/// `process` settles an init the fast path rejected with what the peer's dedup filter answered
/// for its packet number: the first copy of an authentic init on slot 1 holds the slot and gets
/// its reset, and once that reset leaves, a copy with the same packet number is dropped as a
/// duplicate, and doesn't hold the slot, record it as freed or send a reset.
#[test]
fn a_rejected_init_whose_packet_number_repeats_is_a_duplicate_that_leaves_the_slot() {
    let state = Arc::new(crate::queue::ServerState::new(VarInt::from_u8(4), 0));
    let mut view = recv::QueueView::Server(state.view());
    let mut harness = FastPathHarness::new();
    let mut dedup_filter = recv::dedup::StreamFilter::default();

    let init = |harness: &mut FastPathHarness,
                view: &mut recv::QueueView,
                dedup_filter: &mut recv::dedup::StreamFilter| {
        // The registry doesn't hold an acceptor, so the server rejects the init, and the packet
        // authenticates in place, as in the decrypt closure of `process`.
        let (mut packet, opener, decrypt_len) =
            sealed_queue_init_packet(VarInt::from_u8(1), VarInt::from_u8(1), VarInt::from_u8(9));
        let header = single_queue_msg_header(&packet, decrypt_len);
        let result = harness.call(header, &opener, &packet, decrypt_len, view);
        let Err(FastPathError::RejectInit(rejected)) = result else {
            panic!("the server rejects an init without an acceptor");
        };
        assert!(
            packet.decrypt_in_place(&opener).is_ok(),
            "the init is sealed with its own header, so it authenticates"
        );
        let packet_number = packet.packet_number();
        let dedup = dedup_filter.on_packet_number(packet_number);
        let settled = settle_decrypt_result(
            DecryptResult::RejectedInit(rejected),
            dedup,
            *packet.credentials(),
            packet_number,
            VarInt::from_u8(7),
            view,
            &harness.path_entry,
            &mut harness.frame_tx,
            &mut harness.freed_batch_tx,
        );
        (packet_number, settled)
    };

    // The first copy's packet number is new: it holds slot 1 for binding 1 and gets its reset.
    let (_, settled) = init(&mut harness, &mut view, &mut dedup_filter);
    assert!(matches!(settled, Ok(Ok(_))), "a new packet number is taken");
    assert_eq!(take_resets(&mut harness.frame_rx), vec![(1, 1)]);

    // The reset leaves the priority router, which ends the hold and records slot 1 as freed.
    state.freed.release(
        VarInt::from_u8(1),
        VarInt::from_u8(1),
        &harness.path_entry,
        &mut harness.freed_batch_tx,
    );
    let mut freed = crate::bitset::HierarchicalBitSet::new(1);
    state.freed.take(&mut freed).expect("the release records slot 1 as freed");

    // A copy with the same packet number comes again: the dedup filter refuses its number, so
    // it is dropped as a duplicate and leaves slot 1 as the release left it.
    let (packet_number, settled) = init(&mut harness, &mut view, &mut dedup_filter);
    let Err(Error::Duplicate {
        packet_number: duplicate,
        ..
    }) = settled
    else {
        panic!("a packet number the dedup filter refuses is a duplicate");
    };
    assert_eq!(duplicate, packet_number);
    assert!(
        take_resets(&mut harness.frame_rx).is_empty(),
        "a duplicate doesn't send a reset"
    );
    assert!(
        state.freed.take(&mut freed).is_none(),
        "a duplicate doesn't record its slot as freed"
    );
    assert!(
        state.freed.hold(VarInt::from_u8(1), VarInt::from_u8(2)),
        "a duplicate doesn't hold slot 1"
    );
}

/// A rejected init whose slot `bind_for_msg` bound disables the bound stream as the reject drops,
/// as where the path secret map refuses its packet after the decrypt: the stream's reader doesn't
/// reset the binding, so a reset doesn't leave the priority router and release the hold of the
/// slot's genuine rejected init.
#[test]
fn a_dropped_rejected_init_disables_its_bound_stream() {
    let state = Arc::new(crate::queue::ServerState::new(VarInt::from_u8(4), 0));
    let mut view = recv::QueueView::Server(state.view());
    let mut harness = FastPathHarness::new();
    // The registry holds an acceptor whose receiver is dropped, so the server binds the slot
    // and then rejects the init with the stream it bound.
    let registry = acceptor::Registry::<Stream>::new();
    let receiver = registry
        .register(VarInt::from_u8(9), Default::default())
        .expect("the acceptor registers");
    drop(receiver);
    harness.acceptor_registry = registry.local();

    let (packet, opener, decrypt_len) =
        sealed_queue_init_packet(VarInt::from_u8(1), VarInt::from_u8(1), VarInt::from_u8(9));
    let header = single_queue_msg_header(&packet, decrypt_len);
    let result = harness.call(header, &opener, &packet, decrypt_len, &mut view);
    let Err(FastPathError::RejectInit(rejected)) = result else {
        panic!("the server rejects an init whose acceptor's receiver is dropped");
    };
    assert!(rejected.stream.is_some(), "the reject holds the stream it bound");

    drop(rejected);
    assert!(
        take_resets(&mut harness.frame_rx).is_empty(),
        "a dropped reject disables its stream, so its reader doesn't send a reset"
    );
}

/// Drops each value it's sent. The rejections the tests below check return from `process` before
/// it sends anything on its channels.
struct Discard;

impl<T> channel::UnboundedSender<T> for Discard {
    fn send(&mut self, _value: T) -> Result<(), T> {
        Ok(())
    }
}

/// A single QueueMsg frame, routed at an unallocated queue, sealed under `key_id` by the client
/// entry for `peer`, with `source_sender_id` as the sender id in its routing info. Its credential
/// id is the client's `peer_id`, the id of the server entry for `peer`, so a path secret map that
/// holds that server entry finds the packet's credential and opens the packet. Returns the packet
/// and its credentials.
fn sealed_client_packet(
    peer: SocketAddr,
    key_id: VarInt,
    source_sender_id: VarInt,
) -> (
    datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
    Credentials,
) {
    let sealer_entry = PathSecretEntry::builder(peer)
        .endpoint_type(endpoint::Type::Client)
        .build(None);
    let sealer = sealer_entry.secret().application_sealer(key_id);
    let credentials = Credentials {
        id: sealer_entry.secret().peer_id(),
        key_id,
    };

    let payload = b"queue-msg-payload".to_vec();
    let payload_len = VarInt::new(payload.len() as u64).unwrap();
    let header = Header::QueueMsg {
        queue_pair: QueuePair {
            source_queue_id: VarInt::from_u8(7),
            dest_queue_id: VarInt::ZERO,
        },
        binding_id: VarInt::from_u8(1),
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

    let mut header_buf = vec![0u8; header.metadata_len(payload.len())];
    {
        let mut enc = s2n_codec::EncoderBuffer::new(&mut header_buf);
        enc.encode(&header);
        enc.encode(&payload_len);
    }

    let mut buf = vec![0u8; 65536];
    let mut header_reader = &header_buf[..];
    let mut payload_reader = &payload[..];
    let encoded_len = datagram::encoder::encode(
        s2n_codec::EncoderBuffer::new(&mut buf),
        443,
        datagram::RoutingInfo::SenderId { source_sender_id },
        Some(VarInt::ZERO),
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

/// Runs `process` on `packet` with an empty receive cache, so that `process` looks up the
/// packet's credential in `path_secret_map` and, where the map holds it, checks the packet's key
/// id against the receiver's replay window.
fn process_uncached(
    packet: datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
    path_secret_map: &PathSecretMap,
) -> Result<(), Error> {
    let mut harness = FastPathHarness::new();
    let mut recv_cache = recv::Cache::new(crate::endpoint::id::RecvDispatchWorkerId::new(0));
    let clock = crate::time::testing::Clock::new(Duration::from_secs(1));
    let route = <routing::ModuloRoute as routing::SenderRoute>::new(1);
    process(
        Entry::new(packet),
        &mut recv_cache,
        &mut Discard,
        &mut Discard,
        path_secret_map,
        &mut harness.acceptor_registry,
        &mut harness.frame_tx,
        &mut Discard,
        &mut harness.freed_batch_tx,
        &clock,
        &harness.counters,
        &route,
        &mut Discard,
        &harness.stream_clock,
        &harness.reader_metrics,
        &harness.writer_metrics,
        &harness.send_credit_pool,
        &harness.recv_credit_pool,
    )
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

/// The peer's sender ids of the packets each test below sends. Neither is 0, and they differ, so
/// an error whose `sender_id` is any one constant doesn't match both packets.
const SOURCE_SENDER_IDS: [u8; 2] = [3, 9];

/// `process` rejects a packet whose credential the path secret map doesn't hold as
/// `PeerStateLookup`, with the packet's credentials and the sender id the packet came from.
/// `ups::Pending` keys the UnknownPathSecret response by those two, so the response for one sender
/// id doesn't keep the response for another sender id out of the queue.
#[test]
fn peer_state_lookup_carries_the_rejected_packets_sender_id() {
    let path_secret_map = crate::path::secret::map::testing::new(16);
    let peer: SocketAddr = "127.0.0.1:8080".parse().unwrap();

    for source_sender_id in SOURCE_SENDER_IDS.map(VarInt::from_u8) {
        let (packet, credentials) = sealed_client_packet(peer, VarInt::ZERO, source_sender_id);
        assert!(
            path_secret_map.get_by_id(&credentials.id).is_none(),
            "the map must not hold the packet's credential"
        );

        let result = process_uncached(packet, &path_secret_map);
        let got = outcome(&result);
        let Err(Error::PeerStateLookup {
            credentials: rejected,
            sender_id,
            ..
        }) = result
        else {
            panic!(
                "a packet whose credential the map doesn't hold must be rejected as \
                 PeerStateLookup, not {got}"
            );
        };
        assert_eq!(
            rejected, credentials,
            "PeerStateLookup must carry the rejected packet's credentials"
        );
        assert_eq!(
            sender_id, source_sender_id,
            "PeerStateLookup must carry the sender id of the rejected packet"
        );
    }
}

/// `process` rejects a packet whose key id the receiver has seen already as `StaleKey`, with the
/// packet's credentials and the sender id the packet came from. `ups::Pending` keys the
/// ReplayDetected response by those two, so the response for one sender id doesn't keep the
/// response for another sender id out of the queue.
#[test]
fn stale_key_carries_the_rejected_packets_sender_id() {
    let peer: SocketAddr = "127.0.0.1:8080".parse().unwrap();

    for source_sender_id in SOURCE_SENDER_IDS.map(VarInt::from_u8) {
        let path_secret_map = crate::path::secret::map::testing::new(16);
        path_secret_map.test_insert(peer, endpoint::Type::Server);
        let (packet, credentials) = sealed_client_packet(peer, VarInt::ZERO, source_sender_id);
        let entry = path_secret_map
            .get_by_id(&credentials.id)
            .expect("the map must hold the server entry for the packet's credential");

        // The receiver sees the packet's key id once before the packet arrives, as it does for an
        // earlier packet under the same key.
        let first = path_secret_map.check_dedup(&entry, &credentials, None, &mut Vec::new());
        assert!(
            first.is_ok(),
            "the receiver must accept the key id the first time it sees it"
        );

        let result = process_uncached(packet, &path_secret_map);
        let got = outcome(&result);
        let Err(Error::StaleKey {
            credentials: rejected,
            sender_id,
            ..
        }) = result
        else {
            panic!(
                "a packet whose key id the receiver has seen already must be rejected as \
                 StaleKey, not {got}"
            );
        };
        assert_eq!(
            rejected, credentials,
            "StaleKey must carry the rejected packet's credentials"
        );
        assert_eq!(
            sender_id, source_sender_id,
            "StaleKey must carry the sender id of the rejected packet"
        );
    }
}
