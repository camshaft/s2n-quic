// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::socket::fd::{self, udp};
use std::{io, net::SocketAddr};

pub struct BusyPoll<T>(pub T);

impl<T: udp::Socket> udp::Socket for BusyPoll<T> {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
}

impl<T: udp::Socket> super::LocalAddr for BusyPoll<T> {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
}

#[cfg(unix)]
impl<T: udp::Socket> std::os::fd::AsRawFd for BusyPoll<T> {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        fd::AsRaw::as_raw(&self.0)
    }
}

#[cfg(windows)]
impl<T: udp::Socket> fd::AsRaw for BusyPoll<T> {
    fn as_raw(&self) -> fd::Raw {
        fd::AsRaw::as_raw(&self.0)
    }
}
