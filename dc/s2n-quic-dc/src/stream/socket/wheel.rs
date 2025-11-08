// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{handle::Transmission, Protocol, Socket, TransportFeatures};
use crate::{
    msg::{addr::Addr, cmsg},
    socket::send,
    stream::send::state::transmission::Info,
};
use core::task::{Context, Poll};
use s2n_quic_core::{inet::ExplicitCongestionNotification, time::Timestamp};
use std::{
    io::{self, IoSlice, IoSliceMut},
    net::SocketAddr,
    ops,
};

#[derive(Clone)]
pub struct Wheel<const GRANULARITY_US: u64 = 8> {
    wheel: send::wheel::Wheel<Info<()>, GRANULARITY_US>,
    local_addr: SocketAddr,
}

impl<const GRANULARITY_US: u64> Wheel<GRANULARITY_US> {
    pub fn new<Clk: s2n_quic_core::time::Clock>(
        wheel: send::wheel::Wheel<Info<()>, GRANULARITY_US>,
        local_addr: SocketAddr,
    ) -> Self {
        Self { wheel, local_addr }
    }
}

impl<const GRANULARITY_US: u64> ops::Deref for Wheel<GRANULARITY_US> {
    type Target = send::wheel::Wheel<Info<()>, GRANULARITY_US>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.wheel
    }
}

impl<const GRANULARITY_US: u64> Socket for Wheel<GRANULARITY_US> {
    #[inline]
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    #[inline]
    fn protocol(&self) -> Protocol {
        Protocol::Udp
    }

    #[inline]
    fn features(&self) -> TransportFeatures {
        TransportFeatures::UDP
    }

    #[inline]
    fn poll_peek_len(&self, _cx: &mut Context) -> Poll<io::Result<usize>> {
        unimplemented!("Wheel socket is send-only")
    }

    #[inline]
    fn poll_recv(
        &self,
        _cx: &mut Context,
        _addr: &mut Addr,
        _cmsg: &mut cmsg::Receiver,
        _buffer: &mut [IoSliceMut],
    ) -> Poll<io::Result<usize>> {
        unimplemented!("Wheel socket is send-only")
    }

    #[inline]
    fn send_transmission(&self, msg: Transmission, time: Timestamp) {
        // Insert the transmission into the wheel for timed delivery
        self.wheel.insert(msg, time);
    }

    #[inline]
    fn try_send(
        &self,
        _addr: &Addr,
        _ecn: ExplicitCongestionNotification,
        _buffer: &[IoSlice],
    ) -> io::Result<usize> {
        unimplemented!("Wheel socket uses send_transmission instead of try_send")
    }

    #[inline]
    fn poll_send(
        &self,
        _cx: &mut Context,
        _addr: &Addr,
        _ecn: ExplicitCongestionNotification,
        _buffer: &[IoSlice],
    ) -> Poll<io::Result<usize>> {
        unimplemented!("Wheel socket uses send_transmission instead of poll_send")
    }

    #[inline]
    fn send_finish(&self) -> io::Result<()> {
        // No shutdown needed for wheel-based sending
        Ok(())
    }
}
