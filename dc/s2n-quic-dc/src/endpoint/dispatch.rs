// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Inbound packet processing: decrypt, deduplicate, and dispatch frames to flow queues.
//!
//! A single received packet may contain multiple frames (the frame aggregation model).
//! After decryption and packet-number deduplication, we iterate the frame metadata region
//! and dispatch each frame to its appropriate handler based on the frame header type.

use crate::{
    acceptor,
    byte_vec::ByteVec,
    credentials::Credentials,
    endpoint::{
        counters, decode, error,
        frame::{Frame, Header, PriorityInput, SubmissionSender, DEFAULT_TTL},
        id::LocalSenderId,
        msg,
        recv,
        routing,
    },
    flow::{self, queue::AutoWake},
    intrusive::Entry,
    packet::{
        self,
        datagram::{QueuePair, ResetTarget, RoutingInfo},
    },
    path::secret::{map::Entry as PathSecretEntry, Map as PathSecretMap},
    socket::{channel, pool::descriptor},
    stream::{PendingValidation, Reader, Stream, Writer},
    tracing::*,
};
use bytes::BytesMut;
use core::time::Duration;
use s2n_quic_core::varint::VarInt;
use std::{cell::RefCell, rc::Rc};

pub(crate) enum Error {
    PeerStateLookup {
        dest_addr: crate::msg::addr::Addr,
        credentials: Credentials,
        control_out: Vec<u8>,
    },
    Decryption {
        credentials: Credentials,
        packet_number: VarInt,
    },
    Duplicate {
        credentials: Credentials,
        packet_number: VarInt,
    },
    /// `check_dedup` detected that the key-id was already registered (definite replay)
    /// or outside the replay window (possible replay / too old).  The peer should be
    /// notified to trigger a re-handshake.
    StaleKey {
        dest_addr: crate::msg::addr::Addr,
        credentials: Credentials,
        packet_number: VarInt,
        control_out: Vec<u8>,
    },
    MissingSenderId,
}

/// Process a received datagram packet.
///
/// Authenticates (decrypt), deduplicates by packet number, updates ACK state, then
/// dispatches each frame in the packet to its type-specific handler. Response frames
/// (ACKs, QueueReset) are emitted to `response_tx`.
pub(crate) fn process<Clk, Route>(
    packet: Entry<packet::datagram::decoder::Packet<descriptor::Filled>>,
    recv_cache: &mut recv::Cache,
    ack_burst_tx: &mut impl channel::UnboundedSender<Rc<RefCell<recv::Context>>>,
    idle_wheel_tx: &mut impl channel::UnboundedSender<Rc<RefCell<recv::Context>>>,
    path_secret_map: &PathSecretMap,
    acceptor_registry: &mut acceptor::LocalRegistry<PendingValidation>,
    frame_tx: &SubmissionSender,
    response_tx: &mut impl channel::UnboundedSender<PriorityInput>,
    sender_tx: &mut impl channel::UnboundedSender<Entry<msg::Sender>>,
    clock: &Clk,
    counters: &counters::Dispatch,
    route: &Route,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
) -> Result<(), Error>
where
    Clk: s2n_quic_core::time::Clock + crate::time::precision::Clock + ?Sized,
    Route: routing::SenderRoute,
{
    let credentials = *packet.credentials();
    let packet_number = packet.packet_number();
    let routing_info = packet.routing_info();

    let source_sender_id = match routing_info {
        RoutingInfo::SenderId { source_sender_id } => source_sender_id,
        RoutingInfo::None => return Err(Error::MissingSenderId),
    };

    // Collect the fields we need before the closure captures `packet`.
    let app_header_slice: &[u8] = packet.application_header();
    let decrypt_len = packet.decrypt_into_len();
    let ecn = packet.storage().ecn();

    // Get or create peer receive state, decrypting the packet on-demand.
    //
    // The decrypt closure is invoked with the opener (cached on hit, freshly derived
    // on miss).  `post_authentication` is called inside `get_or_insert` only on a
    // cache miss — recording the key-id in the receiver's replay window for the first
    // packet of a new session.  Cache hits skip `post_authentication` because many
    // packets legitimately share the same key-id within a session; per-packet replay
    // protection is handled by `dedup_filter` inside the `Context`.  On a cache miss
    // the Context is inserted only once both decrypt and `post_authentication` succeed,
    // preventing stale path-secret entries from poisoning the cache.
    let mut control_out = Vec::new();
    let decrypt_fn = |opener: &crate::crypto::awslc::open::Application| {
        let _guard = counters.rx_decrypt_time.start();
        let mut buf = BytesMut::with_capacity(decrypt_len);
        let written = packet
            .decrypt_into(opener, bytes::BufMut::chunk_mut(&mut buf))
            .map_err(|err| {
                warn!(
                    %credentials,
                    packet_number = packet_number.as_u64(),
                    error = %err,
                    "decrypt_into failed"
                );
            })
            .ok()?;
        if written != decrypt_len {
            warn!(
                %credentials,
                packet_number = packet_number.as_u64(),
                expected_len = decrypt_len,
                actual_len = written,
                "decrypt_into wrote an unexpected number of bytes"
            );
            return None;
        }
        // SAFETY: `buf` was allocated with `with_capacity(decrypt_len)` and
        // `chunk_mut` exposed exactly that region to `decrypt_into`, which
        // initialized `decrypt_len` bytes.  We returned early unless
        // `written == decrypt_len`.
        unsafe { buf.set_len(decrypt_len) };
        Some(buf)
    };
    let (decrypted, peer_rc, cache_hit) = {
        let _guard = counters.rx_peer_lookup_time.start();
        let remote_addr = packet.storage().remote_address().get();
        match recv_cache.get_or_insert(
            &credentials,
            crate::endpoint::id::RemoteSenderId::new(source_sender_id),
            path_secret_map,
            clock,
            remote_addr,
            &mut control_out,
            route,
            decrypt_fn,
        ) {
            Ok(v) => v,
            Err(recv::CacheError::PathSecretNotFound) => {
                let mut dest_addr = crate::msg::addr::Addr::new(remote_addr);
                dest_addr.set_port(packet.source_control_port());
                return Err(Error::PeerStateLookup {
                    dest_addr,
                    credentials,
                    control_out,
                });
            }
            Err(recv::CacheError::DecryptFailed) => {
                warn!(
                    %credentials,
                    packet_number = packet_number.as_u64(),
                    "failed to decrypt packet"
                );
                return Err(Error::Decryption {
                    credentials,
                    packet_number,
                });
            }
            Err(recv::CacheError::ReplayDetected) => {
                let mut dest_addr = crate::msg::addr::Addr::new(remote_addr);
                dest_addr.set_port(packet.source_control_port());
                return Err(Error::StaleKey {
                    dest_addr,
                    credentials,
                    packet_number,
                    control_out,
                });
            }
        }
    };
    if cache_hit {
        counters.rx_peer_cache_hit.add(1);
    } else {
        counters.rx_peer_cache_miss.add(1);
        let _ = idle_wheel_tx.send(peer_rc.clone());
    }
    let mut peer = peer_rc.borrow_mut();

    // Packet number deduplication
    if peer.dedup_filter.on_packet_number(packet_number).is_err() {
        return Err(Error::Duplicate {
            credentials,
            packet_number,
        });
    }

    // Update activity tracking on the shared PathSecretEntry
    peer.path_entry
        .touch_activity(crate::time::precision::Clock::now(clock));
    peer.ecn_counts.increment(ecn);
    counters.on_ecn(ecn);
    let now = clock.get_time();
    peer.ack_ranges.on_packet_received(packet_number, now);

    counters.rx_packet_size.record_value(decrypt_len as u64);

    let mut payload_storage = decrypted;

    let mut response_frames = PriorityInput::default();

    // Multi-frame packet: `app_header_slice` contains the per-frame metadata
    // (Header type tag + optional payload_len VarInt) and `payload_storage`
    // contains the concatenated, decrypted frame payloads.

    let _dispatch_guard = counters.rx_dispatch_time.start();
    let mut is_ack_eliciting = false;
    let mut frame_count = 0u64;
    for result in decode::decode_frames(app_header_slice) {
        match result {
            Ok((header, frame_payload_len)) => {
                frame_count += 1;
                counters.on_received_frame(&header);
                // Validate that the claimed payload length fits within the
                // remaining payload storage.
                if frame_payload_len > payload_storage.len() {
                    warn!(
                        %credentials,
                        packet_number = packet_number.as_u64(),
                        frame_payload_len,
                        remaining = payload_storage.len(),
                        "frame payload length exceeds remaining packet payload"
                    );
                    break;
                }

                if header.is_ack_eliciting() {
                    is_ack_eliciting = true;
                }

                // Split the frame's payload out of the shared storage.
                let frame_payload = payload_storage.split_to(frame_payload_len);
                dispatch_decoded_frame(
                    header,
                    source_sender_id,
                    frame_payload,
                    &mut peer,
                    &credentials,
                    acceptor_registry,
                    frame_tx,
                    sender_tx,
                    counters,
                    &mut response_frames,
                    waker_sink,
                );
            }
            Err(err) => {
                warn!(
                    %credentials,
                    packet_number = packet_number.as_u64(),
                    ?err,
                    "failed to decode multi-frame packet metadata"
                );
                break;
            }
        }
    }

    if !payload_storage.is_empty() {
        warn!(
            %credentials,
            packet_number = packet_number.as_u64(),
            remaining = payload_storage.len(),
            "multi-frame packet has unconsumed payload bytes"
        );
    }

    counters.rx_frames_per_packet.record_value(frame_count);

    let mut enqueue_pending_ack = false;
    if is_ack_eliciting {
        match peer.ack_state.on_ack_eliciting() {
            Ok(()) | Err(s2n_quic_core::state::Error::NoOp { .. }) => {}
            Err(s2n_quic_core::state::Error::InvalidTransition { .. }) => {
                counters.rx_ack_state_impossible.add(1);
                debug_assert!(false, "on_ack_eliciting transition failed");
            }
        }

        // Only enqueue into the burst queue when the state is Scheduled.
        // When FlushedStale, the ack_completion_task handles re-encoding after
        // the in-flight ACK completes — enqueueing here would leave a stale link
        // that outlives the Scheduled state.
        if !peer.ack_burst.is_linked() && peer.ack_state.is_scheduled() {
            enqueue_pending_ack = true;
        }
    }
    peer.invariants();
    drop(peer);

    if enqueue_pending_ack {
        let _ = ack_burst_tx.send(peer_rc);
    }

    let _ = response_tx.send(response_frames);
    Ok(())
}

// ── Multi-frame dispatch ───────────────────────────────────────────────────

/// Dispatch a single frame decoded from a multi-frame `SenderId` packet.
///
/// This routes each decoded frame to the same handler as its single-frame
/// `RoutingInfo` counterpart, using the packet-level `source_sender_id` for
/// frame types that require it (e.g., QueueBind).
#[allow(clippy::too_many_arguments)]
fn dispatch_decoded_frame(
    header: Header,
    source_sender_id: VarInt,
    payload: BytesMut,
    peer: &mut recv::Context,
    credentials: &Credentials,
    acceptor_registry: &mut acceptor::LocalRegistry<PendingValidation>,
    frame_tx: &SubmissionSender,
    sender_tx: &mut impl channel::UnboundedSender<Entry<msg::Sender>>,
    counters: &counters::Dispatch,
    response_frames: &mut PriorityInput,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
) {
    match header {
        Header::QueueBind {
            queue_pair,
            dest_acceptor_id,
            binding_id,
            is_fin,
        } => {
            handle_queue_bind(
                peer,
                queue_pair,
                dest_acceptor_id,
                binding_id,
                is_fin,
                payload,
                acceptor_registry,
                frame_tx,
                counters,
                response_frames,
                waker_sink,
            );
        }
        Header::QueueData {
            queue_pair,
            binding_id,
            offset,
            is_fin,
        } => {
            handle_queue_data(
                credentials,
                queue_pair,
                binding_id,
                offset,
                is_fin,
                payload,
                &mut peer.queue_dispatcher,
                counters,
                waker_sink,
            );
        }
        Header::QueueControl {
            queue_pair,
            binding_id,
        } => {
            handle_queue_control(
                credentials,
                queue_pair,
                binding_id,
                payload,
                &mut peer.queue_dispatcher,
                counters,
                waker_sink,
            );
        }
        Header::QueueMaxData {
            queue_pair,
            binding_id,
            maximum_data,
        } => {
            handle_queue_max_data(
                credentials,
                queue_pair,
                binding_id,
                maximum_data,
                &mut peer.queue_dispatcher,
                counters,
                waker_sink,
            );
        }
        Header::QueueReset {
            dest_queue_id,
            binding_id,
            reset_target,
            error_code,
        } => {
            handle_queue_reset(
                credentials,
                dest_queue_id,
                binding_id,
                reset_target,
                error_code,
                &mut peer.queue_dispatcher,
                counters,
                waker_sink,
            );
        }
        Header::QueueFree { .. } => {
            /* TODO: handle queue free */
        }
        Header::Ack {
            dest_sender_id,
            ack_delay: ack_delay_micros,
            ..
        } => {
            let ack_delay = Duration::from_micros(ack_delay_micros.as_u64());
            let message = msg::Sender::ReceivedAck {
                local_sender_id: crate::endpoint::id::LocalSenderId::new(dest_sender_id),
                path_secret_entry: peer.path_entry.clone(),
                payload,
                ack_delay,
            };
            if sender_tx.send(Entry::new(message)).is_err() {
                warn!(
                    %credentials,
                    source_sender_id = source_sender_id.as_u64(),
                    dest_sender_id = dest_sender_id.as_u64(),
                    "dropping ACK sender message; sender queue is closed"
                );
            }
        }
    }
}

fn handle_queue_bind(
    peer: &mut recv::Context,
    queue_pair: QueuePair,
    acceptor_id: VarInt,
    binding_id: VarInt,
    is_fin: bool,
    buf: BytesMut,
    acceptor_registry: &mut acceptor::LocalRegistry<PendingValidation>,
    frame_tx: &SubmissionSender,
    counters: &counters::Dispatch,
    response_frames: &mut PriorityInput,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
) {
    let peer_queue_id = queue_pair.source_queue_id;

    // Packet-number dedup already guarantees this is not a retransmit.
    // Allocate a queue and deliver the stream to the acceptor.

    let handle = flow::Handle::client(binding_id, peer.path_entry.clone());
    let (queue_control, queue_stream) =
        peer.queue_dispatcher.alloc_or_grow(handle, Some(peer_queue_id));
    let local_queue_id = queue_control.queue_id();

    if !buf.is_empty() || is_fin {
        queue_stream.push(
            msg::Stream::Data {
                offset: VarInt::ZERO,
                fin: is_fin,
                payload: buf,
            }
            .into(),
        );
    }

    let writer = Writer::new_server(
        frame_tx.clone(),
        peer.path_entry.clone(),
        binding_id,
        queue_pair,
        queue_control,
    );
    let reader = Reader::new_server(
        frame_tx.clone(),
        peer.path_entry.clone(),
        binding_id,
        queue_stream,
        is_fin,
    );

    let stream = PendingValidation::new(Stream::new(reader, writer));

    match acceptor_registry.send(acceptor_id, stream) {
        acceptor::SendResult::Ok { mut evicted, waker } => {
            if let Some(ref mut ev) = evicted {
                ev.reset(crate::stream::endpoint::Error::ServerBusy);
            }
            counters.flow_accepted.add(1);
            let _ = waker_sink.send(waker);
            debug!(
                binding_id = binding_id.as_u64(),
                acceptor_id = acceptor_id.as_u64(),
                server_queue_id = local_queue_id.as_u64(),
                "QueueBind accepted"
            );
        }
        acceptor::SendResult::NotFound => {
            debug!(
                binding_id = binding_id.as_u64(),
                acceptor_id = acceptor_id.as_u64(),
                "QueueBind rejected - acceptor not found"
            );
            push_reset_frame(
                response_frames,
                counters,
                &peer.path_entry,
                peer_queue_id,
                binding_id,
                error::ACCEPTOR_NOT_FOUND,
            );
        }
        acceptor::SendResult::Closed(mut stream, cleanup_waker) => {
            stream.disable();
            let _ = waker_sink.send(cleanup_waker);
            debug!(
                binding_id = binding_id.as_u64(),
                acceptor_id = acceptor_id.as_u64(),
                "QueueBind rejected - acceptor channel closed"
            );
            push_reset_frame(
                response_frames,
                counters,
                &peer.path_entry,
                peer_queue_id,
                binding_id,
                error::ACCEPTOR_NOT_FOUND,
            );
        }
        acceptor::SendResult::NoSlots(mut stream) => {
            stream.disable();
            debug!(
                binding_id = binding_id.as_u64(),
                acceptor_id = acceptor_id.as_u64(),
                "QueueBind rejected - acceptor has no active receivers"
            );
            push_reset_frame(
                response_frames,
                counters,
                &peer.path_entry,
                peer_queue_id,
                binding_id,
                error::ACCEPTOR_NOT_FOUND,
            );
        }
    }
}

fn push_reset_frame(
    response_frames: &mut PriorityInput,
    counters: &counters::Dispatch,
    path_secret_entry: &std::sync::Arc<PathSecretEntry>,
    dest_queue_id: VarInt,
    binding_id: VarInt,
    error_code: VarInt,
) {
    push_reset_frame_with_target(
        response_frames,
        counters,
        path_secret_entry,
        dest_queue_id,
        binding_id,
        ResetTarget::Both,
        error_code,
    );
}

fn push_reset_frame_with_target(
    response_frames: &mut PriorityInput,
    counters: &counters::Dispatch,
    path_secret_entry: &std::sync::Arc<PathSecretEntry>,
    dest_queue_id: VarInt,
    binding_id: VarInt,
    reset_target: ResetTarget,
    error_code: VarInt,
) {
    let frame = Frame {
        header: Header::QueueReset {
            dest_queue_id,
            binding_id,
            reset_target,
            error_code,
        },
        source_sender_id: LocalSenderId::UNSPECIFIED,
        payload: ByteVec::new(),
        path_secret_entry: path_secret_entry.clone(),
        completion: None,
        status: Default::default(),
        ttl: DEFAULT_TTL,
        transmission_time: None,
    };
    counters.on_response_frame(&frame.header);
    response_frames.push(frame.into());
}

// ── QueueData ─────────────────────────────────────────────────────────────

fn handle_queue_data(
    credentials: &Credentials,
    queue_pair: QueuePair,
    binding_id: VarInt,
    offset: VarInt,
    is_fin: bool,
    buf: BytesMut,
    queue_dispatcher: &mut msg::queue::Dispatcher,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
) {
    let local_queue_id = queue_pair.dest_queue_id;

    let request = flow::Request {
        credential_id: credentials.id,
        stream_id: Some(binding_id),
    };

    let payload_len = buf.len();
    let entry = msg::Stream::Data {
        offset,
        fin: is_fin,
        payload: buf,
    }
    .into();

    match queue_dispatcher.send_stream(
        local_queue_id,
        Some(queue_pair.source_queue_id),
        &request,
        entry,
    ) {
        Ok(waker) => {
            let _ = waker_sink.send(waker);
            counters.rx_data_ok.add(1);
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                offset = offset.as_u64(),
                payload_len,
                is_fin,
                "QueueData dispatched"
            );
        }
        Err(flow::queue::Error::Unallocated(_)) => {
            counters.rx_data_unallocated.add(1);
            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueData for unallocated queue - dropping"
            );
        }
        Err(flow::queue::Error::HalfClosed(_)) => {
            counters.rx_data_half_closed.add(1);
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueData for half-closed stream - dropping"
            );
        }
        Err(flow::queue::Error::ValidationFailed(_, reason)) => {
            counters.on_data_validation_failed(reason);
            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                ?reason,
                "QueueData validation failed - dropping"
            );
        }
        Err(flow::queue::Error::PermanentlyClosed) => {
            counters.rx_data_perm_closed.add(1);
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueData for permanently closed queue"
            );
        }
    }
}

// ── QueueControl ──────────────────────────────────────────────────────────

fn handle_queue_control(
    credentials: &Credentials,
    queue_pair: QueuePair,
    binding_id: VarInt,
    buf: BytesMut,
    queue_dispatcher: &mut msg::queue::Dispatcher,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
) {
    let payload_len = buf.len();
    let entry = msg::Control::Frames { payload: buf }.into();
    if dispatch_control_message(
        credentials,
        queue_pair,
        binding_id,
        entry,
        queue_dispatcher,
        counters,
        waker_sink,
    ) {
        trace!(
            binding_id = binding_id.as_u64(),
            queue_id = queue_pair.dest_queue_id.as_u64(),
            payload_len,
            "QueueControl dispatched"
        );
    }
}

// ── QueueMaxData ──────────────────────────────────────────────────────────

fn handle_queue_max_data(
    credentials: &Credentials,
    queue_pair: QueuePair,
    binding_id: VarInt,
    maximum_data: VarInt,
    queue_dispatcher: &mut msg::queue::Dispatcher,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
) {
    let entry = msg::Control::MaxData { maximum_data }.into();
    if dispatch_control_message(
        credentials,
        queue_pair,
        binding_id,
        entry,
        queue_dispatcher,
        counters,
        waker_sink,
    ) {
        trace!(
            binding_id = binding_id.as_u64(),
            queue_id = queue_pair.dest_queue_id.as_u64(),
            maximum_data = maximum_data.as_u64(),
            "QueueMaxData dispatched"
        );
    }
}

/// Dispatches a pre-built control message into the per-queue control channel.
///
/// Returns `true` when the message was accepted by the queue (success path).
/// All error paths are handled internally. Callers that need to emit a success trace should do so after
/// this call when the return value is `true`.
fn dispatch_control_message(
    credentials: &Credentials,
    queue_pair: QueuePair,
    binding_id: VarInt,
    entry: Entry<msg::Control>,
    queue_dispatcher: &mut msg::queue::Dispatcher,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
) -> bool {
    let local_queue_id = queue_pair.dest_queue_id;

    let request = flow::Request {
        credential_id: credentials.id,
        stream_id: Some(binding_id),
    };

    match queue_dispatcher.send_control(
        local_queue_id,
        Some(queue_pair.source_queue_id),
        &request,
        entry,
    ) {
        Ok(waker) => {
            let _ = waker_sink.send(waker);
            counters.rx_flow_control_ok.add(1);
            true
        }
        Err(flow::queue::Error::Unallocated(_)) => {
            counters.rx_flow_control_unallocated.add(1);
            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "queue control for unallocated queue - dropping"
            );
            false
        }
        Err(flow::queue::Error::HalfClosed(_)) => {
            counters.rx_flow_control_half_closed.add(1);
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "queue control for half-closed control queue - dropping"
            );
            false
        }
        Err(flow::queue::Error::ValidationFailed(_, reason)) => {
            counters.on_flow_control_validation_failed(reason);
            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                ?reason,
                "queue control validation failed - dropping"
            );
            false
        }
        Err(flow::queue::Error::PermanentlyClosed) => {
            counters.rx_flow_control_perm_closed.add(1);
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "queue control for permanently closed queue"
            );
            false
        }
    }
}

// ── QueueReset ────────────────────────────────────────────────────────────

fn handle_queue_reset(
    credentials: &Credentials,
    dest_queue_id: VarInt,
    binding_id: VarInt,
    reset_target: ResetTarget,
    error_code: VarInt,
    queue_dispatcher: &mut msg::queue::Dispatcher,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
) {
    let local_queue_id = dest_queue_id;

    let request = flow::Request {
        credential_id: credentials.id,
        stream_id: Some(binding_id),
    };

    match reset_target {
        ResetTarget::Both => {
            counters.rx_reset_both.add(1);
            let stream_entry = msg::Stream::Reset { error_code }.into();
            let control_entry = msg::Control::Reset { error_code }.into();
            let (waker_a, waker_b) = queue_dispatcher.send_both(
                local_queue_id,
                None,
                &request,
                stream_entry,
                control_entry,
            );
            let _ = waker_sink.send(waker_a);
            let _ = waker_sink.send(waker_b);

            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                error_code = error_code.as_u64(),
                "QueueReset(Both) dispatched"
            );
        }
        ResetTarget::Stream => {
            counters.rx_reset_stream.add(1);
            let stream_entry = msg::Stream::Reset { error_code }.into();
            if let Ok(waker) =
                queue_dispatcher.send_stream(local_queue_id, None, &request, stream_entry)
            {
                let _ = waker_sink.send(waker);
            }

            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                error_code = error_code.as_u64(),
                "QueueReset(Stream) dispatched"
            );
        }
        ResetTarget::Control => {
            counters.rx_reset_control.add(1);
            let control_entry = msg::Control::Reset { error_code }.into();
            if let Ok(waker) =
                queue_dispatcher.send_control(local_queue_id, None, &request, control_entry)
            {
                let _ = waker_sink.send(waker);
            }

            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                error_code = error_code.as_u64(),
                "QueueReset(Control) dispatched"
            );
        }
    }
}

