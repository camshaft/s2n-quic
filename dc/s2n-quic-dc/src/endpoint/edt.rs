// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    endpoint::id::{IdMap, LocalSenderId},
    socket::rate::Rate,
    time::precision::Timestamp,
};

pub struct Local {
    edts: IdMap<LocalSenderId, u64>,
    rate: Rate,
}

impl Local {
    pub fn new(socket_count: usize, rate: Rate) -> Self {
        Self {
            edts: IdMap::new(socket_count, 0u64),
            rate,
        }
    }

    pub fn len(&self) -> usize {
        self.edts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn advance(&mut self, sender_idx: LocalSenderId, now: Timestamp, byte_cost: u64) {
        if sender_idx.as_usize() >= self.edts.len() {
            return;
        }

        let cost_nanos = self.rate.nanos_for_bytes(byte_cost);
        let base = self.edts[sender_idx].max(now.nanos);
        self.edts[sender_idx] = base.saturating_add(cost_nanos);
    }

    #[inline]
    pub fn load_score(&self, sender_idx: LocalSenderId) -> u64 {
        if sender_idx.as_usize() >= self.edts.len() {
            return 0;
        }
        self.edts[sender_idx]
    }
}

// ── Pick-two selection curve ─────────────────────────────────────────────────
//
// Shared by every pick-two router (the endpoint's send-socket `PickTwo` and the storage scheduler's
// lane dispatch). Given the absolute EDT load-score delta between the two sampled candidates, returns
// the probability of routing to the higher-scored ("worse") one — a logistic decay floored so a
// structurally-worse target keeps receiving a probe trickle.

/// Worst-case drain-time gap (in nanoseconds) at which a worse candidate bottoms out at the floor
/// probability. EDT load scores are absolute drain-time timestamps in nanoseconds, so the delta
/// between two candidates is a *duration*; once it reaches this gap the candidate is "clearly worse"
/// and routed to only at the [`PICK_TWO_WORSE_FLOOR`] rate. The curve is derived so the smooth
/// logistic decay meets the floor exactly here (no abrupt knee).
pub(crate) const PICK_TWO_WORST_CASE_GAP_NANOS: f64 = 1_000_000.0;

/// Minimum probability of routing to the higher-scored (worse) candidate, also the probability at the
/// worst-case gap. A purely logistic rule would drive the worse candidate's share toward zero and
/// permanently starve a structurally-worse target; this floor keeps every candidate sampled so its
/// load estimate stays fresh. Kept low (1%) so a genuinely worst-case-behind target is sampled only
/// occasionally without stealing real traffic.
pub(crate) const PICK_TWO_WORSE_FLOOR: f64 = 0.01;

/// `ln((1 − floor) / floor)` for the default 1% floor, i.e. `ln(99)`. The only transcendental term in
/// the curve derivation; `f64::ln` is not const-stable, so it is pinned as a literal and the test
/// [`tests::pick_two_floor_ratio_literal`] asserts it still matches the floor.
pub(crate) const PICK_TWO_FLOOR_LN_RATIO: f64 = 4.595_119_850_134_59;

/// Probability of routing to the higher-scored ("worse") candidate given the absolute score `delta`,
/// with a caller-supplied `worst_case_gap` — the delta at which the worse candidate bottoms out at
/// [`PICK_TWO_WORSE_FLOOR`]. `delta` and `worst_case_gap` must be in the **same domain**; the curve is
/// otherwise scale-free, so the same shape serves any pick-two router regardless of what its scores
/// measure.
///
/// The logistic time-scale `τ` is *derived* from the gap and the floor as
/// `τ = worst_case_gap / ln((1 − floor) / floor)`, so `1 / (1 + exp(delta / τ))` equals the floor
/// exactly at `delta == worst_case_gap` (no abrupt knee). At `delta == 0` it is `0.5` (a fair coin
/// flip), ~24% at a quarter of the gap and ~9% at half. Only the `delta`-dependent `exp` runs per call.
///
/// This is the generic form. [`pick_two_worse_probability`] is the nanosecond drain-time
/// specialization used by the EDT routers; the storage shard picker calls this directly with a
/// cost-byte gap (a device's pool capacity) to score credit-pool load.
#[inline]
pub fn pick_two_worse_probability_scaled(delta: u64, worst_case_gap: f64) -> f64 {
    let tau = worst_case_gap / PICK_TWO_FLOOR_LN_RATIO;
    let logistic = 1.0 / (1.0 + (delta as f64 / tau).exp());
    logistic.max(PICK_TWO_WORSE_FLOOR)
}

/// Probability of routing to the higher-scored (worse) candidate given the absolute score delta, in
/// the **nanosecond drain-time** domain (gap = [`PICK_TWO_WORST_CASE_GAP_NANOS`] ≈ 1ms, so τ ≈ 218µs).
/// The specialization of [`pick_two_worse_probability_scaled`] used by every EDT pick-two router (the
/// send-socket `PickTwo` and the storage scheduler's lane dispatch).
#[inline]
pub fn pick_two_worse_probability(delta: u64) -> f64 {
    pick_two_worse_probability_scaled(delta, PICK_TWO_WORST_CASE_GAP_NANOS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::id::Id;

    fn rate_10gbps() -> Rate {
        Rate::new(10.0)
    }

    fn ts(nanos: u64) -> Timestamp {
        Timestamp { nanos }
    }

    #[test]
    fn advance_from_zero() {
        let mut edt = Local::new(2, rate_10gbps());
        let idx = LocalSenderId::from_index(0);
        let now = ts(1_000_000_000);

        edt.advance(idx, now, 1000);

        let score = edt.load_score(idx);
        assert!(score > now.nanos);
        assert!(score < now.nanos + 1000);
    }

    #[test]
    fn advance_monotonic() {
        let mut edt = Local::new(1, rate_10gbps());
        let idx = LocalSenderId::from_index(0);
        let now = ts(1_000_000_000);

        edt.advance(idx, now, 5000);
        let first = edt.load_score(idx);

        edt.advance(idx, now, 5000);
        let second = edt.load_score(idx);
        assert!(second > first);
    }

    #[test]
    fn idle_gap_snaps_forward() {
        let mut edt = Local::new(1, rate_10gbps());
        let idx = LocalSenderId::from_index(0);

        edt.advance(idx, ts(1_000_000_000), 10000);
        let old_score = edt.load_score(idx);

        let future_now = ts(5_000_000_000);
        assert!(future_now.nanos > old_score);

        edt.advance(idx, future_now, 1000);
        let new_score = edt.load_score(idx);
        assert!(new_score > future_now.nanos);
        assert!(new_score < future_now.nanos + 1000);
    }

    #[test]
    fn out_of_bounds_is_noop() {
        let mut edt = Local::new(2, rate_10gbps());
        let oob = LocalSenderId::from_index(5);

        edt.advance(oob, ts(1_000_000_000), 1000);
        assert_eq!(edt.load_score(oob), 0);
    }

    // The nanosecond-domain pick-two curve (`pick_two_worse_probability` + its constants) is also
    // exercised by `endpoint::combinator::tests` (floor-ratio literal + curve shape) via the
    // combinator's re-export.

    #[test]
    fn scaled_curve_generalizes_to_any_domain() {
        // The scale-free curve must have the same shape whatever the gap's units: 0.5 at delta 0,
        // the floor exactly at `delta == gap`, ~9% at half the gap, and monotonic decay past it.
        // Use a byte-domain gap (a 32 MiB pool capacity) wholly unrelated to the ns constants — this
        // is exactly how the storage shard picker calls it.
        let gap_bytes = 32.0 * 1024.0 * 1024.0;
        let gap = gap_bytes as u64;

        assert_eq!(pick_two_worse_probability_scaled(0, gap_bytes), 0.5);

        let p_worst = pick_two_worse_probability_scaled(gap, gap_bytes);
        assert!(
            (p_worst - PICK_TWO_WORSE_FLOOR).abs() < 1e-6,
            "logistic must meet the floor at delta == gap in any domain, got {p_worst}"
        );

        let p_half = pick_two_worse_probability_scaled(gap / 2, gap_bytes);
        assert!(
            (0.08..0.10).contains(&p_half),
            "expected ~0.09 at half the gap, got {p_half}"
        );

        assert!(pick_two_worse_probability_scaled(2 * gap, gap_bytes) <= p_worst);
        assert_eq!(
            pick_two_worse_probability_scaled(u64::MAX, gap_bytes),
            PICK_TWO_WORSE_FLOOR
        );
    }

    #[test]
    fn ns_wrapper_is_the_scaled_curve_at_the_ns_gap() {
        // The nanosecond specialization must be exactly the scaled form fed the ns worst-case gap —
        // refactoring the shared body must not perturb the EDT routers' behaviour.
        for delta in [
            0u64,
            1,
            1_000,
            100_000,
            250_000,
            1_000_000,
            5_000_000,
            u64::MAX,
        ] {
            assert_eq!(
                pick_two_worse_probability(delta),
                pick_two_worse_probability_scaled(delta, PICK_TWO_WORST_CASE_GAP_NANOS),
                "ns wrapper diverged from the scaled curve at delta={delta}"
            );
        }
    }
}
