// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::msg::{addr::Addr, cmsg};
use core::task::{Context, Poll};
use std::{io, io::IoSliceMut, net::SocketAddr};

/// A socket that can receive packets
pub trait Socket: Send + 'static {
    /// Polls for receiving data
    fn poll_recv(
        &self,
        cx: &mut Context,
        addr: &mut Addr,
        cmsg: &mut cmsg::Receiver,
        buffer: &mut [IoSliceMut],
    ) -> Poll<io::Result<usize>>;

    fn poll_recv_batch(
        &self,
        cx: &mut Context,
        messages: &mut [crate::stream::socket::RecvMessage<'_>],
    ) -> Poll<io::Result<usize>> {
        let Some(message) = messages.first_mut() else {
            return Poll::Ready(Ok(0));
        };

        let mut iov = [IoSliceMut::new(message.payload)];
        match self.poll_recv(cx, message.addr, message.cmsg, &mut iov) {
            Poll::Ready(Ok(len)) => {
                message.len = len;
                Poll::Ready(Ok(1))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Returns the local address for the socket
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

// Implement for BusyPoll wrapper
// impl<T: Socket> Socket for crate::stream::socket::BusyPoll<T> {
//     fn poll_recv(
//         &self,
//         cx: &mut Context,
//         addr: &mut Addr,
//         cmsg: &mut cmsg::Receiver,
//         buffer: &mut [IoSliceMut],
//     ) -> Poll<io::Result<usize>> {
//         self.0.poll_recv(cx, addr, cmsg, buffer)
//     }

//     fn local_addr(&self) -> io::Result<SocketAddr> {
//         self.0.local_addr()
//     }
// }

// Bridge implementation: anything that implements stream::socket::Socket also implements recv::Socket
impl<T> Socket for T
where
    T: crate::stream::socket::Socket,
{
    fn poll_recv(
        &self,
        cx: &mut Context,
        addr: &mut Addr,
        cmsg: &mut cmsg::Receiver,
        buffer: &mut [IoSliceMut],
    ) -> Poll<io::Result<usize>> {
        crate::stream::socket::Socket::poll_recv(self, cx, addr, cmsg, buffer)
    }

    fn poll_recv_batch(
        &self,
        cx: &mut Context,
        messages: &mut [crate::stream::socket::RecvMessage<'_>],
    ) -> Poll<io::Result<usize>> {
        crate::stream::socket::Socket::poll_recv_batch(self, cx, messages)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        crate::stream::socket::Socket::local_addr(self)
    }
}
