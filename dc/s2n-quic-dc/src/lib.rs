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

// Use the real libfabric when the feature is enabled
#[cfg(feature = "libfabric")]
mod libfabric;

// Use the polyfill when libfabric feature is not enabled
#[cfg(not(feature = "libfabric"))]
#[path = "libfabric_polyfill.rs"]
mod libfabric;

// Also create the polyfill module for when it's explicitly imported
#[cfg(not(feature = "libfabric"))]
mod libfabric_polyfill;

// pub use s2n_quic_core::dc::{Version, SUPPORTED_VERSIONS};
pub use bytevec::ByteVec;

#[cfg(test)]
mod tests;
