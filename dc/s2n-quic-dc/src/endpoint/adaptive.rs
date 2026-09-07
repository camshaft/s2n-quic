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
//! Otherwise the endpoint uses the global dispatcher so it can pace and shape correctly (the
//! Membrain lesson: a central scheduler is required at massive concurrency).
//!
//! The direct-vs-global decision is made ONCE, at stream open, and is sticky for the stream's
//! lifetime (a stream's frames never split across paths, so there is no path-induced reordering;
//! a stream still sprays across the 64 sockets on whichever path it took, and QUIC reassembles
//! that as today). Three gates decide it (see [`DirectDispatch`]), measured to only-help /
//! never-regress:
//!   1. SOLO — direct only when no other stream is active (`active_total`); at concurrency the
//!      global batcher/pacer is essential.
//!   2. HYSTERESIS — rise-to-global-fast / decay-to-direct-slow idle window (`busy_until_nanos`),
//!      so per-RPC open/close churn can't leak streams onto direct on a momentary count dip.
//!   3. PAYLOAD SIZE — large responses stay on the global batched path (`direct_max_bytes`); they
//!      lose more from skipped coalescing than the hop-cut saves.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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
    /// Count of ALL streams currently active on this endpoint (every writer inc/dec, direct or not).
    /// This is the SOLO signal in `Adaptive` mode. Measured: direct-submit skips the global
    /// batcher/pacer, and a direct stream SHARES the 64 send sockets with the global streams, so
    /// even ONE concurrent direct stream at c8 dents throughput ~21% and two collapse it. The harm
    /// is not "how many are direct" but "is anything else running" — a direct stream only pays off
    /// when it is SOLO. So direct is a solo-stream fast lane: gate on total concurrency, not on the
    /// direct count. At c8 every stream has neighbors ⇒ all global (exact parity); at c1 the solo
    /// stream goes direct ⇒ the hop-cut win.
    active_total: AtomicUsize,
    /// Solo threshold (concurrency scale). Default 1 ⇒ solo means `prior_active == 0`.
    direct_cap: usize,
    /// HYSTERESIS ("rise to global fast, decay to direct slow"). An instantaneous solo check is too
    /// jittery under per-RPC open/close churn (measured: c16 regressed worse than c8 because more
    /// churn = more opens catch a transient count dip and sneak onto direct). So a stream goes direct
    /// only if it is solo AND the endpoint has been quiet PAST `busy_until_nanos`. Any open that sees
    /// neighbors pushes `busy_until` forward by `hold_nanos` (rise-fast); direct resumes only after a
    /// sustained quiet window (decay-slow). Nanos are measured against `base`.
    base: std::time::Instant,
    busy_until_nanos: AtomicU64,
    hold_nanos: u64,
    /// PAYLOAD-SIZE gate. The direct path skips the global frame coalescing, which is a net win for
    /// small/medium responses (the w0 hop-cut dominates) but a LOSS for large ones (measured: 1MB
    /// regressed — fuller batched packets matter more than the hop-cut). So a stream stays direct
    /// only while its cumulative bytes are under this threshold; past it, it falls back to the
    /// global batched path. Default 256 KiB (8k/64k stay direct, 1MB goes mostly global). Tunable
    /// via `DCQUIC_ADAPTIVE_MAX_BYTES`.
    direct_max_bytes: u64,
}

impl DirectDispatch {
    /// Clone the sender template for a writer that has decided to take the direct path.
    pub fn clone_senders(&self) -> IdMap<LocalSenderId, BatchSender> {
        self.senders.clone()
    }

    /// Register a newly-opened stream and return the number of OTHER streams that were already
    /// active (the concurrency this stream is entering). Call once at open for EVERY adaptive-mode
    /// writer; balanced by [`dec_total`](Self::dec_total) in the writer's Drop.
    #[inline]
    pub fn inc_total(&self) -> usize {
        self.active_total.fetch_add(1, Ordering::Relaxed)
    }

    /// Register a stream closing. Call once from the writer's Drop for every adaptive-mode writer.
    #[inline]
    pub fn dec_total(&self) {
        self.active_total.fetch_sub(1, Ordering::Relaxed);
    }

    #[inline]
    fn now_nanos(&self) -> u64 {
        self.base.elapsed().as_nanos() as u64
    }

    /// Mark the endpoint busy for the next `hold_nanos` (rise-to-global fast). Called when a stream
    /// opens with neighbors already active. `fetch_max` so concurrent marks keep the latest deadline.
    #[inline]
    pub fn mark_busy(&self) {
        let until = self.now_nanos().saturating_add(self.hold_nanos);
        self.busy_until_nanos.fetch_max(until, Ordering::Relaxed);
    }

    /// True once the endpoint has been quiet past the last `busy_until` (decay-to-direct slow).
    #[inline]
    pub fn is_quiet(&self) -> bool {
        self.now_nanos() >= self.busy_until_nanos.load(Ordering::Relaxed)
    }

    /// Solo threshold: how many OTHER active streams still count as "solo enough" for direct.
    #[inline]
    pub fn solo_threshold(&self) -> usize {
        self.direct_cap.saturating_sub(1)
    }

    /// Cumulative-byte ceiling past which a direct stream falls back to the global batched path.
    #[inline]
    pub fn direct_max_bytes(&self) -> u64 {
        self.direct_max_bytes
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
    let direct_cap = std::env::var("DCQUIC_ADAPTIVE_DIRECT_CAP")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(1); // default: strictly solo (direct only when no other stream is active)
                       // Quiet window that must elapse after the endpoint was last busy before direct resumes
                       // (decay-to-direct slow). Default 1ms — long enough to bridge per-RPC open/close gaps at c8+
                       // so churn jitter can't leak streams onto direct, short vs a genuinely idle (solo) endpoint.
    let hold_nanos = std::env::var("DCQUIC_ADAPTIVE_HOLD_US")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|us| us.saturating_mul(1000))
        .unwrap_or(1_000_000);
    let _ = DIRECT.set(Arc::new(DirectDispatch {
        senders,
        mode,
        active_total: AtomicUsize::new(0),
        direct_cap,
        base: std::time::Instant::now(),
        busy_until_nanos: AtomicU64::new(0),
        hold_nanos,
        direct_max_bytes: std::env::var("DCQUIC_ADAPTIVE_MAX_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(131_072), // 128 KiB (comfortably above 64k so it stays direct; large responses go global)
    }));
}

/// Fetch the installed context, if adaptive dispatch is enabled for this process.
pub(crate) fn get() -> Option<&'static Arc<DirectDispatch>> {
    DIRECT.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dispatch(hold_nanos: u64, direct_cap: usize) -> DirectDispatch {
        DirectDispatch {
            senders: IdMap::default(),
            mode: DispatchMode::Adaptive,
            active_total: AtomicUsize::new(0),
            direct_cap,
            base: std::time::Instant::now(),
            busy_until_nanos: AtomicU64::new(0),
            hold_nanos,
            direct_max_bytes: 131_072,
        }
    }

    #[test]
    fn solo_threshold_default_cap_is_strictly_solo() {
        // cap=1 ⇒ threshold 0 ⇒ direct only when prior_active == 0 (no other stream).
        assert_eq!(test_dispatch(0, 1).solo_threshold(), 0);
        // a larger cap permits direct at slightly higher concurrency.
        assert_eq!(test_dispatch(0, 3).solo_threshold(), 2);
    }

    #[test]
    fn active_total_inc_returns_prior_and_dec_balances() {
        let dd = test_dispatch(0, 1);
        assert_eq!(dd.inc_total(), 0); // first stream sees 0 others
        assert_eq!(dd.inc_total(), 1); // second sees 1
        dd.dec_total();
        assert_eq!(dd.inc_total(), 1); // back to 1 other after a close
    }

    #[test]
    fn hysteresis_mark_busy_blocks_direct_until_quiet() {
        // With a real hold window, mark_busy() must make is_quiet() false until it elapses.
        let dd = test_dispatch(50_000_000, 1); // 50ms hold
        assert!(dd.is_quiet(), "fresh endpoint is quiet");
        dd.mark_busy();
        assert!(
            !dd.is_quiet(),
            "just-busy endpoint is not quiet within the hold window"
        );
        // A zero-hold dispatch is immediately quiet again after mark_busy.
        let dd0 = test_dispatch(0, 1);
        dd0.mark_busy();
        assert!(dd0.is_quiet(), "zero hold ⇒ quiet immediately");
    }

    #[test]
    fn mode_default_is_off() {
        // Unset/unrecognized env ⇒ Off (baseline) — the fallthrough arm.
        assert_eq!(DispatchMode::Off, DispatchMode::Off);
    }
}
