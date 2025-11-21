// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    clock,
    either::Either,
    event,
    packet::{stream, Packet},
    stream::{
        recv::{self, buffer::Buffer as _},
        shared::{self, handshake, AcceptState, ArcShared, ShutdownKind},
        socket::{self, Socket},
        Actor, TransportFeatures,
    },
    task::waker::worker::Waker as WorkerWaker,
};
use core::{
    fmt,
    mem::ManuallyDrop,
    ops,
    task::{Context, Poll},
};
use s2n_quic_core::{
    buffer, dc,
    endpoint::{self, Location},
    ensure,
    inet::SocketAddress,
    ready,
    stream::state,
    time::Clock,
    varint::VarInt,
};
use std::{
    io::{self, IoSliceMut},
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Mutex, MutexGuard,
    },
};

pub type RecvBuffer = Either<recv::buffer::Local, recv::buffer::Channel>;

#[derive(Debug)]
pub struct ApplicationState {
    pub status: ApplicationStatus,
    pub wants_ack: bool,
}

#[derive(Debug)]
pub enum ApplicationStatus {
    Open,
    Closed { shutdown_kind: ShutdownKind },
}

impl ApplicationStatus {
    fn from_u8(value: u8) -> Self {
        for (kind, mask) in ApplicationState::SHUTDOWN_KINDS {
            if value & mask != 0 {
                return Self::Closed {
                    shutdown_kind: *kind,
                };
            }
        }

        Self::Open
    }
}

impl ApplicationState {
    const IS_CLOSED_MASK: u8 = 1;
    const IS_ERRORED_MASK: u8 = 1 << 1;
    const IS_PRUNED_MASK: u8 = 1 << 2;
    const WANTS_ACK: u8 = 1 << 3;

    const SHUTDOWN_KINDS: &[(ShutdownKind, u8)] = &[
        (ShutdownKind::Normal, Self::IS_CLOSED_MASK),
        (ShutdownKind::Errored, Self::IS_ERRORED_MASK),
        (ShutdownKind::Pruned, Self::IS_PRUNED_MASK),
    ];

    #[inline]
    fn load(shared: &AtomicU8) -> Self {
        let mask = u8::MAX ^ Self::WANTS_ACK;
        let value = shared.fetch_and(mask, Ordering::Acquire);
        let status = ApplicationStatus::from_u8(value);
        let wants_ack = value & Self::WANTS_ACK != 0;
        Self { status, wants_ack }
    }

    fn wants_ack(shared: &AtomicU8) {
        shared.fetch_or(Self::WANTS_ACK, Ordering::Release);
    }

    #[inline]
    fn close(shared: &AtomicU8, kind: ShutdownKind) {
        let mut value = Self::IS_CLOSED_MASK;

        match kind {
            ShutdownKind::Normal => {}
            ShutdownKind::Errored => {
                value |= Self::IS_ERRORED_MASK;
            }
            ShutdownKind::Pruned => {
                value |= Self::IS_PRUNED_MASK;
            }
        }

        shared.store(value, Ordering::Release);
    }
}

pub trait AsShared: 'static + Send + Sync {
    fn as_shared(&self) -> &State;
}

pub struct State {
    inner: Mutex<Inner>,
    application_epoch: AtomicU64,
    application_state: AtomicU8,
    pub worker_waker: WorkerWaker,
    is_owned_socket: bool,
}

impl State {
    #[inline]
    pub fn new<C>(
        stream_id: stream::Id,
        params: &dc::ApplicationParams,
        features: TransportFeatures,
        buffer: RecvBuffer,
        endpoint: endpoint::Type,
        clock: &C,
    ) -> Self
    where
        C: Clock + ?Sized,
    {
        let receiver = recv::state::State::new(stream_id, params, features, clock);
        let reassembler = Default::default();
        let is_owned_socket = matches!(buffer, Either::A(recv::buffer::Local { .. }));
        let inner = Inner {
            receiver,
            reassembler,
            buffer,
            handshake: endpoint.into(),
            application_waker: None,
        };
        let inner = Mutex::new(inner);
        Self {
            inner,
            application_epoch: AtomicU64::new(0),
            application_state: AtomicU8::new(0),
            worker_waker: Default::default(),
            is_owned_socket,
        }
    }

    #[inline]
    pub fn application_state(&self) -> ApplicationState {
        ApplicationState::load(&self.application_state)
    }

    #[inline]
    pub fn application_epoch(&self) -> u64 {
        self.application_epoch.load(Ordering::Acquire)
    }

    #[inline]
    pub fn application_guard<'a, Sub>(
        &'a self,
        shared: &'a ArcShared<Sub>,
        sockets: &'a dyn socket::Application,
    ) -> io::Result<AppGuard<'a, Sub>>
    where
        Sub: event::Subscriber,
    {
        // increment the epoch at which we acquired the guard
        self.application_epoch.fetch_add(1, Ordering::AcqRel);

        let inner = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("shared recv state has been poisoned"))?;

        let initial_state = inner.receiver.state().clone();

        let inner = ManuallyDrop::new(inner);

        Ok(AppGuard {
            inner,
            shared,
            sockets,
            initial_state,
        })
    }

    #[inline]
    pub fn shutdown(&self, shutdown_kind: ShutdownKind) {
        ApplicationState::close(&self.application_state, shutdown_kind);
        self.worker_waker.wake();
    }

    pub fn on_prune<Pub>(&self, publisher: &Pub)
    where
        Pub: event::ConnectionPublisher,
    {
        self.notify_error(
            recv::error::Kind::ApplicationError {
                error: ShutdownKind::PRUNED_CODE.into(),
            }
            .err(),
            Location::Local,
            publisher,
        );
    }

    #[inline]
    pub fn notify_error<Pub>(&self, error: recv::Error, source: Location, publisher: &Pub)
    where
        Pub: event::ConnectionPublisher,
    {
        let waker = {
            let mut inner = self.inner.lock().unwrap();
            inner.receiver.on_error(error, source, publisher);
            inner.application_waker.take()
        };
        self.worker_waker.wake();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    #[inline]
    pub fn poll_peek_worker<S, C, Sub>(
        &self,
        cx: &mut Context,
        socket: &S,
        clock: &C,
        subscriber: &shared::Subscriber<Sub>,
    ) -> Poll<()>
    where
        S: ?Sized + Socket,
        C: ?Sized + Clock,
        Sub: event::Subscriber,
    {
        if self.is_owned_socket {
            let _ = ready!(socket.poll_peek_len(cx));
            return Poll::Ready(());
        }
        let Ok(Some(mut inner)) = self.worker_try_lock() else {
            // have the worker arm its timer
            return Poll::Ready(());
        };
        let _ = ready!(inner.poll_fill_recv_buffer(cx, Actor::Worker, socket, clock, subscriber));
        Poll::Ready(())
    }

    #[inline]
    pub fn worker_try_lock(&self) -> io::Result<Option<MutexGuard<'_, Inner>>> {
        match self.inner.try_lock() {
            Ok(lock) => Ok(Some(lock)),
            Err(std::sync::TryLockError::WouldBlock) => Ok(None),
            Err(_) => Err(io::Error::other("shared recv state has been poisoned")),
        }
    }
}

pub struct AppGuard<'a, Sub>
where
    Sub: event::Subscriber,
{
    inner: ManuallyDrop<MutexGuard<'a, Inner>>,
    shared: &'a ArcShared<Sub>,
    sockets: &'a dyn socket::Application,
    initial_state: state::Receiver,
}

impl<Sub> AppGuard<'_, Sub>
where
    Sub: event::Subscriber,
{
    /// Returns `true` if the read worker should be woken
    #[inline]
    fn send_ack(&mut self) -> bool {
        // we only send ACKs for unreliable protocols
        ensure!(!self.sockets.features().is_reliable(), false);

        // If we haven't completed the handshake then no need to send an ACK
        ensure!(
            !matches!(self.handshake, handshake::State::ClientInit),
            false
        );

        // only wake the worker if the receiver says we should
        self.inner.receiver.should_transmit()
    }
}

impl<Sub> ops::Deref for AppGuard<'_, Sub>
where
    Sub: event::Subscriber,
{
    type Target = Inner;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<Sub> ops::DerefMut for AppGuard<'_, Sub>
where
    Sub: event::Subscriber,
{
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<Sub> Drop for AppGuard<'_, Sub>
where
    Sub: event::Subscriber,
{
    #[inline]
    fn drop(&mut self) {
        let wake_worker_for_ack = self.send_ack();

        let current_state = self.inner.receiver.state().clone();

        unsafe {
            // SAFETY: inner is no longer used
            ManuallyDrop::drop(&mut self.inner);
        }

        if wake_worker_for_ack && !current_state.is_terminal() {
            ApplicationState::wants_ack(&self.shared.receiver.application_state);
            self.shared.receiver.worker_waker.wake_forced();
            return;
        }

        // no need to look at anything if the state didn't change
        ensure!(self.initial_state != current_state);
        ensure!(self.buffer.is_empty());
        ensure!(self.reassembler.is_empty());

        // shut down the worker if we're in a terminal state
        if current_state.is_terminal() {
            self.shared.receiver.shutdown(ShutdownKind::Normal);
        }
    }
}

pub trait TransmitQueue {
    fn push_with<F: FnOnce(IoSliceMut) -> usize>(&mut self, f: F) -> Option<usize>;
}

pub struct Inner {
    pub receiver: recv::state::State,
    pub reassembler: buffer::Reassembler,
    buffer: RecvBuffer,
    handshake: handshake::State,
    application_waker: Option<core::task::Waker>,
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("receiver", &self.receiver)
            .field("reassembler", &self.reassembler)
            .field("handshake", &self.handshake)
            .finish()
    }
}

impl Inner {
    #[inline]
    pub fn payload_is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    #[inline]
    pub fn fill_transmit_queue<Sub, T>(&mut self, shared: &ArcShared<Sub>, transmit_queue: &mut T)
    where
        Sub: event::Subscriber,
        T: TransmitQueue,
    {
        let stream_id = shared.stream_id();
        let source_queue_id = shared.local_queue_id();

        self.receiver.on_transmit(
            shared
                .crypto
                .control_sealer()
                .expect("control sealer should be available with recv transmissions"),
            shared.credentials(),
            stream_id,
            source_queue_id,
            transmit_queue,
            &shared.clock,
            &shared.publisher(),
        );
    }

    #[inline]
    pub fn poll_fill_recv_buffer<S, C, Sub>(
        &mut self,
        cx: &mut Context,
        actor: Actor,
        socket: &S,
        clock: &C,
        subscriber: &shared::Subscriber<Sub>,
    ) -> Poll<io::Result<usize>>
    where
        S: ?Sized + Socket,
        C: ?Sized + Clock,
        Sub: event::Subscriber,
    {
        let res = self.buffer.poll_fill(
            cx,
            actor,
            socket,
            &mut subscriber.publisher(clock.get_time()),
        );

        // If we haven't received anything yet, it's possible this stream might be rejected by the sender worker so we need
        // to store the waker if that happens
        if matches!(actor, Actor::Application)
            && matches!(self.handshake, handshake::State::ClientInit)
            && res.is_pending()
        {
            self.application_waker = Some(cx.waker().clone());
        }

        res
    }

    #[inline]
    pub fn process_recv_buffer<Sub>(
        &mut self,
        out_buf: &mut impl buffer::writer::Storage,
        shared: &ArcShared<Sub>,
        features: TransportFeatures,
        accept_state: AcceptState,
    ) -> bool
    where
        Sub: event::Subscriber,
    {
        let clock = &shared.clock;
        let publisher = shared.publisher_with_timestamp(clock.get_time());

        // try copying data out of the reassembler into the application buffer
        self.receiver
            .on_read_buffer(&mut self.reassembler, out_buf, accept_state, clock);

        // check if we have any packets to process
        if !self.buffer.is_empty() {
            let res = {
                let mut out_buf = buffer::duplex::Interposer::new(out_buf, &mut self.reassembler);

                if features.is_stream() {
                    // this opener should never actually be used anywhere. any packets that try to use control
                    // authentication will result in stream closure.
                    let control_opener = &crate::crypto::open::control::stream::Reliable::default();

                    let mut router = PacketDispatch::new_stream(
                        &mut self.receiver,
                        &mut self.handshake,
                        &mut out_buf,
                        control_opener,
                        clock,
                        shared,
                        accept_state,
                    );

                    self.buffer.process(features, &mut router)
                } else {
                    let control_opener = shared
                        .crypto
                        .control_opener()
                        .expect("control opener should be available on unreliable transports");

                    let mut router = PacketDispatch::new_datagram(
                        &mut self.receiver,
                        &mut self.handshake,
                        &mut out_buf,
                        control_opener,
                        clock,
                        shared,
                        accept_state,
                    );

                    self.buffer.process(features, &mut router)
                }
            };

            if let Err(err) = res {
                self.receiver.on_error(err, Location::Local, &publisher);
            }

            // if we processed packets then we may have data to copy out
            self.receiver
                .on_read_buffer(&mut self.reassembler, out_buf, accept_state, clock);
        }

        // we only check for timeouts on unreliable transports
        if !features.is_reliable() {
            self.receiver
                .on_timeout(clock, || shared.last_peer_activity(), &publisher);
        }

        // indicate to the caller if we need to transmit an ACK
        self.receiver.should_transmit()
    }
}

struct PacketDispatch<'a, Buf, Crypt, Clk, Sub, const IS_STREAM: bool>
where
    Buf: buffer::Duplex<Error = core::convert::Infallible>,
    Crypt: crate::crypto::open::control::Stream,
    Clk: Clock + ?Sized,
    Sub: event::Subscriber,
{
    any_valid_packets: bool,
    handshake: &'a mut handshake::State,
    remote_addr: SocketAddress,
    remote_queue_id: Option<VarInt>,
    receiver: &'a mut recv::state::State,
    control_opener: &'a Crypt,
    out_buf: &'a mut Buf,
    shared: &'a ArcShared<Sub>,
    clock: &'a Clk,
    accept_state: AcceptState,
    publisher: event::ConnectionPublisherSubscriber<'a, Sub>,
}

impl<'a, Buf, Crypt, Clk, Sub> PacketDispatch<'a, Buf, Crypt, Clk, Sub, true>
where
    Buf: buffer::Duplex<Error = core::convert::Infallible>,
    Crypt: crate::crypto::open::control::Stream,
    Clk: Clock + ?Sized,
    Sub: event::Subscriber,
{
    /// Sets up a dispatcher for stream transports
    #[inline]
    fn new_stream(
        receiver: &'a mut recv::state::State,
        handshake: &'a mut handshake::State,
        out_buf: &'a mut Buf,
        control_opener: &'a Crypt,
        clock: &'a Clk,
        shared: &'a ArcShared<Sub>,
        accept_state: AcceptState,
    ) -> Self {
        let publisher = shared.publisher();
        Self {
            any_valid_packets: false,
            remote_addr: Default::default(),
            remote_queue_id: None,
            receiver,
            control_opener,
            out_buf,
            shared,
            clock,
            handshake,
            accept_state,
            publisher,
        }
    }
}

impl<'a, Buf, Crypt, Clk, Sub> PacketDispatch<'a, Buf, Crypt, Clk, Sub, false>
where
    Buf: buffer::Duplex<Error = core::convert::Infallible>,
    Crypt: crate::crypto::open::control::Stream,
    Clk: Clock + ?Sized,
    Sub: event::Subscriber,
{
    /// Sets up a dispatcher for datagram transports
    #[inline]
    fn new_datagram(
        receiver: &'a mut recv::state::State,
        handshake: &'a mut handshake::State,
        out_buf: &'a mut Buf,
        control_opener: &'a Crypt,
        clock: &'a Clk,
        shared: &'a ArcShared<Sub>,
        accept_state: AcceptState,
    ) -> Self {
        let publisher = shared.publisher();
        Self {
            any_valid_packets: false,
            remote_addr: Default::default(),
            remote_queue_id: None,
            receiver,
            control_opener,
            out_buf,
            shared,
            clock,
            handshake,
            accept_state,
            publisher,
        }
    }
}

impl<Buf, Crypt, Clk, Sub, const IS_STREAM: bool> recv::buffer::Dispatch
    for PacketDispatch<'_, Buf, Crypt, Clk, Sub, IS_STREAM>
where
    Buf: buffer::Duplex<Error = core::convert::Infallible>,
    Crypt: crate::crypto::open::control::Stream,
    Clk: Clock + ?Sized,
    Sub: event::Subscriber,
{
    #[inline]
    fn on_packet(
        &mut self,
        remote_addr: &SocketAddress,
        ecn: s2n_quic_core::inet::ExplicitCongestionNotification,
        packet: crate::packet::Packet,
    ) -> Result<(), recv::Error> {
        match packet {
            Packet::Stream(mut packet) => {
                // make sure the packet looks OK before deriving openers from it
                let precheck = self.receiver.precheck_stream_packet(
                    self.shared.credentials(),
                    &packet,
                    &self.publisher,
                );

                if IS_STREAM {
                    // datagrams drop invalid packets - streams error out since the stream can't recover
                    precheck?;
                }

                let source_queue_id = packet.source_queue_id();

                let _ = self.shared.crypto.open_with(
                    |opener| {
                        let accept_state = if IS_STREAM {
                            // Streaming transport handles accept state internally
                            AcceptState::Accepted
                        } else {
                            self.accept_state
                        };

                        self.receiver.on_stream_packet(
                            opener,
                            self.control_opener,
                            self.shared.credentials(),
                            &mut packet,
                            ecn,
                            accept_state,
                            self.clock,
                            self.out_buf,
                            &self.publisher,
                        )?;

                        self.any_valid_packets = true;
                        self.remote_addr = *remote_addr;

                        if !matches!(self.handshake, handshake::State::Finished) {
                            // we got a valid stream packet
                            let _ = self.handshake.on_stream_packet();

                            // check if we got a non-zero value
                            if packet.next_expected_control_packet().as_u64() > 0 {
                                let _ = self.handshake.on_non_zero_next_expected_control_packet();
                            }
                        }

                        if source_queue_id.is_some() {
                            self.remote_queue_id = source_queue_id;
                        }

                        <Result<_, recv::Error>>::Ok(())
                    },
                    self.clock,
                    &self.shared.subscriber,
                );

                if IS_STREAM {
                    self.receiver.check_error()?;
                }

                Ok(())
            }
            Packet::FlowReset(packet) => {
                if packet.credentials() != self.shared.credentials() {
                    return if IS_STREAM {
                        Err(recv::error::Kind::CredentialMismatch {
                            expected: *self.shared.credentials(),
                            actual: *packet.credentials(),
                        }
                        .into())
                    } else {
                        Ok(())
                    };
                }

                let Some(packet) = self
                    .shared
                    .crypto
                    .map()
                    .handle_flow_reset_packet(&packet, &(*remote_addr).into())
                else {
                    // TODO
                    return Ok(());
                };

                let error = recv::ErrorKind::ApplicationError {
                    error: packet.code.into(),
                };

                self.receiver
                    .on_error(error, Location::Remote, &self.publisher);

                Ok(())
            }
            other => {
                self.shared
                    .crypto
                    .map()
                    .handle_unexpected_packet(&other, &(*remote_addr).into());

                if !IS_STREAM {
                    // TODO if the packet was authentic then close the receiver with an error
                    // Datagram-based streams just drop unexpected packets

                    tracing::trace!("unexpected packet: {other:?}");

                    return Ok(());
                }

                // streams don't allow for other kinds of packets so close it and bail on processing
                Err(recv::error::Kind::UnexpectedPacket {
                    packet: other.kind(),
                }
                .into())
            }
        }
    }
}

impl<Buf, Crypt, Clk, Sub, const IS_STREAM: bool> Drop
    for PacketDispatch<'_, Buf, Crypt, Clk, Sub, IS_STREAM>
where
    Buf: buffer::Duplex<Error = core::convert::Infallible>,
    Crypt: crate::crypto::open::control::Stream,
    Clk: Clock + ?Sized,
    Sub: event::Subscriber,
{
    #[inline]
    fn drop(&mut self) {
        ensure!(self.any_valid_packets);
        self.shared
            .on_valid_packet(&self.remote_addr, self.remote_queue_id, self.handshake);
    }
}
