// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Stream3 Writer: Fragmentation and flow control
//!
//! The Writer breaks application data into MTU-sized frames and submits them to the pipeline.
//! It manages both local flow control (how much data we can have in flight) and remote flow
//! control (the peer's MAX_DATA window). The pipeline handles retransmission, ACKs, and
//! congestion control.
//!
//! ## Completion Channel Semantics
//!
//! The Writer uses a specialized completion channel (datagram_completion) that distinguishes
//! between normal and abnormal closure:
//!
//! 1. **Normal (graceful) closure**: When the Writer (receiver) is dropped normally after
//!    sending FIN, the `should_transmit` flag remains true so the pipeline continues best-effort
//!    transmission of queued frames. Completion notifications are silently dropped since the
//!    application no longer cares. This allows the application to drop the Writer immediately
//!    after calling shutdown() without blocking transmission.
//!
//! 2. **Abnormal (panic) closure**: When the Writer is dropped during a panic, both
//!    `should_transmit` and `receiver_alive` flags are cleared, and a FlowReset with
//!    ABNORMAL_TERMINATION is sent to the peer. The pipeline will cancel all pending
//!    transmissions and not attempt to send them. This ensures the peer is notified when
//!    the sender crashes.
//!
//! The Drop implementation checks `std::thread::panicking()` to distinguish between these cases.

// TODOs:
//
// Flow control:
//
// * Auto-tune max_inflight_bytes based on completion queue delivery rate. Currently using a
//   fixed budget. If completions arrive quickly, grow the budget to keep the pipe full. If
//   slow, shrink to avoid buffering data that doesn't contribute to throughput. Similar in
//   spirit to recv_budget in the existing streams.
//
// Performance:
//
// * Pace out frame transmissions at 1us interval — right now we're passing `None` for
//   transmission_time. We also need to remember the last transmission time so we don't go
//   backward if we do another burst.
//
// * MTU estimation is overly conservative. MAX_FLOW_DATA_HEADER_OVERHEAD assumes worst-case
//   VarInt sizes for all fields (8 bytes each), but many fields have known values at frame
//   construction time (stream_id, queue_ids, offset). We should compute the actual header
//   size using the known varint-encoded lengths for fields we know, and only use worst-case
//   for fields the transport fills later (source_sender_id, packet_number). This could
//   reclaim 20-30 bytes per frame for typical streams.
//
// Observability:
//
// * No mechanism to report FIN acknowledgment to the application. After sending FIN, the
//   Writer relies on the pipeline to deliver it but has no poll_shutdown_complete or
//   similar. Currently by design (see Completion Channel Semantics), but limits the
//   application's ability to confirm graceful close.
//
// * No idle timeout detection at the stream level. If the peer disappears silently, the
//   Writer only learns about it when a completion eventually fails (PeerDead/TransmissionError).
//   The gap between the peer dying and the Writer finding out could be large.
//
// Testing:
//
// * Deterministic tests using bach for: flow control stalls and recovery, FIN delivery,
//   early data with FlowInit, completion failure handling, panic-drop behavior, and
//   multi-stream contention on shared pipeline resources.

use crate::{
    byte_vec::ByteVec,
    intrusive_queue::Queue,
    packet::{
        control,
        datagram::{partial::MAX_FLOW_DATA_HEADER_OVERHEAD, QueuePair, ResetTarget},
    },
    path::secret::map::Entry as PathSecretEntry,
    stream3::{
        endpoint::{
            msg,
            reset_error::{self, ResetError},
        },
        frame::{self, Frame, Header, SubmissionSender, DEFAULT_TTL},
    },
};
use s2n_quic_core::{
    buffer::{self, writer::Storage},
    state::{event, is},
    task::waker,
    varint::VarInt,
};
use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tracing::{debug, trace};

pub struct Writer(Box<Inner>);

struct Inner {
    /// Channel to submit frames to the wheel
    frame_tx: SubmissionSender,
    /// Receiver for completion notifications from the pipeline
    completion_rx: frame::CompletionReceiver,
    /// Control-side channel for receiving MAX_DATA frames
    control_rx: msg::queue::Control,
    /// Path secret entry providing MTU and crypto material
    path_secret_entry: Arc<PathSecretEntry>,
    /// Cached packet size (MTU minus header overhead) for fragmentation
    packet_size: u16,
    /// Stream identifier
    stream_id: VarInt,
    /// Acceptor ID for server routing
    acceptor_id: VarInt,
    /// Next byte offset to send
    next_offset: VarInt,
    /// Number of bytes currently in flight (not yet acknowledged)
    inflight_bytes: u64,
    /// Maximum number of bytes allowed in flight (local flow control)
    max_inflight_bytes: u64,
    /// Remote flow control budget: maximum offset we can send to
    remote_max_data: VarInt,
    /// Current status of the writer
    status: Status,
    /// Reset error code if the stream was reset by the peer
    reset_error_code: Option<VarInt>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Status {
    /// Initial state before sending FlowInit
    #[default]
    Init,
    /// FlowInit sent, waiting for acknowledgment
    FlowInitSent,
    /// Flow established and open for writes
    Open,
    /// FIN sent
    FinSent,
    /// Shutdown completed
    Shutdown,
}

impl Status {
    is!(is_init, Init);
    is!(is_flow_init_sent, FlowInitSent);
    is!(is_open, Open);
    is!(is_fin_sent, FinSent);
    is!(is_shutdown, Shutdown);
    is!(is_terminal, FinSent | Shutdown);

    event! {
        on_send_flow_init(Init => FlowInitSent);
        on_flow_established(FlowInitSent => Open);
        on_send_fin(FlowInitSent | Open => FinSent);
        on_shutdown(Init | FlowInitSent | Open | FinSent => Shutdown);
    }
}

impl Writer {
    pub(crate) fn new_client(
        frame_tx: SubmissionSender,
        path_secret_entry: Arc<PathSecretEntry>,
        stream_id: VarInt,
        acceptor_id: VarInt,
        control_rx: msg::queue::Control,
    ) -> Self {
        let completion_rx = frame::completion_channel();
        let parameters = path_secret_entry.parameters();
        let mtu = parameters.max_datagram_size();
        let packet_size = mtu.saturating_sub(MAX_FLOW_DATA_HEADER_OVERHEAD);
        let max_inflight_bytes = parameters.local_send_max_data.as_u64();
        let remote_max_data = VarInt::ZERO;

        Self(Box::new(Inner {
            frame_tx,
            completion_rx,
            control_rx,
            path_secret_entry,
            packet_size,
            stream_id,
            acceptor_id,
            next_offset: VarInt::ZERO,
            inflight_bytes: 0,
            max_inflight_bytes,
            remote_max_data,
            status: Status::Init,
            reset_error_code: None,
        }))
    }

    pub(crate) fn new_server(
        frame_tx: SubmissionSender,
        path_secret_entry: Arc<PathSecretEntry>,
        stream_id: VarInt,
        control_rx: msg::queue::Control,
    ) -> Self {
        let completion_rx = frame::completion_channel();
        let parameters = path_secret_entry.parameters();
        let mtu = parameters.max_datagram_size();
        let packet_size = mtu.saturating_sub(MAX_FLOW_DATA_HEADER_OVERHEAD);
        let max_inflight_bytes = parameters.local_send_max_data.as_u64();
        let initial_remote_max_data = parameters.remote_max_data;

        Self(Box::new(Inner {
            frame_tx,
            completion_rx,
            control_rx,
            path_secret_entry,
            packet_size,
            stream_id,
            acceptor_id: VarInt::ZERO,
            next_offset: VarInt::ZERO,
            inflight_bytes: 0,
            max_inflight_bytes,
            remote_max_data: initial_remote_max_data,
            status: Status::Open,
            reset_error_code: None,
        }))
    }

    pub async fn write_from<S>(&mut self, buf: &mut S) -> io::Result<usize>
    where
        S: buffer::reader::storage::Infallible,
    {
        core::future::poll_fn(|cx| self.poll_write_from(cx, buf, false)).await
    }

    pub async fn write_all_from<S>(&mut self, buf: &mut S) -> io::Result<usize>
    where
        S: buffer::reader::storage::Infallible,
    {
        let mut total = 0;
        loop {
            total += self.write_from(buf).await?;
            if buf.buffer_is_empty() {
                return Ok(total);
            }
        }
    }

    pub async fn write_from_fin<S>(&mut self, buf: &mut S) -> io::Result<usize>
    where
        S: buffer::reader::storage::Infallible,
    {
        core::future::poll_fn(|cx| self.poll_write_from(cx, buf, true)).await
    }

    pub async fn write_all_from_fin<S>(&mut self, buf: &mut S) -> io::Result<usize>
    where
        S: buffer::reader::storage::Infallible,
    {
        let mut total = 0;
        loop {
            total += self.write_from_fin(buf).await?;
            if buf.buffer_is_empty() {
                return Ok(total);
            }
        }
    }

    pub fn poll_write_from<S>(
        &mut self,
        cx: &mut Context,
        buf: &mut S,
        is_fin: bool,
    ) -> Poll<io::Result<usize>>
    where
        S: buffer::reader::storage::Infallible,
    {
        waker::debug_assert_contract(cx, |cx| self.0.poll_write_from(cx, buf, is_fin))
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        self.0.shutdown()
    }

    pub(crate) fn force_shutdown(&mut self) {
        self.0.completion_rx.cancel();
        self.0.status.on_shutdown().ok();
    }
}

impl Inner {
    fn poll_write_from<S>(
        &mut self,
        cx: &mut Context,
        buf: &mut S,
        is_fin: bool,
    ) -> Poll<io::Result<usize>>
    where
        S: buffer::reader::storage::Infallible,
    {
        if self.status.is_shutdown() {
            if let Some(error_code) = self.reset_error_code {
                let reset_error: ResetError = error_code.into();
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    reset_error,
                )));
            }
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }

        if self.status.is_fin_sent() {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }

        self.poll_completions(cx)?;
        let _ = self.poll_remote_budget(cx)?;

        if self.status.is_init() {
            let (written, is_fin) = self.send_flow_init_with_early_data(buf, is_fin)?;

            if written > 0 || is_fin {
                return Poll::Ready(Ok(written));
            }

            return Poll::Pending;
        }

        if self.status.is_flow_init_sent() {
            trace!(
                stream_id = self.stream_id.as_u64(),
                "Writer blocked in FlowInitSent - waiting for remote MAX_DATA"
            );
            return Poll::Pending;
        }

        let available = self.min_send_budget();
        if available == 0 && !is_fin {
            return Poll::Pending;
        }

        let written = self.send_data(buf, is_fin)?;

        Poll::Ready(Ok(written))
    }

    fn shutdown(&mut self) -> io::Result<()> {
        if self.status.is_shutdown() {
            return Ok(());
        }

        if self.status.is_fin_sent() {
            self.status.on_shutdown().unwrap();
            return Ok(());
        }

        self.send_fin_packet()?;
        self.status.on_shutdown().unwrap();

        Ok(())
    }

    fn send_reset_frame(
        &mut self,
        error_code: VarInt,
        reset_target: ResetTarget,
    ) -> io::Result<()> {
        let Some(remote_queue_id) = self.control_rx.remote_queue_id() else {
            debug!(
                stream_id = self.stream_id.as_u64(),
                "Cannot send reset before flow established"
            );
            return Ok(());
        };

        let frame = Frame {
            source_sender_id: VarInt::MAX,
            header: Header::FlowReset {
                dest_queue_id: remote_queue_id,
                stream_id: self.stream_id,
                reset_target,
                error_code,
            },
            payload: ByteVec::new(),
            path_secret_entry: self.path_secret_entry.clone(),
            completion: None,
            status: frame::TransmissionStatus::default(),
            ttl: DEFAULT_TTL,
            transmission_time: None,
        };

        self.send_frame(frame)?;

        debug!(
            stream_id = self.stream_id.as_u64(),
            error_code = error_code.as_u64(),
            ?reset_target,
            "Sent FlowReset"
        );

        Ok(())
    }

    fn send_fin_packet(&mut self) -> io::Result<()> {
        if self.status.is_init() {
            self.send_flow_init_with_early_data(&mut buffer::reader::storage::Empty, true)?;
        } else if self.status.is_open() {
            let queue_pair = QueuePair {
                source_queue_id: self.control_rx.queue_id(),
                dest_queue_id: self
                    .control_rx
                    .remote_queue_id()
                    .expect("remote_queue_id must be set when Open"),
            };

            let frame = Frame {
                source_sender_id: VarInt::MAX,
                header: Header::FlowData {
                    queue_pair,
                    stream_id: self.stream_id,
                    offset: self.next_offset,
                    is_fin: true,
                },
                payload: ByteVec::new(),
                path_secret_entry: self.path_secret_entry.clone(),
                completion: Some(self.completion_rx.sender()),
                status: frame::TransmissionStatus::default(),
                ttl: DEFAULT_TTL,
                transmission_time: None,
            };

            self.send_frame(frame)?;

            debug!(stream_id = self.stream_id.as_u64(), "Sent FIN");
            self.status.on_send_fin().unwrap();
        }

        Ok(())
    }

    fn poll_completions(&mut self, cx: &mut Context) -> io::Result<()> {
        use crate::stream3::frame::{FailureReason, TransmissionStatus};

        match self.completion_rx.poll_swap(cx) {
            Poll::Ready(Some(queue)) => {
                let mut freed_bytes = 0u64;
                let mut failure = None;

                for completed in queue.iter() {
                    match completed.status {
                        TransmissionStatus::Acknowledged => {
                            freed_bytes += completed.payload.len() as u64;
                        }
                        TransmissionStatus::Failed(reason) => {
                            failure.get_or_insert(reason);
                            freed_bytes += completed.payload.len() as u64;

                            debug!(
                                stream_id = self.stream_id.as_u64(),
                                ?reason,
                                "Transmission failed"
                            );
                        }
                        TransmissionStatus::Pending => {
                            tracing::warn!(
                                stream_id = self.stream_id.as_u64(),
                                "Received completion with Pending status"
                            );
                        }
                    }
                }

                self.inflight_bytes = self.inflight_bytes.saturating_sub(freed_bytes);

                trace!(
                    stream_id = self.stream_id.as_u64(),
                    freed_bytes,
                    inflight_bytes = self.inflight_bytes,
                    "Completions received"
                );

                if let Some(reason) = failure {
                    return match reason {
                        FailureReason::UnknownPathSecret => {
                            self.status.on_shutdown().ok();
                            Err(io::Error::new(
                                io::ErrorKind::ConnectionRefused,
                                "path secret rejected by peer",
                            ))
                        }
                        FailureReason::PeerDead => {
                            self.status.on_shutdown().ok();
                            Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "peer declared dead (idle timeout)",
                            ))
                        }
                        FailureReason::TransmissionError => {
                            let error_code = reset_error::RETRANSMISSIONS_EXHAUSTED;
                            let _ = self.send_reset_frame(error_code, ResetTarget::Both);
                            self.status.on_shutdown().ok();
                            Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "transmission failed after retries",
                            ))
                        }
                        FailureReason::Cancelled => {
                            self.status.on_shutdown().ok();
                            Err(io::Error::new(
                                io::ErrorKind::Interrupted,
                                "transmission cancelled",
                            ))
                        }
                    };
                }

                Ok(())
            }
            Poll::Ready(None) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "completion channel closed",
            )),
            Poll::Pending => Ok(()),
        }
    }

    fn poll_remote_budget(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        match self.control_rx.poll_swap(cx) {
            Poll::Ready(Ok(queue)) => {
                debug!(
                    stream_id = self.stream_id.as_u64(),
                    status = ?self.status,
                    msg_count = queue.len(),
                    "poll_remote_budget received messages"
                );
                for msg in queue {
                    match msg.into_inner() {
                        msg::Control::Frames { mut payload } => {
                            if self.handle_control_frames(&mut *payload).is_err() {
                                let error_code = reset_error::FRAME_DECODE_ERROR;
                                self.reset_error_code = Some(error_code);
                                self.status.on_shutdown().ok();

                                let _ = self.send_reset_frame(error_code, ResetTarget::Both);

                                let reset_error: ResetError = error_code.into();
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    reset_error,
                                )));
                            }

                            if self.status.on_flow_established().is_ok() {
                                debug_assert!(self.control_rx.remote_queue_id().is_some());
                                debug!(stream_id = self.stream_id.as_u64(), "Flow established");
                            }
                        }
                        msg::Control::Reset { error_code } => {
                            self.reset_error_code = Some(error_code);
                            self.status.on_shutdown().ok();
                            let reset_error: ResetError = error_code.into();
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::ConnectionReset,
                                reset_error,
                            )));
                        }
                    }
                }

                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(_)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "control channel closed",
            ))),
            Poll::Pending => {
                trace!(
                    stream_id = self.stream_id.as_u64(),
                    status = ?self.status,
                    "poll_remote_budget pending - no control messages"
                );
                Poll::Pending
            }
        }
    }

    fn handle_control_frames(&mut self, payload: &mut [u8]) -> Result<(), s2n_codec::DecoderError> {
        use s2n_quic_core::frame::{FrameMut, MaxData};

        let mut frames_iter = control::decoder::ControlFramesMut::new(payload);

        while let Some(frame) = frames_iter.next() {
            match frame? {
                FrameMut::MaxData(MaxData { maximum_data }) => {
                    let prev_max = self.remote_max_data;
                    self.remote_max_data = self.remote_max_data.max(maximum_data);
                    trace!(
                        stream_id = self.stream_id.as_u64(),
                        prev_max = prev_max.as_u64(),
                        new_max = self.remote_max_data.as_u64(),
                        "Received MAX_DATA"
                    );
                }
                frame => {
                    trace!(
                        stream_id = self.stream_id.as_u64(),
                        frame = ?frame,
                        "Ignoring control frame"
                    );
                }
            }
        }

        Ok(())
    }

    fn send_flow_init_with_early_data<S>(
        &mut self,
        buf: &mut S,
        is_fin: bool,
    ) -> io::Result<(usize, bool)>
    where
        S: buffer::reader::storage::Infallible,
    {
        let (payload, bytes_read, actual_fin) = self.prepare_early_data(buf, is_fin)?;

        let frame = Frame {
            source_sender_id: VarInt::MAX,
            header: Header::FlowInit {
                source_queue_id: self.control_rx.queue_id(),
                dest_acceptor_id: self.acceptor_id,
                attempt_id: VarInt::MAX,
                stream_id: self.stream_id,
                is_fin: actual_fin,
            },
            payload,
            path_secret_entry: self.path_secret_entry.clone(),
            completion: Some(self.completion_rx.sender()),
            status: frame::TransmissionStatus::default(),
            ttl: DEFAULT_TTL,
            transmission_time: None,
        };

        self.send_frame(frame)?;

        self.status.on_send_flow_init().unwrap();

        if actual_fin {
            self.status.on_send_fin().unwrap();
        }

        debug!(
            stream_id = self.stream_id.as_u64(),
            bytes_read,
            is_fin = actual_fin,
            "Sent FlowInit with early data"
        );

        Ok((bytes_read, actual_fin))
    }

    fn prepare_early_data<S>(
        &mut self,
        buf: &mut S,
        is_fin: bool,
    ) -> io::Result<(ByteVec, usize, bool)>
    where
        S: buffer::reader::storage::Infallible,
    {
        if is_fin && buf.buffer_is_empty() {
            return Ok((ByteVec::new(), 0, true));
        }

        if buf.buffer_is_empty() {
            return Ok((ByteVec::new(), 0, false));
        }

        if self.remaining_offset_capacity() == 0 {
            return Err(offset_overflow_error());
        }

        let mtu = self.packet_size as usize;
        let local_available = self
            .max_inflight_bytes
            .saturating_sub(self.inflight_bytes) as usize;
        let chunk_len = mtu
            .min(buf.buffered_len())
            .min(self.remaining_offset_capacity())
            .min(local_available);

        let mut payload = ByteVec::new();
        {
            let mut writer = payload.with_write_limit(chunk_len);
            buf.infallible_copy_into(&mut writer);
        }

        let bytes_read = payload.len();

        self.advance_offset(bytes_read)?;

        let actual_is_fin = is_fin && buf.buffer_is_empty();

        Ok((payload, bytes_read, actual_is_fin))
    }

    fn min_send_budget(&self) -> u64 {
        let local_available = self.max_inflight_bytes.saturating_sub(self.inflight_bytes);
        let remote_available = self
            .remote_max_data
            .as_u64()
            .saturating_sub(self.next_offset.as_u64());

        local_available.min(remote_available)
    }

    fn send_data<S>(&mut self, buf: &mut S, is_fin: bool) -> io::Result<usize>
    where
        S: buffer::reader::storage::Infallible,
    {
        let mtu = self.packet_size as usize;
        let mut written = 0;

        let mut need_fin_packet = is_fin && buf.buffer_is_empty();

        loop {
            if !need_fin_packet && buf.buffer_is_empty() {
                break;
            }

            let remaining_offset_capacity = self.remaining_offset_capacity();
            if !need_fin_packet && remaining_offset_capacity == 0 {
                if written == 0 {
                    return Err(offset_overflow_error());
                }
                break;
            }

            let available = self.min_send_budget();
            if !need_fin_packet && available == 0 {
                break;
            }

            let chunk_len = if need_fin_packet {
                0
            } else {
                mtu.min(buf.buffered_len())
                    .min(available as usize)
                    .min(remaining_offset_capacity)
            };

            let mut payload = ByteVec::new();
            if chunk_len > 0 {
                let mut writer = payload.with_write_limit(chunk_len);
                buf.infallible_copy_into(&mut writer);
            }

            let payload_len = payload.len();
            let offset = self.next_offset;
            let is_last_chunk = buf.buffer_is_empty();
            let include_fin = is_fin && is_last_chunk;

            let queue_pair = QueuePair {
                source_queue_id: self.control_rx.queue_id(),
                dest_queue_id: self
                    .control_rx
                    .remote_queue_id()
                    .expect("remote_queue_id must be set when Open"),
            };

            let frame = Frame {
                source_sender_id: VarInt::MAX,
                header: Header::FlowData {
                    queue_pair,
                    stream_id: self.stream_id,
                    offset,
                    is_fin: include_fin,
                },
                payload,
                path_secret_entry: self.path_secret_entry.clone(),
                completion: Some(self.completion_rx.sender()),
                status: frame::TransmissionStatus::default(),
                ttl: DEFAULT_TTL,
                transmission_time: None,
            };

            self.send_frame(frame)?;

            self.advance_offset(payload_len)?;
            written += payload_len;

            trace!(
                stream_id = self.stream_id.as_u64(),
                offset = offset.as_u64(),
                payload_len,
                is_fin = include_fin,
                "Sending FlowData"
            );

            if include_fin {
                self.status.on_send_fin().ok();
            }

            need_fin_packet = false;
        }

        Ok(written)
    }

    fn remaining_offset_capacity(&self) -> usize {
        let remaining = VarInt::MAX
            .as_u64()
            .saturating_sub(self.next_offset.as_u64());

        usize::try_from(remaining).unwrap_or(usize::MAX)
    }

    fn advance_offset(&mut self, payload_len: usize) -> io::Result<()> {
        self.next_offset = self
            .next_offset
            .checked_add_usize(payload_len)
            .ok_or_else(offset_overflow_error)?;
        self.inflight_bytes += payload_len as u64;
        Ok(())
    }

    fn send_frame(&mut self, frame: Frame) -> io::Result<()> {
        let mut batch = Queue::new();
        batch.push_back(frame.into());
        self.frame_tx
            .send_batch(batch)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "frame channel closed"))
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        debug!(
            stream_id = self.0.stream_id.as_u64(),
            status = ?self.0.status,
            next_offset = self.0.next_offset.as_u64(),
            inflight_bytes = self.0.inflight_bytes,
            remote_max_data = self.0.remote_max_data.as_u64(),
            "Writer dropping"
        );

        if std::thread::panicking() {
            self.0.completion_rx.cancel();

            let error_code = reset_error::ABNORMAL_TERMINATION;
            let _ = self.0.send_reset_frame(error_code, ResetTarget::Both);
            debug!(
                stream_id = self.0.stream_id.as_u64(),
                "Writer dropped during panic - sent FlowReset and cancelled transmissions"
            );
        } else {
            let _ = self.shutdown();
        }
    }
}

fn offset_overflow_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "stream offset overflow")
}

/// Bach/bolero simulation tests for the stream3 writer.
///
/// These tests generate random sequences of writer operations (Write, Shutdown) interleaved
/// with random pipeline responses (Ack, Fail, MaxData, Reset, Hold) and assert that no
/// writer invariants are violated:
///
/// - `inflight_bytes` never exceeds `max_inflight_bytes`
/// - frame offsets are monotonically non-overlapping
/// - `offset + payload_len` never exceeds `remote_max_data` at the time of sending
/// - FIN is sent at most once
/// - no new FlowData frames are sent after the peer has issued a Reset
///
/// The `bach_writer_sim` test additionally runs each scenario inside a deterministic bach
/// time simulation with real async scheduling.
#[cfg(test)]
mod sim_tests {
    use super::*;
    use crate::{
        flow,
        intrusive_queue::{Entry, Queue},
        socket::{
            channel::{self, Receiver as _},
            pool::descriptor::Unfilled,
        },
        stream3::frame,
    };
    use bolero::TypeGenerator;
    use s2n_codec::{Encoder, EncoderBuffer, EncoderValue as _};
    use s2n_quic_core::{task::waker, varint::VarInt};
    use std::task::Poll;

    // ── Operations ────────────────────────────────────────────────────────────

    /// Operations that the writer side applies.
    #[derive(Clone, Debug, TypeGenerator)]
    enum WriterOp {
        /// Write N bytes of application data.
        Write(#[generator(0u16..=1024)] u16),
        /// Graceful shutdown (send FIN and stop).
        Shutdown,
    }

    /// Operations that the simulated pipeline applies.
    #[derive(Clone, Debug, TypeGenerator)]
    enum PipelineOp {
        /// Acknowledge all pending (and held) frames.
        AckAll,
        /// Fail all pending (and held) frames with the given reason.
        FailAll(FailureKind),
        /// Send a MAX_DATA update to the writer's control channel.
        MaxData(#[generator(0u64..=65536)] u64),
        /// Peer sends a Reset to the writer.
        Reset(#[generator(0u8..=10)] u8),
        /// Accumulate frames without returning completions (slow pipeline).
        Hold,
    }

    /// Which failure reason the pipeline uses when failing frames.
    #[derive(Clone, Copy, Debug, TypeGenerator)]
    enum FailureKind {
        PeerDead,
        TransmissionError,
        UnknownPathSecret,
        Cancelled,
    }

    impl FailureKind {
        fn into_status(self) -> frame::TransmissionStatus {
            frame::TransmissionStatus::Failed(match self {
                Self::PeerDead => frame::FailureReason::PeerDead,
                Self::TransmissionError => frame::FailureReason::TransmissionError,
                Self::UnknownPathSecret => frame::FailureReason::UnknownPathSecret,
                Self::Cancelled => frame::FailureReason::Cancelled,
            })
        }
    }

    // ── Scenario ──────────────────────────────────────────────────────────────

    /// The full scenario that the fuzzer generates for one test iteration.
    #[derive(Clone, Debug, TypeGenerator)]
    struct Scenario {
        /// Initial remote MAX_DATA window.
        ///
        /// `0` means client-mode (no writes until `MaxData` establishes flow).
        /// Non-zero means server-mode (flow already established).
        #[generator(0u32..=8192)]
        initial_remote_max_data: u32,
        /// Local in-flight budget (max bytes we allow in flight at once).
        #[generator(64u64..=8192)]
        max_inflight_bytes: u64,
        /// Interleaved writer and pipeline operations.
        ops: Vec<(WriterOp, PipelineOp)>,
    }

    // ── Harness ───────────────────────────────────────────────────────────────

    type FrameRx = channel::intrusive_queue::sharded::Receiver<
        crate::intrusive_queue::EntryAdapter<Frame>,
    >;

    struct Harness {
        inner: Inner,
        frame_rx: FrameRx,
        /// Completion sender for returning frames back to the writer.
        completion_sender: frame::CompletionSender,
        /// Frames held by the pipeline without completing them.
        held_frames: Vec<Entry<Frame>>,
        /// Keep the allocator alive so its internal senders (which hold `IS_OPEN`) are
        /// not dropped and do not close the writer's control channel prematurely.
        _allocator: msg::queue::Allocator,
        /// Keep the stream receiver alive alongside the control receiver so the
        /// descriptor is not freed before the test is done.
        _stream_rx: msg::queue::Stream,
        // ── Invariant state ──
        /// Highest byte offset sent so far (exclusive end of last frame).
        next_expected_offset: u64,
        /// Whether we have already seen a FIN frame.
        fin_seen: bool,
        /// Whether the pipeline has already sent a Reset to the writer.
        reset_sent: bool,
    }

    fn make_harness(scenario: &Scenario) -> Harness {
        let (frame_tx, frame_rx) = channel::intrusive_queue::sharded::new::<Frame>(1);
        let waker_ref = waker::noop();
        frame_rx.register(&waker_ref);

        let path_secret_entry =
            PathSecretEntry::fake("127.0.0.1:8080".parse().unwrap(), None);
        let stream_id = VarInt::from_u8(42);
        let handle = flow::Handle::client(stream_id, path_secret_entry.clone());
        let mut allocator = msg::queue::Allocator::new();
        let (control_rx, stream_rx) =
            allocator.alloc_or_grow(handle, Some(VarInt::from_u8(7)));

        let completion_rx = frame::completion_channel();
        let completion_sender = completion_rx.sender();

        let initial_max_data =
            VarInt::try_from(scenario.initial_remote_max_data as u64).unwrap_or(VarInt::MAX);
        // Server mode has flow already established; client mode starts in Init.
        let (status, remote_max_data) = if scenario.initial_remote_max_data > 0 {
            (Status::Open, initial_max_data)
        } else {
            (Status::Init, VarInt::ZERO)
        };

        let inner = Inner {
            frame_tx,
            completion_rx,
            control_rx,
            path_secret_entry,
            packet_size: 512,
            stream_id,
            acceptor_id: VarInt::ZERO,
            next_offset: VarInt::ZERO,
            inflight_bytes: 0,
            max_inflight_bytes: scenario.max_inflight_bytes,
            remote_max_data,
            status,
            reset_error_code: None,
        };

        Harness {
            inner,
            frame_rx,
            completion_sender,
            held_frames: Vec::new(),
            _allocator: allocator,
            _stream_rx: stream_rx,
            next_expected_offset: 0,
            fin_seen: false,
            reset_sent: false,
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Encode a QUIC MAX_DATA frame (type 0x10 + varint) into a fresh `descriptor::Filled`.
    fn encode_max_data(value: u64) -> Option<crate::socket::pool::descriptor::Filled> {
        let max_data = VarInt::try_from(value).ok()?;
        // 1 byte type tag + variable-length integer
        let size = 1 + max_data.encoding_size();
        let unfilled = Unfilled::new(size as u16)?;
        unfilled
            .fill_with(|_addr, _cmsg, mut iov| {
                let mut enc = EncoderBuffer::new(&mut iov[..size]);
                enc.encode(&0x10u8); // MAX_DATA frame type tag
                enc.encode(&max_data);
                Ok::<_, core::convert::Infallible>(size)
            })
            .ok()
            .map(|segs| segs.take_filled())
    }

    fn noop_cx() -> core::task::Context<'static> {
        let waker = Box::leak(Box::new(waker::noop()));
        core::task::Context::from_waker(waker)
    }

    // ── Harness methods ───────────────────────────────────────────────────────

    impl Harness {
        /// Non-blocking drain of all currently available frames from `frame_rx`.
        fn drain_frames(&mut self) -> Vec<Entry<Frame>> {
            let waker_ref = waker::noop();
            let mut cx = core::task::Context::from_waker(&waker_ref);
            let mut frames = Vec::new();
            loop {
                match self.frame_rx.poll_recv(&mut cx) {
                    Poll::Ready(Some(list)) => frames.extend(list),
                    _ => break,
                }
            }
            frames
        }

        /// Verify per-frame invariants.
        ///
        /// Called for every frame that arrives in `frame_rx` before the pipeline
        /// acts on it.  By this point the writer has already sent the frame, so
        /// `inner.remote_max_data` reflects the value that was in place at the
        /// time the frame was built (it can only grow afterwards).
        fn check_frame_invariants(&mut self, frame: &Frame) {
            if let Header::FlowData {
                offset, is_fin, ..
            } = frame.header
            {
                let payload_len = frame.payload_len() as u64;
                let end = offset.as_u64() + payload_len;

                // No new data frames after the peer sent a Reset.
                assert!(
                    !self.reset_sent,
                    "FlowData frame received after pipeline sent Reset: offset={} len={}",
                    offset.as_u64(),
                    payload_len,
                );

                // Offsets must be monotonically non-decreasing.
                assert!(
                    offset.as_u64() >= self.next_expected_offset,
                    "non-monotonic offset: frame starts at {} but expected >= {}",
                    offset.as_u64(),
                    self.next_expected_offset,
                );

                // The frame must not exceed the remote MAX_DATA window.
                assert!(
                    end <= self.inner.remote_max_data.as_u64(),
                    "frame [{}..{}) exceeds remote_max_data {}",
                    offset.as_u64(),
                    end,
                    self.inner.remote_max_data.as_u64(),
                );

                // Advance the expected offset tracker.
                if end > self.next_expected_offset {
                    self.next_expected_offset = end;
                }

                // FIN must appear at most once.
                if is_fin {
                    assert!(!self.fin_seen, "FIN was sent more than once");
                    self.fin_seen = true;
                }
            }
        }

        /// Check the per-step state invariants on the writer.
        fn check_state_invariants(&self) {
            assert!(
                self.inner.inflight_bytes <= self.inner.max_inflight_bytes,
                "inflight_bytes {} exceeds max_inflight_bytes {}",
                self.inner.inflight_bytes,
                self.inner.max_inflight_bytes,
            );
        }

        /// Return completions for all frames with Acknowledged status.
        fn ack_frames(&self, frames: Vec<Entry<Frame>>) {
            if frames.is_empty() {
                return;
            }
            let mut queue = Queue::new();
            for mut entry in frames {
                entry.status = frame::TransmissionStatus::Acknowledged;
                queue.push_back(entry);
            }
            self.completion_sender.send_batch(queue).ok();
        }

        /// Return completions for all frames with the given failure status.
        fn fail_frames(&self, frames: Vec<Entry<Frame>>, kind: FailureKind) {
            if frames.is_empty() {
                return;
            }
            let mut queue = Queue::new();
            for mut entry in frames {
                entry.status = kind.into_status();
                queue.push_back(entry);
            }
            self.completion_sender.send_batch(queue).ok();
        }

        /// Push a MAX_DATA update into the writer's control channel.
        fn push_max_data(&self, value: u64) {
            if let Some(payload) = encode_max_data(value) {
                self.inner
                    .control_rx
                    .push(Entry::new(msg::Control::Frames { payload }));
            }
        }

        /// Push a peer-initiated Reset into the writer's control channel.
        fn push_reset(&self, code: u8) {
            let error_code = VarInt::try_from(code as u64).unwrap_or(VarInt::ZERO);
            self.inner
                .control_rx
                .push(Entry::new(msg::Control::Reset { error_code }));
        }

        /// Drain frames from `frame_rx`, check their invariants, then apply the
        /// pipeline op to them.
        fn apply_pipeline_op(&mut self, op: &PipelineOp) {
            let new_frames = self.drain_frames();
            for f in &new_frames {
                self.check_frame_invariants(f);
            }

            match op {
                PipelineOp::AckAll => {
                    let mut all = self.held_frames.drain(..).collect::<Vec<_>>();
                    all.extend(new_frames);
                    self.ack_frames(all);
                }
                PipelineOp::FailAll(kind) => {
                    let mut all = self.held_frames.drain(..).collect::<Vec<_>>();
                    all.extend(new_frames);
                    self.fail_frames(all, *kind);
                }
                PipelineOp::MaxData(v) => {
                    self.push_max_data(*v);
                    // Hold the new frames without completing them.
                    self.held_frames.extend(new_frames);
                }
                PipelineOp::Reset(code) => {
                    self.reset_sent = true;
                    self.push_reset(*code);
                    // Drop all pending frames — no completions sent.
                    drop(new_frames);
                    self.held_frames.clear();
                }
                PipelineOp::Hold => {
                    self.held_frames.extend(new_frames);
                }
            }
        }

        /// Apply one writer op.  Returns `true` when the writer has shut down.
        fn apply_writer_op(
            &mut self,
            op: &WriterOp,
            cx: &mut core::task::Context,
        ) -> bool {
            match op {
                WriterOp::Write(n) => {
                    let data = vec![0u8; *n as usize];
                    let mut buf: &[u8] = &data;
                    let _ = self.inner.poll_write_from(cx, &mut buf, false);
                    false
                }
                WriterOp::Shutdown => {
                    let _ = self.inner.shutdown();
                    true
                }
            }
        }

        /// Let the writer process any completions and control messages that the
        /// pipeline has queued since the last step.
        fn drain_writer_inbox(&mut self, cx: &mut core::task::Context) {
            // poll_completions and poll_remote_budget are called inside
            // poll_write_from, but we call them directly here so the writer
            // can react even when it is not actively writing.
            let _ = self.inner.poll_completions(cx);
            let _ = self.inner.poll_remote_budget(cx);
        }
    }

    // ── Core runner ───────────────────────────────────────────────────────────

    fn run_scenario(scenario: &Scenario) {
        let mut harness = make_harness(scenario);
        let mut cx = noop_cx();

        for (writer_op, pipeline_op) in &scenario.ops {
            // 1. Apply the writer op (may send frames or return Pending).
            let done = harness.apply_writer_op(writer_op, &mut cx);
            harness.check_state_invariants();

            // 2. Apply the pipeline op (drain frames, check invariants, react).
            harness.apply_pipeline_op(pipeline_op);

            // 3. Let the writer pick up the pipeline's responses.
            harness.drain_writer_inbox(&mut cx);
            harness.check_state_invariants();

            if done {
                break;
            }
        }

        // After all ops: drain and check any remaining frames.
        let remaining = harness.drain_frames();
        for f in &remaining {
            harness.check_frame_invariants(&f);
        }
        harness.check_state_invariants();
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// Property-based test: run random scenarios without time simulation.
    #[test]
    fn bolero_writer_sim() {
        crate::testing::without_tracing(|| {
            bolero::check!()
                .with_type::<Scenario>()
                .with_test_time(core::time::Duration::from_secs(30))
                .for_each(|scenario| run_scenario(scenario));
        });
    }

    /// Same property-based test inside a bach deterministic time simulation.
    ///
    /// Each iteration runs in its own `sim()` environment, which exercises
    /// real async scheduling and virtual time (e.g. tokio::task::yield_now).
    #[test]
    fn bach_writer_sim() {
        crate::testing::without_tracing(|| {
            bolero::check!()
                .with_type::<Scenario>()
                .with_test_time(core::time::Duration::from_secs(30))
                .for_each(|scenario| {
                    let scenario = scenario.clone();
                    crate::testing::sim(|| {
                        // SAFETY: bach's sim executor is single-threaded; the future
                        // never crosses thread boundaries.  We wrap the non-Send
                        // future so bach::spawn can accept it.
                        struct SendWrapper<F>(F);
                        unsafe impl<F> Send for SendWrapper<F> {}
                        unsafe impl<F> Sync for SendWrapper<F> {}
                        impl<F: core::future::Future> core::future::Future for SendWrapper<F> {
                            type Output = F::Output;
                            fn poll(
                                self: core::pin::Pin<&mut Self>,
                                cx: &mut core::task::Context<'_>,
                            ) -> core::task::Poll<Self::Output> {
                                // SAFETY: we never move the inner future after pinning.
                                unsafe {
                                    core::pin::Pin::new_unchecked(
                                        &mut self.get_unchecked_mut().0,
                                    )
                                    .poll(cx)
                                }
                            }
                        }

                        async fn run(scenario: Scenario) {
                            let mut harness = make_harness(&scenario);
                            let mut cx = noop_cx();

                            for (writer_op, pipeline_op) in &scenario.ops {
                                let done = harness.apply_writer_op(writer_op, &mut cx);
                                harness.check_state_invariants();

                                // Yield so bach's scheduler can run other work.
                                tokio::task::yield_now().await;

                                harness.apply_pipeline_op(pipeline_op);
                                harness.drain_writer_inbox(&mut cx);
                                harness.check_state_invariants();

                                if done {
                                    break;
                                }
                            }

                            let remaining = harness.drain_frames();
                            for f in &remaining {
                                harness.check_frame_invariants(&f);
                            }
                            harness.check_state_invariants();
                        }

                        use bach::ext::{PrimaryExt as _, SpawnExt as _};
                        SendWrapper(run(scenario)).primary().spawn();
                    });
                });
        });
    }
}

#[cfg(feature = "tokio")]
impl tokio::io::AsyncWrite for Writer {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.poll_write_from(cx, &mut buf, false)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[std::io::IoSlice],
    ) -> Poll<Result<usize, io::Error>> {
        let mut buf = buffer::reader::storage::IoSlice::new(buf);
        self.poll_write_from(cx, &mut buf, false)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        self.shutdown().into()
    }

    fn is_write_vectored(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        flow,
        socket::channel::{self, Receiver as _},
    };
    use bytes::Bytes;

    fn new_test_inner() -> (
        Inner,
        channel::intrusive_queue::sharded::Receiver<crate::intrusive_queue::EntryAdapter<Frame>>,
    ) {
        let (frame_tx, frame_rx) = channel::intrusive_queue::sharded::new::<Frame>(1);
        let waker = s2n_quic_core::task::waker::noop();
        frame_rx.register(&waker);

        let path_secret_entry = PathSecretEntry::fake("127.0.0.1:8080".parse().unwrap(), None);
        let stream_id = VarInt::from_u8(42);
        let handle = flow::Handle::client(stream_id, path_secret_entry.clone());
        let mut allocator = msg::queue::Allocator::new();
        let (control_rx, _stream_rx) = allocator.alloc_or_grow(handle, Some(VarInt::from_u8(7)));

        let inner = Inner {
            frame_tx,
            completion_rx: frame::completion_channel(),
            control_rx,
            path_secret_entry,
            packet_size: 1200,
            stream_id,
            acceptor_id: VarInt::ZERO,
            next_offset: VarInt::ZERO,
            inflight_bytes: 0,
            max_inflight_bytes: 4096,
            remote_max_data: VarInt::from_u16(4096),
            status: Status::Open,
            reset_error_code: None,
        };

        (inner, frame_rx)
    }

    fn completed_frame(
        path_secret_entry: Arc<PathSecretEntry>,
        stream_id: VarInt,
        payload_len: usize,
        status: frame::TransmissionStatus,
    ) -> Frame {
        let mut payload = ByteVec::new();
        if payload_len > 0 {
            payload.push_back(Bytes::from(vec![0; payload_len]));
        }

        Frame {
            source_sender_id: VarInt::MAX,
            header: Header::FlowData {
                queue_pair: QueuePair {
                    source_queue_id: VarInt::from_u8(1),
                    dest_queue_id: VarInt::from_u8(2),
                },
                stream_id,
                offset: VarInt::ZERO,
                is_fin: false,
            },
            payload,
            path_secret_entry,
            completion: None,
            status,
            ttl: DEFAULT_TTL,
            transmission_time: None,
        }
    }

    fn send_completions(inner: &Inner, completions: impl IntoIterator<Item = Frame>) {
        let mut queue = Queue::new();
        for completion in completions {
            queue.push_back(completion.into());
        }
        inner.completion_rx.sender().send_batch(queue).unwrap();
    }

    fn noop_cx() -> core::task::Context<'static> {
        let waker = Box::leak(Box::new(s2n_quic_core::task::waker::noop()));
        core::task::Context::from_waker(waker)
    }

    #[test]
    fn poll_completions_prefers_first_failure_and_skips_later_reset() {
        let (mut inner, mut frame_rx) = new_test_inner();
        inner.inflight_bytes = 23;

        send_completions(
            &inner,
            [
                completed_frame(
                    inner.path_secret_entry.clone(),
                    inner.stream_id,
                    5,
                    frame::TransmissionStatus::Acknowledged,
                ),
                completed_frame(
                    inner.path_secret_entry.clone(),
                    inner.stream_id,
                    7,
                    frame::TransmissionStatus::Failed(frame::FailureReason::UnknownPathSecret),
                ),
                completed_frame(
                    inner.path_secret_entry.clone(),
                    inner.stream_id,
                    11,
                    frame::TransmissionStatus::Failed(frame::FailureReason::TransmissionError),
                ),
            ],
        );

        let mut cx = noop_cx();
        let err = inner.poll_completions(&mut cx).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(inner.inflight_bytes, 0);
        assert!(inner.status.is_shutdown());
        assert!(matches!(frame_rx.poll_recv(&mut cx), Poll::Pending));
    }

    #[test]
    fn poll_completions_keeps_first_transmission_error() {
        let (mut inner, mut frame_rx) = new_test_inner();
        inner.inflight_bytes = 18;

        send_completions(
            &inner,
            [
                completed_frame(
                    inner.path_secret_entry.clone(),
                    inner.stream_id,
                    7,
                    frame::TransmissionStatus::Failed(frame::FailureReason::TransmissionError),
                ),
                completed_frame(
                    inner.path_secret_entry.clone(),
                    inner.stream_id,
                    11,
                    frame::TransmissionStatus::Failed(frame::FailureReason::PeerDead),
                ),
            ],
        );

        let mut cx = noop_cx();
        let err = inner.poll_completions(&mut cx).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(inner.inflight_bytes, 0);
        assert!(inner.status.is_shutdown());

        let sent = match frame_rx.poll_recv(&mut cx) {
            Poll::Ready(Some(sent)) => sent,
            other => panic!("expected reset frame, got {other:?}"),
        };

        let sent = sent.iter().collect::<Vec<_>>();
        assert_eq!(sent.len(), 1);
        assert!(matches!(
            sent[0].header,
            Header::FlowReset {
                error_code,
                reset_target: ResetTarget::Both,
            ..
            } if error_code == reset_error::RETRANSMISSIONS_EXHAUSTED
        ));
    }

    #[test]
    fn prepare_early_data_returns_error_before_consuming_on_offset_overflow() {
        let (mut inner, _) = new_test_inner();
        inner.next_offset = VarInt::MAX;

        let mut buf = &b"x"[..];
        let err = inner.prepare_early_data(&mut buf, false).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(buf, b"x");
        assert_eq!(inner.inflight_bytes, 0);
        assert_eq!(inner.next_offset, VarInt::MAX);
    }

    #[test]
    fn send_data_allows_fin_at_varint_max() {
        let (mut inner, mut frame_rx) = new_test_inner();
        inner.next_offset = VarInt::MAX;

        let mut buf = buffer::reader::storage::Empty;
        let written = inner.send_data(&mut buf, true).unwrap();

        assert_eq!(written, 0);
        assert!(inner.status.is_fin_sent());
        assert_eq!(inner.next_offset, VarInt::MAX);

        let mut cx = noop_cx();
        let sent = match frame_rx.poll_recv(&mut cx) {
            Poll::Ready(Some(sent)) => sent,
            other => panic!("expected FIN frame, got {other:?}"),
        };

        let sent = sent.iter().collect::<Vec<_>>();
        assert_eq!(sent.len(), 1);
        assert!(matches!(
            sent[0].header,
            Header::FlowData {
                offset,
                is_fin: true,
                ..
            } if offset == VarInt::MAX
        ));
    }

    #[test]
    fn send_data_caps_payload_at_varint_max() {
        let (mut inner, mut frame_rx) = new_test_inner();
        inner.next_offset = VarInt::MAX - VarInt::from_u8(1);
        inner.remote_max_data = VarInt::MAX;

        let mut buf = &b"xy"[..];
        let written = inner.send_data(&mut buf, false).unwrap();

        assert_eq!(written, 1);
        assert_eq!(buf, b"y");
        assert_eq!(inner.inflight_bytes, 1);
        assert_eq!(inner.next_offset, VarInt::MAX);

        let mut cx = noop_cx();
        let sent = match frame_rx.poll_recv(&mut cx) {
            Poll::Ready(Some(sent)) => sent,
            other => panic!("expected data frame, got {other:?}"),
        };

        let sent = sent.iter().collect::<Vec<_>>();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].payload.len(), 1);
    }

    #[test]
    fn send_data_returns_error_before_consuming_when_offset_is_max() {
        let (mut inner, mut frame_rx) = new_test_inner();
        inner.next_offset = VarInt::MAX;
        inner.remote_max_data = VarInt::MAX;

        let mut buf = &b"x"[..];
        let err = inner.send_data(&mut buf, false).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(buf, b"x");
        assert_eq!(inner.next_offset, VarInt::MAX);
        assert_eq!(inner.inflight_bytes, 0);

        let mut cx = noop_cx();
        assert!(matches!(frame_rx.poll_recv(&mut cx), Poll::Pending));
    }
}
