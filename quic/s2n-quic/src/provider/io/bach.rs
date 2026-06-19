// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Provides an implementation of the [`io::Provider`](crate::provider::io::Provider) backed by
//! [`bach::net`], the simulated network used by the s2n-quic-dc data plane in deterministic tests.
//!
//! This lets a full QUIC/TLS handshake run inside the same `bach` simulation as the dc data plane,
//! sharing simulated addresses, latency, and packet monitors.

use s2n_quic_core::{endpoint::Endpoint, inet::SocketAddress};
use s2n_quic_platform::io::bach;
use std::io;

pub use self::bach::{Builder, Io as Provider};

impl super::Provider for Provider {
    type PathHandle = bach::PathHandle;
    type Error = io::Error;

    fn start<E: Endpoint<PathHandle = Self::PathHandle>>(
        self,
        endpoint: E,
    ) -> Result<SocketAddress, Self::Error> {
        let (_join_handle, local_addr) = Provider::start(self, endpoint)?;
        Ok(local_addr)
    }
}
