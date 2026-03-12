// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use criterion::Criterion;

pub mod crypto;
pub mod datagram;
pub mod send_wheel;
pub mod streams;

pub fn benchmarks(c: &mut Criterion) {
    crypto::benchmarks(c);
    datagram::benchmarks(c);
    send_wheel::benches(c);
    streams::benchmarks(c);
}
