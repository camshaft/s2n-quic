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

// Re-export the appropriate libfabric sys crate
// When libfabric feature is enabled, use real libfabric with custom UDP provider for fallback
#[cfg(feature = "libfabric")]
use ofi_libfabric_sys as libfabric_sys;

// When libfabric-polyfill feature is enabled, use FFI polyfill
#[cfg(all(not(feature = "libfabric"), feature = "libfabric-polyfill"))]
use ofi_libfabric_sys_polyfill as libfabric_sys;

#[cfg(not(any(feature = "libfabric", feature = "libfabric-polyfill")))]
compile_error!("Either 'libfabric' or 'libfabric-polyfill' feature must be enabled");

// The libfabric module uses the sys crate through the re-export
#[cfg(any(feature = "libfabric", feature = "libfabric-polyfill"))]
mod libfabric;

// Custom UDP-based provider that registers with libfabric
// Only available when using real libfabric (not the polyfill)
#[cfg(feature = "libfabric")]
mod libfabric_udp_provider;

// pub use s2n_quic_core::dc::{Version, SUPPORTED_VERSIONS};
pub use bytevec::ByteVec;

#[cfg(test)]
mod tests;
