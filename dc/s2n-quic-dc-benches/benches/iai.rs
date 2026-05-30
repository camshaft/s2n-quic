// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Iai benchmark harness.
//!
//! Runs every benchmark from [`s2n_quic_dc_benches::benchmarks`] once each so
//! that the binary can be executed under Valgrind to collect instruction counts
//! and cache statistics.
//!
//! # Usage
//!
//! ```sh
//! # Build
//! cargo build --bench iai --features iai --profile bench
//!
//! # Cachegrind (instruction + cache counts)
//! valgrind --tool=cachegrind \
//!     --cachegrind-out-file=cachegrind.out \
//!     ./target/bench/deps/iai-*
//! cg_annotate cachegrind.out
//!
//! # Callgrind (instruction counts with per-function detail)
//! valgrind --tool=callgrind \
//!     --callgrind-out-file=callgrind.out.%p \
//!     --instr-atstart=yes \
//!     ./target/bench/deps/iai-*
//! callgrind_annotate callgrind.out.*
//! ```

fn main() {
    let mut criterion = s2n_quic_dc_benches::bench::Criterion::new();
    s2n_quic_dc_benches::benchmarks(&mut criterion);
}
