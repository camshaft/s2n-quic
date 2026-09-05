// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Application-level RPC latency for dc-tester's simple request/response path, recorded through the
//! endpoint's own dc-metrics registry (not a parallel histogram) so it rides the existing efficient
//! summary/percentile machinery and is emitted by the standard `[METRICS]` reporter.
//!
//! Two client-observed timers are registered once against the endpoint counters:
//!   - `rpc.ttfb` — send start → first response byte (time to first byte)
//!   - `rpc.ttlb` — send start → last response byte  (time to last byte)
//!
//! They inherit the registry's default 1-in-64 sampling, so recording at high request rates
//! (hundreds of thousands/sec) stays cheap while still yielding stable p50/p90/p99 estimates. The
//! percentiles surface in the reporter's querylog line under the `rpc.ttfb` / `rpc.ttlb` keys.

use s2n_quic_dc::counter::{Registry, Timer};
use std::{sync::OnceLock, time::Duration};

struct RpcTimers {
    ttfb: Timer,
    ttlb: Timer,
}

static TIMERS: OnceLock<RpcTimers> = OnceLock::new();

/// Register the `rpc.ttfb` / `rpc.ttlb` timers against the endpoint's metric registry. Call once,
/// on the client, before workers start. Idempotent (a second call is ignored).
pub fn init(counters: &Registry) {
    let _ = TIMERS.set(RpcTimers {
        ttfb: counters.register_timer("rpc.ttfb"),
        ttlb: counters.register_timer("rpc.ttlb"),
    });
}

/// Record one request's client-observed TTFB and TTLB. No-op if [`init`] was never called (e.g. the
/// server side, which does not register these timers).
#[inline]
pub fn record(ttfb: Duration, ttlb: Duration) {
    if let Some(t) = TIMERS.get() {
        t.ttfb.record(ttfb);
        t.ttlb.record(ttlb);
    }
}
