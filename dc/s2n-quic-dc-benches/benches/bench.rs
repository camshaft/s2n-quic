// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(not(feature = "iai"))]
use criterion::{criterion_group, criterion_main};

#[cfg(not(feature = "iai"))]
criterion_group!(benches, s2n_quic_dc_benches::benchmarks);
#[cfg(not(feature = "iai"))]
criterion_main!(benches);

#[cfg(feature = "iai")]
fn main() {
    let mut c = s2n_quic_dc_benches::bench::Criterion::new();
    s2n_quic_dc_benches::benchmarks(&mut c);
}
