// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Deterministic in-memory backend for bach tests.
//!
//! Each lane is one task that drains its submission channel and, for every admitted op, **spawns one
//! async task on the bach executor** that sleeps until the op's simulated completion time
//! (`now + latency(kind, len)`) and then *processes* the op and **completes it in place**
//! ([`combinator::complete`](crate::fs::combinator::complete)) — releasing its credit to its own
//! device pool and notifying its submitter. Bach's discrete-event scheduler *is* the timer — there is
//! no hand-rolled deadline heap or timer wheel; out-of-order completion falls out naturally (a short
//! op spawned later wakes before a long op spawned earlier). This mirrors how `bach`'s own network
//! impl models per-packet delay. No fd, no thread, no real IO.
//!
//! How an op is *processed* — fill a read buffer and stamp `Done`, or inject a `Failed`, or anything
//! else a test wants to assert — is a pluggable closure ([`BachBackend::with_processor`]). The default
//! ([`BachBackend::new`]) is [`process_default`]: a deterministic read fill (`byte i = (offset+i) as
//! u8`) so the ordered-spray test can verify per-block delivery, and `Done(len)`. A test that wants
//! failures (the old `FailBackend`) just supplies a closure that stamps `Failed`.

use crate::{
    fs::{
        backend::{Backend, LaneSetup},
        combinator::{complete, complete_cancelled, DRAIN_BUDGET},
        op::{IoBuf, IoKind, IoOp, IoStatus},
    },
    intrusive::Entry,
    socket::channel::{intrusive::unsync, Map, ReceiverExt as _},
    time::precision::{self, Timer as _},
};
use core::time::Duration;

/// Latency model for the bach backend.
#[derive(Clone, Copy, Debug)]
pub struct Latency {
    pub read: Duration,
    pub write: Duration,
    /// Extra latency per byte transferred (0 for a pure fixed-latency model).
    pub per_byte_nanos: u64,
}

impl Default for Latency {
    fn default() -> Self {
        // Asymmetric by default: writes cost more, matching NAND. Fixed per-op; the test overrides.
        Self {
            read: Duration::from_micros(100),
            write: Duration::from_micros(200),
            per_byte_nanos: 0,
        }
    }
}

impl Latency {
    fn for_op(&self, kind: IoKind, len: u32) -> Duration {
        let base = match kind {
            IoKind::Read => self.read,
            IoKind::Write => self.write,
            _ => self.read,
        };
        base + Duration::from_nanos(self.per_byte_nanos.saturating_mul(len as u64))
    }
}

/// The default op processor: deterministically fill a read buffer and stamp `Done(len)`. The fill
/// (`byte i = (offset + i) as u8`) lets the ordered-spray test verify per-block delivery order.
pub fn process_default(op: &mut IoOp) {
    let len = op.len as usize;
    let base = op.offset;
    let fill = |slice: &mut [u8]| {
        for (i, b) in slice.iter_mut().enumerate() {
            *b = (base.wrapping_add(i as u64) & 0xff) as u8;
        }
    };
    match &mut op.buf {
        IoBuf::Read(buf) => {
            // The buffer arrives pre-sized (capacity >= len) and logically empty; fill its spare
            // capacity and set the length — the no-memset path the real backends use.
            debug_assert!(buf.is_empty(), "read buffer arrives logically empty");
            let spare = buf.spare_capacity_mut();
            let n = len.min(spare.len());
            for (i, slot) in spare[..n].iter_mut().enumerate() {
                slot.write((base.wrapping_add(i as u64) & 0xff) as u8);
            }
            // SAFETY: we just initialized the first `n` bytes of spare capacity.
            unsafe { buf.set_len(n) };
        }
        // A direct read fills the caller's aligned buffer in place (zero-copy).
        IoBuf::Direct(buf) if op.kind.is_read() => fill(buf.as_mut_slice()),
        IoBuf::Direct(_) | IoBuf::Write(_) | IoBuf::None => {}
    }
    op.status = IoStatus::Done(len);
}

/// A deterministic bach backend parameterized over the (precision) clock and an op-processor closure.
///
/// `P: Fn(&mut IoOp) + Clone` runs (after the simulated latency elapses) to produce each op's result.
/// It is `Clone` because every lane and every per-op task gets its own copy; closures that capture
/// only `Copy`/`Clone` state satisfy this.
pub struct BachBackend<Clk, P = fn(&mut IoOp)> {
    clock: Clk,
    latency: Latency,
    process: P,
}

impl<Clk> BachBackend<Clk> {
    /// A backend whose ops complete successfully via [`process_default`].
    pub fn new(clock: Clk, latency: Latency) -> Self {
        Self {
            clock,
            latency,
            process: process_default as fn(&mut IoOp),
        }
    }
}

impl<Clk, P> BachBackend<Clk, P> {
    /// A backend that runs `process` on each op after its latency elapses — e.g. to inject failures
    /// or model real request handling. Subsumes the old dedicated fail/echo test backends.
    pub fn with_processor(clock: Clk, latency: Latency, process: P) -> Self {
        Self {
            clock,
            latency,
            process,
        }
    }
}

impl<Clk, P> Backend for BachBackend<Clk, P>
where
    Clk: precision::Clock + Clone,
    P: Fn(&mut IoOp) + Clone + 'static,
{
    type Lane = unsync::Sender<crate::intrusive::EntryAdapter<IoOp>>;

    fn spawn_lanes(&self, setup: LaneSetup) -> Vec<Self::Lane> {
        let mut handles = Vec::with_capacity(setup.lane_count);
        for _ in 0..setup.lane_count {
            let (tx, rx) = unsync::new::<IoOp>();
            let lane = LaneTask {
                rx,
                clock: self.clock.clone(),
                latency: self.latency,
                process: self.process.clone(),
            };
            // The lane drain loop is `!Send` but bach is single-threaded; spawn it via the global bach
            // spawner (the same path the per-op timer tasks use) rather than threading a `Spawner`
            // through the backend trait — which the real backends do not need.
            crate::runtime::bach::spawn_named("fs.bach.lane", lane.run());
            handles.push(tx);
        }
        handles
    }
}

/// One task per lane: drain submissions and spawn a per-op timer task for each. The lane task itself
/// holds no pending state — every op's delay lives in its own spawned task, scheduled by bach.
struct LaneTask<Clk: precision::Clock, P> {
    rx: unsync::Receiver<crate::intrusive::EntryAdapter<IoOp>>,
    clock: Clk,
    latency: Latency,
    process: P,
}

impl<Clk, P> LaneTask<Clk, P>
where
    Clk: precision::Clock + Clone,
    P: Fn(&mut IoOp) + Clone + 'static,
{
    async fn run(self) {
        let LaneTask {
            rx,
            clock,
            latency,
            process,
        } = self;

        // Drain the lane via the standard `Map` + `drain_budgeted` combinator; the map closure spawns
        // one bach task per op. When the channel closes, `drain_budgeted` returns and the lane ends —
        // already-spawned op tasks finish on their own (each owns its entry, counters, timer, and
        // processor).
        Map::new(rx, move |entry: Entry<IoOp>| {
            // The op completes at `now + latency` on the simulated clock; bach schedules the wake.
            let deadline = clock.now() + latency.for_op(entry.kind, entry.len);
            let process = process.clone();
            let mut timer = clock.timer();
            crate::runtime::bach::spawn_named("fs.bach.op", async move {
                timer.sleep_until(deadline).await;
                let mut entry = entry;
                // If the submitter dropped its receiver while the op waited out its latency, skip
                // processing — the result has nowhere to go. `complete_cancelled` still releases the
                // op's credit, so conservation holds.
                if entry.is_cancelled() {
                    complete_cancelled(entry);
                    return;
                }
                crate::fs::trace::backend_start(&entry);
                process(&mut entry);
                crate::fs::trace::backend_done(&entry);
                // Complete the op in place: record on its own device counters, release its credit to
                // its device pool, notify its submitter (or drop it if the receiver is gone).
                complete(entry);
            });
        })
        .drain_budgeted(Some(DRAIN_BUDGET))
        .await;
    }
}
