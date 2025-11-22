// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    clock::Timer,
    event,
    packet::stream::PacketSpace,
    stream::{
        send::state::transmission,
        shared::{AcceptState, ArcShared, ShutdownKind},
        socket::Socket,
        Actor,
    },
};
use core::task::{Context, Poll};
use s2n_quic_core::{
    buffer,
    dc::ApplicationParams,
    endpoint, ensure,
    inet::{ExplicitCongestionNotification, SocketAddress},
    ready,
    time::clock::Timer as _,
};
use std::{io, time::Duration};
use tracing::{debug, trace};

const INITIAL_TIMEOUT: Duration = Duration::from_millis(2);

mod waiting {
    use s2n_quic_core::state::{event, is};

    #[derive(Clone, Debug, Default, PartialEq)]
    pub enum State {
        PeekPacket,
        EpochTimeout,
        #[default]
        Cooldown,
        DataRecvd,
        Detached,
        TimeWait,
        Finished,
    }

    impl State {
        is!(is_peek_packet, PeekPacket);
        is!(is_time_wait, TimeWait);
        event! {
            on_peek_packet(PeekPacket => EpochTimeout);
            on_cooldown_elapsed(Cooldown => PeekPacket);
            on_epoch_unchanged(EpochTimeout => PeekPacket);
            on_application_progress(PeekPacket | EpochTimeout | Cooldown => Cooldown);
            on_application_detach(PeekPacket | EpochTimeout | Cooldown => Detached);
            on_data_received(PeekPacket | EpochTimeout | Cooldown => DataRecvd);
            on_time_wait(Detached | DataRecvd => TimeWait);
            on_finished(PeekPacket | EpochTimeout | Cooldown | Detached | DataRecvd | TimeWait => Finished);
        }
    }

    #[test]
    fn dot_test() {
        insta::assert_snapshot!(State::dot());
    }
}

#[repr(u8)]
pub(crate) enum ErrorCode {
    /// The application dropped the stream without errors
    None = 0,
    /// General error code for application-level errors
    Application = 1,
}

pub struct Worker<S, Sub>
where
    S: Socket,
    Sub: event::Subscriber,
{
    shared: ArcShared<Sub>,
    last_observed_epoch: u64,
    state: waiting::State,
    peek_timer: Timer,
    idle_timer: Timer,
    idle_timeout_duration: Duration,
    backoff: u8,
    should_transmit: bool,
    socket: S,
    transmission_buffer: transmission::Builder,
    accept_state: AcceptState,
}

impl<S, Sub> Worker<S, Sub>
where
    S: Socket,
    Sub: event::Subscriber,
{
    #[inline]
    pub fn new(
        socket: S,
        shared: ArcShared<Sub>,
        endpoint: endpoint::Type,
        parameters: &ApplicationParams,
    ) -> Self {
        let idle_timeout_duration = parameters
            .max_idle_timeout()
            .unwrap_or_else(|| Duration::from_secs(30));
        let peek_timer = Timer::new_with_timeout(&shared.clock, INITIAL_TIMEOUT);
        let idle_timer = Timer::new_with_timeout(&shared.clock, idle_timeout_duration);

        let state = match endpoint {
            // on the client we delay before reading from the socket
            endpoint::Type::Client => waiting::State::Cooldown,
            // on the server we need the application to read after accepting, otherwise the peer
            // won't know what our port is
            endpoint::Type::Server => waiting::State::EpochTimeout,
        };

        Self {
            shared,
            last_observed_epoch: 0,
            state,
            peek_timer,
            idle_timer,
            idle_timeout_duration,
            backoff: 0,
            should_transmit: false,
            socket,
            transmission_buffer: Default::default(),
            accept_state: AcceptState::Waiting,
        }
    }

    #[inline]
    pub fn update_waker(&self, cx: &mut Context) {
        self.shared.receiver.worker_waker.update(cx.waker());
    }

    #[inline]
    pub fn poll(&mut self, cx: &mut Context) -> Poll<()> {
        #[cfg(debug_assertions)]
        let _span = {
            use s2n_quic_core::varint::VarInt;
            let local_queue_id = self.shared.local_queue_id().map(VarInt::as_u64);
            let remote_queue_id = self.shared.remote_queue_id().as_u64();
            tracing::warn_span!("worker::recv::poll", local_queue_id, remote_queue_id).entered()
        };

        s2n_quic_core::task::waker::debug_assert_contract(cx, |cx| {
            ready!(self.poll_impl(cx));
            tracing::trace!("read worker shutting down");
            Poll::Ready(())
        })
    }

    #[inline]
    fn poll_impl(&mut self, cx: &mut Context) -> Poll<()> {
        if let Poll::Ready(Err(err)) = self.poll_flush_socket(cx) {
            tracing::error!(socket_error = ?err);
            // TODO should we return? if we get a send error it's most likely fatal
            return Poll::Ready(());
        }

        if let Poll::Ready(Err(err)) = self.poll_socket(cx) {
            tracing::error!(socket_error = ?err);
            // TODO should we return? if we get a recv error it's most likely fatal
            return Poll::Ready(());
        }

        if let Poll::Ready(Err(err)) = self.poll_transmit(cx) {
            tracing::error!(transmit_error = ?err);
            return Poll::Ready(());
        }

        // go until we get into the finished state
        if let waiting::State::Finished = &self.state {
            return Poll::Ready(());
        }

        {
            let target = self.shared.last_peer_activity() + self.idle_timeout_duration;
            self.idle_timer.update(target);
            if self.idle_timer.poll_ready(cx).is_ready() {
                let publisher = self.shared.publisher();
                self.shared.receiver.notify_error(
                    super::ErrorKind::IdleTimeout.err(),
                    endpoint::Location::Local,
                    &publisher,
                );
                return Poll::Ready(());
            }
        }

        if self.should_transmit {
            cx.waker().wake_by_ref();
        }

        Poll::Pending
    }

    #[inline]
    fn poll_socket(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        loop {
            match &self.state {
                waiting::State::PeekPacket => {
                    // check to see if the application is progressing before peeking the socket
                    ensure!(!self.is_application_progressing(), continue);

                    // check if we have something pending
                    ready!(self.shared.receiver.poll_peek_worker(
                        cx,
                        &self.socket,
                        &self.shared.clock,
                        &self.shared.subscriber,
                    ));

                    self.arm_peek_timer();
                    self.state.on_peek_packet().unwrap();
                    continue;
                }
                waiting::State::EpochTimeout => {
                    // check to see if the application is progressing before checking the timer
                    ensure!(!self.is_application_progressing(), continue);

                    ready!(self.peek_timer.poll_ready(cx));

                    // the application isn't making progress so emit the timer expired event
                    self.state.on_epoch_unchanged().unwrap();

                    // only log this message after the first observation
                    if self.last_observed_epoch > 0 {
                        debug!("application reading too slowly from socket");
                    }

                    // reset the backoff with the assumption that the application will go slow in
                    // the future
                    self.backoff = 0;

                    // drain the socket if the application isn't going fast enough
                    return self.poll_drain_recv_socket(cx);
                }
                waiting::State::Cooldown => {
                    // check to see if the application is progressing before checking the timer
                    ensure!(!self.is_application_progressing(), continue);

                    ready!(self.peek_timer.poll_ready(cx));

                    // go back to waiting for a packet
                    let _ = self.state.on_cooldown_elapsed();
                    continue;
                }
                waiting::State::Detached | waiting::State::DataRecvd => {
                    // check if we have any packets in the queue
                    let _ = self.poll_drain_recv_socket(cx);

                    // transition to time wait and arm the timer
                    ensure!(self.state.on_time_wait().is_ok(), continue);

                    // TODO instead of arming a timer, we should add a mode to the `stream` receiver
                    // that allows it to be marked as "free" for reuse while holding the last control
                    // packet that this worker sent. The recv socket pool would look at the credentials
                    // on each packet to see if it should intercept and respond with an old control packet
                    // in case the sender didn't see the control packet. This is similar to TCP Reuse/Recycle.
                    let now = self.shared.clock.get_time();
                    let target = now + Duration::from_millis(500);
                    self.peek_timer.update(target);
                }
                waiting::State::TimeWait => {
                    // check if we have any packets in the socket
                    let _ = self.poll_drain_recv_socket(cx);

                    // make sure we're still in `TimeWait` after looking at the socket
                    ensure!(self.state.is_time_wait(), continue);

                    // wait for the timer to expire
                    ready!(self.peek_timer.poll_ready(cx));

                    // after the timer expires, then transition to the finished state
                    let _ = self.state.on_finished();
                }
                waiting::State::Finished => {
                    // nothing left to do
                    return Ok(()).into();
                }
            }
        }
    }

    #[inline]
    fn is_application_progressing(&mut self) -> bool {
        let state = self.shared.receiver.application_state();

        self.should_transmit |= state.wants_ack;

        // check to see if the application shut down
        if let super::shared::ApplicationStatus::Closed { shutdown_kind } = state.status {
            if matches!(shutdown_kind, ShutdownKind::Pruned) {
                // if the stream was pruned then we don't need to do anything else
                let publisher = self.shared.publisher();
                self.shared.receiver.notify_error(
                    super::ErrorKind::ApplicationError {
                        error: ShutdownKind::PRUNED_CODE.into(),
                    }
                    .err(),
                    endpoint::Location::Local,
                    &publisher,
                );
                let _ = self.state.on_finished();
                self.peek_timer.cancel();
                self.idle_timer.cancel();
                return true;
            }

            if let Ok(Some(mut recv)) = self.shared.receiver.worker_try_lock() {
                let mut should_error = true;

                // check to see if we have anything in the reassembler - if so indicate that we
                // didn't read the whole thing
                should_error &= !recv.reassembler.is_reading_complete();
                should_error &= recv.reassembler.total_received_len() > 0;

                let error = if let Some(code) = shutdown_kind.error_code() {
                    code
                } else if should_error {
                    // we still had data in our buffer so notify the sender
                    ErrorCode::Application as u8
                } else {
                    // no error - the application is just going away
                    ErrorCode::None as u8
                };

                let publisher = self.shared.publisher();
                recv.receiver.stop_sending(error.into(), &publisher);

                if recv.receiver.is_finished() {
                    let _ = self.state.on_finished();
                }
            }

            let _ = self.state.on_application_detach();

            return true;
        }

        let current_epoch = self.shared.receiver.application_epoch();

        // If the application incremented its epoch then it's been accepted
        if current_epoch > 0 {
            self.accept_state = AcceptState::Accepted;
        }

        // make sure the epoch has changed since we last saw it before cooling down
        ensure!(self.last_observed_epoch < current_epoch, false);

        // record the new observation
        self.last_observed_epoch = current_epoch;

        // the application is making progress since the packet is different - loop back to cooldown
        trace!("application is making progress");

        // delay when we read from the socket again to avoid spinning
        let _ = self.state.on_application_progress();
        self.arm_peek_timer();

        // after successful progress from the application we want to intervene less
        self.backoff = (self.backoff + 1).min(10);

        true
    }

    #[inline]
    fn poll_drain_recv_socket(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        let mut received_packets = 0;

        let _res = self.process_packets(cx, &mut received_packets);

        ensure!(
            self.should_transmit,
            if received_packets == 0 {
                Poll::Pending
            } else {
                Ok(()).into()
            }
        );

        self.poll_transmit(cx)
    }

    #[inline]
    fn poll_transmit(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        let state = self.shared.receiver.application_state();
        self.should_transmit |= state.wants_ack;
        ensure!(self.should_transmit, Poll::Ready(Ok(())));

        // send an ACK if needed
        if let Some(mut recv) = self.shared.receiver.worker_try_lock()? {
            let mut transmission_queue = TransmissionQueue {
                buffer: &mut self.transmission_buffer,
                socket_address: self.shared.remote_addr(),
                max_segments: self.shared.gso.max_segments(),
                shared: &self.shared,
            };
            recv.fill_transmit_queue(&self.shared, &mut transmission_queue);

            if !self.transmission_buffer.is_empty() {
                self.should_transmit = false;
            }

            if recv.receiver.state().is_data_received() {
                let _ = self.state.on_data_received();
            }

            if recv.receiver.is_finished() {
                let _ = self.state.on_finished();
            }
        } else {
            cx.waker().wake_by_ref();
        }

        ready!(self.poll_flush_socket(cx))?;

        Ok(()).into()
    }

    #[inline]
    fn process_packets(
        &mut self,
        cx: &mut Context,
        received_packets: &mut usize,
    ) -> io::Result<()> {
        // loop until we hit Pending from the socket
        loop {
            // try_lock the state before reading so we don't consume a packet the application is
            // about to read
            let Some(mut recv) = self.shared.receiver.worker_try_lock()? else {
                // if the application is locking the state then we don't want to transmit, since it
                // will do that for us
                break;
            };

            // make sure to process any left over packets, if any
            if !recv.payload_is_empty() {
                self.should_transmit |= recv.process_recv_buffer(
                    &mut buffer::writer::storage::Empty,
                    &self.shared,
                    self.socket.features(),
                    self.accept_state,
                );
            }

            let res = recv.poll_fill_recv_buffer(
                cx,
                Actor::Worker,
                &self.socket,
                &self.shared.clock,
                &self.shared.subscriber,
            );

            match res {
                Poll::Pending => break,
                Poll::Ready(res) => res?,
            };

            *received_packets += 1;

            // process the packet we just received
            self.should_transmit |= recv.process_recv_buffer(
                &mut buffer::writer::storage::Empty,
                &self.shared,
                self.socket.features(),
                self.accept_state,
            );
        }

        Ok(())
    }

    #[inline]
    fn poll_flush_socket(&mut self, _cx: &mut Context) -> Poll<io::Result<()>> {
        let count = self.transmission_buffer.len();
        ensure!(count > 0, Poll::Ready(Ok(())));

        // Only transmit the last two packets we have pending
        if count > 2 {
            self.transmission_buffer.clear_head(count - 2);
            debug_assert_eq!(self.transmission_buffer.len(), 2);
        }

        for (mut entry, _application_len) in self.transmission_buffer.drain() {
            // don't subscribe to completion events
            entry.completion = None;
            self.socket.send_transmission(entry);
        }

        Ok(()).into()
    }

    #[inline]
    fn arm_peek_timer(&mut self) {
        // TODO do we derive this from RTT?
        let mut timeout = INITIAL_TIMEOUT;
        // don't back off on packet peeks
        if !self.state.is_peek_packet() {
            timeout *= (self.backoff as u32) + 1;
        }
        let now = self.shared.clock.get_time();
        let target = now + timeout;

        self.peek_timer.update(target);
    }
}

struct TransmissionQueue<'a, Sub>
where
    Sub: event::Subscriber,
{
    buffer: &'a mut transmission::Builder,
    socket_address: SocketAddress,
    max_segments: usize,
    shared: &'a ArcShared<Sub>,
}

impl<Sub> super::shared::TransmitQueue for TransmissionQueue<'_, Sub>
where
    Sub: event::Subscriber,
{
    fn push_with<F: FnOnce(io::IoSliceMut) -> usize>(&mut self, f: F) -> Option<usize> {
        let descriptor = self.shared.segment_alloc.alloc_or_grow();
        let descriptor = descriptor
            .fill_with(|addr, _cmsg, payload| {
                let len = f(payload);
                addr.set(self.socket_address);
                if len == 0 {
                    return Err(());
                }
                Ok(len)
            })
            .ok()?;
        let mut descriptor = descriptor.take_filled();

        let ecn = ExplicitCongestionNotification::Ect0;
        let now = self.shared.clock.get_time();

        descriptor.set_ecn(ecn);

        let max_segments = self.max_segments;

        let len: u16 = descriptor.len();

        let info = transmission::Info {
            packet_len: 0,
            descriptor: None.into(),
            stream_offset: Default::default(),
            payload_len: 0,
            flags: Default::default(),
            time_sent: now,
            ecn,
        };

        let meta = transmission::Meta {
            packet_space: PacketSpace::Recovery,
            has_more_app_data: false,
            final_offset: None,
        };

        let transmission_alloc = || {
            self.shared
                .sender
                .alloc_transmission(max_segments, PacketSpace::Recovery)
        };

        self.buffer.push_segment(
            (Default::default(), info),
            meta,
            0,
            descriptor,
            max_segments,
            transmission_alloc,
        );

        Some(len as _)
    }
}
