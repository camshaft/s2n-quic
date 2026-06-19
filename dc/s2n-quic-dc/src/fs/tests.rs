// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bach smoke tests for the IO scheduler.
//!
//! These run under the deterministic discrete-event simulator and assert the M1 properties: the
//! credit pool's fair share across equal-priority streams, strict priority across tiers, per-device
//! isolation, cooperative backpressure (no thread blowup), ordered spray for `materialize`, and
//! credit conservation (acquire-on-submit / release-on-complete, zero leak at quiescence).

use crate::{
    counter::Registry,
    fs::{
        backend::bach::{BachBackend, Latency},
        backend::{Backend, LaneSetup},
        config::{Config, CostModel, DeviceConfig, OpWeights, PoolMode},
        device::DeviceId,
        op::{IoOp, IoStatus},
        scheduler::{BlockRef, Scheduler},
    },
    runtime::Spawner,
    sched::{CreditConfig, Rate, TierPriority},
    testing::{ext::*, sim},
    time::bach::Clock,
    tracing::*,
};
use core::time::Duration;
use std::{cell::RefCell, rc::Rc};

/// A backend whose lanes are immediately dead: it builds the lane submit senders but drops the
/// matching receivers right away (modelling a lane task that failed/exited while the scheduler keeps
/// accepting submits — e.g. a syscall/uring lane that hit a fatal error in M2/M3). The dispatch task's
/// `lane.send(...)` then returns `Err(op)` and the op is routed to the completion sink stamped Failed.
struct DeadLaneBackend;

impl Backend for DeadLaneBackend {
    type Lane =
        crate::socket::channel::intrusive::unsync::Sender<crate::intrusive::EntryAdapter<IoOp>>;

    fn spawn_lanes<S: Spawner>(&self, setup: LaneSetup, _spawner: &mut S) -> Vec<Self::Lane> {
        let mut handles = Vec::with_capacity(setup.lane_count);
        for _ in 0..setup.lane_count {
            let (tx, rx) = crate::socket::channel::intrusive::unsync::new::<IoOp>();
            // Drop the receiver immediately: the lane channel is now closed, so every send fails.
            drop(rx);
            handles.push(tx);
        }
        handles
    }
}

/// A bach backend whose processor stamps every op `Failed` (modelling a real backend whose syscall
/// errored — e.g. EIO / O_DIRECT EINVAL). The old dedicated `FailBackend` is now just `BachBackend`
/// with a failing processor closure.
fn fail_backend(
    clock: Clock,
    kind: std::io::ErrorKind,
) -> BachBackend<Clock, impl Fn(&mut IoOp) + Clone> {
    BachBackend::with_processor(clock, Latency::default(), move |op: &mut IoOp| {
        op.status = IoStatus::Failed(kind);
    })
}

/// Build a scheduler over `devices` with a `Bach` backend at the given latency and `ring_count`.
fn build(devices: Vec<DeviceConfig>, ring_count: usize, latency: Latency) -> Scheduler {
    let config = Config {
        devices,
        ring_count,
    };
    let clock = Clock::default();
    let backend = BachBackend::new(clock.clone(), latency);
    let mut spawn = bach_spawner();
    Scheduler::new(&config, &backend, &mut spawn, &Registry::default(), clock)
}

/// The bach spawner used by every sim test (drives `!Send` futures on the bach executor).
fn bach_spawner() -> crate::runtime::bach::Local {
    crate::runtime::bach::Local::new(0)
}

/// A submit whose op cannot be handed to its lane (closed lane channel — e.g. a syscall/uring lane
/// that hit a fatal error in M2/M3) must (1) fail the submit promptly with `Err` rather than hang
/// the parked future forever, and (2) release the op's borrowed credit so the pool does not leak.
#[test]
fn lane_send_failure_fails_submit_and_conserves_credit() {
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
        let config = Config {
            devices: vec![iops_device(8)],
            ring_count: 1,
        };
        let clock = Clock::default();
        let mut spawn = bach_spawner();
        let scheduler = Scheduler::new(
            &config,
            &DeadLaneBackend,
            &mut spawn,
            &Registry::default(),
            clock,
        );
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
/// `High` streams should out-complete the two `Background` streams.
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
            let get = |name: &str| {
                c.iter()
                    .find(|(l, _)| *l == name)
                    .map(|(_, n)| *n)
                    .unwrap_or(0)
            };
            let high = get("high-a") + get("high-b");
            let bg = get("bg-a") + get("bg-b");
            info!(high, bg, "strict priority across tiers");
            assert!(
                high > bg,
                "High tier did not out-complete Background ({high} vs {bg})"
            );
        }
        .primary()
        .spawn();
    });
}

/// Device isolation: a saturated device must not steal a second device's budget.
#[test]
fn device_isolation() {
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
        let scheduler = build(vec![iops_device(2), iops_device(2)], 2, Latency::default());
        let busy = DeviceId(0);
        let quiet = DeviceId(1);

        let quiet_done = Rc::new(RefCell::new(0u64));

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

        {
            let h = scheduler.handle();
            let quiet_done = quiet_done.clone();
            async move {
                let start = bach::time::Instant::now();
                let mut offset = 0u64;
                let mut n = 0u64;
                while start.elapsed() < 40.ms() {
                    if h.read(quiet, FD, offset, 4096, TierPriority::Medium)
                        .await
                        .is_ok()
                    {
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

/// Ordered spray: a `materialize` stream over blocks scattered across two devices, with the bach
/// backend completing blocks out of order, must deliver bytes in strict FIFO order. The deterministic
/// fill (`byte i = (offset+i) as u8`) lets us verify each block.
#[test]
fn materialize_delivers_in_order() {
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
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

            let mut stream = h.materialize(blocks.clone(), TierPriority::High);
            let mut delivered = 0u64;
            while let Some(chunk) = stream.next().await {
                let buf = chunk.expect("block read failed");
                let block = &blocks[delivered as usize];
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

/// Cross-device head-of-line blocking: a materialize stream whose blocks alternate between a
/// **credit-starved** device (depth 1) and a roomy one must still make progress and deliver all
/// blocks in FIFO order. With one shared acquire slot, a block parked on the starved device's credit
/// would block submitting the *next* block — even though its device has free credit — and the stream
/// would crawl at the starved device's rate or wedge. The per-device acquire slots let the roomy
/// device's blocks submit while the starved device's block is parked.
#[test]
fn materialize_cross_device_no_hol() {
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
        // Device 0: depth 1 (one op in flight at a time) — the credit-starved device. Device 1:
        // depth 8 — roomy. Equal latency so ordering is driven by submission, not completion time.
        let latency = Latency {
            read: Duration::from_micros(100),
            write: Duration::from_micros(100),
            per_byte_nanos: 0,
        };
        let scheduler = build(vec![iops_device(1), iops_device(8)], 2, latency);
        let h = scheduler.handle();
        let devices = scheduler.devices().clone();
        async move {
            // 20 blocks alternating devices; device 0 (even indices) is depth-1 so its blocks must
            // serialize, while device 1 (odd indices) can have many in flight at once.
            let blocks: Vec<BlockRef> = (0..20u64)
                .map(|i| BlockRef::whole(DeviceId((i % 2) as u32), FD, i * 1024, 1024))
                .collect();

            let mut stream = h.materialize(blocks.clone(), TierPriority::High);
            let mut delivered = 0u64;
            while let Some(chunk) = stream.next().await {
                let buf = chunk.expect("block read failed");
                let block = &blocks[delivered as usize];
                assert_eq!(buf.len(), block.len as usize, "block {delivered} wrong len");
                for (j, b) in buf.iter().enumerate() {
                    let expected = (block.offset.wrapping_add(j as u64) & 0xff) as u8;
                    assert_eq!(*b, expected, "block {delivered} byte {j} out of order");
                }
                delivered += 1;
            }
            assert_eq!(delivered, 20, "did not deliver all blocks across devices");
            // Conservation across both pools at quiescence.
            for (id, device) in devices.iter() {
                for pool in device.pools.all() {
                    assert_eq!(
                        pool.debug_free_total(),
                        pool.debug_capacity() as i64,
                        "credit leak on device {id:?}"
                    );
                }
            }
            info!(
                delivered,
                "materialize: cross-device delivered in order, no HOL wedge"
            );
        }
        .primary()
        .spawn();
    });
}

/// ADVERSARIAL REPRO (case #7): dropping a `MaterializeStream` mid-spray — with some blocks in
/// flight (credit on `op.flow_credits`, awaiting backend completion) and some parked on a starved
/// device's credit — must conserve credit at quiescence. On drop:
///   * every `DeviceSlot`'s `SubmitterAlloc` releases any residual `pending_credits` (parked grant),
///   * `ToSubmit` slots hold no credit (acquired only on enqueue), so they release nothing,
///   * in-flight ops still complete through the backend → dispatcher, which releases their credit
///     BEFORE discarding the completion onto the now-dead `completion_rx` (receiver_alive == false).
/// If the dispatcher released after the dead-receiver check, or if a residual alloc grant were
/// abandoned without release, this would leak. Assert the pools return to full capacity.
#[test]
fn materialize_drop_mid_spray_conserves_credit() {
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
        // Non-trivial latency so ops stay in flight across the drop; device 0 is depth-1 (starved,
        // forces parked-on-credit ToSubmit blocks), device 1 is roomy (many in flight).
        let latency = Latency {
            read: Duration::from_micros(500),
            write: Duration::from_micros(500),
            per_byte_nanos: 0,
        };
        let scheduler = build(vec![iops_device(1), iops_device(8)], 2, latency);
        let h = scheduler.handle();
        let devices = scheduler.devices().clone();
        async move {
            // 40 blocks alternating devices: device 0 (even) serializes at depth 1, device 1 (odd)
            // piles up in flight. More blocks than either pool depth, so at the drop point there are
            // both in-flight ops and ToSubmit blocks parked on device 0's credit.
            let blocks: Vec<BlockRef> = (0..40u64)
                .map(|i| BlockRef::whole(DeviceId((i % 2) as u32), FD, i * 1024, 1024))
                .collect();

            let mut stream = h.materialize(blocks, TierPriority::High);
            // Deliver a few blocks to drive the spray into steady state (ops in flight + parked),
            // then drop the stream mid-flight.
            let mut delivered = 0u64;
            while delivered < 4 {
                match stream.next().await {
                    Some(Ok(_)) => delivered += 1,
                    Some(Err(e)) => panic!("unexpected block error: {e}"),
                    None => break,
                }
            }
            // Prove the drop is genuinely mid-flight: at least one pool has credit out (in-flight
            // ops + parked-on-credit ToSubmit blocks), so this exercises the release-on-late-
            // completion path rather than a quiescent no-op drop.
            let in_flight = devices.iter().any(|(_, device)| {
                device
                    .pools
                    .all()
                    .any(|pool| pool.debug_free_total() < pool.debug_capacity() as i64)
            });
            assert!(
                in_flight,
                "test precondition: expected credit in flight at drop point"
            );
            drop(stream);

            // Let any in-flight ops complete (their completions route to the dropped receiver, but
            // the dispatcher releases their credit first) and the dropped allocs release residuals.
            10.ms().sleep().await;

            for (id, device) in devices.iter() {
                for pool in device.pools.all() {
                    let free = pool.debug_free_total();
                    let cap = pool.debug_capacity() as i64;
                    info!(?id, free, cap, "drop-mid-spray conservation check");
                    assert_eq!(
                        free, cap,
                        "credit leaked on device {id:?} after mid-spray drop"
                    );
                }
            }
            info!("materialize: drop mid-spray conserved credit");
        }
        .primary()
        .spawn();
    });
}

/// A backend completion of `Failed` must surface as `Err` from `read()`/`write()` — never as `Ok`
/// with a bogus buffer.
#[test]
fn failed_completion_surfaces_as_err() {
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
        let config = Config {
            devices: vec![iops_device(8)],
            ring_count: 1,
        };
        let clock = Clock::default();
        let backend = fail_backend(clock.clone(), std::io::ErrorKind::Other);
        let mut spawn = bach_spawner();
        let scheduler = Scheduler::new(&config, &backend, &mut spawn, &Registry::default(), clock);
        let dev = DeviceId(0);

        let h = scheduler.handle();
        let devices = scheduler.devices().clone();
        async move {
            let read = h.read(dev, FD, 0, 4096, TierPriority::Medium).await;
            assert!(read.is_err(), "failed read must surface as Err, got Ok");
            let write = h
                .write(
                    dev,
                    FD,
                    0,
                    bytes::Bytes::from_static(b"abcd"),
                    TierPriority::Medium,
                )
                .await;
            assert!(write.is_err(), "failed write must surface as Err, got Ok");
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
/// forever (an atomic `IoOp` has no partial-submit escape).
#[test]
fn oversize_op_fails_fast() {
    let _no_snap = crate::testing::without_snapshots();
    sim(|| {
        let scheduler = build(vec![iops_device(4)], 1, Latency::default());
        let dev = DeviceId(0);
        let h = scheduler.handle();
        async move {
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
