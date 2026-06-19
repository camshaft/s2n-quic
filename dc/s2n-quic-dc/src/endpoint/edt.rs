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

/// Time-scale `τ` (nanoseconds) of the logistic, *derived* from the worst-case gap and the floor:
/// `τ = gap / ln((1 − floor) / floor)`, so the logistic equals the floor exactly at `delta == gap`.
/// Const-folded (division is const-stable). ≈ 218µs with the default knobs.
const PICK_TWO_TAU_NANOS: f64 = PICK_TWO_WORST_CASE_GAP_NANOS / PICK_TWO_FLOOR_LN_RATIO;

/// Probability of routing to the higher-scored (worse) candidate given the absolute score delta.
///
/// Logistic decay `1 / (1 + exp(delta / τ))` clamped up to [`PICK_TWO_WORSE_FLOOR`], with `τ` =
/// [`PICK_TWO_TAU_NANOS`]. At `delta == 0` this is `0.5` (a fair coin flip); with the default 1ms/1%
/// knobs the worse candidate gets ~9% at half the worst-case gap and ~24% at a quarter of it. Only
/// the `delta`-dependent `exp` runs per call — every other term is a compile-time constant.
#[inline]
pub(crate) fn pick_two_worse_probability(delta: u64) -> f64 {
    let logistic = 1.0 / (1.0 + (delta as f64 / PICK_TWO_TAU_NANOS).exp());
    logistic.max(PICK_TWO_WORSE_FLOOR)
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

    // The pick-two selection curve (`pick_two_worse_probability` + its constants) is exercised by the
    // existing tests in `endpoint::combinator::tests` (floor-ratio literal + curve shape), which
    // reach these items via the combinator's re-export.
}
