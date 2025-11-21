// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    congestion,
    credentials::Credentials,
    crypto, event,
    packet::{
        self,
        stream::{self, decoder, encoder, PacketSpace},
    },
    recovery,
    stream::{
        processing,
        send::{
            application::state::Message,
            error::{self, Error},
            filter::Filter,
        },
        DEFAULT_IDLE_TIMEOUT,
    },
};
use core::{task::Poll, time::Duration};
use s2n_codec::{DecoderBufferMut, EncoderBuffer, EncoderValue};
use s2n_quic_core::{
    dc::ApplicationParams,
    endpoint::Location,
    ensure,
    event::IntoEvent as _,
    frame::{self, FrameMut},
    inet::ExplicitCongestionNotification,
    interval_set::IntervalSet,
    packet::number::PacketNumberSpace,
    path::{ecn, INITIAL_PTO_BACKOFF},
    random,
    recovery::{bandwidth::Bandwidth, Pto, RttEstimator},
    stream::state,
    time::{
        timer::{self, Provider as _},
        Clock, Timer, Timestamp,
    },
    varint::VarInt,
};
use std::collections::BinaryHeap;
use tracing::trace;

pub mod fin;
pub mod max_data;
pub mod probe;
pub mod reset;
pub mod retransmission;
pub mod transmission;

type PacketMap<Info> = s2n_quic_core::packet::number::Map<Info>;

#[derive(Debug)]
pub struct SentStreamPacket {
    info: transmission::Info,
    cc_info: congestion::PacketInfo,
}

#[derive(Debug)]
pub struct SentRecoveryPacket {
    info: transmission::Info,
    cc_info: congestion::PacketInfo,
    max_stream_packet_number_lost: VarInt,
}

#[derive(Clone, Debug, Default)]
pub struct InflightCounters {
    pub probes: u32,
    pub with_payload: u32,
    pub with_final_offset: u32,
    pub with_reset: u32,
}

impl InflightCounters {
    pub fn on_transmit(&mut self, info: &transmission::Info) {
        self.update(info, |count| *count += 1);
    }

    pub fn on_finish(&mut self, info: &transmission::Info) {
        self.update(info, |count| *count -= 1);
    }

    fn update(&mut self, info: &transmission::Info, mut update: impl FnMut(&mut u32)) {
        if info.is_probe() {
            update(&mut self.probes);
        }

        if info.payload_len > 0 {
            update(&mut self.with_payload);
        }

        if info.flags.included_final_offset() {
            update(&mut self.with_final_offset);
        }

        if info.flags.included_reset() {
            update(&mut self.with_reset);
        }
    }

    pub fn has_inflight_packets(
        &self,
        unacked_ranges: &IntervalSet<VarInt>,
        fin: &fin::Fin,
        reset: &reset::Reset,
    ) -> bool {
        let mut has_inflight_packets = !unacked_ranges.is_empty() && self.with_payload > 0;
        has_inflight_packets |= !fin.is_acked() && self.with_final_offset > 0;
        has_inflight_packets |= reset.waiting_ack() && self.with_reset > 0;
        has_inflight_packets
    }
}

#[derive(Debug)]
pub struct State {
    rtt_estimator: RttEstimator,
    sent_stream_packets: PacketMap<SentStreamPacket>,
    max_stream_packet_number: VarInt,
    max_stream_packet_number_lost: VarInt,
    sent_recovery_packets: PacketMap<SentRecoveryPacket>,
    recovery_packet_number: u64,
    last_sent_packet: Option<Timestamp>,
    control_filter: Filter,
    next_expected_control_packet: VarInt,
    cca: congestion::Controller,
    ecn: ecn::Controller,
    pto: Pto,
    pto_backoff: u32,
    counters: InflightCounters,
    idle_timer: Timer,
    idle_timeout: Duration,
    reset: reset::Reset,
    unacked_ranges: IntervalSet<VarInt>,
    max_data: max_data::MaxData,
    max_tx_offset: VarInt,
    local_max_data_window: VarInt,
    peer_activity: Option<PeerActivity>,
    max_datagram_size: u16,
    max_sent_segment_size: u16,
    is_reliable: bool,
    fin: fin::Fin,
    retransmissions: BinaryHeap<retransmission::Segment>,
    #[cfg(debug_assertions)]
    pending_retransmissions: IntervalSet<VarInt>,
}

#[derive(Clone, Copy, Debug)]
pub struct PeerActivity {
    pub made_progress: bool,
}

impl State {
    #[inline]
    pub fn new(stream_id: stream::Id, params: &ApplicationParams) -> Self {
        let max_datagram_size = params.max_datagram_size();
        let initial_max_data = params.remote_max_data;
        let local_max_data = params.local_send_max_data;

        // initialize the pending data left to send
        let mut unacked_ranges = IntervalSet::new();
        unacked_ranges.insert(VarInt::ZERO..=VarInt::MAX).unwrap();

        let cca = congestion::Controller::new(max_datagram_size);

        let max_tx_offset = VarInt::from_u16(max_datagram_size) * 64;

        Self {
            next_expected_control_packet: VarInt::ZERO,
            rtt_estimator: recovery::rtt_estimator(),
            cca,
            sent_stream_packets: Default::default(),
            max_stream_packet_number: VarInt::ZERO,
            max_stream_packet_number_lost: VarInt::ZERO,
            sent_recovery_packets: Default::default(),
            recovery_packet_number: 0,
            last_sent_packet: None,
            control_filter: Default::default(),
            ecn: ecn::Controller::default(),
            pto: Pto::default(),
            pto_backoff: INITIAL_PTO_BACKOFF,
            counters: Default::default(),
            idle_timer: Default::default(),
            idle_timeout: params.max_idle_timeout().unwrap_or(DEFAULT_IDLE_TIMEOUT),
            reset: Default::default(),
            unacked_ranges,
            max_data: max_data::MaxData::new(initial_max_data),
            max_tx_offset,
            local_max_data_window: local_max_data,
            peer_activity: None,
            max_datagram_size,
            max_sent_segment_size: 0,
            is_reliable: stream_id.is_reliable,
            fin: Default::default(),
            retransmissions: Default::default(),
            #[cfg(debug_assertions)]
            pending_retransmissions: Default::default(),
        }
    }

    /// Initializes the worker as a client
    #[inline]
    pub fn init_client(&mut self, clock: &impl Clock) {
        // make sure a packet gets sent soon if the application doesn't
        self.force_arm_pto_timer(clock);
        self.update_idle_timer(clock);
    }

    #[inline]
    pub fn init_server(&mut self, clock: &impl Clock) {
        self.update_idle_timer(clock);
    }

    /// Returns the current flow offset
    #[inline]
    pub fn flow_offset(&self) -> VarInt {
        self.max_tx_offset
            .min(self.max_data.max_data())
            .min(self.local_offset())
            .min(self.cca_offset())
    }

    pub fn state(&self) -> state::Sender {
        if let Some(state) = self.reset.state() {
            return state;
        }

        if self.unacked_ranges.is_empty() {
            return if self.fin.is_acked() {
                state::Sender::DataRecvd
            } else {
                state::Sender::DataSent
            };
        }

        state::Sender::Send
    }

    fn cca_offset(&self) -> VarInt {
        let mut extra_window = self
            .cca
            .congestion_window()
            .saturating_sub(self.cca.bytes_in_flight());

        // only give CCA credits to the application if we were able to retransmit everything considered lost
        if !self.retransmissions.is_empty() {
            extra_window = 0;
        }

        self.max_data.max_sent_offset() + extra_window as usize
    }

    fn local_offset(&self) -> VarInt {
        self.unacked_ranges
            .min_value()
            .map_or(VarInt::MAX, |unacked_start| {
                unacked_start.saturating_add(self.local_max_data_window)
            })
    }

    #[inline]
    pub fn send_quantum_packets(&self) -> u8 {
        let send_quantum = (self.cca.send_quantum() as u64).div_ceil(self.max_datagram_size as u64);
        send_quantum.try_into().unwrap_or(u8::MAX)
    }

    pub fn bandwidth(&self) -> Bandwidth {
        self.cca.bandwidth()
    }

    /// Called by the worker when it receives a control packet from the peer
    #[inline]
    pub fn on_control_packet<C, Clk, Pub>(
        &mut self,
        control_key: &C,
        ecn: ExplicitCongestionNotification,
        packet: &mut packet::control::decoder::Packet,
        random: &mut dyn random::Generator,
        clock: &Clk,
        transmission_queue: &transmission::Queue,
        publisher: &Pub,
    ) -> Result<(), processing::Error>
    where
        C: crypto::open::control::Stream,
        Clk: Clock,
        Pub: event::ConnectionPublisher,
    {
        match self.on_control_packet_impl(
            control_key,
            ecn,
            packet,
            random,
            clock,
            transmission_queue,
            publisher,
        ) {
            Ok(None) => {}
            Ok(Some(error)) => return Err(error),
            Err(error) => {
                self.on_error(error, Location::Local, publisher);
            }
        }

        self.invariants();

        Ok(())
    }

    #[inline(always)]
    fn on_control_packet_impl<C, Clk, Pub>(
        &mut self,
        control_key: &C,
        _ecn: ExplicitCongestionNotification,
        packet: &mut packet::control::decoder::Packet,
        random: &mut dyn random::Generator,
        clock: &Clk,
        transmission_queue: &transmission::Queue,
        publisher: &Pub,
    ) -> Result<Option<processing::Error>, Error>
    where
        C: crypto::open::control::Stream,
        Clk: Clock,
        Pub: event::ConnectionPublisher,
    {
        // only process the packet after we know it's authentic
        let res = control_key.verify(packet.header(), packet.auth_tag());

        publisher.on_stream_control_packet_received(event::builder::StreamControlPacketReceived {
            packet_number: packet.packet_number().as_u64(),
            packet_len: packet.total_len(),
            control_data_len: packet.control_data().len(),
            is_authenticated: res.is_ok(),
        });

        // drop the packet if it failed to authenticate
        if let Err(err) = res {
            return Ok(Some(err.into()));
        }

        // check if we've already seen the packet
        ensure!(
            self.control_filter.on_packet(packet).is_ok(),
            Ok(Some(processing::Error::Duplicate))
        );

        let packet_number = packet.packet_number();

        // raise our next expected control packet
        {
            let pn = packet_number.saturating_add(VarInt::from_u8(1));
            let pn = self.next_expected_control_packet.max(pn);
            self.next_expected_control_packet = pn;
        }

        let recv_time = clock.get_time();
        let mut made_progress = false;
        let mut max_acked_stream = None;
        let mut max_acked_recovery = None;
        let mut loaded_transmit_queue = false;

        for frame in packet.control_frames_mut() {
            let frame = frame.map_err(|err| error::Kind::FrameError { decoder: err }.err())?;

            trace!(?frame);

            match frame {
                FrameMut::Padding(_) => {
                    continue;
                }
                FrameMut::Ping(_) => {
                    // no need to do anything special here
                }
                FrameMut::Ack(ack) => {
                    if !core::mem::replace(&mut loaded_transmit_queue, true) {
                        // make sure we have a current view of the application transmissions
                        self.load_completion_queue(transmission_queue, clock);
                    }

                    if ack.ecn_counts.is_some() {
                        self.on_frame_ack::<_, _, _, true>(
                            &ack,
                            random,
                            &recv_time,
                            &mut made_progress,
                            &mut max_acked_stream,
                            &mut max_acked_recovery,
                            publisher,
                        )?;
                    } else {
                        self.on_frame_ack::<_, _, _, false>(
                            &ack,
                            random,
                            &recv_time,
                            &mut made_progress,
                            &mut max_acked_stream,
                            &mut max_acked_recovery,
                            publisher,
                        )?;
                    }
                }
                FrameMut::MaxData(frame) => {
                    if let Some(diff) = self.max_data.on_max_data_frame(frame.maximum_data) {
                        publisher.on_stream_max_data_received(
                            event::builder::StreamMaxDataReceived {
                                increase: diff.as_u64(),
                                new_max_data: frame.maximum_data.as_u64(),
                            },
                        );
                    }
                }
                FrameMut::ConnectionClose(close) => {
                    let error = if close.frame_type.is_some() {
                        error::Kind::TransportError {
                            code: close.error_code,
                        }
                    } else {
                        error::Kind::ApplicationError {
                            error: close.error_code.into(),
                        }
                    };

                    let error = error.err();
                    self.on_error(error, Location::Remote, publisher);
                    return Err(error);
                }
                _ => continue,
            }
        }

        for (space, pn) in [
            (stream::PacketSpace::Stream, max_acked_stream),
            (stream::PacketSpace::Recovery, max_acked_recovery),
        ] {
            if let Some(pn) = pn {
                self.detect_lost_packets(random, &recv_time, space, pn, publisher)?;
            }
        }

        self.on_peer_activity(made_progress);

        Ok(None)
    }

    pub fn on_fin_known(&mut self, final_offset: VarInt) {
        ensure!(self.fin.on_known(final_offset).is_ok());
        self.unacked_ranges
            .remove(final_offset..=VarInt::MAX)
            .unwrap();

        trace!(%final_offset, ?self.unacked_ranges, "fin known");
    }

    pub fn next_expected_control_packet(&self) -> VarInt {
        self.next_expected_control_packet
    }

    pub fn max_datagram_size(&self) -> u16 {
        self.max_datagram_size
    }

    pub fn error(&self) -> Option<(&error::Error, Location)> {
        self.reset.error()
    }

    #[inline]
    fn on_frame_ack<Ack, Clk, Pub, const IS_STREAM: bool>(
        &mut self,
        ack: &frame::Ack<Ack>,
        random: &mut dyn random::Generator,
        clock: &Clk,
        made_progress: &mut bool,
        max_acked_stream: &mut Option<VarInt>,
        max_acked_recovery: &mut Option<VarInt>,
        publisher: &Pub,
    ) -> Result<(), Error>
    where
        Ack: frame::ack::AckRanges,
        Clk: Clock,
        Pub: event::ConnectionPublisher,
    {
        let mut cca_args = None;
        let mut bytes_acked = 0;

        macro_rules! impl_ack_processing {
            ($space:ident, $sent_packets:ident, $on_packet_number:expr) => {
                for range in ack.ack_ranges() {
                    let pmin = PacketNumberSpace::Initial.new_packet_number(*range.start());
                    let pmax = PacketNumberSpace::Initial.new_packet_number(*range.end());
                    let range = s2n_quic_core::packet::number::PacketNumberRange::new(pmin, pmax);
                    for (num, packet) in self.$sent_packets.remove_range(range) {
                        let num_varint = unsafe { VarInt::new_unchecked(num.as_u64()) };

                        #[allow(clippy::redundant_closure_call)]
                        ($on_packet_number)(num_varint, &packet);

                        let _ = self.unacked_ranges.remove(packet.info.tracking_range());

                        self.ecn
                            .on_packet_ack(packet.info.time_sent, packet.info.ecn);
                        bytes_acked += packet.info.cca_len() as usize;

                        // record the most recent packet
                        if cca_args
                            .as_ref()
                            .map_or(true, |prev: &(Timestamp, _)| prev.0 < packet.info.time_sent)
                        {
                            cca_args = Some((packet.info.time_sent, packet.cc_info));
                        }

                        self.counters.on_finish(&packet.info);

                        // If we got an ACK for a packet that included the final offset then notify the fin state
                        if packet.info.flags.included_final_offset() {
                            self.fin.on_ack();
                        }

                        if !packet.info.is_probe() {
                            *made_progress = true;
                        }

                        publisher.on_stream_packet_acked(event::builder::StreamPacketAcked {
                            packet_len: packet.info.packet_len as usize,
                            stream_offset: packet.info.stream_offset.as_u64(),
                            payload_len: packet.info.payload_len as usize,
                            packet_number: num.as_u64(),
                            time_sent: packet.info.time_sent.into_event(),
                            lifetime: clock
                                .get_time()
                                .saturating_duration_since(packet.info.time_sent),
                            is_retransmission: PacketSpace::$space.is_recovery()
                                && !packet.info.is_probe(),
                        });
                    }
                }
            };
        }

        if IS_STREAM {
            impl_ack_processing!(
                Stream,
                sent_stream_packets,
                |packet_number: VarInt, _packet: &SentStreamPacket| {
                    *max_acked_stream = (*max_acked_stream).max(Some(packet_number));
                }
            );
        } else {
            impl_ack_processing!(
                Recovery,
                sent_recovery_packets,
                |packet_number: VarInt, sent_packet: &SentRecoveryPacket| {
                    *max_acked_recovery = (*max_acked_recovery).max(Some(packet_number));
                    *max_acked_stream =
                        (*max_acked_stream).max(Some(sent_packet.max_stream_packet_number_lost));
                }
            );
        };

        if let Some((time_sent, cc_info)) = cca_args {
            let now = clock.get_time();
            let ack_delay = ack.ack_delay();
            let rtt_sample = now
                .saturating_duration_since(time_sent)
                .saturating_sub(ack_delay)
                .max(Duration::from_micros(1));

            self.rtt_estimator.update_rtt(
                Duration::ZERO,
                rtt_sample,
                now,
                true,
                PacketNumberSpace::ApplicationData,
            );

            self.cca.on_packet_ack(
                cc_info.first_sent_time,
                bytes_acked,
                cc_info,
                &self.rtt_estimator,
                random,
                now,
            );
        }

        Ok(())
    }

    #[inline]
    fn detect_lost_packets<Clk, Pub>(
        &mut self,
        random: &mut dyn random::Generator,
        clock: &Clk,
        packet_space: stream::PacketSpace,
        max: VarInt,
        publisher: &Pub,
    ) -> Result<(), Error>
    where
        Clk: Clock,
        Pub: event::ConnectionPublisher,
    {
        let Some(loss_threshold) = max.checked_sub(VarInt::from_u8(2)) else {
            return Ok(());
        };

        let is_unrecoverable = false;

        macro_rules! impl_loss_detection {
            ($sent_packets:ident, $on_packet:expr) => {{
                let lost_min = PacketNumberSpace::Initial.new_packet_number(VarInt::ZERO);
                let lost_max = PacketNumberSpace::Initial.new_packet_number(loss_threshold);
                let range = s2n_quic_core::packet::number::PacketNumberRange::new(lost_min, lost_max);
                for (num, mut packet) in self.$sent_packets.remove_range(range) {
                    let num_varint = unsafe { VarInt::new_unchecked(num.as_u64()) };
                    // TODO create a path and publisher
                    // self.ecn.on_packet_loss(packet.time_sent, packet.ecn, now, path, publisher);

                    let now = clock.get_time();

                    self.cca.on_packet_lost(
                        packet.info.cca_len() as _,
                        packet.cc_info,
                        random,
                        now,
                    );

                    publisher.on_stream_packet_lost(event::builder::StreamPacketLost {
                        packet_len: packet.info.packet_len as _,
                        stream_offset: packet.info.stream_offset.as_u64(),
                        payload_len: packet.info.payload_len as _,
                        packet_number: num.as_u64(),
                        time_sent: packet.info.time_sent.into_event(),
                        lifetime: now.saturating_duration_since(packet.info.time_sent),
                        is_retransmission: packet_space.is_recovery() && !packet.info.is_probe(),
                    });

                    #[allow(clippy::redundant_closure_call)]
                    ($on_packet)(num_varint, &packet);

                    self.counters.on_finish(&packet.info);

                    // TODO don't retransmit if the range is already ACK'd elsewhere

                    if let Some(retransmission) = packet.info.try_retransmit() {
                        // update our local packet number to be at least 1 more than the largest lost
                        // packet number
                        let min_recovery_packet_number = num.as_u64() + 1;
                        self.recovery_packet_number =
                            self.recovery_packet_number.max(min_recovery_packet_number);

                        self.retransmissions.push(retransmission);
                    } else {
                        // TODO how do we know if the retransmission is in-flight or not?
                    }
                }
            }}
        }

        match packet_space {
            stream::PacketSpace::Stream => impl_loss_detection!(sent_stream_packets, |_, _| {}),
            stream::PacketSpace::Recovery => {
                impl_loss_detection!(
                    sent_recovery_packets,
                    |_packet_number: VarInt, sent_packet: &SentRecoveryPacket| {
                        self.max_stream_packet_number_lost = self
                            .max_stream_packet_number_lost
                            .max(sent_packet.max_stream_packet_number_lost + 1);
                    }
                )
            }
        }

        ensure!(
            !is_unrecoverable,
            Err(error::Kind::RetransmissionFailure.err())
        );

        self.invariants();

        Ok(())
    }

    #[inline]
    fn on_peer_activity(&mut self, made_progress: bool) {
        if let Some(prev) = self.peer_activity.as_mut() {
            prev.made_progress |= made_progress;
        } else {
            self.peer_activity = Some(PeerActivity { made_progress });
        }
    }

    #[inline]
    pub fn before_sleep<Clk: Clock>(&mut self, clock: &Clk) {
        self.process_peer_activity();

        // make sure our timers are armed
        self.update_idle_timer(clock);
        self.update_pto_timer(clock);

        if self.has_inflight_packets() {
            debug_assert!(self.pto.is_armed());
        }

        if self.unacked_ranges.is_empty() && self.fin.is_acked() {
            debug_assert!(self.state().is_terminal());
        }

        trace!(
            unacked_ranges = ?self.unacked_ranges,
            retransmissions = self.retransmissions.len(),
            stream_packets_in_flight = self.sent_stream_packets.iter().count(),
            recovery_packets_in_flight = self.sent_recovery_packets.iter().count(),
            pto_timer = ?self.pto.next_expiration(),
            idle_timer = ?self.idle_timer.next_expiration(),
            ?self.counters,
            state = ?self.state(),
            ?self.fin,
        );

        self.invariants();
    }

    #[inline]
    fn process_peer_activity(&mut self) {
        let Some(PeerActivity { made_progress }) = self.peer_activity.take() else {
            return;
        };

        // If the peer is making progress then reset our PTO backoff. Otherwise, we could
        // get caught in a loop.
        if made_progress {
            self.reset_pto_timer();
        }

        // re-arm the idle timer as long as we're not in terminal state
        if !self.state().is_terminal() {
            self.idle_timer.cancel();
        }
    }

    #[inline]
    pub fn on_time_update<Clk, Ld, Pub>(
        &mut self,
        clock: &Clk,
        load_last_activity: Ld,
        publisher: &Pub,
    ) where
        Clk: Clock,
        Ld: Fn() -> Timestamp,
        Pub: event::ConnectionPublisher,
    {
        if self.poll_idle_timer(clock, load_last_activity).is_ready() {
            self.on_error(error::Kind::IdleTimeout, Location::Local, publisher);
            return;
        }

        let packets_in_flight = self.has_inflight_packets();

        if self
            .pto
            .on_timeout(packets_in_flight, clock.get_time())
            .is_ready()
        {
            // TODO where does this come from
            let max_pto_backoff = 1024;
            self.pto_backoff = self.pto_backoff.saturating_mul(2).min(max_pto_backoff);
        }
    }

    #[inline]
    fn poll_idle_timer<Clk, Ld>(&mut self, clock: &Clk, load_last_activity: Ld) -> Poll<()>
    where
        Clk: Clock,
        Ld: Fn() -> Timestamp,
    {
        let now = clock.get_time();

        for i in 0..2 {
            if let Some(expiration) = self.idle_timer.next_expiration() {
                if !expiration.has_elapsed(now) {
                    return Poll::Pending;
                }
                self.idle_timer.cancel();
                if i > 0 {
                    break;
                }
            }

            // if that expired then load the last activity from the peer and update the idle timer with
            // the value
            let last_peer_activity = load_last_activity();
            self.update_idle_timer(&last_peer_activity);
        }

        Poll::Ready(())
    }

    #[inline]
    fn update_idle_timer(&mut self, clock: &impl Clock) {
        ensure!(!self.idle_timer.is_armed());

        let now = clock.get_time();
        self.idle_timer.set(now + self.idle_timeout);
    }

    #[inline]
    fn update_pto_timer(&mut self, clock: &impl Clock) {
        ensure!(!self.pto.is_armed());

        if self.has_inflight_packets() {
            self.force_arm_pto_timer(clock);
        }
    }

    fn has_inflight_packets(&self) -> bool {
        self.counters
            .has_inflight_packets(&self.unacked_ranges, &self.fin, &self.reset)
    }

    #[inline]
    fn force_arm_pto_timer(&mut self, clock: &impl Clock) {
        let mut pto_period = self
            .rtt_estimator
            .pto_period(self.pto_backoff, PacketNumberSpace::Initial);

        // the `Timestamp::elapsed` function rounds up to the nearest 1ms so we need to set a min value
        // otherwise we'll prematurely trigger a PTO
        pto_period = pto_period.max(Duration::from_millis(2));

        self.pto.update(clock.get_time(), pto_period);
    }

    #[inline]
    fn reset_pto_timer(&mut self) {
        self.pto_backoff = INITIAL_PTO_BACKOFF;
        self.pto.cancel();
    }

    /// Called by the worker thread when it becomes aware of the application having transmitted a
    /// segment
    #[inline]
    pub fn load_completion_queue(&mut self, queue: &transmission::Queue, clock: &impl Clock) {
        let mut should_reset_pto = false;

        queue.drain_completion_queue(|transmission| {
            let (packet_number, mut info) = transmission.info;

            // Use the actual transmission time rather than when it was submitted to give better RTT estimates
            debug_assert!(
                transmission.transmission_time.has_elapsed(clock.get_time()),
                "{} >= {}",
                clock.get_time(),
                transmission.transmission_time
            );
            info.time_sent = transmission.transmission_time;

            let meta = transmission.meta;
            let has_more_app_data = meta.has_more_app_data;
            self.max_sent_segment_size = self.max_sent_segment_size.max(info.packet_len);

            // Check if we need to update the fin state
            if let Some(final_offset) = meta.final_offset {
                self.on_fin_known(final_offset);
                self.fin.on_transmit();
            }

            // Store the buffer so we can retransmit if lost
            if info.payload_len > 0 {
                info.descriptor = Some(transmission.segment);
            }

            if meta.packet_space.is_stream() {
                should_reset_pto = true;
            }

            self.on_transmit_segment(
                meta.packet_space,
                packet_number,
                info,
                has_more_app_data,
                clock,
            );
        });

        if should_reset_pto {
            // if we just sent some packets then we can use those as probes
            self.reset_pto_timer();
        }

        self.invariants();
    }

    #[inline]
    fn on_transmit_segment(
        &mut self,
        packet_space: stream::PacketSpace,
        packet_number: VarInt,
        info: transmission::Info,
        has_more_app_data: bool,
        clock: &impl Clock,
    ) {
        // the BBR implementation requires monotonic time so track that
        let mut cca_time_sent = info.time_sent;

        match packet_space {
            stream::PacketSpace::Stream => {
                if let Some(min) = self.last_sent_packet {
                    cca_time_sent = info.time_sent.max(min);
                }
            }
            stream::PacketSpace::Recovery => {
                if let Some(min) = self.last_sent_packet {
                    cca_time_sent = info.time_sent.max(min);
                }
            }
        }
        self.last_sent_packet = Some(cca_time_sent);

        let cc_info = self.cca.on_packet_sent(
            cca_time_sent,
            info.cca_len(),
            has_more_app_data,
            &self.rtt_estimator,
        );

        // update the max offset that we've transmitted
        self.max_data
            .on_transmit(info.end_offset(), clock, self.idle_timeout / 2);
        self.counters.on_transmit(&info);

        if let stream::PacketSpace::Recovery = packet_space {
            let packet_number = PacketNumberSpace::Initial.new_packet_number(packet_number);
            let max_stream_packet_number_lost = self
                .max_stream_packet_number_lost
                .max(self.max_stream_packet_number + 1);

            #[cfg(debug_assertions)]
            let _ = self.pending_retransmissions.remove(info.range());

            if cfg!(debug_assertions)
                && !self.sent_recovery_packets.is_empty()
                && self
                    .sent_recovery_packets
                    .get_range()
                    .max()
                    .is_some_and(|v| v >= packet_number)
            {
                panic!("application packet numbers should be transmitted in order {packet_number:?}: {info:?} - {:?}", self.sent_recovery_packets);
            }

            self.sent_recovery_packets.insert(
                packet_number,
                SentRecoveryPacket {
                    info,
                    cc_info,
                    max_stream_packet_number_lost,
                },
            );
        } else {
            if packet_number == VarInt::ZERO {
                debug_assert_eq!(packet_number, self.max_stream_packet_number);
            } else {
                debug_assert_eq!(
                    packet_number,
                    self.max_stream_packet_number + 1,
                    "application packet numbers should be transmitted in order {info:?}"
                );
            }
            self.max_stream_packet_number = self.max_stream_packet_number.max(packet_number);

            self.max_stream_packet_number_lost = self
                .max_stream_packet_number
                .max(self.max_stream_packet_number_lost)
                + 1;

            self.max_tx_offset += VarInt::from_u16(info.payload_len);
            self.recovery_packet_number = self
                .recovery_packet_number
                .max(self.max_stream_packet_number.as_u64() + 1);

            let packet_number = PacketNumberSpace::Initial.new_packet_number(packet_number);
            self.sent_stream_packets
                .insert(packet_number, SentStreamPacket { info, cc_info });
        }
    }

    fn requires_transmission(&self) -> bool {
        let mut enabled = false;

        enabled |= self.pto.transmissions() > 0;
        enabled |= self.fin.is_queued();
        enabled |= self.reset.is_queued();

        enabled
    }

    #[inline]
    pub fn fill_transmit_queue<C, Clk, M, Pub>(
        &mut self,
        control_key: &C,
        credentials: &Credentials,
        stream_id: &stream::Id,
        source_queue_id: Option<VarInt>,
        clock: &Clk,
        packets: &mut M,
        publisher: &Pub,
    ) -> Result<(), Error>
    where
        C: crypto::seal::control::Stream,
        Clk: Clock,
        M: Message,
        Pub: event::ConnectionPublisher,
    {
        if let Err(error) = self.fill_transmit_queue_impl(
            control_key,
            credentials,
            stream_id,
            source_queue_id,
            clock,
            packets,
            publisher,
        ) {
            self.on_error(error, Location::Local, publisher);
            return Err(error);
        }

        Ok(())
    }

    #[inline]
    fn fill_transmit_queue_impl<C, Clk, M, Pub>(
        &mut self,
        control_key: &C,
        credentials: &Credentials,
        stream_id: &stream::Id,
        source_queue_id: Option<VarInt>,
        clock: &Clk,
        packets: &mut M,
        publisher: &Pub,
    ) -> Result<(), Error>
    where
        C: crypto::seal::control::Stream,
        Clk: Clock,
        M: Message,
        Pub: event::ConnectionPublisher,
    {
        self.process_peer_activity();

        // skip a packet number if we're probing
        if self.pto.transmissions() > 0 {
            self.recovery_packet_number =
                (self.recovery_packet_number + 1).max(*self.max_stream_packet_number + 1);
            self.max_stream_packet_number_lost += 1;
        }

        self.try_transmit_retransmissions(control_key, clock, packets, publisher)?;
        self.try_transmit_probe(
            control_key,
            credentials,
            stream_id,
            source_queue_id,
            packets,
            clock,
        )?;

        Ok(())
    }

    #[inline]
    fn try_transmit_retransmissions<C, Clk, M, Pub>(
        &mut self,
        control_key: &C,
        clock: &Clk,
        packets: &mut M,
        publisher: &Pub,
    ) -> Result<(), Error>
    where
        C: crypto::seal::control::Stream,
        Clk: Clock,
        M: Message,
        Pub: event::ConnectionPublisher,
    {
        // We'll only have retransmissions if we're reliable
        ensure!(self.is_reliable, Ok(()));

        while let Some(retransmission) = self.retransmissions.peek() {
            // If the CCA is requesting fast retransmission we can bypass the CWND check
            if !self.cca.requires_fast_retransmission() {
                // make sure we fit in the current congestion window
                let remaining_cca_window = self
                    .cca
                    .congestion_window()
                    .saturating_sub(self.cca.bytes_in_flight());
                ensure!(
                    retransmission.payload_len as u32 <= remaining_cca_window,
                    break
                );
            }

            let mut info = self
                .retransmissions
                .pop()
                .expect("retransmission should be available");

            let packet_number =
                VarInt::new(self.recovery_packet_number).expect("2^62 is a lot of packets");
            self.recovery_packet_number += 1;

            let packet_len = {
                let buffer = info.descriptor.payload_mut();

                debug_assert!(!buffer.is_empty(), "empty retransmission buffer submitted");

                {
                    let buffer = DecoderBufferMut::new(buffer);
                    match decoder::Packet::retransmit(
                        buffer,
                        stream::PacketSpace::Recovery,
                        packet_number,
                        control_key,
                    ) {
                        Ok(info) => info,
                        Err(err) => {
                            // this shouldn't ever happen
                            debug_assert!(false, "{err:?}");
                            return Err(error::Kind::RetransmissionFailure.err());
                        }
                    }
                }

                buffer.len() as u16
            };

            let time_sent = clock.get_time();

            {
                let stream_offset = info.stream_offset;
                let payload_len = info.payload_len;
                let flags = info.flags;
                debug_assert!(!flags.is_probe(), "probes should not be retransmitted");
                let descriptor = info.descriptor;

                // TODO store this as part of the packet queue
                let ecn = ExplicitCongestionNotification::Ect0;

                let info = transmission::Info {
                    packet_len,
                    stream_offset,
                    payload_len,
                    flags,
                    descriptor: None,
                    time_sent,
                    ecn,
                };

                #[cfg(debug_assertions)]
                let _ = self.pending_retransmissions.insert(info.range());

                let meta = transmission::Meta {
                    packet_space: PacketSpace::Recovery,
                    has_more_app_data: true,
                    final_offset: self.fin.value(),
                };

                let event = transmission::Event {
                    info,
                    meta,
                    packet_number,
                };

                publisher.on_stream_packet_transmitted(event::builder::StreamPacketTransmitted {
                    packet_len: packet_len as usize,
                    stream_offset: stream_offset.as_u64(),
                    payload_len: payload_len as usize,
                    packet_number: packet_number.as_u64(),
                    is_fin: flags.included_final_byte(),
                    is_retransmission: true,
                });

                // consider this transmission a probe if needed
                if self.pto.transmissions() > 0 {
                    self.pto.on_transmit_once();
                }

                packets.push(event, descriptor);
            }
        }

        Ok(())
    }

    #[inline]
    pub fn try_transmit_probe<C, M, Clk>(
        &mut self,
        control_key: &C,
        credentials: &Credentials,
        stream_id: &stream::Id,
        source_queue_id: Option<VarInt>,
        packets: &mut M,
        clock: &Clk,
    ) -> Result<(), Error>
    where
        C: crypto::seal::control::Stream,
        Clk: Clock,
        M: Message,
    {
        while self.requires_transmission() {
            // probes are not congestion-controlled
            let res = packets.push_with(|mut buffer| {
                let min_len = stream::encoder::MAX_RETRANSMISSION_HEADER_LEN + 128;
                assert!(buffer.len() >= min_len);

                let packet_number =
                    VarInt::new(self.recovery_packet_number).expect("2^62 is a lot of packets");
                self.recovery_packet_number += 1;

                let offset = self.max_data.max_sent_offset();
                let final_offset = self.fin.try_transmit();

                let included_final_byte = Some(offset) == final_offset;
                let included_final_offset = final_offset.is_some();
                let mut flags = transmission::Flags::empty()
                    .with_included_final_byte(included_final_byte)
                    .with_included_final_offset(included_final_offset)
                    .with_probe(true);

                let mut payload = probe::Probe {
                    offset,
                    final_offset,
                };

                let mut control_data_len = VarInt::ZERO;
                let control_data = if let Some(frame) = self.reset.try_transmit() {
                    flags = flags.with_included_reset(true);
                    control_data_len = VarInt::try_from(frame.encoding_size()).unwrap();

                    Some(frame)
                } else {
                    None
                };

                let encoder = EncoderBuffer::new(&mut buffer);
                let packet_len = encoder::probe(
                    encoder,
                    source_queue_id,
                    *stream_id,
                    packet_number,
                    self.next_expected_control_packet,
                    VarInt::ZERO,
                    &mut &[][..],
                    control_data_len,
                    &control_data,
                    &mut payload,
                    control_key,
                    credentials,
                );

                let payload_len = 0;

                debug_assert!(
                    packet_len < u16::MAX as usize,
                    "cannot write larger packets than 2^16"
                );
                let packet_len = packet_len as u16;

                let time_sent = clock.get_time();

                let ecn = ExplicitCongestionNotification::Ect0;

                let info = transmission::Info {
                    packet_len,
                    stream_offset: offset,
                    payload_len,
                    flags,
                    descriptor: None,
                    time_sent,
                    ecn,
                };

                let meta = transmission::Meta {
                    packet_space: PacketSpace::Recovery,
                    has_more_app_data: false,
                    final_offset,
                };

                transmission::Event {
                    packet_number,
                    info,
                    meta,
                }
            });

            if res.is_none() {
                break;
            }

            if self.pto.transmissions() > 0 {
                self.pto.on_transmit_once();
            }
        }

        Ok(())
    }

    #[inline]
    #[track_caller]
    pub fn on_error<E, Pub>(&mut self, error: E, source: Location, publisher: &Pub)
    where
        Error: From<E>,
        Pub: event::ConnectionPublisher,
    {
        let error = Error::from(error);
        ensure!(self.reset.on_error(error, source).is_ok());
        publisher.on_stream_sender_errored(event::builder::StreamSenderErrored { error, source });

        self.retransmissions.clear();
        self.sent_stream_packets.clear();
        self.sent_recovery_packets.clear();
        self.unacked_ranges.clear();
    }

    #[cfg(debug_assertions)]
    #[inline]
    fn invariants(&self) {
        if !self.unacked_ranges.is_empty() {
            let mut unacked_ranges = self.unacked_ranges.clone();
            let last = unacked_ranges.inclusive_ranges().next_back().unwrap();
            unacked_ranges.remove(last).unwrap();

            for (_pn, packet) in self.sent_stream_packets.iter() {
                if packet.info.payload_len == 0 {
                    continue;
                }

                if packet.info.descriptor.is_some() {
                    unacked_ranges.remove(packet.info.range()).unwrap();
                }
            }

            for (_pn, packet) in self.sent_recovery_packets.iter() {
                if packet.info.payload_len == 0 {
                    continue;
                }

                if packet.info.descriptor.is_some() {
                    unacked_ranges.remove(packet.info.range()).unwrap();
                }
            }

            for v in self.retransmissions.iter() {
                if v.payload_len == 0 {
                    continue;
                }
                unacked_ranges.remove(v.range()).unwrap();
            }

            for range in self.pending_retransmissions.inclusive_ranges() {
                unacked_ranges.remove(range).unwrap();
            }

            assert!(
                unacked_ranges.is_empty(),
                "unacked ranges should be empty: {unacked_ranges:?}\n state\n {self:#?}"
            );
        }
    }

    #[cfg(not(debug_assertions))]
    #[inline(always)]
    fn invariants(&self) {}
}

impl timer::Provider for State {
    #[inline]
    fn timers<Q: timer::Query>(&self, query: &mut Q) -> timer::Result {
        // if we're in a terminal state then no timers are needed
        ensure!(!self.state().is_terminal(), Ok(()));
        self.pto.timers(query)?;
        self.max_data.timers(query)?;
        self.idle_timer.timers(query)?;
        Ok(())
    }
}
