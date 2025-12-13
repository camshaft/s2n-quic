// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod bytevec;
pub mod causality;
pub mod client;
pub mod message;
pub mod priority;
pub mod server;
pub mod stream;
pub mod worker;

// Re-export ofi-libfabric-sys (vendored or system)
#[cfg(any(feature = "libfabric-system", feature = "libfabric-vendored"))]
use ofi_libfabric_sys as libfabric_sys;

#[cfg(not(any(feature = "libfabric-system", feature = "libfabric-vendored")))]
compile_error!("Either 'libfabric-system' or 'libfabric-vendored' feature must be enabled");

// The libfabric module uses the sys crate through the re-export
#[cfg(any(feature = "libfabric-system", feature = "libfabric-vendored"))]
mod libfabric;

// Custom UDP-based provider that registers with libfabric
#[cfg(any(feature = "libfabric-system", feature = "libfabric-vendored"))]
mod libfabric_udp_provider;

// pub use s2n_quic_core::dc::{Version, SUPPORTED_VERSIONS};
pub use bytevec::ByteVec;

#[cfg(test)]
mod tests;
