// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Provides an implementation of the [`io::Provider`](crate::provider::io::Provider)
//! using [`bach::net::UdpSocket`], allowing regular s2n-quic endpoints to run
//! inside a `sim()` environment where [`bach::net::monitor`] intercepts all
//! packets.

use s2n_quic_core::{endpoint::Endpoint, inet::SocketAddress};
use s2n_quic_platform::io::bach_net;
use std::io;

pub use self::bach_net::{Io, PathHandle};

impl super::Provider for Io {
    type PathHandle = PathHandle;
    type Error = io::Error;

    fn start<E: Endpoint<PathHandle = Self::PathHandle>>(
        self,
        endpoint: E,
    ) -> Result<SocketAddress, Self::Error> {
        Io::start(self, endpoint)
    }
}
