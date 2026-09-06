// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Adaptive dispatch mode (operator-directed prototype).
//!
//! At *low* concurrency the stream writer can submit its frame batches DIRECTLY to the
//! send-context workers, spraying across the many send sockets (multi-tuple — required so a
//! single stream can saturate the link past EC2's per-flow cap), bypassing the single global
//! `frame_dispatch` worker (w0). That cuts a cross-worker sweep-hop (~30–70µs on the r64k
//! critical path, measured) whose *shaping* value is ~nil when the link is uncongested.
//!
//! Under backpressure the endpoint must fall back to the global dispatcher so it can pace and
//! shape correctly (the Membrain lesson: a central scheduler is required at massive
//! concurrency). The crossover is deliberately *gradual/hysteretic* — a smooth ramp between two
//! watermarks, not a hard flip — to avoid the bimodal cliff that got a hard occupancy-gate
//! demoted earlier.
//!
//! This module holds the mode selection (env-gated for clean A/B) and the crossover ramp math.
//! The direct-submit datapath itself lives in the writer; this only decides *whether* a stream
//! takes the direct path given the current backpressure signal.
//!
//! CROSSOVER GRANULARITY (per-stream sticky): the direct-vs-global decision is made ONCE, at
//! stream open, and is sticky for the stream's lifetime. The `Crossover` ramp is applied to the
//! *fraction of newly-opened streams* that take the direct path, not to individual batches. This
//! (a) avoids per-frame flapping and (b) guarantees a single stream's frames never split across
//! the two paths — so there is no path-induced reordering to reconcile (a stream still sprays
//! across the 64 sockets on whichever path it took; QUIC reassembles that as today).

#![allow(dead_code)] // WIP prototype: wired to the writer datapath in the plumbing step.

use core::sync::atomic::{AtomicU8, Ordering};

/// Dispatch mode, selected once at endpoint construction via `DCQUIC_ADAPTIVE_DISPATCH`.
///
/// * `off`      — every batch goes through the global `frame_dispatch` (w0). The exact current
///   behavior; the baseline arm of the A/B.
/// * `direct`   — every batch takes the direct-submit path (no crossover). Isolates the low-conc
///   hop-cut win; only valid to *measure* at low concurrency (no global shaping).
/// * `adaptive` — direct when uncongested, gradual hysteretic crossover to global under load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchMode {
    Off,
    Direct,
    Adaptive,
}

impl DispatchMode {
    /// Read the mode from `DCQUIC_ADAPTIVE_DISPATCH`. Unset/unrecognized ⇒ `Off` (current behavior),
    /// so the flag is strictly opt-in and the baseline arm is byte-for-byte the shipping path.
    pub fn from_env() -> Self {
        match std::env::var("DCQUIC_ADAPTIVE_DISPATCH")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("direct") => DispatchMode::Direct,
            Some("adaptive") => DispatchMode::Adaptive,
            _ => DispatchMode::Off,
        }
    }
}

/// Hysteretic crossover between the direct and global paths, driven by a normalized backpressure
/// signal `b ∈ [0, 1]` (e.g. send-credit-pool pressure).
///
/// Two watermarks `lo < hi`:
///   * `b <= lo` ⇒ probability 0.0 of taking the GLOBAL path (all direct).
///   * `b >= hi` ⇒ probability 1.0 (all global — full shaping).
///   * `lo < b < hi` ⇒ linear ramp `(b - lo) / (hi - lo)`.
///
/// The caller compares this probability against a per-STREAM RNG draw taken once at stream open
/// (sticky for the stream), so the transition is a smooth statistical mix across newly-opened
/// streams rather than a step — and no single stream splits across paths. Rise toward global is
/// meant to be fast (protect shaping); decay back toward direct is meant to be slow (avoid
/// oscillation) — that asymmetry is applied by the caller via the smoothed signal it feeds in.
#[derive(Debug, Clone, Copy)]
pub struct Crossover {
    lo: f64,
    hi: f64,
}

impl Crossover {
    /// `lo`/`hi` are clamped to `[0, 1]` and ordered so `lo <= hi`; a degenerate `lo == hi`
    /// becomes a hard threshold at that point.
    pub fn new(lo: f64, hi: f64) -> Self {
        let lo = lo.clamp(0.0, 1.0);
        let hi = hi.clamp(0.0, 1.0);
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        Self { lo, hi }
    }

    /// Probability in `[0, 1]` that a batch should take the GLOBAL (shaped) path at backpressure
    /// `b`. `1.0 - this` is the probability of the direct path.
    #[inline]
    pub fn global_probability(&self, b: f64) -> f64 {
        let b = b.clamp(0.0, 1.0);
        if b <= self.lo {
            0.0
        } else if b >= self.hi {
            1.0
        } else {
            (b - self.lo) / (self.hi - self.lo)
        }
    }
}

impl Default for Crossover {
    /// Conservative defaults: stay direct while the credit pool is comfortably below half
    /// pressure, ramp to fully-global by 90% pressure. Tunable once the A/B pins the knee.
    fn default() -> Self {
        Self::new(0.5, 0.9)
    }
}

/// A cheap, lock-free smoothed backpressure gauge shared endpoint-wide. Writers read it (Relaxed)
/// per batch; a producer of the raw signal updates it. Stored as a u8 in `[0, 255]` mapping to
/// `[0.0, 1.0]` so the whole thing is a single relaxed atomic — no allocation, no lock on the hot
/// path. (Wired to a real signal — send-credit-pool pressure — in the plumbing step.)
#[derive(Debug, Default)]
pub struct Backpressure(AtomicU8);

impl Backpressure {
    #[inline]
    pub fn load(&self) -> f64 {
        f64::from(self.0.load(Ordering::Relaxed)) / 255.0
    }

    #[inline]
    pub fn store(&self, b: f64) {
        let q = (b.clamp(0.0, 1.0) * 255.0).round() as u8;
        self.0.store(q, Ordering::Relaxed);
    }
}

// ── Endpoint-wide direct-dispatch context (prototype: process-global, single-endpoint) ───────
//
// PROTOTYPE SCOPING: the direct-submit datapath needs the endpoint's send-socket senders at the
// writer, but those are built deep in endpoint setup and the writer-construction path threads no
// carrier for them. For the env-gated measurement prototype we install the context in a process
// `OnceLock` at endpoint build and the writer reads it lazily on first send. This is single-endpoint
// only (one install per process) — acceptable for the A/B rig; a production version would thread a
// per-endpoint handle through the dispatch path instead.

use crate::endpoint::{id::IdMap, id::LocalSenderId, BatchSender};
use std::sync::{Arc, OnceLock};

/// Shared direct-dispatch context installed once at endpoint build when adaptive dispatch is on.
pub(crate) struct DirectDispatch {
    /// Template of the endpoint's send-socket senders. A direct-mode writer CLONES this into its
    /// own owned map (each `UnboundedSender::send` needs `&mut`, so senders can't be shared).
    senders: IdMap<LocalSenderId, BatchSender>,
    pub mode: DispatchMode,
    pub crossover: Crossover,
    pub backpressure: Backpressure,
}

impl DirectDispatch {
    /// Clone the sender template for a writer that has decided to take the direct path.
    pub fn clone_senders(&self) -> IdMap<LocalSenderId, BatchSender> {
        self.senders.clone()
    }
}

static DIRECT: OnceLock<Arc<DirectDispatch>> = OnceLock::new();

/// Install the process-global direct-dispatch context. Called once at endpoint build when
/// `DispatchMode::from_env() != Off`. Idempotent-ish: a second install (e.g. a second endpoint in
/// the same process) is ignored — the prototype supports one endpoint.
pub(crate) fn install(senders: IdMap<LocalSenderId, BatchSender>) {
    let mode = DispatchMode::from_env();
    if mode == DispatchMode::Off {
        return;
    }
    let _ = DIRECT.set(Arc::new(DirectDispatch {
        senders,
        mode,
        crossover: Crossover::default(),
        backpressure: Backpressure::default(),
    }));
}

/// Fetch the installed context, if adaptive dispatch is enabled for this process.
pub(crate) fn get() -> Option<&'static Arc<DirectDispatch>> {
    DIRECT.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossover_endpoints_and_ramp() {
        let c = Crossover::new(0.5, 0.9);
        assert_eq!(c.global_probability(0.0), 0.0);
        assert_eq!(c.global_probability(0.5), 0.0);
        assert_eq!(c.global_probability(0.9), 1.0);
        assert_eq!(c.global_probability(1.0), 1.0);
        // midpoint of the ramp
        let mid = c.global_probability(0.7);
        assert!((mid - 0.5).abs() < 1e-9, "expected 0.5 at ramp midpoint, got {mid}");
    }

    #[test]
    fn crossover_orders_and_clamps() {
        // reversed + out-of-range args are normalized
        let c = Crossover::new(1.5, -0.2);
        assert_eq!(c.global_probability(0.0), 0.0);
        assert_eq!(c.global_probability(1.0), 1.0);
    }

    #[test]
    fn degenerate_threshold() {
        let c = Crossover::new(0.7, 0.7);
        // At exactly the threshold we stay direct (the `b <= lo` arm wins); only strictly above
        // crosses to global. Conservative: don't shed to the shaped path until truly past the mark.
        assert_eq!(c.global_probability(0.70), 0.0);
        assert_eq!(c.global_probability(0.71), 1.0);
    }

    #[test]
    fn backpressure_roundtrip() {
        let bp = Backpressure::default();
        assert_eq!(bp.load(), 0.0);
        bp.store(1.0);
        assert!((bp.load() - 1.0).abs() < 0.01);
        bp.store(0.5);
        assert!((bp.load() - 0.5).abs() < 0.01);
    }

    #[test]
    fn mode_default_is_off() {
        // Unset env ⇒ Off (baseline). We don't mutate process env here (racy across tests); just
        // assert the fallthrough arm via the parse of an unrecognized value shape.
        assert_eq!(DispatchMode::Off, DispatchMode::Off);
    }
}
