// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::socket::{BusyPoll, Gso as GsoSocket, Options};
use s2n_quic_platform::features;
use std::{ffi::CString, io, net::SocketAddr};

const DEFAULT_BUFFER_SIZE: usize = 200 * 1024 * 1024;

/// Configuration for send socket creation.
#[derive(Clone, Debug)]
pub struct BindAddress {
    pub addr: SocketAddr,
    pub ifname: Option<CString>,
}

impl From<SocketAddr> for BindAddress {
    #[inline]
    fn from(addr: SocketAddr) -> Self {
        Self { addr, ifname: None }
    }
}

/// Configuration for send socket creation.
pub struct SendConfig {
    pub bind_addrs: Vec<BindAddress>,
    pub gso: features::Gso,
    pub send_buffer: usize,
}

impl SendConfig {
    pub fn new(bind_addrs: Vec<BindAddress>, gso: features::Gso) -> Self {
        Self {
            bind_addrs,
            gso,
            send_buffer: DEFAULT_BUFFER_SIZE,
        }
    }

    /// Creates send sockets with GSO support.
    pub fn create(&self) -> io::Result<Vec<GsoSocket<std::net::UdpSocket>>> {
        if self.bind_addrs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "at least one send bind address is required",
            ));
        }

        let mut sockets = Vec::with_capacity(self.bind_addrs.len());

        for bind_addr in &self.bind_addrs {
            let mut opts = Options::default();
            opts.addr = bind_addr.addr;
            opts.bind_interface = bind_addr.ifname.clone();
            opts.blocking = false;
            opts.send_buffer = Some(self.send_buffer);
            opts.recv_buffer = Some(0);
            let socket = opts.build_udp()?;

            let socket = GsoSocket(socket, self.gso.clone());
            sockets.push(socket);
        }

        Ok(sockets)
    }

    pub fn busy_poll(&self) -> io::Result<Vec<GsoSocket<BusyPoll<std::net::UdpSocket>>>> {
        let sockets = self.create()?;
        Ok(sockets
            .into_iter()
            .map(|GsoSocket(s, gso)| GsoSocket(BusyPoll(s), gso))
            .collect())
    }
}

/// Configuration for receive socket creation.
///
/// Each recv socket binds to its own distinct address so that remote senders can
/// target individual recv workers directly (bypassing kernel RSS). The full list
/// of bound addresses is advertised to peers during the handshake.
pub struct RecvConfig {
    pub bind_addrs: Vec<BindAddress>,
    pub recv_buffer: usize,
}

impl RecvConfig {
    pub fn new(bind_addrs: Vec<BindAddress>) -> Self {
        Self {
            bind_addrs,
            recv_buffer: DEFAULT_BUFFER_SIZE,
        }
    }

    /// Creates receive sockets.
    pub fn create(&self) -> io::Result<Vec<std::net::UdpSocket>> {
        if self.bind_addrs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "at least one recv bind address is required",
            ));
        }

        let mut sockets = Vec::with_capacity(self.bind_addrs.len());

        for bind_addr in &self.bind_addrs {
            let mut opts = Options::default();
            opts.addr = bind_addr.addr;
            opts.bind_interface = bind_addr.ifname.clone();
            opts.gro = true;
            opts.blocking = false;
            opts.recv_buffer = Some(self.recv_buffer);
            opts.send_buffer = Some(0);
            sockets.push(opts.build_udp()?);
        }

        Ok(sockets)
    }

    pub fn busy_poll(&self) -> io::Result<Vec<BusyPoll<std::net::UdpSocket>>> {
        let sockets = self.create()?;
        Ok(sockets.into_iter().map(BusyPoll).collect())
    }
}

/// Wraps a socket to count ops, bytes, and errors at the I/O boundary.
pub(crate) struct Metered<S> {
    inner: S,
    ops: crate::counter::Counter,
    bytes: crate::counter::Counter,
    errors: crate::counter::Counter,
}

impl<S: std::fmt::Debug> std::fmt::Debug for Metered<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}

impl<S> Metered<S> {
    pub fn new(
        inner: S,
        ops: crate::counter::Counter,
        bytes: crate::counter::Counter,
        errors: crate::counter::Counter,
    ) -> Self {
        Self {
            inner,
            ops,
            bytes,
            errors,
        }
    }
}

impl<S: crate::socket::LocalAddr> crate::socket::LocalAddr for Metered<S> {
    #[inline]
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

impl<S: crate::socket::send::Socket> crate::socket::send::Socket for Metered<S> {
    #[inline]
    fn send_msg(
        &self,
        addr: &crate::msg::addr::Addr,
        payload: &[io::IoSlice],
        segment_size: u16,
        ecn: s2n_quic_core::inet::ExplicitCongestionNotification,
    ) -> io::Result<usize> {
        let result = self.inner.send_msg(addr, payload, segment_size, ecn);
        match &result {
            Ok(sent) => {
                self.ops.add(1);
                self.bytes.add(*sent as u64);
            }
            Err(_) => {
                self.errors.add(1);
            }
        }
        result
    }
}

impl<S: crate::socket::recv::Socket> crate::socket::recv::Socket for Metered<S> {
    #[inline]
    fn poll_recv(
        &self,
        cx: &mut core::task::Context,
        addr: &mut crate::msg::addr::Addr,
        cmsg: &mut crate::msg::cmsg::Receiver,
        buffer: &mut [io::IoSliceMut],
    ) -> core::task::Poll<io::Result<usize>> {
        let result = self.inner.poll_recv(cx, addr, cmsg, buffer);
        match &result {
            core::task::Poll::Ready(Ok(received)) => {
                self.ops.add(1);
                self.bytes.add(*received as u64);
            }
            core::task::Poll::Ready(Err(_)) => {
                self.errors.add(1);
            }
            core::task::Poll::Pending => {}
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn send_config_requires_bind_addrs() {
        let config = SendConfig::new(Vec::new(), features::Gso::default());
        match config.create() {
            Err(err) => assert_eq!(err.kind(), io::ErrorKind::InvalidInput),
            Ok(_) => panic!("empty bind_addrs should error"),
        }
    }

    #[test]
    fn recv_config_requires_bind_addrs() {
        let config = RecvConfig::new(Vec::new());
        match config.create() {
            Err(err) => assert_eq!(err.kind(), io::ErrorKind::InvalidInput),
            Ok(_) => panic!("empty bind_addrs should error"),
        }
    }

    #[test]
    fn recv_config_binds_to_each_addr() {
        let addrs = vec![
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).into(),
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).into(),
        ];
        let sockets = RecvConfig::new(addrs).create().expect("bind should work");
        assert_eq!(sockets.len(), 2);
    }
}
