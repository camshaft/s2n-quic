// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bach smoke tests for the IO scheduler.
//!
//! These run under the deterministic discrete-event simulator and assert the M1 properties: the
//! credit pool's fair share across equal-priority streams, strict priority across tiers, per-device
//! isolation, cooperative backpressure (no thread blowup), ordered spray for `materialize`, and
//! credit conservation (acquire-on-submit / release-on-complete, zero leak at quiescence).

use crate::{
    fs::{
        backend::mock::{Latency, MockBackend},
        backend::{Backend, LaneSetup, LaneSubmit},
        config::{BackendKind, Config, CostModel, DeviceConfig, OpWeights, PoolMode},
        device::DeviceId,
        scheduler::{BlockRef, Scheduler},
        SpawnHandle,
    },
    sched::{CreditConfig, Rate, TierPriority},
    testing::{ext::*, sim},
    time::bach::Clock,
    tracing::*,
};
use core::time::Duration;
use std::{cell::RefCell, rc::Rc};

/// A backend whose lanes are immediately dead: it builds the lane submit senders but drops the
/// matching receivers right away (modelling a lane task that failed/exited while the scheduler keeps
/// accepting submits — e.g. a syscall/uring lane that hit a fatal error in M2/M3). `push_to_lane`'s
/// `lane.send(...)` then returns `Err(op)` and the op (with its `flow_credits`) is dropped.
struct DeadLaneBackend;

impl Backend for DeadLaneBackend {
    fn spawn_lanes(&self, setup: LaneSetup) -> Vec<LaneSubmit> {
        let mut handles = Vec::with_capacity(setup.lane_count);
        for _ in 0..setup.lane_count {
            let (tx, rx) =
                crate::socket::channel::intrusive::unsync::new::<crate::fs::op::IoOp>();
            // Drop the receiver immediately: the lane channel is now closed, so every send fails.
            drop(rx);
            handles.push(Box::new(tx) as LaneSubmit);
        }
        handles
    }
}

/// A backend whose lanes immediately complete every op as `Failed` (modelling a real backend whose
/// syscall errored — e.g. EIO / O_DIRECT EINVAL). It drains its lane and routes the op straight to
/// the completion sink with a `Failed` status, so the submit path's error mapping is exercised.
struct FailBackend {
    kind: std::io::ErrorKind,
}

impl Backend for FailBackend {
    fn spawn_lanes(&self, setup: LaneSetup) -> Vec<LaneSubmit> {
        use crate::fs::op::{IoOp, IoStatus};
        use crate::socket::channel::{intrusive::unsync, Budget, Receiver as _};
        let mut handles = Vec::with_capacity(setup.lane_count);
        for _ in 0..setup.lane_count {
            let (tx, mut rx) = unsync::new::<IoOp>();
            let completion = setup.completion.boxed_clone();
            let err_kind = self.kind;
            setup.spawn.spawn(async move {
                let mut budget = Budget::new(1 << 20);
                core::future::poll_fn(move |cx| {
                    budget.reset();
                    loop {
                        match rx.poll_recv(cx, &mut budget) {
                            core::task::Poll::Ready(Some(mut entry)) => {
                                entry.status = IoStatus::Failed(err_kind);
                                completion.send(entry);
                            }
                            core::task::Poll::Ready(None) => return core::task::Poll::Ready(()),
                            core::task::Poll::Pending => return core::task::Poll::Pending,
                        }
                    }
                })
                .await;
            });
            handles.push(Box::new(tx) as LaneSubmit);
        }
        handles
    }
}

/// A submit whose op cannot be handed to its lane (closed lane channel — e.g. a syscall/uring lane
/// that hit a fatal error in M2/M3) must (1) fail the submit promptly with `Err` rather than hang
/// the parked future forever, and (2) release the op's borrowed credit so the pool does not leak.
/// Before the fix, `push_to_lane` dropped the credit-bearing op silently and the submit future
/// parked forever (the per-op completion channel cannot signal a dropped sender to the waiter).
#[test]
fn lane_send_failure_fails_submit_and_conserves_credit() {
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
        let config = Config {
            devices: vec![iops_device(8)],
            ring_count: 1,
            backend: BackendKind::Mock,
        };
        let clock = Clock::default();
        let scheduler = Scheduler::new(&config, &DeadLaneBackend, bach_spawn(), clock);
        let dev = DeviceId(0);

        let h = scheduler.handle();
        let devices = scheduler.devices().clone();
        async move {
            // Each read acquires credit, fails to push to the dead lane, and must resolve to Err
            // promptly (not hang). `.await` directly — if the future hung, the sim would never
            // reach quiescence and the test would time out.
            for i in 0..5u64 {
                let result = h.read(dev, FD, i * 4096, 4096, TierPriority::Medium).await;
                assert!(
                    result.is_err(),
                    "submit to a closed lane must fail, not succeed or hang"
                );
            }
            // Conservation: every acquired credit was released back to the pool on the failed path.
            let device = devices.get(dev).unwrap();
            for pool in device.pools.all() {
                let free = pool.debug_free_total();
                let cap = pool.debug_capacity() as i64;
                info!(free, cap, "lane-send-failure conservation check");
                assert_eq!(free, cap, "credit leaked when lane send failed");
            }
        }
        .primary()
        .spawn();
    });
}

/// Wrap a `!Send` future so it can be handed to `bach::spawn` (which requires `Send`). Sound because
/// bach is single-threaded and never moves a task across threads — the same shim `runtime.rs` uses.
struct SendWrapper<F>(F);
// SAFETY: bach executes on a single thread; the future is never sent across threads.
unsafe impl<F> Send for SendWrapper<F> {}
impl<F: core::future::Future> core::future::Future for SendWrapper<F> {
    type Output = F::Output;
    fn poll(
        self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        unsafe {
            core::future::Future::poll(
                core::pin::Pin::new_unchecked(&mut self.get_unchecked_mut().0),
                cx,
            )
        }
    }
}

/// A `SpawnHandle` that drives `!Send` futures on the bach executor.
fn bach_spawn() -> SpawnHandle {
    SpawnHandle::new(|fut| {
        bach::spawn(SendWrapper(fut));
    })
}

/// Build a scheduler over `devices` with a `Mock` backend at the given latency and `ring_count`.
fn build(devices: Vec<DeviceConfig>, ring_count: usize, latency: Latency) -> Scheduler {
    let config = Config {
        devices,
        ring_count,
        backend: BackendKind::Mock,
    };
    let clock = Clock::default();
    let backend = MockBackend::new(clock.clone(), latency);
    Scheduler::new(&config, &backend, bach_spawn(), clock)
}

/// A tiny IOPS device: `queue_depth` outstanding ops, one IOP per 4 KiB, paced generously so the
/// credit pool (not the pacer) governs concurrency in these tests.
fn iops_device(queue_depth: u64) -> DeviceConfig {
    DeviceConfig {
        // Floor the fair-share slice at 1 op and cap a single acquire at 1 op so each block read is
        // its own credit unit — the cleanest shape for observing fair share across streams.
        pool_mode: PoolMode::Shared(
            CreditConfig::new(queue_depth)
                .with_max_single_acquire_uniform(queue_depth.max(1))
                .with_min_grant_slice_uniform(1)
                .without_refill(),
        ),
        rate: Rate::new(100.0),
        cost_model: CostModel::Iops { io_unit: 4096 },
        op_weights: OpWeights::default(),
    }
}

const FD: i32 = 7;

/// Run `n` identical reader streams at `priority` against `device` for a fixed simulated window,
/// returning each stream's completion count. Each stream keeps exactly one op in flight (submit →
/// await → submit), so the concurrent demand equals the stream count; set the device queue depth
/// below that to force credit contention.
fn run_streams(
    device: DeviceConfig,
    streams: Vec<(&'static str, TierPriority)>,
    window: Duration,
) -> Rc<RefCell<Vec<(&'static str, u64)>>> {
    let scheduler = build(vec![device], 1, Latency::default());
    let dev = DeviceId(0);
    let counts: Rc<RefCell<Vec<(&'static str, u64)>>> = Rc::new(RefCell::new(Vec::new()));

    for (label, priority) in streams {
        let h = scheduler.handle();
        let counts = counts.clone();
        async move {
            let mut done = 0u64;
            let start = bach::time::Instant::now();
            let mut offset = 0u64;
            while start.elapsed() < window {
                if h.read(dev, FD, offset, 4096, priority).await.is_ok() {
                    done += 1;
                    offset += 4096;
                }
            }
            counts.borrow_mut().push((label, done));
        }
        .primary()
        .spawn();
    }

    // Keep the scheduler (and its devices) alive past the window for the conservation check.
    let counts_ret = counts.clone();
    let devices = scheduler.devices().clone();
    async move {
        (window.as_millis() as u64 + 10).ms().sleep().await;
        // Conservation: at quiescence every acquired credit was released back to the pool.
        let device = devices.get(dev).unwrap();
        for pool in device.pools.all() {
            assert_eq!(
                pool.debug_free_total(),
                pool.debug_capacity() as i64,
                "credit leak at quiescence"
            );
        }
        info!("device pool conserved at quiescence");
    }
    .primary()
    .spawn();

    counts_ret
}

/// Demand-elastic fair share *within a tier*: four equal-priority streams contend for a device
/// whose queue depth (2) is below their combined demand (4). The distributor's `slice =
/// free/num_waiters` round-robin should hand all four near-equal throughput.
#[test]
fn fair_share_within_tier() {
    // The credit pool emits a per-op metric `trace!` for every acquire/release, so a throughput sim
    // floods the snapshot buffer with thousands of identical counter lines (the same reason every
    // credit-driven sim test in stream/{reader,writer} opts out). The gate here is the runtime
    // fairness/conservation assertions, not a verbatim metric log.
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
        let counts = run_streams(
            iops_device(2),
            vec![
                ("a", TierPriority::Medium),
                ("b", TierPriority::Medium),
                ("c", TierPriority::Medium),
                ("d", TierPriority::Medium),
            ],
            40.ms(),
        );
        async move {
            55.ms().sleep().await;
            let mut c = counts.borrow().clone();
            c.sort();
            let counts_only: Vec<u64> = c.iter().map(|(_, n)| *n).collect();
            let (min, max) = (
                *counts_only.iter().min().unwrap(),
                *counts_only.iter().max().unwrap(),
            );
            info!(?counts_only, min, max, "fair share within tier");
            assert!(min > 0, "a contending stream was starved within its tier");
            // Near-equal: the spread is a small fraction of the max (FIFO round-robin rotation).
            assert!(
                max - min <= (max / 4).max(2),
                "unfair within tier: spread {min}..{max}"
            );
        }
        .primary()
        .spawn();
    });
}

/// Strict priority *across tiers*: with the queue depth (2) below combined demand (4), the two
/// `High` streams should out-complete the two `Background` streams. (Strict priority can starve the
/// lower tier under sustained higher-tier load — "head-of-line blocking by design" — so we assert
/// dominance, not Background liveness.)
#[test]
fn strict_priority_across_tiers() {
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
        let counts = run_streams(
            iops_device(2),
            vec![
                ("high-a", TierPriority::High),
                ("high-b", TierPriority::High),
                ("bg-a", TierPriority::Background),
                ("bg-b", TierPriority::Background),
            ],
            40.ms(),
        );
        async move {
            55.ms().sleep().await;
            let c = counts.borrow().clone();
            let get = |name: &str| c.iter().find(|(l, _)| *l == name).map(|(_, n)| *n).unwrap_or(0);
            let high = get("high-a") + get("high-b");
            let bg = get("bg-a") + get("bg-b");
            info!(high, bg, "strict priority across tiers");
            assert!(high > bg, "High tier did not out-complete Background ({high} vs {bg})");
        }
        .primary()
        .spawn();
    });
}

/// Device isolation: a saturated device must not steal a second device's budget. Two devices, one
/// pounded by many streams, the other lightly used; the light device's stream should still complete
/// at its own pace, and each device's credit conserves independently.
#[test]
fn device_isolation() {
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
        let scheduler = build(vec![iops_device(2), iops_device(2)], 2, Latency::default());
        let busy = DeviceId(0);
        let quiet = DeviceId(1);

        let quiet_done = Rc::new(RefCell::new(0u64));

        // Four streams saturate the busy device.
        for _ in 0..4 {
            let h = scheduler.handle();
            async move {
                let start = bach::time::Instant::now();
                let mut offset = 0u64;
                while start.elapsed() < 40.ms() {
                    let _ = h.read(busy, FD, offset, 4096, TierPriority::Medium).await;
                    offset += 4096;
                }
            }
            .primary()
            .spawn();
        }

        // One stream uses the quiet device.
        {
            let h = scheduler.handle();
            let quiet_done = quiet_done.clone();
            async move {
                let start = bach::time::Instant::now();
                let mut offset = 0u64;
                let mut n = 0u64;
                while start.elapsed() < 40.ms() {
                    if h.read(quiet, FD, offset, 4096, TierPriority::Medium).await.is_ok() {
                        n += 1;
                        offset += 4096;
                    }
                }
                *quiet_done.borrow_mut() = n;
            }
            .primary()
            .spawn();
        }

        let quiet_done_r = quiet_done.clone();
        let devices = scheduler.devices().clone();
        async move {
            50.ms().sleep().await;
            let n = *quiet_done_r.borrow();
            info!(quiet_completed = n, "isolation: quiet device throughput");
            assert!(n > 0, "quiet device starved by busy device");
            for (id, device) in devices.iter() {
                for pool in device.pools.all() {
                    assert_eq!(
                        pool.debug_free_total(),
                        pool.debug_capacity() as i64,
                        "credit leak on device {id:?}"
                    );
                }
            }
            info!("isolation: both device pools conserved");
        }
        .primary()
        .spawn();
    });
}

/// Ordered spray: a `materialize` stream over blocks scattered across two devices, with the mock
/// completing blocks out of order (writes/reads of differing latency), must deliver bytes in strict
/// FIFO order. The deterministic fill (`byte i = (offset+i) as u8`) lets us verify each block.
#[test]
fn materialize_delivers_in_order() {
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
        // Differing per-device latency so completions arrive out of submission order.
        let latency = Latency {
            read: Duration::from_micros(100),
            write: Duration::from_micros(100),
            per_byte_nanos: 0,
        };
        let scheduler = build(vec![iops_device(8), iops_device(8)], 2, latency);

        let h = scheduler.handle();
        async move {
            // 10 blocks of 1 KiB, alternating devices so they land on different lanes and complete
            // out of order; offsets are strictly increasing in submission order.
            let blocks: Vec<BlockRef> = (0..10u64)
                .map(|i| {
                    let dev = DeviceId((i % 2) as u32);
                    BlockRef::whole(dev, FD, i * 1024, 1024)
                })
                .collect();

            let mut stream = h.materialize(blocks.clone(), TierPriority::High, 4);
            let mut delivered = 0u64;
            while let Some(chunk) = stream.next().await {
                let buf = chunk.expect("block read failed");
                let block = &blocks[delivered as usize];
                // Verify FIFO: the n-th delivered block is the n-th submitted block.
                assert_eq!(buf.len(), block.len as usize, "block {delivered} wrong len");
                for (j, b) in buf.iter().enumerate() {
                    let expected = (block.offset.wrapping_add(j as u64) & 0xff) as u8;
                    assert_eq!(*b, expected, "block {delivered} byte {j} out of order");
                }
                delivered += 1;
            }
            assert_eq!(delivered, 10, "did not deliver all blocks");
            info!(delivered, "materialize: all blocks delivered in FIFO order");
        }
        .primary()
        .spawn();
    });
}

/// A backend completion of `Failed` must surface as `Err` from `read()`/`write()` — never as `Ok`
/// with a bogus buffer. (Before the fix, `read()` ignored `op.status` and returned the empty/short
/// buffer, which `materialize` would then reassemble and deliver as if valid.)
#[test]
fn failed_completion_surfaces_as_err() {
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
        let config = Config {
            devices: vec![iops_device(8)],
            ring_count: 1,
            backend: BackendKind::Mock,
        };
        let clock = Clock::default();
        let backend = FailBackend {
            kind: std::io::ErrorKind::Other,
        };
        let scheduler = Scheduler::new(&config, &backend, bach_spawn(), clock);
        let dev = DeviceId(0);

        let h = scheduler.handle();
        let devices = scheduler.devices().clone();
        async move {
            let read = h.read(dev, FD, 0, 4096, TierPriority::Medium).await;
            assert!(read.is_err(), "failed read must surface as Err, got Ok");
            let write = h
                .write(dev, FD, 0, bytes::Bytes::from_static(b"abcd"), TierPriority::Medium)
                .await;
            assert!(write.is_err(), "failed write must surface as Err, got Ok");
            // Credit still conserves on the failure path (dispatcher releases on the failed op).
            let device = devices.get(dev).unwrap();
            for pool in device.pools.all() {
                assert_eq!(
                    pool.debug_free_total(),
                    pool.debug_capacity() as i64,
                    "credit leaked on failed completion"
                );
            }
            info!("failed completion surfaced as Err and conserved credit");
        }
        .primary()
        .spawn();
    });
}

/// An op whose credit cost exceeds its device pool capacity must fail fast with `Err`, not park
/// forever (an atomic `IoOp` has no partial-submit escape). With a 4-IOP pool and a 4 KiB IO unit, a
/// 64 KiB read costs 16 IOPS > 4 and must be rejected immediately.
#[test]
fn oversize_op_fails_fast() {
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
        let scheduler = build(vec![iops_device(4)], 1, Latency::default());
        let dev = DeviceId(0);
        let h = scheduler.handle();
        async move {
            // 64 KiB / 4 KiB = 16 IOPS, well above the queue depth of 4.
            let result = h.read(dev, FD, 0, 64 * 1024, TierPriority::Medium).await;
            assert!(
                matches!(
                    result.as_ref().map_err(|e| e.kind()),
                    Err(std::io::ErrorKind::InvalidInput)
                ),
                "oversize op must fail fast with InvalidInput, got {result:?}"
            );
            info!("oversize op rejected fast");
        }
        .primary()
        .spawn();
    });
}

/// An out-of-range, caller-supplied `DeviceId` must surface as `Err`, not panic the shared worker.
#[test]
fn out_of_range_device_id_errs() {
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
        let scheduler = build(vec![iops_device(4)], 1, Latency::default());
        let bad = DeviceId(99);
        let h = scheduler.handle();
        async move {
            let result = h.read(bad, FD, 0, 4096, TierPriority::Medium).await;
            assert!(
                matches!(
                    result.as_ref().map_err(|e| e.kind()),
                    Err(std::io::ErrorKind::InvalidInput)
                ),
                "out-of-range device id must error, got {result:?}"
            );
            info!("out-of-range device id rejected");
        }
        .primary()
        .spawn();
    });
}
