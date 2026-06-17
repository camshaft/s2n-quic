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
        frame::{Frame, Header, SubmissionSender, DEFAULT_TTL},
        msg, recv, routing,
    },
    intrusive::Entry,
    packet::{
        self,
        datagram::{QueuePair, ResetTarget, RoutingInfo},
    },
    path::secret::{map::Entry as PathSecretEntry, Map as PathSecretMap},
    queue::AutoWake,
    socket::{channel, pool::descriptor},
    stream::{Reader, Stream, Writer},
    tracing::*,
};
use bytes::BytesMut;
use core::time::Duration;
use s2n_quic_core::varint::VarInt;
use std::{cell::RefCell, rc::Rc, sync::Arc};

#[cfg(test)]
mod tests;

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

/// Result of the decrypt closure: either the fast path completed dispatch inline,
/// or we have a BytesMut that needs multi-frame dispatch.
enum DecryptResult {
    FastPath(AutoWake),
    SlowPath(BytesMut),
}

enum FastPathError {
    HeaderMismatch,
    WriteFailed,
    /// The packet's routing fields steered it to a binding we are rejecting
    /// (stale/unallocated/future binding, missing acceptor, or not a server view), so the
    /// scatter-decrypt never ran and the packet is still un-authenticated. The caller must
    /// authenticate it in place before ACKing.
    ///
    /// Why this matters: a binding rejection is not retransmittable, so `process` ACKs it to
    /// stop the peer resending. But `queue_id`/`binding_id` are cleartext routing fields carried
    /// as AEAD associated data — an in-flight corruption of those bytes reroutes an otherwise
    /// valid packet (destined for a *live* stream) to a dead binding. ACKing it un-authenticated
    /// would make the sender free that packet number and stop retransmitting, leaving a permanent
    /// hole on the real stream. Authenticating means a tampered packet fails the AEAD tag check
    /// and the ACK is suppressed, so the genuine packet is retransmitted and recovers.
    AuthForDrop,
}

/// Fast path: decrypt a single-QueueMsg-frame packet directly into the slot buffer.
///
/// Handles both init (with binding setup) and non-init frames.
#[allow(clippy::too_many_arguments)]
fn decrypt_fast_path(
    header: Header,
    opener: &crate::crypto::awslc::open::Application,
    packet: &packet::datagram::decoder::Packet<descriptor::Filled>,
    decrypt_len: usize,
    queue_view: &mut recv::QueueView,
    acceptor_registry: &mut acceptor::LocalRegistry<Stream>,
    frame_tx: &mut SubmissionSender,
    freed_batch_tx: &mut crate::queue::FreedBatchTx,
    counters: &counters::Dispatch,
    path_entry: &Arc<PathSecretEntry>,
    stream_clock: &crate::time::DefaultClock,
    reader_metrics: &Arc<crate::stream::metrics::ReaderMetrics>,
    writer_metrics: &Arc<crate::stream::metrics::WriterMetrics>,
    send_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
    recv_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
) -> Result<AutoWake, FastPathError> {
    let Header::QueueMsg {
        queue_pair,
        binding_id,
        msg_id,
        stream_offset,
        largest_offset,
        message_size,
        chunk_size,
        chunk_index,
        is_fin,
        is_wakeup,
        blocked,
        dest_acceptor_id,
        priority,
    } = header
    else {
        return Err(FastPathError::HeaderMismatch);
    };

    // Handle init: bind the slot before attempting push_msg
    if let Some(acceptor_id) = dest_acceptor_id {
        let Some(server_view) = queue_view.as_server_mut() else {
            // Un-authenticated reject — caller must authenticate before ACKing. See `AuthForDrop`.
            return Err(FastPathError::AuthForDrop);
        };

        let Some(acceptor_sender) = acceptor_registry.get(acceptor_id) else {
            counters.rx_init_no_acceptor.add(1);
            server_view.record_freed(queue_pair.dest_queue_id, path_entry, freed_batch_tx);
            send_reset(
                path_entry,
                queue_pair,
                binding_id,
                error::ACCEPTOR_NOT_FOUND,
                frame_tx,
            );
            // Un-authenticated reject — caller must authenticate before ACKing. See `AuthForDrop`.
            return Err(FastPathError::AuthForDrop);
        };

        match server_view.bind_for_msg(
            queue_pair.dest_queue_id,
            binding_id,
            path_entry,
            freed_batch_tx,
        ) {
            Ok(crate::queue::BindResult::NewBinding {
                waker,
                stream,
                control,
                release_bytes,
            }) => {
                // `bind_for_msg` only allocates the slot, so this waker is empty
                // and `release_bytes` zero today; the real wake/release comes from
                // the `send_msg` below. Unlike the slow path there is no waker sink
                // here (the fast path returns a single `AutoWake`), so let this
                // `waker` drop-wake as its failsafe and release any bytes
                // explicitly — never silently dropped.
                recv_credit_pool.release(release_bytes);
                drop(waker);
                let writer = Writer::new_server(
                    frame_tx.clone(),
                    path_entry.clone(),
                    queue_pair.source_queue_id,
                    acceptor_id,
                    control,
                    stream_clock.clone(),
                    writer_metrics.clone(),
                    send_credit_pool.clone(),
                    priority,
                );
                let reader = Reader::new_server(
                    frame_tx.clone(),
                    path_entry.clone(),
                    queue_pair.source_queue_id,
                    acceptor_id,
                    stream,
                    is_fin,
                    stream_clock.clone(),
                    reader_metrics.clone(),
                    recv_credit_pool.clone(),
                    priority,
                );
                let new_stream = Stream::new(reader, writer);
                match acceptor_sender.send(new_stream) {
                    Ok((mut evicted, acceptor_waker)) => {
                        if let Some(ref mut ev) = evicted {
                            ev.reset(crate::stream::endpoint::Error::ServerBusy);
                        }
                        counters.queue_accepted.add(1);
                        // We'll return the msg waker; acceptor waker fires on drop
                        drop(AutoWake::new(acceptor_waker));
                    }
                    Err(acceptor::channel::SendError::Closed(mut s)) => {
                        s.disable();
                        counters.rx_init_acceptor_closed.add(1);
                        // `disable()` tears the bound slot down locally only (no frame). The
                        // client is still establishing this binding and the QueueInit it sent
                        // gets ACKed, so without an explicit reset it wedges in InitSent forever.
                        // Notify it like the acceptor-not-found arm does.
                        send_reset(
                            path_entry,
                            queue_pair,
                            binding_id,
                            error::ACCEPTOR_NOT_FOUND,
                            frame_tx,
                        );
                    }
                    Err(acceptor::channel::SendError::NoSlots(mut s)) => {
                        s.disable();
                        counters.rx_init_acceptor_no_slots.add(1);
                        // See the `Closed` arm: the client must be reset explicitly or it
                        // wedges in InitSent after its QueueInit is ACKed.
                        send_reset(
                            path_entry,
                            queue_pair,
                            binding_id,
                            error::SERVER_BUSY,
                            frame_tx,
                        );
                    }
                }
            }
            Ok(crate::queue::BindResult::Bound {
                waker,
                release_bytes,
            }) => {
                // Slot already bound (init retransmit / coalesced frames). As with
                // NewBinding above: no waker sink here, so release bytes
                // explicitly and let `waker` drop-wake as its failsafe.
                recv_credit_pool.release(release_bytes);
                drop(waker);
            }
            // Un-authenticated reject — caller must authenticate before ACKing. See `AuthForDrop`.
            Err(_) => return Err(FastPathError::AuthForDrop),
        }
    }

    // Scatter-decrypt directly into the slot buffer
    let waker = queue_view.send_msg(
        queue_pair.dest_queue_id,
        binding_id,
        msg_id.as_u64(),
        stream_offset.as_u64(),
        largest_offset.as_u64(),
        message_size.as_u64() as u32,
        chunk_size.as_u64() as u16,
        chunk_index.as_u64() as u32,
        decrypt_len as u32,
        is_fin,
        is_wakeup,
        blocked,
        |ptr, len| {
            let dest = unsafe { bytes::buf::UninitSlice::from_raw_parts_mut(ptr, len as usize) };
            packet
                .decrypt_into(opener, dest)
                .map_err(|_| ())
                .and_then(|written| {
                    if written == len as usize {
                        Ok(())
                    } else {
                        Err(())
                    }
                })
        },
    );

    Ok(match waker {
        Ok((w, release_bytes)) => {
            if w.is_some() {
                counters.rx_msg_segment_completed.add(1);
                counters
                    .rx_msg_segment_size
                    .record_value(message_size.as_u64());
                let chunks = message_size.as_u64().div_ceil(chunk_size.as_u64().max(1));
                counters.rx_msg_chunks_per_segment.record_value(chunks);
            }
            recv_credit_pool.release(release_bytes);
            w
        }
        // `send_msg` rejected on binding validation *before* invoking the scatter-decrypt
        // write callback, so the packet was never authenticated. Caller must authenticate
        // before ACKing — see `AuthForDrop`.
        Err(crate::queue::MsgError::Queue(_)) => return Err(FastPathError::AuthForDrop),
        // The `MsgTable` rejected the chunk's geometry *before* the scatter-decrypt ran, so the
        // packet is likewise un-authenticated. Geometry fields (`msg_id`, `chunk_index`,
        // `message_size`, `chunk_size`, …) are cleartext AEAD associated data, so an in-flight
        // corruption of any of them lands here for an otherwise-valid live-stream packet —
        // authenticate before ACKing so a tampered packet is dropped (→ retransmit) rather than
        // ACKed into a permanent stream hole. See `AuthForDrop`.
        Err(crate::queue::MsgError::InsertRejected) => return Err(FastPathError::AuthForDrop),
        Err(crate::queue::MsgError::Write(_)) => return Err(FastPathError::WriteFailed),
    })
}

/// Process a received datagram packet.
///
/// Authenticates (decrypt), deduplicates by packet number, updates ACK state, then
/// dispatches each frame in the packet to its type-specific handler. Response frames
/// (ACKs, QueueValidateRequest, QueueReset) are emitted to `response_tx`.
pub(crate) fn process<Clk, Route>(
    mut packet: Entry<packet::datagram::decoder::Packet<descriptor::Filled>>,
    recv_cache: &mut recv::Cache,
    ack_burst_tx: &mut impl channel::UnboundedSender<Rc<RefCell<recv::Context>>>,
    idle_wheel_tx: &mut impl channel::UnboundedSender<Rc<RefCell<recv::Context>>>,
    path_secret_map: &PathSecretMap,
    acceptor_registry: &mut acceptor::LocalRegistry<Stream>,
    frame_tx: &mut SubmissionSender,
    sender_tx: &mut impl channel::UnboundedSender<Entry<msg::Sender>>,
    freed_batch_tx: &mut crate::queue::FreedBatchTx,
    clock: &Clk,
    counters: &counters::Dispatch,
    route: &Route,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
    stream_clock: &crate::time::DefaultClock,
    reader_metrics: &Arc<crate::stream::metrics::ReaderMetrics>,
    writer_metrics: &Arc<crate::stream::metrics::WriterMetrics>,
    send_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
    recv_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
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

    // Collect the fields we need before the closure borrows `packet` mutably. All of these
    // are `Copy`, so nothing keeps `packet` borrowed across the closure — the fast path's
    // `AuthForDrop` recovery needs `&mut packet` to decrypt in place.
    let decrypt_len = packet.decrypt_into_len();
    let ecn = packet.storage().ecn();
    let remote_addr = packet.storage().remote_address().get();
    let source_control_port = packet.source_control_port();

    // Detect single-QueueMsg frame for the fast path (scatter-decrypt into slot buffer).
    let single_queue_msg =
        decode::detect_single_queue_msg(packet.application_header(), decrypt_len);

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
    let decrypt_fn = |opener: &crate::crypto::awslc::open::Application,
                      queue_view: &mut recv::QueueView,
                      path_entry: &Arc<PathSecretEntry>|
     -> Option<DecryptResult> {
        let _guard = counters.rx_decrypt_time.start();

        // Fast path: single QueueMsg frame — decrypt directly into the slot buffer.
        if let Some(header) = single_queue_msg {
            crate::endpoint::frame_trace::record(
                crate::endpoint::frame_trace::Direction::InboundFastPath,
                &header,
                Some(packet_number),
            );
            return match decrypt_fast_path(
                header,
                opener,
                &packet,
                decrypt_len,
                queue_view,
                acceptor_registry,
                frame_tx,
                freed_batch_tx,
                counters,
                path_entry,
                stream_clock,
                reader_metrics,
                writer_metrics,
                send_credit_pool,
                recv_credit_pool,
            ) {
                Ok(waker) => Some(DecryptResult::FastPath(waker)),
                // The packet routed to a binding we are rejecting, so the scatter-decrypt
                // never ran. Authenticate it in place before the caller ACKs: a tampered
                // packet fails here and returns `None` → `CacheError::DecryptFailed` → no ACK
                // → the genuine packet is retransmitted. See `FastPathError::AuthForDrop`.
                Err(FastPathError::AuthForDrop) => match packet.decrypt_in_place(opener) {
                    Ok(()) => Some(DecryptResult::FastPath(AutoWake::default())),
                    Err(_) => None,
                },
                Err(FastPathError::HeaderMismatch) | Err(FastPathError::WriteFailed) => None,
            };
        }

        // Slow path: allocate BytesMut, decrypt into it, dispatch frames later.
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
        unsafe { buf.set_len(decrypt_len) };
        Some(DecryptResult::SlowPath(buf))
    };
    let (decrypt_result, peer_rc, cache_hit) = {
        let _guard = counters.rx_peer_lookup_time.start();
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
                dest_addr.set_port(source_control_port);
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
                dest_addr.set_port(source_control_port);
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

    // Fast path: single QueueMsg already dispatched during decrypt — just send the waker.
    let payload_storage = match decrypt_result {
        DecryptResult::FastPath(waker) => {
            let _ = waker_sink.send(waker);
            counters.on_received_frame(&single_queue_msg.unwrap());
            counters.rx_frames_per_packet.record_value(1);

            // QueueMsg is always ack-eliciting — drive the ACK state machine.
            match peer.ack_state.on_ack_eliciting() {
                Ok(()) | Err(s2n_quic_core::state::Error::NoOp { .. }) => {}
                Err(s2n_quic_core::state::Error::InvalidTransition { .. }) => {
                    counters.rx_ack_state_impossible.add(1);
                    debug_assert!(false, "on_ack_eliciting transition failed");
                }
            }
            if peer.ack_state.is_flushed_stale() {
                if let Some(flushed_at) = peer.flushed_at {
                    let now = crate::time::precision::Clock::now(clock);
                    counters
                        .rx_ack_flush_blocked
                        .record(now.duration_since(flushed_at));
                }
            }
            let enqueue_pending_ack = !peer.ack_burst.is_linked() && peer.ack_state.is_scheduled();
            peer.invariants();
            drop(peer);

            if enqueue_pending_ack {
                let _ = ack_burst_tx.send(peer_rc);
            }

            return Ok(());
        }
        DecryptResult::SlowPath(buf) => buf,
    };
    let mut payload_storage = payload_storage;

    // Multi-frame packet: the application header holds the per-frame metadata
    // (Header type tag + optional payload_len VarInt) and `payload_storage`
    // contains the concatenated, decrypted frame payloads. The slow path decrypts into a
    // separate buffer (not in place), so the packet's cleartext application header is intact.
    let app_header_slice: &[u8] = packet.application_header();

    let _dispatch_guard = counters.rx_dispatch_time.start();
    let mut is_ack_eliciting = false;
    let mut frame_count = 0u64;
    for result in decode::decode_frames(app_header_slice) {
        match result {
            Ok((header, frame_payload_len)) => {
                frame_count += 1;
                counters.on_received_frame(&header);
                crate::endpoint::frame_trace::record(
                    crate::endpoint::frame_trace::Direction::Inbound,
                    &header,
                    Some(packet_number),
                );
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
                    freed_batch_tx,
                    sender_tx,
                    counters,
                    waker_sink,
                    stream_clock,
                    reader_metrics,
                    writer_metrics,
                    send_credit_pool,
                    recv_credit_pool,
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

        if peer.ack_state.is_flushed_stale() {
            if let Some(flushed_at) = peer.flushed_at {
                let now = crate::time::precision::Clock::now(clock);
                counters
                    .rx_ack_flush_blocked
                    .record(now.duration_since(flushed_at));
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

    Ok(())
}

// ── Multi-frame dispatch ───────────────────────────────────────────────────

/// Dispatch a single frame decoded from a multi-frame `SenderId` packet.
///
/// This routes each decoded frame to the same handler as its single-frame
/// `RoutingInfo` counterpart, using the packet-level `source_sender_id` for
/// frame types that require it (e.g., QueueInit).
#[allow(clippy::too_many_arguments)]
fn dispatch_decoded_frame(
    header: Header,
    source_sender_id: VarInt,
    payload: BytesMut,
    peer: &mut recv::Context,
    credentials: &Credentials,
    acceptor_registry: &mut acceptor::LocalRegistry<Stream>,
    frame_tx: &mut SubmissionSender,
    freed_batch_tx: &mut crate::queue::FreedBatchTx,
    sender_tx: &mut impl channel::UnboundedSender<Entry<msg::Sender>>,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
    stream_clock: &crate::time::DefaultClock,
    reader_metrics: &Arc<crate::stream::metrics::ReaderMetrics>,
    writer_metrics: &Arc<crate::stream::metrics::WriterMetrics>,
    send_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
    recv_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
) {
    match header {
        Header::QueueData {
            queue_pair,
            binding_id,
            offset,
            largest_offset,
            is_fin,
            blocked,
            dest_acceptor_id,
            priority,
        } => {
            // `largest_offset` is already reconstructed to absolute by decode.
            let peer_max_offset = largest_offset.as_u64();
            if let Some(acceptor_id) = dest_acceptor_id {
                handle_queue_data_init(
                    peer,
                    queue_pair,
                    binding_id,
                    offset,
                    peer_max_offset,
                    is_fin,
                    blocked,
                    acceptor_id,
                    payload,
                    acceptor_registry,
                    frame_tx,
                    freed_batch_tx,
                    counters,
                    waker_sink,
                    stream_clock,
                    reader_metrics,
                    writer_metrics,
                    send_credit_pool,
                    recv_credit_pool,
                    priority,
                );
            } else {
                handle_queue_data(
                    peer,
                    queue_pair,
                    binding_id,
                    offset,
                    peer_max_offset,
                    is_fin,
                    blocked,
                    payload,
                    counters,
                    waker_sink,
                    recv_credit_pool,
                );
            }
        }
        Header::QueueControl {
            queue_pair,
            binding_id,
        } => {
            handle_queue_control(peer, queue_pair, binding_id, payload, counters, waker_sink);
        }
        Header::QueueMaxData {
            queue_pair,
            binding_id,
            maximum_data,
        } => {
            handle_queue_max_data(
                peer,
                queue_pair,
                binding_id,
                maximum_data,
                counters,
                waker_sink,
            );
        }
        Header::QueueReset {
            queue_pair,
            binding_id,
            reset_target,
            error_code,
            init,
        } => {
            handle_queue_reset(
                peer,
                queue_pair,
                binding_id,
                reset_target,
                error_code,
                init,
                acceptor_registry,
                frame_tx,
                freed_batch_tx,
                counters,
                waker_sink,
                stream_clock,
                reader_metrics,
                writer_metrics,
                send_credit_pool,
                recv_credit_pool,
            );
        }
        Header::QueueFree {
            free_request_id,
            smallest_queue_id,
        } => {
            handle_queue_free(
                peer,
                free_request_id,
                smallest_queue_id,
                payload,
                counters,
                waker_sink,
            );
        }
        Header::Ack {
            dest_sender_id,
            ack_delay: ack_delay_micros,
            largest_acknowledged,
            ack_range,
            ecn_counts,
            // `is_ack_eliciting` (this ACK acting as a PING) is consumed earlier
            // via `header.is_ack_eliciting()` when deciding whether to schedule
            // an ACK back — see the ack-eliciting accounting in `process`. It is
            // not needed to build the inbound `ReceivedAck`, so spell it out
            // rather than dropping it with `..`.
            is_ack_eliciting: _,
        } => {
            let ack_delay = Duration::from_micros(ack_delay_micros.as_u64());
            let message = msg::Sender::ReceivedAck {
                local_sender_id: crate::endpoint::id::LocalSenderId::new(dest_sender_id),
                path_secret_entry: peer.path_entry.clone(),
                payload,
                ack_delay,
                largest_acknowledged,
                ack_range,
                ecn_counts,
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
        Header::QueueMsg {
            queue_pair,
            binding_id,
            msg_id,
            stream_offset,
            largest_offset,
            message_size,
            chunk_size,
            chunk_index,
            is_fin,
            is_wakeup,
            blocked,
            dest_acceptor_id,
            priority,
        } => {
            // `largest_offset` is already reconstructed to absolute by decode.
            let peer_max_offset = largest_offset.as_u64();
            if let Some(acceptor_id) = dest_acceptor_id {
                handle_queue_msg_init(
                    peer,
                    queue_pair,
                    binding_id,
                    acceptor_id,
                    msg_id,
                    stream_offset,
                    peer_max_offset,
                    message_size,
                    chunk_size,
                    chunk_index,
                    is_fin,
                    is_wakeup,
                    blocked,
                    payload,
                    acceptor_registry,
                    frame_tx,
                    freed_batch_tx,
                    counters,
                    waker_sink,
                    stream_clock,
                    reader_metrics,
                    writer_metrics,
                    send_credit_pool,
                    recv_credit_pool,
                    priority,
                );
            } else {
                handle_queue_msg(
                    peer,
                    queue_pair,
                    binding_id,
                    msg_id,
                    stream_offset,
                    peer_max_offset,
                    message_size,
                    chunk_size,
                    chunk_index,
                    is_fin,
                    is_wakeup,
                    blocked,
                    payload,
                    counters,
                    waker_sink,
                    recv_credit_pool,
                );
            }
        }
        Header::Ping => {}
        Header::QueueDataBlocked {
            queue_pair,
            binding_id,
            desired_offset,
        } => {
            handle_queue_data_blocked(
                peer,
                queue_pair,
                binding_id,
                desired_offset,
                counters,
                waker_sink,
            );
        }
        Header::QueueDbg {
            dump_id,
            queue_pair,
            binding_id,
        } => {
            handle_queue_dbg(
                peer,
                dump_id,
                queue_pair,
                binding_id,
                credentials,
                counters,
                waker_sink,
            );
        }
    }
}

// ── QueueDbg ───────────────────────────────────────────────────────────────

/// Handle a received `QueueDbg` stuck-stream diagnostic marker (see the `queue-dbg` feature).
///
/// Dumps the whole `recv::Context` — stamped with the shared `dump_id` so the line joins the
/// end-to-end trace — then wakes the parked peer Reader **and** Writer by pushing a `Debug` entry
/// onto both queue halves. Each woken handle logs its own `Inner` plus the peer's claimed routing
/// identity, so a binding/queue divergence shows up as a mismatch. Unlike the sibling handlers,
/// a delivery failure here is itself a finding (the binding is gone/diverged), so it is logged in
/// full rather than silently dropped.
///
/// When the diagnostic is disabled (the common production case) the whole body is gated behind
/// [`dbg::on_enabled`], which folds to nothing: the frame still decodes (the wire stays
/// compatible) but nothing is logged, pushed, or woken.
#[allow(clippy::too_many_arguments)]
fn handle_queue_dbg(
    peer: &mut recv::Context,
    dump_id: VarInt,
    queue_pair: QueuePair,
    binding_id: VarInt,
    credentials: &Credentials,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
) {
    let _ = counters;
    crate::endpoint::dbg::on_enabled(|| {
        let dest_queue_id = queue_pair.dest_queue_id;
        let peer_cred_id = credentials.id;

        // Record this QueueDbg into the flight recorder (so the dump's newest record carries the
        // trigger dump_id), then dump the whole ring. The first QueueDbg produces the dump; later
        // ones simply re-arm the single-flag handoff.
        crate::endpoint::frame_trace::record(
            crate::endpoint::frame_trace::Direction::Inbound,
            &Header::QueueDbg {
                dump_id,
                queue_pair,
                binding_id,
            },
            None,
        );
        crate::endpoint::frame_trace::trigger();

        info!(
            dump_id = dump_id.as_u64(),
            %credentials,
            binding_id = binding_id.as_u64(),
            dest_queue_id = dest_queue_id.as_u64(),
            source_queue_id = queue_pair.source_queue_id.as_u64(),
            context = ?peer,
            "QueueDbg received: recv::Context state"
        );

        // Wake whichever of the peer Reader/Writer is parked so each dumps its own `Inner`. Push to
        // both halves; the entry carries the peer-claimed identity so the handle can flag a mismatch.
        let stream_err = match peer.queue_view.send_stream(
            dest_queue_id,
            binding_id,
            crate::endpoint::msg::Stream::Debug {
                dump_id: dump_id.as_u64(),
                peer_queue_pair: queue_pair,
                peer_binding_id: binding_id,
                peer_cred_id,
            }
            .into(),
        ) {
            Ok((waker, release_bytes)) => {
                let _ = waker_sink.send(waker);
                // A diagnostic marker carries no payload, so there is nothing to release.
                debug_assert_eq!(release_bytes, 0);
                None
            }
            Err(err) => Some(queue_error_reason(&err)),
        };

        let control_err = match peer.queue_view.send_control(
            dest_queue_id,
            binding_id,
            crate::endpoint::msg::Control::Debug {
                dump_id: dump_id.as_u64(),
                peer_queue_pair: queue_pair,
                peer_binding_id: binding_id,
                peer_cred_id,
            }
            .into(),
        ) {
            Ok(waker) => {
                let _ = waker_sink.send(waker);
                None
            }
            Err(err) => Some(queue_error_reason(&err)),
        };

        // A delivery failure is itself a finding — the binding is gone or has diverged. Read back
        // the slot's *actual* current state so the log shows the divergence: what binding the slot
        // holds now vs. the `binding_id` the marker (and the wedged peer stream) is using, whether
        // the slot is unallocated, and whether either receiver half is still attached. This is the
        // smoking gun for the record_freed/FutureBinding/StaleBinding class.
        if stream_err.is_some() || control_err.is_some() {
            let diag = peer.queue_view.slot_diag(dest_queue_id);
            info!(
                dump_id = dump_id.as_u64(),
                %credentials,
                binding_id = binding_id.as_u64(),
                queue_id = dest_queue_id.as_u64(),
                source_queue_id = queue_pair.source_queue_id.as_u64(),
                stream_reason = stream_err,
                control_reason = control_err,
                slot_found = diag.is_some(),
                slot_stored_binding = diag.map(|d| d.stored_binding),
                slot_unallocated = diag.map(|d| d.unallocated),
                slot_advertised_window = diag.map(|d| d.advertised_window),
                slot_stream_has_sender = diag.map(|d| d.stream_has_sender),
                slot_stream_has_receiver = diag.map(|d| d.stream_has_receiver),
                slot_stream_depth = diag.map(|d| d.stream_depth),
                slot_control_has_sender = diag.map(|d| d.control_has_sender),
                slot_control_has_receiver = diag.map(|d| d.control_has_receiver),
                slot_control_depth = diag.map(|d| d.control_depth),
                "QueueDbg delivery failed (binding gone/diverged); slot state attached"
            );
        }
    });
}

/// A static label for a queue dispatch error, for diagnostic logging without requiring the
/// (entry-carrying) `Error<T>` to be `Debug`/`Display`.
fn queue_error_reason<T>(err: &crate::queue::Error<T>) -> &'static str {
    match err {
        crate::queue::Error::Unallocated(_) => "unallocated",
        crate::queue::Error::HalfClosed(_) => "half_closed",
        crate::queue::Error::SenderClosed => "sender_closed",
        crate::queue::Error::StaleBinding(_) => "stale_binding",
        crate::queue::Error::FutureBinding(_) => "future_binding",
        crate::queue::Error::CapExceeded(_) => "cap_exceeded",
    }
}

// ── QueueData ──────────────────────────────────────────────────────────────

fn handle_queue_free(
    peer: &mut recv::Context,
    free_request_id: VarInt,
    smallest_queue_id: VarInt,
    payload: BytesMut,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
) {
    let Some(client_view) = peer.queue_view.as_client_mut() else {
        debug!("QueueFree received on server context - dropping");
        return;
    };

    let decoder = DeltaDecoder::new(smallest_queue_id, &payload);
    let result = client_view.free(free_request_id, decoder, &mut |w| {
        let _ = waker_sink.send(AutoWake::new(Some(w)));
    });
    counters
        .rx_queue_free_slots
        .record_value(result.slots as u64);
    counters
        .rx_queue_free_ranges
        .record_value(result.ranges as u64);
}

struct DeltaDecoder<'a> {
    pending: Option<VarInt>,
    payload: s2n_codec::DecoderBuffer<'a>,
}

impl<'a> DeltaDecoder<'a> {
    fn new(smallest_queue_id: VarInt, payload: &'a [u8]) -> Self {
        Self {
            pending: Some(smallest_queue_id),
            payload: s2n_codec::DecoderBuffer::new(payload),
        }
    }
}

impl Iterator for DeltaDecoder<'_> {
    type Item = Result<core::ops::RangeInclusive<VarInt>, s2n_codec::DecoderError>;

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.pending.take()?;
        let mut end = start;

        loop {
            if self.payload.is_empty() {
                return Some(Ok(start..=end));
            }
            let (delta, buffer) = match self.payload.decode::<VarInt>() {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            let next_id = match end.checked_add(delta + VarInt::from_u8(1)) {
                Some(v) => v,
                None => {
                    return Some(Err(s2n_codec::DecoderError::InvariantViolation(
                        "delta overflow",
                    )))
                }
            };
            self.payload = buffer;
            if delta == VarInt::ZERO {
                end = next_id;
            } else {
                self.pending = Some(next_id);
                return Some(Ok(start..=end));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_queue_data(
    peer: &mut recv::Context,
    queue_pair: QueuePair,
    binding_id: VarInt,
    offset: VarInt,
    peer_max_offset: u64,
    is_fin: bool,
    blocked: bool,
    buf: BytesMut,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
    recv_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
) {
    let local_queue_id = queue_pair.dest_queue_id;
    let payload_len = buf.len();
    let entry = msg::Stream::Data {
        offset,
        peer_max_offset: VarInt::new(peer_max_offset).unwrap_or(VarInt::MAX),
        fin: is_fin,
        blocked,
        payload: buf,
    }
    .into();

    match peer
        .queue_view
        .send_stream(local_queue_id, binding_id, entry)
    {
        Ok((waker, release_bytes)) => {
            let _ = waker_sink.send(waker);
            recv_credit_pool.release(release_bytes);
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
        Err(crate::queue::Error::Unallocated(_)) => {
            counters.rx_data_unallocated.add(1);
            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueData for unallocated queue - dropping"
            );
        }
        Err(crate::queue::Error::HalfClosed(_)) => {
            counters.rx_data_half_closed.add(1);
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueData for half-closed stream - dropping"
            );
        }
        Err(crate::queue::Error::StaleBinding(_)) => {
            counters.rx_data_stale_binding.add(1);
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueData stale binding - dropping"
            );
        }
        Err(crate::queue::Error::FutureBinding(_)) => {
            counters.rx_data_future_binding.add(1);
            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueData future binding - dropping"
            );
        }
        Err(crate::queue::Error::SenderClosed) => {
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueData for closed sender - dropping"
            );
        }
        Err(crate::queue::Error::CapExceeded(_)) => {
            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueData queue_id exceeds cap - dropping"
            );
        }
    }
}

// ── QueueMsg ──────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn handle_queue_msg(
    peer: &mut recv::Context,
    queue_pair: QueuePair,
    binding_id: VarInt,
    msg_id: VarInt,
    stream_offset: VarInt,
    peer_max_offset: u64,
    message_size: VarInt,
    chunk_size: VarInt,
    chunk_index: VarInt,
    is_fin: bool,
    is_wakeup: bool,
    blocked: bool,
    payload: BytesMut,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
    recv_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
) {
    let local_queue_id = queue_pair.dest_queue_id;
    let payload_len = payload.len() as u32;

    match peer.queue_view.send_msg(
        local_queue_id,
        binding_id,
        msg_id.as_u64(),
        stream_offset.as_u64(),
        peer_max_offset,
        message_size.as_u64() as u32,
        chunk_size.as_u64() as u16,
        chunk_index.as_u64() as u32,
        payload_len,
        is_fin,
        is_wakeup,
        blocked,
        |ptr, len| -> Result<(), ()> {
            unsafe {
                core::ptr::copy_nonoverlapping(payload.as_ptr(), ptr, len as usize);
            }
            Ok(())
        },
    ) {
        Ok((waker, release_bytes)) => {
            if waker.is_some() {
                counters.rx_msg_segment_completed.add(1);
                counters
                    .rx_msg_segment_size
                    .record_value(message_size.as_u64());
                let chunks = message_size.as_u64().div_ceil(chunk_size.as_u64().max(1));
                counters.rx_msg_chunks_per_segment.record_value(chunks);
            }
            let _ = waker_sink.send(waker);
            recv_credit_pool.release(release_bytes);
        }
        Err(crate::queue::MsgError::Queue(crate::queue::Error::Unallocated(_))) => {
            counters.rx_data_unallocated.add(1);
        }
        Err(crate::queue::MsgError::Queue(crate::queue::Error::HalfClosed(_))) => {
            counters.rx_data_half_closed.add(1);
        }
        Err(crate::queue::MsgError::Queue(crate::queue::Error::StaleBinding(_))) => {
            counters.rx_data_stale_binding.add(1);
        }
        Err(crate::queue::MsgError::Queue(crate::queue::Error::FutureBinding(_))) => {
            counters.rx_data_future_binding.add(1);
        }
        Err(crate::queue::MsgError::Queue(crate::queue::Error::SenderClosed)) => {}
        Err(crate::queue::MsgError::Queue(crate::queue::Error::CapExceeded(_))) => {}
        Err(crate::queue::MsgError::InsertRejected) => {
            // The MsgTable rejected this chunk's geometry. On the slow path the packet was already
            // authenticated (decrypted up front before frame dispatch), so dropping the chunk is
            // safe and the packet is still legitimately ACKed by the caller.
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueMsg mixed-path insert rejected - dropping"
            );
        }
        Err(crate::queue::MsgError::Write(_)) => {
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueMsg mixed-path write callback failed - dropping"
            );
        }
    }
}

// ── QueueMsg Init ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn handle_queue_msg_init(
    peer: &mut recv::Context,
    queue_pair: QueuePair,
    binding_id: VarInt,
    acceptor_id: VarInt,
    msg_id: VarInt,
    stream_offset: VarInt,
    peer_max_offset: u64,
    message_size: VarInt,
    chunk_size: VarInt,
    chunk_index: VarInt,
    is_fin: bool,
    is_wakeup: bool,
    blocked: bool,
    payload: BytesMut,
    acceptor_registry: &mut acceptor::LocalRegistry<Stream>,
    frame_tx: &mut SubmissionSender,
    freed_batch_tx: &mut crate::queue::FreedBatchTx,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
    stream_clock: &crate::time::DefaultClock,
    reader_metrics: &Arc<crate::stream::metrics::ReaderMetrics>,
    writer_metrics: &Arc<crate::stream::metrics::WriterMetrics>,
    send_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
    recv_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
    priority: crate::credit::Priority,
) {
    let Some(server_view) = peer.queue_view.as_server_mut() else {
        return;
    };

    let Some(acceptor_sender) = acceptor_registry.get(acceptor_id) else {
        counters.rx_init_no_acceptor.add(1);
        server_view.record_freed(queue_pair.dest_queue_id, &peer.path_entry, freed_batch_tx);
        send_reset(
            &peer.path_entry,
            queue_pair,
            binding_id,
            error::ACCEPTOR_NOT_FOUND,
            frame_tx,
        );
        return;
    };

    // Bind the slot (creates receivers) without pushing data — data goes through push_msg.
    match server_view.bind_for_msg(
        queue_pair.dest_queue_id,
        binding_id,
        &peer.path_entry,
        freed_batch_tx,
    ) {
        Ok(crate::queue::BindResult::NewBinding {
            waker,
            stream,
            control,
            release_bytes,
        }) => {
            // `bind_for_msg` only allocates the slot, so today this waker is empty
            // and `release_bytes` is zero (the real wake/release for this packet
            // comes from the `push_msg` below). Deliver them through the proper
            // channels anyway rather than relying on `AutoWake`'s drop-wake
            // failsafe — if `bind_for_msg` ever produces a live waker (e.g. to
            // wake a parked acceptor) it is routed, not merely dropped.
            let _ = waker_sink.send(waker);
            recv_credit_pool.release(release_bytes);
            let writer = Writer::new_server(
                frame_tx.clone(),
                peer.path_entry.clone(),
                queue_pair.source_queue_id,
                acceptor_id,
                control,
                stream_clock.clone(),
                writer_metrics.clone(),
                send_credit_pool.clone(),
                priority,
            );
            let reader = Reader::new_server(
                frame_tx.clone(),
                peer.path_entry.clone(),
                queue_pair.source_queue_id,
                acceptor_id,
                stream,
                is_fin,
                stream_clock.clone(),
                reader_metrics.clone(),
                recv_credit_pool.clone(),
                priority,
            );
            let new_stream = Stream::new(reader, writer);

            match acceptor_sender.send(new_stream) {
                Ok((mut evicted, acceptor_waker)) => {
                    if let Some(ref mut ev) = evicted {
                        ev.reset(crate::stream::endpoint::Error::ServerBusy);
                    }
                    counters.queue_accepted.add(1);
                    let _ = waker_sink.send(AutoWake::new(acceptor_waker));
                }
                Err(acceptor::channel::SendError::Closed(mut stream)) => {
                    stream.disable();
                    counters.rx_init_acceptor_closed.add(1);
                    // `disable()` is a local-only teardown; reset the client explicitly so its
                    // ACKed QueueInit doesn't leave the writer wedged in InitSent forever.
                    send_reset(
                        &peer.path_entry,
                        queue_pair,
                        binding_id,
                        error::ACCEPTOR_NOT_FOUND,
                        frame_tx,
                    );
                }
                Err(acceptor::channel::SendError::NoSlots(mut stream)) => {
                    stream.disable();
                    counters.rx_init_acceptor_no_slots.add(1);
                    send_reset(
                        &peer.path_entry,
                        queue_pair,
                        binding_id,
                        error::SERVER_BUSY,
                        frame_tx,
                    );
                }
            }
        }
        Ok(crate::queue::BindResult::Bound {
            waker,
            release_bytes,
        }) => {
            // Already bound (init retransmit / coalesced frames). Deliver any
            // waker/bytes through the proper channels rather than relying on
            // drop-wake — see the NewBinding arm. The push_msg below does the
            // real wake/release for this packet.
            let _ = waker_sink.send(waker);
            recv_credit_pool.release(release_bytes);
        }
        Err(_) => {
            return;
        }
    }

    // Now push the actual chunk data through the msg path.
    handle_queue_msg(
        peer,
        queue_pair,
        binding_id,
        msg_id,
        stream_offset,
        peer_max_offset,
        message_size,
        chunk_size,
        chunk_index,
        is_fin,
        is_wakeup,
        blocked,
        payload,
        counters,
        waker_sink,
        recv_credit_pool,
    );
}

// ── QueueData Init ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn handle_queue_data_init(
    peer: &mut recv::Context,
    queue_pair: QueuePair,
    binding_id: VarInt,
    offset: VarInt,
    peer_max_offset: u64,
    is_fin: bool,
    blocked: bool,
    acceptor_id: VarInt,
    buf: BytesMut,
    acceptor_registry: &mut acceptor::LocalRegistry<Stream>,
    frame_tx: &mut SubmissionSender,
    freed_batch_tx: &mut crate::queue::FreedBatchTx,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
    stream_clock: &crate::time::DefaultClock,
    reader_metrics: &Arc<crate::stream::metrics::ReaderMetrics>,
    writer_metrics: &Arc<crate::stream::metrics::WriterMetrics>,
    send_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
    recv_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
    priority: crate::credit::Priority,
) {
    let Some(server_view) = peer.queue_view.as_server_mut() else {
        error!(
            binding_id = binding_id.as_u64(),
            queue_id = queue_pair.dest_queue_id.as_u64(),
            "QueueData init received on client context - dropping"
        );
        return;
    };

    let Some(acceptor_sender) = acceptor_registry.get(acceptor_id) else {
        counters.rx_init_no_acceptor.add(1);
        debug!(
            binding_id = binding_id.as_u64(),
            acceptor_id = acceptor_id.as_u64(),
            "QueueData init rejected - acceptor not found, sending reset"
        );
        server_view.record_freed(queue_pair.dest_queue_id, &peer.path_entry, freed_batch_tx);
        send_reset(
            &peer.path_entry,
            queue_pair,
            binding_id,
            error::ACCEPTOR_NOT_FOUND,
            frame_tx,
        );
        return;
    };

    let entry = msg::Stream::Data {
        offset,
        peer_max_offset: VarInt::new(peer_max_offset).unwrap_or(VarInt::MAX),
        fin: is_fin,
        blocked,
        payload: buf,
    }
    .into();

    match server_view.bind_and_send_stream(
        queue_pair.dest_queue_id,
        binding_id,
        entry,
        &peer.path_entry,
        freed_batch_tx,
    ) {
        Ok(crate::queue::BindResult::NewBinding {
            waker,
            stream,
            control,
            release_bytes,
        }) => {
            let writer = Writer::new_server(
                frame_tx.clone(),
                peer.path_entry.clone(),
                queue_pair.source_queue_id,
                acceptor_id,
                control,
                stream_clock.clone(),
                writer_metrics.clone(),
                send_credit_pool.clone(),
                priority,
            );
            let peer_fin = is_fin;
            let reader = Reader::new_server(
                frame_tx.clone(),
                peer.path_entry.clone(),
                queue_pair.source_queue_id,
                acceptor_id,
                stream,
                peer_fin,
                stream_clock.clone(),
                reader_metrics.clone(),
                recv_credit_pool.clone(),
                priority,
            );
            let new_stream = Stream::new(reader, writer);

            match acceptor_sender.send(new_stream) {
                Ok((mut evicted, acceptor_waker)) => {
                    if let Some(ref mut ev) = evicted {
                        ev.reset(crate::stream::endpoint::Error::ServerBusy);
                    }
                    counters.queue_accepted.add(1);
                    let _ = waker_sink.send(AutoWake::new(acceptor_waker));
                }
                Err(acceptor::channel::SendError::Closed(mut stream)) => {
                    stream.disable();
                    counters.rx_init_acceptor_closed.add(1);
                    // `disable()` is a local-only teardown; reset the client explicitly so its
                    // ACKed QueueInit doesn't leave the writer wedged in InitSent forever.
                    send_reset(
                        &peer.path_entry,
                        queue_pair,
                        binding_id,
                        error::ACCEPTOR_NOT_FOUND,
                        frame_tx,
                    );
                }
                Err(acceptor::channel::SendError::NoSlots(mut stream)) => {
                    stream.disable();
                    counters.rx_init_acceptor_no_slots.add(1);
                    send_reset(
                        &peer.path_entry,
                        queue_pair,
                        binding_id,
                        error::SERVER_BUSY,
                        frame_tx,
                    );
                }
            }

            let _ = waker_sink.send(waker);
            recv_credit_pool.release(release_bytes);

            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = queue_pair.dest_queue_id.as_u64(),
                acceptor_id = acceptor_id.as_u64(),
                "QueueData init - new binding created"
            );
        }
        Ok(crate::queue::BindResult::Bound {
            waker,
            release_bytes,
        }) => {
            let _ = waker_sink.send(waker);
            recv_credit_pool.release(release_bytes);
            counters.rx_data_ok.add(1);
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = queue_pair.dest_queue_id.as_u64(),
                "QueueData init - pushed to existing binding"
            );
        }
        Err(_) => {
            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = queue_pair.dest_queue_id.as_u64(),
                "QueueData init bind failed - dropping"
            );
        }
    }
}

/// Send a plain (non-init) reset in response to the frame whose routing is
/// `incoming_pair`. The reset travels back toward the originator, so the pair is
/// reversed: its `dest_queue_id` is the originator's slot (where the reset is
/// delivered) and its `source_queue_id` is our slot. This is a rejection — the
/// binding does not (and will not) exist here — so no init fields are sent.
fn send_reset(
    path_secret_entry: &Arc<PathSecretEntry>,
    incoming_pair: QueuePair,
    binding_id: VarInt,
    error_code: VarInt,
    frame_tx: &mut SubmissionSender,
) {
    let frame = Frame {
        header: Header::QueueReset {
            queue_pair: incoming_pair.reverse(),
            binding_id,
            reset_target: ResetTarget::Both,
            error_code,
            init: None,
        },
        payload: ByteVec::new(),
        path_secret_entry: path_secret_entry.clone(),
        completion: None,
        status: crate::endpoint::frame::TransmissionStatus::default(),
        ttl: DEFAULT_TTL,
        enqueued_at: None,
        flow_credits: 0,
    };
    let _ = frame_tx.send_batch(Entry::new(frame));
}

// ── QueueControl ───────────────────────────────────────────────────────────

fn handle_queue_control(
    peer: &mut recv::Context,
    queue_pair: QueuePair,
    binding_id: VarInt,
    buf: BytesMut,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
) {
    let local_queue_id = queue_pair.dest_queue_id;
    let payload_len = buf.len();
    let entry = msg::Control::Frames { payload: buf }.into();

    match peer
        .queue_view
        .send_control(local_queue_id, binding_id, entry)
    {
        Ok(waker) => {
            let _ = waker_sink.send(waker);
            counters.rx_queue_control_ok.add(1);
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                payload_len,
                "QueueControl dispatched"
            );
        }
        Err(crate::queue::Error::Unallocated(_)) => {
            counters.rx_queue_control_unallocated.add(1);
            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueControl for unallocated queue - dropping"
            );
        }
        Err(crate::queue::Error::HalfClosed(_)) => {
            counters.rx_queue_control_half_closed.add(1);
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueControl for half-closed queue - dropping"
            );
        }
        Err(_) => {
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueControl dispatch error - dropping"
            );
        }
    }
}

// ── QueueMaxData ───────────────────────────────────────────────────────────

fn handle_queue_max_data(
    peer: &mut recv::Context,
    queue_pair: QueuePair,
    binding_id: VarInt,
    maximum_data: VarInt,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
) {
    let local_queue_id = queue_pair.dest_queue_id;
    let entry = msg::Control::MaxData { maximum_data }.into();

    match peer
        .queue_view
        .send_control(local_queue_id, binding_id, entry)
    {
        Ok(waker) => {
            let _ = waker_sink.send(waker);
            counters.rx_queue_control_ok.add(1);
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                maximum_data = maximum_data.as_u64(),
                "QueueMaxData dispatched"
            );
        }
        Err(crate::queue::Error::Unallocated(_)) => {
            counters.rx_queue_control_unallocated.add(1);
            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueMaxData for unallocated queue - dropping"
            );
        }
        Err(crate::queue::Error::HalfClosed(_)) => {
            counters.rx_queue_control_half_closed.add(1);
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueMaxData for half-closed queue - dropping"
            );
        }
        Err(_) => {
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueMaxData dispatch error - dropping"
            );
        }
    }
}

// ── QueueDataBlocked ───────────────────────────────────────────────────────

/// Deliver a standalone writer-blocked signal to the reader.
///
/// Unlike `QueueMaxData` (reader→writer, routed via the control half), this is writer→reader and
/// the reader drains only the stream half, so it rides the stream queue as `msg::Stream::Blocked`.
/// `send_stream` returns an `AutoWake` so a reader parked in `poll_read_into` with no pending data
/// is still woken to process the signal.
fn handle_queue_data_blocked(
    peer: &mut recv::Context,
    queue_pair: QueuePair,
    binding_id: VarInt,
    desired_offset: VarInt,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
) {
    let local_queue_id = queue_pair.dest_queue_id;
    // A real `QueueDataBlocked` frame from the peer writer: streaming back-pressure, open-ended
    // demand → `synthetic: false` so the reader applies multiplicative slow-start growth.
    let entry = msg::Stream::Blocked {
        desired_offset,
        synthetic: false,
    }
    .into();

    match peer
        .queue_view
        .send_stream(local_queue_id, binding_id, entry)
    {
        Ok((waker, release_bytes)) => {
            let _ = waker_sink.send(waker);
            // A blocked signal carries no payload, so there is nothing to release; assert the
            // invariant rather than silently relying on it.
            debug_assert_eq!(release_bytes, 0);
            counters.rx_data_ok.add(1);
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                desired_offset = desired_offset.as_u64(),
                "QueueDataBlocked dispatched"
            );
        }
        Err(_) => {
            // Unallocated / half-closed / stale binding — the signal is advisory, so drop it.
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = local_queue_id.as_u64(),
                "QueueDataBlocked dispatch error - dropping"
            );
        }
    }
}

// ── QueueReset ─────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn handle_queue_reset(
    peer: &mut recv::Context,
    queue_pair: QueuePair,
    binding_id: VarInt,
    reset_target: ResetTarget,
    error_code: VarInt,
    init: Option<crate::endpoint::frame::Init>,
    acceptor_registry: &mut acceptor::LocalRegistry<Stream>,
    frame_tx: &mut SubmissionSender,
    freed_batch_tx: &mut crate::queue::FreedBatchTx,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
    stream_clock: &crate::time::DefaultClock,
    reader_metrics: &Arc<crate::stream::metrics::ReaderMetrics>,
    writer_metrics: &Arc<crate::stream::metrics::WriterMetrics>,
    send_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
    recv_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
) {
    let dest_queue_id = queue_pair.dest_queue_id;

    // An init reset (one carrying the acceptor id + priority, alongside the
    // sender's queue id in `queue_pair`) can arrive before the QueueInit that
    // would bind the slot — e.g. a client drops its reader before its first
    // write is confirmed, emitting STOP_SENDING. Without binding here the reset
    // would hit an unallocated slot and be silently discarded (`send_control` →
    // `Unallocated`), wedging the peer writer until the idle timeout. Establish
    // the binding from the reset itself — exactly as `handle_queue_data_init`
    // does for a data frame, with the same {acceptor_id, priority} — so a server
    // stream exists to deliver the reset to. Idempotent: if the init already
    // bound the slot this is a no-op `Bound` result and delivery below proceeds.
    if let Some(init) = init {
        bind_for_reset(
            peer,
            queue_pair,
            binding_id,
            init,
            acceptor_registry,
            frame_tx,
            freed_batch_tx,
            counters,
            waker_sink,
            stream_clock,
            reader_metrics,
            writer_metrics,
            send_credit_pool,
            recv_credit_pool,
        );
    }

    match reset_target {
        ResetTarget::Both => {
            counters.rx_reset_both.add(1);
            let stream_entry = msg::Stream::Reset { error_code }.into();
            let control_entry = msg::Control::Reset { error_code }.into();
            if let Ok((waker, _)) =
                peer.queue_view
                    .send_stream(dest_queue_id, binding_id, stream_entry)
            {
                let _ = waker_sink.send(waker);
            }
            if let Ok(waker) =
                peer.queue_view
                    .send_control(dest_queue_id, binding_id, control_entry)
            {
                let _ = waker_sink.send(waker);
            }
            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = dest_queue_id.as_u64(),
                error_code = error_code.as_u64(),
                "QueueReset(Both) dispatched"
            );
        }
        ResetTarget::Stream => {
            counters.rx_reset_stream.add(1);
            let stream_entry = msg::Stream::Reset { error_code }.into();
            if let Ok((waker, _)) =
                peer.queue_view
                    .send_stream(dest_queue_id, binding_id, stream_entry)
            {
                let _ = waker_sink.send(waker);
            }
            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = dest_queue_id.as_u64(),
                error_code = error_code.as_u64(),
                "QueueReset(Stream) dispatched"
            );
        }
        ResetTarget::Control => {
            counters.rx_reset_control.add(1);
            let control_entry = msg::Control::Reset { error_code }.into();
            if let Ok(waker) =
                peer.queue_view
                    .send_control(dest_queue_id, binding_id, control_entry)
            {
                let _ = waker_sink.send(waker);
            }
            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = dest_queue_id.as_u64(),
                error_code = error_code.as_u64(),
                "QueueReset(Control) dispatched"
            );
        }
    }
}

/// Establish a server binding from an init reset, mirroring the `NewBinding`
/// arm of [`handle_queue_data_init`] but without any data entry.
///
/// A reset that races ahead of its QueueInit must still be able to create the
/// server stream so the eventual writer observes it. This binds the slot
/// (creating the stream/control receivers), builds the server `Reader`/`Writer`
/// pair, and hands the stream to the acceptor — after which the reset delivery
/// in [`handle_queue_reset`] pushes the actual reset entry into the now-bound
/// slot. If the slot is already bound (the init won the race) this is a no-op.
#[allow(clippy::too_many_arguments)]
fn bind_for_reset(
    peer: &mut recv::Context,
    queue_pair: QueuePair,
    binding_id: VarInt,
    init: crate::endpoint::frame::Init,
    acceptor_registry: &mut acceptor::LocalRegistry<Stream>,
    frame_tx: &mut SubmissionSender,
    freed_batch_tx: &mut crate::queue::FreedBatchTx,
    counters: &counters::Dispatch,
    waker_sink: &mut impl channel::UnboundedSender<AutoWake>,
    stream_clock: &crate::time::DefaultClock,
    reader_metrics: &Arc<crate::stream::metrics::ReaderMetrics>,
    writer_metrics: &Arc<crate::stream::metrics::WriterMetrics>,
    send_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
    recv_credit_pool: &crate::sync::Arc<crate::credit::Pool>,
) {
    let dest_queue_id = queue_pair.dest_queue_id;
    let source_queue_id = queue_pair.source_queue_id;
    let crate::endpoint::frame::Init {
        dest_acceptor_id: acceptor_id,
        priority,
    } = init;

    let Some(server_view) = peer.queue_view.as_server_mut() else {
        error!(
            binding_id = binding_id.as_u64(),
            queue_id = dest_queue_id.as_u64(),
            "init reset received on client context - not binding"
        );
        return;
    };

    let Some(acceptor_sender) = acceptor_registry.get(acceptor_id) else {
        counters.rx_init_no_acceptor.add(1);
        debug!(
            binding_id = binding_id.as_u64(),
            acceptor_id = acceptor_id.as_u64(),
            "init reset rejected - acceptor not found, sending reset"
        );
        server_view.record_freed(dest_queue_id, &peer.path_entry, freed_batch_tx);
        send_reset(
            &peer.path_entry,
            queue_pair,
            binding_id,
            error::ACCEPTOR_NOT_FOUND,
            frame_tx,
        );
        return;
    };

    match server_view.bind_for_msg(dest_queue_id, binding_id, &peer.path_entry, freed_batch_tx) {
        Ok(crate::queue::BindResult::NewBinding {
            waker,
            stream,
            control,
            release_bytes,
        }) => {
            // Use the priority the peer stamped on the init reset, identical to
            // what a racing data/msg init would carry, so the bound stream lands
            // on the same credit tier regardless of which init frame wins.
            let writer = Writer::new_server(
                frame_tx.clone(),
                peer.path_entry.clone(),
                source_queue_id,
                acceptor_id,
                control,
                stream_clock.clone(),
                writer_metrics.clone(),
                send_credit_pool.clone(),
                priority,
            );
            // No data arrived with the reset, so the reader has not observed a
            // peer FIN: `peer_fin_received = false`.
            let reader = Reader::new_server(
                frame_tx.clone(),
                peer.path_entry.clone(),
                source_queue_id,
                acceptor_id,
                stream,
                false,
                stream_clock.clone(),
                reader_metrics.clone(),
                recv_credit_pool.clone(),
                priority,
            );
            let new_stream = Stream::new(reader, writer);

            match acceptor_sender.send(new_stream) {
                Ok((mut evicted, acceptor_waker)) => {
                    if let Some(ref mut ev) = evicted {
                        ev.reset(crate::stream::endpoint::Error::ServerBusy);
                    }
                    counters.queue_accepted.add(1);
                    let _ = waker_sink.send(AutoWake::new(acceptor_waker));
                }
                Err(acceptor::channel::SendError::Closed(mut stream)) => {
                    stream.disable();
                    counters.rx_init_acceptor_closed.add(1);
                    send_reset(
                        &peer.path_entry,
                        queue_pair,
                        binding_id,
                        error::ACCEPTOR_NOT_FOUND,
                        frame_tx,
                    );
                }
                Err(acceptor::channel::SendError::NoSlots(mut stream)) => {
                    stream.disable();
                    counters.rx_init_acceptor_no_slots.add(1);
                    send_reset(
                        &peer.path_entry,
                        queue_pair,
                        binding_id,
                        error::SERVER_BUSY,
                        frame_tx,
                    );
                }
            }

            let _ = waker_sink.send(waker);
            recv_credit_pool.release(release_bytes);

            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = dest_queue_id.as_u64(),
                acceptor_id = acceptor_id.as_u64(),
                "init reset - new binding created"
            );
        }
        Ok(crate::queue::BindResult::Bound {
            waker,
            release_bytes,
        }) => {
            // The init already bound the slot; nothing to create. Reset delivery
            // proceeds against the existing binding.
            let _ = waker_sink.send(waker);
            recv_credit_pool.release(release_bytes);
            trace!(
                binding_id = binding_id.as_u64(),
                queue_id = dest_queue_id.as_u64(),
                "init reset - slot already bound"
            );
        }
        Err(_) => {
            debug!(
                binding_id = binding_id.as_u64(),
                queue_id = dest_queue_id.as_u64(),
                "init reset bind failed - dropping"
            );
        }
    }
}
