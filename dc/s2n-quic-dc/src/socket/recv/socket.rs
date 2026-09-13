// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    msg::{addr::Addr, cmsg},
    socket::{
        fd::{self, udp},
        BusyPoll,
    },
};
use core::task::{Context, Poll};
use s2n_quic_core::ensure;
use std::{io, io::IoSliceMut};

/// A socket that can receive packets
pub trait Socket: crate::socket::LocalAddr + Send + 'static {
    /// Polls for receiving data
    fn poll_recv(
        &self,
        cx: &mut Context,
        addr: &mut Addr,
        cmsg: &mut cmsg::Receiver,
        buffer: &mut [IoSliceMut],
    ) -> Poll<io::Result<usize>>;

    /// Returns the underlying OS receive descriptor, when the socket is backed by a real kernel UDP
    /// socket.
    ///
    /// The io_uring recv backend drives the socket by its raw fd on a dedicated ring thread, so it
    /// can only adopt a socket that exposes one. Sockets without a real fd (e.g. the in-simulation
    /// bach socket) return `None` and always fall back to the cooperative syscall recv path. The
    /// default returns `None`; the UDP-backed wrappers forward the real descriptor.
    #[inline]
    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }
}

impl<T: Socket + Sync> Socket for std::sync::Arc<T> {
    #[inline]
    fn poll_recv(
        &self,
        cx: &mut Context,
        addr: &mut Addr,
        cmsg: &mut cmsg::Receiver,
        buffer: &mut [IoSliceMut],
    ) -> Poll<io::Result<usize>> {
        (**self).poll_recv(cx, addr, cmsg, buffer)
    }

    #[inline]
    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        (**self).raw_fd()
    }
}

impl<T> Socket for BusyPoll<T>
where
    T: udp::Socket,
{
    #[inline]
    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        Some(std::os::fd::AsRawFd::as_raw_fd(&self.0))
    }

    #[inline]
    fn poll_recv(
        &self,
        _cx: &mut Context,
        addr: &mut Addr,
        cmsg: &mut cmsg::Receiver,
        buffer: &mut [IoSliceMut],
    ) -> Poll<io::Result<usize>> {
        ensure!(!buffer.is_empty(), Ok(0).into());

        debug_assert!(
            buffer.iter().any(|s| !s.is_empty()),
            "trying to recv into an empty buffer"
        );

        loop {
            let res = udp::recv(&self.0, addr, cmsg, buffer, fd::Flags::default());

            match res {
                Ok(0) => continue,
                Ok(len) => return Ok(len).into(),
                Err(ref e)
                    if [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted]
                        .contains(&e.kind()) =>
                {
                    return Poll::Pending;
                }
                Err(err) => return Err(err).into(),
            }
        }
    }
}

/// A recv socket driven by the tokio runtime's IO reactor.
///
/// [`BusyPoll`] returns bare `Poll::Pending` on an empty socket and relies on the busy-poll executor
/// re-polling every task each spin (a noop waker) to eventually read again. That only works under the
/// busy-poll runtime. Under `runtime::tokio::Handle` each worker is a waker-driven current-thread
/// runtime, so a recv worker that parks without registering a waker is never re-polled and the socket
/// is never drained — the data-plane handshake then fails with "frame channel closed".
///
/// This wrapper registers the socket fd with tokio's reactor via [`AsyncFd`](tokio::io::unix::AsyncFd)
/// so `poll_recv` registers the task's waker on `WouldBlock` and the worker is woken when the fd is
/// readable — letting the endpoint run its data plane on tokio without busy-polling.
#[cfg(feature = "tokio")]
pub struct Tokio<T> {
    inner: T,
    // Registered with the tokio reactor at construction time (see `Tokio::new`), which runs during
    // endpoint setup under the caller's tokio runtime. `FdRef` borrows the raw fd rather than owning
    // the socket, so `inner` stays usable for `udp::recv` and the socket's lifetime governs the fd
    // (the `AsyncFd` is dropped no later than `self`).
    async_fd: tokio::io::unix::AsyncFd<FdRef>,
}

/// Non-owning `AsRawFd` holder so the `AsyncFd` can register readiness without taking ownership of
/// the UDP socket (which the wrapper retains for `udp::recv`).
#[cfg(feature = "tokio")]
struct FdRef(std::os::fd::RawFd);

#[cfg(feature = "tokio")]
impl std::os::fd::AsRawFd for FdRef {
    #[inline]
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0
    }
}

#[cfg(feature = "tokio")]
impl<T: std::os::fd::AsRawFd> Tokio<T> {
    /// Wrap `inner` and register its fd with the current tokio runtime's reactor for read readiness.
    ///
    /// Must be called from within a tokio runtime (endpoint setup runs under one) — the `AsyncFd`
    /// binds to that runtime's IO driver, which drives readiness for the fd thereafter.
    #[inline]
    pub fn new(inner: T) -> io::Result<Self> {
        let async_fd = tokio::io::unix::AsyncFd::with_interest(
            FdRef(std::os::fd::AsRawFd::as_raw_fd(&inner)),
            tokio::io::Interest::READABLE,
        )?;
        Ok(Self { inner, async_fd })
    }
}

#[cfg(feature = "tokio")]
impl<T: udp::Socket> crate::socket::LocalAddr for Tokio<T> {
    #[inline]
    fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.inner.local_addr()
    }
}

#[cfg(feature = "tokio")]
impl<T> Socket for Tokio<T>
where
    T: udp::Socket + std::os::fd::AsRawFd,
{
    #[inline]
    fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
        Some(std::os::fd::AsRawFd::as_raw_fd(&self.inner))
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        addr: &mut Addr,
        cmsg: &mut cmsg::Receiver,
        buffer: &mut [IoSliceMut],
    ) -> Poll<io::Result<usize>> {
        ensure!(!buffer.is_empty(), Ok(0).into());

        debug_assert!(
            buffer.iter().any(|s| !s.is_empty()),
            "trying to recv into an empty buffer"
        );

        loop {
            let mut guard = match self.async_fd.poll_read_ready(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                Poll::Pending => return Poll::Pending,
            };

            match udp::recv(&self.inner, addr, cmsg, buffer, fd::Flags::default()) {
                Ok(0) => continue,
                Ok(len) => return Ok(len).into(),
                Err(ref e)
                    if [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted]
                        .contains(&e.kind()) =>
                {
                    // Not readable (or a spurious wake): clear readiness so the next
                    // `poll_read_ready` registers our waker and returns `Pending` until the reactor
                    // reports the fd readable again.
                    guard.clear_ready();
                    continue;
                }
                Err(err) => return Err(err).into(),
            }
        }
    }
}
