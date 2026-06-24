// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod router;
mod socket;

#[cfg(any(test, feature = "testing"))]
mod bach;

/// io_uring recv backend (Linux only) — a multishot-recv ring as an alternative packet source.
#[cfg(target_os = "linux")]
pub mod uring;

pub use socket::Socket;
