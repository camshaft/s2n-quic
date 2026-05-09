// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(not(loom))]
pub mod acceptor;
#[cfg(not(loom))]
pub mod allocator;
#[cfg(not(loom))]
pub mod busy_poll;
#[cfg(not(loom))]
pub mod byte_vec;
#[cfg(not(loom))]
pub mod clock;
#[cfg(not(loom))]
pub mod congestion;
#[cfg(not(loom))]
pub mod control;
#[cfg(not(loom))]
pub mod credentials;
#[cfg(not(loom))]
pub mod crypto;
#[cfg(not(loom))]
pub mod datagram;
#[cfg(not(loom))]
pub mod either;
#[cfg(not(loom))]
pub mod event;
#[cfg(not(loom))]
pub mod flow;
pub mod intrusive_queue;
#[cfg(not(loom))]
pub mod msg;
#[cfg(not(loom))]
pub mod packet;
#[cfg(not(loom))]
pub mod path;
#[cfg(not(loom))]
pub mod psk;
#[cfg(not(loom))]
pub mod random;
#[cfg(not(loom))]
pub mod recovery;
pub mod socket;
#[cfg(not(loom))]
pub mod stream;
#[cfg(not(loom))]
pub mod stream2;
#[cfg(not(loom))]
pub mod sync;
#[cfg(not(loom))]
pub mod task;
#[cfg(not(loom))]
pub mod uds;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use s2n_quic_core::dc::{Version, SUPPORTED_VERSIONS};
