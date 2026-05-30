// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Benchmark backend abstraction.
//!
//! All benchmark modules write against a single, criterion-compatible API.
//! The actual backend is selected at compile time via the `iai` feature flag:
//!
//! | feature | backend | measures |
//! |---------|---------|---------|
//! | *(none)* | [criterion](https://docs.rs/criterion) | wall-clock time |
//! | `iai` | custom runner + callgrind client requests | instruction counts |
//!
//! # Querying the backend at runtime
//!
//! Benchmark code can branch on the active backend when needed:
//!
//! ```no_run
//! use s2n_quic_dc_benches::bench::{backend, Backend};
//!
//! match backend() {
//!     Backend::Criterion => { /* timing-sensitive setup */ }
//!     Backend::Iai => { /* instruction-count-friendly setup */ }
//! }
//! ```
//!
//! # Running under Valgrind
//!
//! Build and run the `bench` binary under callgrind or cachegrind:
//!
//! ```sh
//! cargo build --bench bench --features iai --profile bench
//!
//! # Callgrind: produces one output file per benchmark (labelled by name)
//! valgrind --tool=callgrind \
//!     --callgrind-out-file=callgrind.%p \
//!     --dump-instr=yes \
//!     ./target/bench/deps/bench-*
//! callgrind_annotate callgrind.out.<pid>.*
//!
//! # Cachegrind: single output file for the whole run
//! valgrind --tool=cachegrind \
//!     --cachegrind-out-file=cachegrind.out \
//!     ./target/bench/deps/bench-*
//! cg_annotate cachegrind.out
//! ```

/// The benchmark backend currently in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Wall-clock benchmarking via [criterion](https://docs.rs/criterion).
    Criterion,
    /// Instruction-count benchmarking for use with Valgrind
    /// (cachegrind or callgrind).
    Iai,
}

/// Returns the active benchmark backend.
#[inline]
pub const fn backend() -> Backend {
    #[cfg(not(feature = "iai"))]
    {
        Backend::Criterion
    }
    #[cfg(feature = "iai")]
    {
        Backend::Iai
    }
}

// ── Criterion backend (default) ───────────────────────────────────────────────

#[cfg(not(feature = "iai"))]
pub use criterion::{BenchmarkId, Criterion, Throughput};
#[cfg(not(feature = "iai"))]
pub use std::hint::black_box;

// ── Iai backend ───────────────────────────────────────────────────────────────

#[cfg(feature = "iai")]
pub use iai_backend::{black_box, BenchmarkId, Criterion, Throughput};

#[cfg(feature = "iai")]
mod iai_backend {
    use core::fmt;
    use std::ffi::CString;
    pub use std::hint::black_box;

    // ── Throughput ────────────────────────────────────────────────────────────

    /// Throughput hint (kept for API parity; not used for measurement in iai
    /// mode since we track instruction counts rather than data rates).
    #[derive(Clone, Debug)]
    #[non_exhaustive]
    pub enum Throughput {
        Bytes(u64),
        BytesDecimal(u64),
        Elements(u64),
    }

    // ── BenchmarkId ───────────────────────────────────────────────────────────

    /// A benchmark identifier with a human-readable name and optional
    /// parameter, matching [criterion's `BenchmarkId`][crit] API.
    ///
    /// [crit]: https://docs.rs/criterion/latest/criterion/struct.BenchmarkId.html
    #[derive(Clone, Debug)]
    pub struct BenchmarkId {
        display: String,
    }

    impl BenchmarkId {
        /// Create a new identifier from a `name` and a `parameter`.
        ///
        /// Matches `criterion::BenchmarkId::new`.
        pub fn new<S: fmt::Display, P: fmt::Display>(id: S, parameter: P) -> Self {
            Self {
                display: format!("{id}/{parameter}"),
            }
        }
    }

    impl fmt::Display for BenchmarkId {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.display)
        }
    }

    // ── Bencher ───────────────────────────────────────────────────────────────

    /// Single-iteration benchmark runner.
    ///
    /// In iai mode each closure is executed **exactly once**.  Running the
    /// resulting binary under `valgrind --tool=cachegrind` (or `callgrind`)
    /// then yields stable instruction counts and cache statistics.
    pub struct Bencher;

    impl Bencher {
        /// Run `f` once (iai mode) rather than many times (criterion mode).
        #[inline(always)]
        pub fn iter<F: FnMut()>(&mut self, mut f: F) {
            f();
        }

        /// Run `f` with a single iteration count and ignore the returned
        /// duration (iai mode does not measure wall time).
        #[inline(always)]
        pub fn iter_custom<F>(&mut self, mut f: F)
        where
            F: FnMut(u64) -> std::time::Duration,
        {
            let _ = f(1);
        }
    }

    // ── BenchmarkGroup ────────────────────────────────────────────────────────

    /// An iai-mode analogue of `criterion::BenchmarkGroup`.
    pub struct BenchmarkGroup<'c> {
        criterion: &'c mut Criterion,
        group_name: String,
    }

    impl<'c> BenchmarkGroup<'c> {
        /// Set throughput (no-op in iai mode; kept for API parity).
        #[inline]
        pub fn throughput(&mut self, _throughput: Throughput) -> &mut Self {
            self
        }

        /// Register and immediately run a benchmark with a borrowed input.
        ///
        /// Matches `criterion::BenchmarkGroup::bench_with_input`.
        pub fn bench_with_input<I, F>(
            &mut self,
            id: BenchmarkId,
            input: &I,
            mut f: F,
        ) -> &mut Self
        where
            I: ?Sized,
            F: FnMut(&mut Bencher, &I),
        {
            let full_name = format!("{}/{id}", self.group_name);
            let mut b = Bencher;
            self.criterion
                .run_bench(&full_name, &mut || f(&mut b, input));
            self
        }

        /// Register and immediately run a benchmark without input.
        ///
        /// Matches `criterion::BenchmarkGroup::bench_function`.
        pub fn bench_function<F>(&mut self, id: impl fmt::Display, mut f: F) -> &mut Self
        where
            F: FnMut(&mut Bencher),
        {
            let full_name = format!("{}/{id}", self.group_name);
            let mut b = Bencher;
            self.criterion.run_bench(&full_name, &mut || f(&mut b));
            self
        }

        /// Finish the group.  No-op in iai mode; present for API parity.
        pub fn finish(self) {}
    }

    // ── Criterion ─────────────────────────────────────────────────────────────

    /// Top-level iai benchmark runner.
    ///
    /// Create one instance, pass it to your `benchmarks` function, then let
    /// it drop (the `Drop` impl prints a summary).  Each call to
    /// [`BenchmarkGroup::bench_with_input`] / [`BenchmarkGroup::bench_function`]
    /// immediately executes the closure once so that Valgrind can count the
    /// resulting instructions.
    pub struct Criterion {
        count: usize,
    }

    impl Criterion {
        /// Create a new iai runner.
        pub fn new() -> Self {
            Self { count: 0 }
        }

        /// Open a named benchmark group.
        ///
        /// Matches `criterion::Criterion::benchmark_group`.
        pub fn benchmark_group<S: Into<String>>(&mut self, name: S) -> BenchmarkGroup<'_> {
            BenchmarkGroup {
                criterion: self,
                group_name: name.into(),
            }
        }

        /// Run a single benchmark closure, printing its name to stderr.
        ///
        /// Emits callgrind client requests around the closure:
        ///
        /// - [`callgrind::zero_stats`] before running — so the dump only captures
        ///   instructions from this benchmark, not any prior ones.
        /// - [`callgrind::dump_stats`] after running — labelled with the benchmark
        ///   name, which produces one output file per benchmark when running under
        ///   `valgrind --tool=callgrind`.
        ///
        /// Both requests are no-ops when not running under Valgrind, so the binary
        /// can also be used under cachegrind or executed without Valgrind at all.
        fn run_bench(&mut self, name: &str, f: &mut dyn FnMut()) {
            self.count += 1;
            eprintln!("[iai] {:>4}  {name}", self.count);
            // Zero stats so the dump captures only this benchmark's instructions.
            crabgrind::callgrind::zero_stats();
            f();
            // Emit a per-benchmark dump so a single execution yields one output
            // file per harness, enabling fine-grained instruction-count analysis.
            let label = CString::new(name).unwrap_or_else(|e| {
                let pos = e.nul_position();
                eprintln!(
                    "[iai] warning: benchmark name {name:?} contains a null byte at \
                     position {pos}; dump label will be truncated"
                );
                CString::new(&name[..pos]).unwrap()
            });
            crabgrind::callgrind::dump_stats(Some(label.as_c_str()));
        }
    }

    impl Default for Criterion {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for Criterion {
        fn drop(&mut self) {
            if self.count > 0 {
                eprintln!("[iai] completed {count} benchmarks", count = self.count);
            }
        }
    }
}
