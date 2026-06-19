// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Deterministic in-memory backend for bach tests.
//!
//! Modeled on bach's net layer, which spawns a task per packet: each lane drains its submission
//! channel and, for every op, **spawns a task** that sleeps for the op's latency on the simulated
//! clock and then completes it. There is no manual deadline heap or timer bookkeeping — the
//! per-op `sleep` integrates with the discrete-event scheduler directly, and out-of-order
//! completion falls out for free (a short op submitted later can wake before a long op submitted
//! earlier). No fd, no thread, no real IO. Reads get a deterministic fill so the ordered-spray
//! test can verify per-block delivery.

use crate::{
    intrusive::Entry,
    fs::{
        backend::{Backend, CompletionSink, LaneSetup, LaneSubmit},
        op::{IoBuf, IoKind, IoOp, IoStatus},
        SpawnHandle,
    },
    socket::channel::{intrusive::unsync, Budget, Receiver as _},
    time::precision::{self, Timer as _},
};
use core::{task::Poll, time::Duration};

/// Latency model for the mock backend.
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

/// A deterministic mock backend parameterized over the (precision) clock.
pub struct MockBackend<Clk> {
    clock: Clk,
    latency: Latency,
}

impl<Clk> MockBackend<Clk> {
    pub fn new(clock: Clk, latency: Latency) -> Self {
        Self { clock, latency }
    }
}

impl<Clk> Backend for MockBackend<Clk>
where
    Clk: precision::Clock + Clone,
{
    fn spawn_lanes(&self, setup: LaneSetup) -> Vec<LaneSubmit> {
        let mut handles = Vec::with_capacity(setup.lane_count);
        for _ in 0..setup.lane_count {
            let (tx, rx) = unsync::new::<IoOp>();
            let lane = LaneTask {
                rx,
                completion: setup.completion.boxed_clone(),
                clock: self.clock.clone(),
                latency: self.latency,
                spawn: setup.spawn.clone(),
                closed: false,
            };
            setup.spawn.spawn(lane.run());
            handles.push(Box::new(tx) as LaneSubmit);
        }
        handles
    }
}

/// Drains a lane's submission channel and spawns a completion task per op.
struct LaneTask<Clk: precision::Clock> {
    rx: unsync::Receiver<crate::intrusive::EntryAdapter<IoOp>>,
    completion: Box<dyn CompletionSink>,
    clock: Clk,
    latency: Latency,
    spawn: SpawnHandle,
    closed: bool,
}

impl<Clk: precision::Clock + Clone> LaneTask<Clk> {
    async fn run(mut self) {
        let mut budget = Budget::new(1 << 20);
        core::future::poll_fn(move |cx| {
            if self.closed {
                return Poll::Ready(());
            }
            budget.reset();
            loop {
                match self.rx.poll_recv(cx, &mut budget) {
                    Poll::Ready(Some(entry)) => {
                        self.spawn_completion(entry);
                        if budget.is_exhausted() {
                            // Budget spent mid-drain with the queue still non-empty: the receiver
                            // never reached its waker-registration branch, so a future `send` won't
                            // wake us. Self-wake to re-poll, mirroring the distributor loop.
                            cx.waker().wake_by_ref();
                            return Poll::Pending;
                        }
                    }
                    Poll::Ready(None) => {
                        self.closed = true;
                        return Poll::Ready(());
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
        })
        .await;
    }

    /// Spawn a task that sleeps for the op's latency, then completes and forwards it.
    fn spawn_completion(&self, op: Entry<IoOp>) {
        let latency = self.latency.for_op(op.kind, op.len);
        let deadline = precision::Timestamp {
            nanos: self.clock.now().nanos + latency.as_nanos() as u64,
        };
        let mut timer = self.clock.timer();
        let completion = self.completion.boxed_clone();
        self.spawn.spawn(async move {
            timer.sleep_until(deadline).await;
            let mut op = op;
            complete(&mut op);
            completion.send(op);
        });
    }
}

/// Stamp a successful completion: fill a read buffer to its requested length and record the result.
fn complete(op: &mut IoOp) {
    let len = op.len as usize;
    match &mut op.buf {
        IoBuf::Read(buf) => {
            // Deterministic fill: byte i = (offset + i) as u8, so the test can verify ordering.
            buf.clear();
            buf.resize(len, 0);
            let base = op.offset;
            for (i, b) in buf.iter_mut().enumerate() {
                *b = (base.wrapping_add(i as u64) & 0xff) as u8;
            }
        }
        IoBuf::Write(_) | IoBuf::None => {}
    }
    op.status = IoStatus::Done(len);
}
