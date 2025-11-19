// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    clock::{Clock, Timer},
    event,
    packet::{stream::PacketSpace, Packet},
    stream::{
        recv::buffer::{self, Buffer},
        send::{
            error,
            queue::Queue,
            shared::Event,
            state::{ErrorState, State},
        },
        shared::{self, handshake},
        socket::Socket,
        Actor, TransportFeatures,
    },
};
use core::task::{Context, Poll};
use s2n_quic_core::{
    endpoint::{self, Location},
    ensure,
    inet::{ExplicitCongestionNotification, SocketAddress},
    random, ready,
    recovery::bandwidth::Bandwidth,
    time::{
        clock::{self, Timer as _},
        timer::{self, Provider as _},
        Timestamp,
    },
    varint::VarInt,
};
use std::sync::Arc;

mod waiting {
    use s2n_quic_core::state::event;

    #[derive(Clone, Debug, Default, PartialEq)]
    pub enum State {
        #[default]
        Acking,
        Detached,
        ShuttingDown,
        Finished,
    }

    impl State {
        event! {
            on_application_detach(Acking => Detached);
            on_shutdown(Acking | Detached => ShuttingDown);
            on_finished(ShuttingDown => Finished);
        }
    }

    #[test]
    fn dot_test() {
        insta::assert_snapshot!(State::dot());
    }
}

pub struct Worker<S, B, R, Sub, C>
where
    S: Socket,
    B: Buffer,
    R: random::Generator,
    Sub: event::Subscriber,
    C: Clock,
{
    shared: Arc<shared::Shared<Sub, C>>,
    sender: State,
    recv_buffer: B,
    random: R,
    state: waiting::State,
    timer: Timer,
    socket: S,
    handshake: handshake::State,
    transmit_queue: Queue,
}

#[derive(Debug)]
struct Snapshot {
    flow_offset: VarInt,
    send_quantum: usize,
    max_datagram_size: u16,
    ecn: ExplicitCongestionNotification,
    next_expected_control_packet: VarInt,
    timeout: Option<Timestamp>,
    bandwidth: Bandwidth,
    error: Option<ErrorState>,
}

impl Snapshot {
    #[inline]
    fn apply<Sub, C>(&self, initial: &Self, shared: &shared::Shared<Sub, C>)
    where
        Sub: event::Subscriber,
        C: Clock,
    {
        shared.sender.flow.release(self.flow_offset);

        if initial.send_quantum != self.send_quantum {
            let send_quantum = (self.send_quantum as u64).div_ceil(self.max_datagram_size as u64);
            let send_quantum = send_quantum.try_into().unwrap_or(u8::MAX);
            shared
                .sender
                .path
                .update_info(self.ecn, send_quantum, self.max_datagram_size);
        }

        if initial.next_expected_control_packet < self.next_expected_control_packet {
            shared
                .sender
                .path
                .set_next_expected_control_packet(self.next_expected_control_packet);
        }

        if initial.bandwidth != self.bandwidth {
            shared.sender.set_bandwidth(self.bandwidth);
        }

        if let Some(error) = self.error {
            if initial.error.is_none() {
                shared.sender.flow.set_error(error.error);

                if let Some(err) = error.error.for_recv() {
                    let publisher = shared.publisher();
                    shared.receiver.notify_error(err, error.source, &publisher);
                }
            }
        }
    }
}

impl<S, B, R, Sub, C> Worker<S, B, R, Sub, C>
where
    S: Socket,
    B: Buffer,
    R: random::Generator,
    Sub: event::Subscriber,
    C: Clock,
{
    #[inline]
    pub fn new(
        socket: S,
        recv_buffer: B,
        random: R,
        shared: Arc<shared::Shared<Sub, C>>,
        mut sender: State,
        endpoint: endpoint::Type,
    ) -> Self {
        let timer = Timer::new(&shared.clock);
        let state = Default::default();

        // if this is a client then set up the sender
        if endpoint.is_client() {
            sender.init_client(&shared.clock);
        } else {
            sender.init_server(&shared.clock);
        }

        let handshake = match endpoint {
            endpoint::Type::Client => handshake::State::ClientInit,
            endpoint::Type::Server => handshake::State::ServerInit,
        };

        Self {
            shared,
            sender,
            recv_buffer,
            random,
            state,
            timer,
            socket,
            handshake,
            transmit_queue: Default::default(),
        }
    }

    #[inline]
    pub fn update_waker(&self, cx: &mut Context) {
        self.shared.sender.worker_waker.update(cx.waker());
    }

    #[inline]
    pub fn poll(&mut self, cx: &mut Context) -> Poll<()> {
        s2n_quic_core::task::waker::debug_assert_contract(cx, |cx| {
            ready!(self.poll_impl(cx));
            tracing::trace!("write worker shutting down");
            Poll::Ready(())
        })
    }

    #[inline]
    fn poll_impl(&mut self, cx: &mut Context) -> Poll<()> {
        let initial = self.snapshot();

        tracing::trace!(
            view = "before",
            sender_state = ?self.sender.state,
            worker_state = ?self.state,
            snapshot = ?initial,
        );

        self.shared.sender.worker_waker.on_worker_wake();

        self.poll_once(cx);

        // check if the application sent us any more messages
        if !self
            .shared
            .sender
            .worker_waker
            .on_worker_sleep()
            .is_working()
        {
            // yield to the runtime
            cx.waker().wake_by_ref();
        }

        let current = self.snapshot();

        tracing::trace!(
            view = "after",
            sender_state = ?self.sender.state,
            worker_state = ?self.state,
            snapshot = ?current,
        );

        let timeout = current.timeout.filter(|_| {
            // only set a timeout if we're not finished
            !matches!(self.state, waiting::State::Finished)
        });

        current.apply(&initial, &self.shared);

        if let Some(target) = timeout {
            self.timer.update(target);
            if self.timer.poll_ready(cx).is_ready() {
                // If the timer fired then we need to schedule the worker again
                cx.waker().wake_by_ref();
            }
            Poll::Pending
        } else {
            // If the sender has no timeout then we're finished
            debug_assert!(self.sender.state.is_terminal(), "{:?}", self.sender.state);
            self.state = waiting::State::Finished;
            self.timer.cancel();
            Poll::Ready(())
        }
    }

    #[inline]
    fn poll_once(&mut self, cx: &mut Context) {
        self.sender
            .load_completion_queue(&self.shared.sender.transmission_queue, &self.shared.clock);

        let _ = self.poll_messages(cx);
        let _ = self.poll_socket(cx);

        let _ = self.poll_timers(cx);
        let _ = self.poll_transmit(cx);
        self.after_transmit();
    }

    #[inline]
    fn poll_messages(&mut self, cx: &mut Context) -> Poll<()> {
        let _ = cx;

        while let Some(message) = self.shared.sender.pop_worker_message() {
            match message.event {
                Event::Shutdown { kind, mut queue } => {
                    self.transmit_queue.append(&mut queue);

                    // if the application is panicking then we notify the peer
                    if let Some(error) = kind.error_code() {
                        let error = error::Kind::ApplicationError {
                            error: error.into(),
                        };
                        let publisher = self.shared.publisher();
                        self.sender.on_error(error, Location::Local, &publisher);
                    }

                    // transition to a detached state
                    if self.state.on_application_detach().is_ok() {
                        break;
                    }
                }
            }
        }

        Poll::Ready(())
    }

    #[inline]
    fn poll_socket(&mut self, cx: &mut Context) -> Poll<()> {
        loop {
            let mut publisher = self.shared.publisher();
            // try to receive until we get blocked
            let res =
                ready!(self
                    .recv_buffer
                    .poll_fill(cx, Actor::Worker, &self.socket, &mut publisher));

            debug_assert!(!self.recv_buffer.is_empty());

            if let Err(err) = res {
                // the error is fatal so shut down
                if !matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) {
                    let _ = self.state.on_finished();
                }

                return Poll::Ready(());
            }

            self.process_recv_buffer();
        }
    }

    #[inline]
    fn process_recv_buffer(&mut self) {
        ensure!(!self.recv_buffer.is_empty());

        let random = &mut self.random;
        let clock = clock::Cached::new(&self.shared.clock);
        let opener = self
            .shared
            .crypto
            .control_opener()
            .expect("control crypto should be available");

        let had_error = self.sender.error.is_some();
        let publisher = self.shared.publisher();

        {
            let mut router = Router {
                shared: &self.shared,
                opener,
                random,
                sender: &mut self.sender,
                clock,
                remote_addr: Default::default(),
                remote_queue_id: None,
                any_valid_packets: false,
                handshake: &mut self.handshake,
                publisher: &publisher,
            };

            let _ = self
                .recv_buffer
                .process(TransportFeatures::UDP, &mut router);
        }

        if !had_error {
            if let Some(error) = self.sender.error.as_ref() {
                if let Some(err) = error.error.for_recv() {
                    self.shared
                        .receiver
                        .notify_error(err, error.source, &publisher);
                }
            }
        }
    }

    #[inline]
    fn poll_timers(&mut self, cx: &mut Context) -> Poll<()> {
        let _ = cx;
        let shared = &self.shared;
        let clock = clock::Cached::new(&shared.clock);
        let publisher = shared.publisher();
        self.sender
            .on_time_update(&clock, || shared.last_peer_activity(), &publisher);
        Poll::Ready(())
    }

    #[inline]
    fn poll_transmit(&mut self, cx: &mut Context) -> Poll<()> {
        loop {
            ready!(self.poll_transmit_flush(cx));

            match self.state {
                waiting::State::Acking => {
                    self.fill_transmit_queue();
                }
                waiting::State::Detached => {
                    // make sure we have the current view from the application
                    let final_offset = self.shared.sender.flow.stream_offset();
                    self.sender.on_fin_known(final_offset);

                    // transition to shutting down
                    let _ = self.state.on_shutdown();

                    continue;
                }
                waiting::State::ShuttingDown => {
                    self.fill_transmit_queue();

                    if self.sender.state.is_terminal() {
                        let _ = self.state.on_finished();
                    }
                }
                waiting::State::Finished => break,
            }

            ensure!(!self.transmit_queue.is_empty(), break);
        }

        Poll::Ready(())
    }

    #[inline]
    fn fill_transmit_queue(&mut self) {
        let control_sealer = self
            .shared
            .crypto
            .control_sealer()
            .expect("control crypto should be available");

        let publisher = self.shared.publisher();
        let stream_id = self.shared.stream_id();
        let source_queue_id = self.shared.local_queue_id();
        let pool = &self.shared.segment_alloc;
        let remote_address = self.shared.remote_addr();

        let max_segments = self
            .shared
            .gso
            .max_segments()
            .min(self.sender.cca.send_quantum());

        self.transmit_queue.push_buffer(
            &remote_address,
            max_segments,
            pool,
            || {
                self.shared
                    .sender
                    .alloc_transmission(max_segments, PacketSpace::Recovery)
            },
            |packets| {
                let _ = self.sender.fill_transmit_queue(
                    control_sealer,
                    self.shared.credentials(),
                    &stream_id,
                    source_queue_id,
                    &self.shared.clock,
                    packets,
                    &publisher,
                );
            },
        );
    }

    #[inline]
    fn poll_transmit_flush(&mut self, cx: &mut Context) -> Poll<()> {
        ensure!(!self.transmit_queue.is_empty(), Poll::Ready(()));

        self.transmit_queue
            .set_bandwidth(self.sender.cca.bandwidth());

        while !self.transmit_queue.is_empty() {
            let _ = ready!(self.transmit_queue.poll_flush(
                cx,
                usize::MAX,
                &self.socket,
                &self.shared.clock,
                &self.shared.subscriber,
            ));
        }

        Poll::Ready(())
    }

    #[inline]
    fn after_transmit(&mut self) {
        self.sender
            .load_completion_queue(&self.shared.sender.transmission_queue, &self.shared.clock);

        self.sender
            .before_sleep(&clock::Cached::new(&self.shared.clock));
    }

    #[inline]
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            flow_offset: self.sender.flow_offset(),
            send_quantum: self.sender.cca.send_quantum(),
            // TODO get this from the ECN controller
            ecn: ExplicitCongestionNotification::Ect0,
            max_datagram_size: self.sender.max_datagram_size,
            next_expected_control_packet: self.sender.next_expected_control_packet,
            timeout: self.next_expiration(),
            bandwidth: self.sender.cca.bandwidth(),
            error: self.sender.error,
        }
    }
}

impl<S, B, R, Sub, C> timer::Provider for Worker<S, B, R, Sub, C>
where
    S: Socket,
    B: Buffer,
    R: random::Generator,
    Sub: event::Subscriber,
    C: Clock,
{
    fn timers<Q: timer::Query>(&self, query: &mut Q) -> timer::Result {
        self.sender.timers(query)?;
        self.transmit_queue.timers(query)
    }
}

struct Router<'a, Sub, C, R, P>
where
    Sub: event::Subscriber,
    C: Clock,
    R: random::Generator,
    P: event::ConnectionPublisher,
{
    shared: &'a shared::Shared<Sub, C>,
    sender: &'a mut State,
    opener: &'a crate::crypto::awslc::open::control::Stream,
    clock: clock::Cached<'a, C>,
    remote_addr: SocketAddress,
    remote_queue_id: Option<VarInt>,
    random: &'a mut R,
    any_valid_packets: bool,
    handshake: &'a mut handshake::State,
    publisher: &'a P,
}

impl<Sub, C, R, P> buffer::Dispatch for Router<'_, Sub, C, R, P>
where
    Sub: event::Subscriber,
    C: Clock,
    R: random::Generator,
    P: event::ConnectionPublisher,
{
    fn on_packet(
        &mut self,
        remote_addr: &SocketAddress,
        ecn: ExplicitCongestionNotification,
        packet: crate::packet::Packet,
    ) -> Result<(), crate::stream::recv::Error> {
        let credentials = *self.shared.credentials();

        macro_rules! secret_control {
            ($packet:expr, $handle:ident, | $authenticated:ident | $kind:expr) => {{
                let packet = $packet;

                ensure!(packet.credential_id() == &credentials.id, Ok(()));

                let Some($authenticated) = self
                    .shared
                    .crypto
                    .map()
                    .$handle(&$packet, &(*remote_addr).into())
                else {
                    return Ok(());
                };

                self.sender.on_error(
                    {
                        use error::Kind::*;
                        $kind
                    },
                    Location::Local,
                    self.publisher,
                );
                self.shared.receiver.notify_error(
                    {
                        use crate::stream::recv::ErrorKind::*;
                        ($kind).into()
                    },
                    Location::Local,
                    self.publisher,
                );
            }};
        }

        match packet {
            Packet::Control(mut packet) => {
                // make sure we're processing the expected stream
                ensure!(packet.credentials() == &credentials, Ok(()));

                let remote_queue_id = packet.source_queue_id();

                let res = self.sender.on_control_packet(
                    self.opener,
                    ecn,
                    &mut packet,
                    self.random,
                    &self.clock,
                    &self.shared.sender.transmission_queue,
                    self.publisher,
                );

                if res.is_ok() {
                    self.any_valid_packets = true;
                    self.remote_addr = *remote_addr;
                    let _ = self.handshake.on_control_packet();
                    if remote_queue_id.is_some() {
                        self.remote_queue_id = remote_queue_id;
                    }
                }
            }
            Packet::FlowReset(packet) => {
                ensure!(packet.credentials() == &credentials, Ok(()));

                secret_control!(packet, handle_flow_reset_packet, |packet| {
                    ApplicationError {
                        error: packet.code.into(),
                    }
                })
            }
            Packet::StaleKey(packet) => {
                secret_control!(packet, handle_stale_key_packet, |packet| {
                    // make sure that this stream would be rejected before processing
                    ensure!(packet.min_key_id > credentials.key_id, Ok(()));

                    KeyReplayMaybePrevented {
                        gap: Some(packet.min_key_id.as_u64() - credentials.key_id.as_u64()),
                    }
                })
            }
            Packet::ReplayDetected(packet) => {
                secret_control!(packet, handle_replay_detected_packet, |packet| {
                    // make sure the rejected key id matches the credentials we're using
                    ensure!(packet.rejected_key_id == credentials.key_id, Ok(()));

                    KeyReplayPrevented
                })
            }
            Packet::UnknownPathSecret(packet) => {
                secret_control!(packet, handle_unknown_path_secret_packet, |_packet| {
                    UnknownPathSecret
                })
            }
            other => self
                .shared
                .crypto
                .map()
                .handle_unexpected_packet(&other, &(*remote_addr).into()),
        }

        Ok(())
    }
}

impl<Sub, C, R, P> Drop for Router<'_, Sub, C, R, P>
where
    Sub: event::Subscriber,
    C: Clock,
    R: random::Generator,
    P: event::ConnectionPublisher,
{
    #[inline]
    fn drop(&mut self) {
        ensure!(self.any_valid_packets);

        self.shared
            .on_valid_packet(&self.remote_addr, self.remote_queue_id, self.handshake);
    }
}
