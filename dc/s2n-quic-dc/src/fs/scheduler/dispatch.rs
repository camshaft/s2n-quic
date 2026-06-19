// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A device's submission dispatch task: route admitted ops from **that device's** `Send` submission
//! channel to one of **that device's** execution lanes.
//!
//! There is one dispatch task per device (built by the registrar on the worker, alongside the
//! device's lanes and distributor) — devices do not share a router, so the pick-two below balances
//! load only across the lanes belonging to a single device.
//!
//! This is the storage analog of the endpoint's `frame_dispatch` → `PickTwo` stage, and it routes
//! the **same way**: a per-lane earliest-departure-time ([`edt::Local`]) load score plus stochastic
//! pick-two with a round-robin first candidate. The first candidate is chosen round-robin (striping
//! consecutive ops across lanes); the second is a distinct random lane; the lower-scored lane usually
//! wins, but the higher-scored ("worse") lane is still chosen with a delta-dependent probability so a
//! structurally-busier lane keeps getting a probe trickle and its load estimate stays fresh. The
//! chosen lane's EDT is advanced by the op's `byte_cost`, so a burst of cheap ops and a burst of
//! expensive ops balance by *work*, not op count. Pacing already happened upstream (the per-device
//! credit pool gated admission); EDT here balances lane occupancy, exactly as `PickTwo` balances send
//! sockets after the send pool admitted a frame.
//!
//! The task owns the device's `!Send` lane senders (so the `Arc<Device>` never has to) and drains the
//! cross-thread submission channel via the standard `Map` + `drain_budgeted` combinator — no hand-rolled poll
//! loop. On a closed lane (a backend that exited on a fatal error) the op is stamped `Failed` and
//! **completed in place** via [`combinator::complete`](crate::fs::combinator::complete) so its
//! submitter's await resolves with an error and its credit is released exactly once, rather than
//! hanging.

use crate::{
    endpoint::{
        edt,
        id::{Id as _, LocalSenderId},
    },
    fs::{
        combinator::complete,
        device::LocalRingId,
        op::{IoOp, IoStatus},
    },
    intrusive::Entry,
    sched::{ByteCost as _, Map, Rate, ReceiverExt as _, UnboundedSender},
    socket::channel::intrusive::sync as sync_chan,
    xorshift::Rng,
};

/// Per-lane routing state for stochastic pick-two: the EDT load scores, the RNG, and the round-robin
/// cursor for the first candidate. Mirrors the fields `endpoint::combinator::PickTwo` holds.
struct Router {
    lane_edts: edt::Local,
    rng: Rng,
    round_robin: usize,
    lanes: usize,
}

impl Router {
    fn new(lanes: usize) -> Self {
        // EDT scores lane *occupancy* in time. A nominal unit rate suffices: only relative deltas
        // between lanes drive selection, and `byte_cost` (the credit cost) is the work unit advanced
        // per op. (The real device pacing already happened at admission.)
        let lane_edts = edt::Local::new(lanes, Rate::new(1.0));
        Self {
            lane_edts,
            rng: Rng::new(),
            round_robin: 0,
            lanes,
        }
    }

    /// Choose a lane for an op of `byte_cost` at `now`, advancing the chosen lane's EDT. Pick-two:
    /// round-robin first candidate, random distinct second, route to the lower combined score but
    /// give the worse candidate a logistic probe probability.
    fn choose(&mut self, byte_cost: u64, now: crate::time::precision::Timestamp) -> usize {
        let len = self.lanes;
        let chosen = if len <= 1 {
            0
        } else {
            let rr = self.round_robin;
            self.round_robin = if rr + 1 >= len { 0 } else { rr + 1 };
            let idx1 = rr;
            // Second candidate: random, distinct from idx1.
            let idx2 = if len == 2 {
                idx1 ^ 1
            } else {
                let mut raw = self.rng.next_usize(len - 1);
                if raw >= idx1 {
                    raw += 1;
                }
                raw
            };
            let score1 = self.lane_edts.load_score(LocalSenderId::from_index(idx1));
            let score2 = self.lane_edts.load_score(LocalSenderId::from_index(idx2));
            let (better, worse) = if score1 <= score2 {
                (idx1, idx2)
            } else {
                (idx2, idx1)
            };
            let delta = score1.abs_diff(score2);
            if self.rng.next_f64() < edt::pick_two_worse_probability(delta) {
                worse
            } else {
                better
            }
        };
        self.lane_edts
            .advance(LocalSenderId::from_index(chosen), now, byte_cost);
        chosen
    }
}

/// Drain admitted ops and route each to a backend lane via per-lane EDT + stochastic pick-two. Owns
/// the `!Send` lane senders.
pub(super) async fn dispatch_loop<L>(
    rx: sync_chan::Receiver<IoOp>,
    mut lanes: Vec<L>,
    clock: crate::time::DefaultClock,
) where
    L: UnboundedSender<Entry<IoOp>>,
{
    let mut router = Router::new(lanes.len().max(1));
    Map::new(rx, move |mut entry: Entry<IoOp>| {
        let byte_cost = entry.byte_cost();
        let lane = router.choose(byte_cost, clock.now());
        entry.ring_id = LocalRingId(lane as u32);
        if let Err(mut undeliverable) = lanes[lane].send(entry) {
            // Lane closed: record on the op's own device counters, stamp Failed, and complete it in
            // place so the submitter resolves with an error and its credit is released exactly once
            // (never a hang).
            undeliverable.device.counters.lane_closed.add(1);
            undeliverable.status = IoStatus::Failed(std::io::ErrorKind::BrokenPipe);
            complete(undeliverable);
        }
    })
    .drain_budgeted(Some(crate::fs::combinator::DRAIN_BUDGET))
    .await;
}
