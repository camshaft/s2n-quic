// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod bytevec;
pub mod causality;
pub mod client;
pub mod message;
pub mod priority;
pub mod server;
pub mod stream;
pub mod transport;
pub mod worker;

// Re-export the appropriate libfabric sys crate based on features
#[cfg(all(feature = "libfabric", not(feature = "libfabric-polyfill")))]
use ofi_libfabric_sys as libfabric_sys;

#[cfg(all(not(feature = "libfabric"), feature = "libfabric-polyfill"))]
use ofi_libfabric_sys_polyfill as libfabric_sys;

// The libfabric module uses the sys crate through the re-export
#[cfg(any(feature = "libfabric", feature = "libfabric-polyfill"))]
mod libfabric;

// pub use s2n_quic_core::dc::{Version, SUPPORTED_VERSIONS};
pub use bytevec::ByteVec;

#[cfg(test)]
mod tests;
