// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! `fs-tester` — a configurable stress tester for the s2n-quic-dc storage IO scheduler
//! ([`s2n_quic_dc::fs`]).
//!
//! It drives a chosen backend (bounded blocking syscall pool, or io_uring on Linux) with a workload
//! described by a TOML config (or CLI overrides): read/write mix, op size, device queue depth,
//! concurrency, buffered vs. `O_DIRECT`. It reports achieved IOPS, throughput, and per-op latency
//! percentiles so we can see how the scheduler behaves under different shapes — the analog of
//! `dc-tester` for storage IO.
//!
//! Example:
//!   fs-tester --backend uring --direct --op-size 4096 --streams 64 --queue-depth 256 --duration 10
//!   fs-tester --config workload.toml

mod config;

use clap::Parser;
use config::{Backend, Config, OpMix};
use s2n_quic_dc::{
    busy_poll::clock::Clock,
    fs::{
        backend::{blocking::Runtime, syscall::SyscallBackend},
        config::{Config as FsConfig, CostModel, DeviceConfig, OpWeights, PoolMode},
        device::Device,
        direct::{File, Options, ALIGNMENT},
        scheduler::Scheduler,
        SubmitHandle,
    },
    runtime::tokio::Local,
    sched::{CreditConfig, Rate, TierPriority as Priority},
};
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};

#[derive(Parser, Debug)]
#[command(about = "Stress tester for the s2n-quic-dc storage IO scheduler")]
struct Args {
    /// Path to a TOML workload config. CLI flags below override individual fields.
    #[arg(long)]
    config: Option<String>,
    #[arg(long)]
    backend: Option<String>,
    #[arg(long)]
    lanes: Option<usize>,
    #[arg(long)]
    direct: bool,
    #[arg(long)]
    queue_depth: Option<u64>,
    #[arg(long)]
    streams: Option<usize>,
    #[arg(long)]
    op_size: Option<usize>,
    #[arg(long)]
    op_mix: Option<String>,
    #[arg(long)]
    duration: Option<u64>,
    #[arg(long)]
    file: Option<String>,
}

fn main() {
    let args = Args::parse();
    let mut cfg = match &args.config {
        Some(path) => Config::from_toml(path).expect("failed to load config"),
        None => Config::default(),
    };
    // Apply CLI overrides.
    if let Some(b) = &args.backend {
        cfg.backend = match b.as_str() {
            "syscall" => Backend::Syscall,
            "uring" => Backend::Uring,
            other => panic!("unknown backend {other:?} (expected syscall|uring)"),
        };
    }
    if let Some(v) = args.lanes {
        cfg.lanes = v;
    }
    if args.direct {
        cfg.direct = true;
    }
    if let Some(v) = args.queue_depth {
        cfg.queue_depth = v;
    }
    if let Some(v) = args.streams {
        cfg.streams = v;
    }
    if let Some(v) = args.op_size {
        cfg.op_size = v;
    }
    if let Some(m) = &args.op_mix {
        cfg.op_mix = match m.as_str() {
            "read" => OpMix::Read,
            "write" => OpMix::Write,
            "mixed" => OpMix::Mixed,
            other => panic!("unknown op-mix {other:?} (expected read|write|mixed)"),
        };
    }
    if let Some(v) = args.duration {
        cfg.duration_secs = v;
    }
    if args.file.is_some() {
        cfg.file = args.file.clone();
    }

    if cfg.direct && cfg.op_size % ALIGNMENT != 0 {
        panic!("direct mode requires op_size to be a multiple of {ALIGNMENT}");
    }

    println!("config: {cfg:?}");
    run(cfg);
}

fn run(cfg: Config) {
    // The fs scheduler pipeline is !Send/single-worker; drive it on a tokio current-thread LocalSet.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move { run_workload(cfg).await });
}

async fn run_workload(cfg: Config) {
    // Test file.
    let path = cfg.file.clone().unwrap_or_else(|| {
        format!(
            "{}/fs-tester-{}.dat",
            std::env::temp_dir().display(),
            std::process::id()
        )
    });
    let file = Arc::new(
        File::open(
            &path,
            Options {
                truncate: true,
                size: cfg.file_size,
                direct: cfg.direct,
            },
        )
        .expect("open test file"),
    );
    // Carry the file as an `Fd` (Arc-backed) so every in-flight op pins it open — no UAF if the
    // `file` handle is dropped before the op completes.
    let fd = s2n_quic_dc::fs::op::Fd::new(file.clone());

    // One device sized in bytes (bandwidth-cost model), queue depth = capacity. Registered after the
    // scheduler is built (the new lazy-registration API), which hands back the `Arc<Device>` handle.
    let device_cfg = DeviceConfig {
        pool_mode: PoolMode::Shared(
            CreditConfig::new(
                cfg.queue_depth
                    .saturating_mul(cfg.op_size as u64)
                    .max(cfg.op_size as u64),
            )
            .with_max_single_acquire_uniform(cfg.op_size as u64)
            .without_refill(),
        ),
        rate: Rate::new(1_000_000.0), // effectively unlimited; the pool (queue depth) governs
        cost_model: CostModel::Bytes,
        op_weights: OpWeights::default(),
    };
    let fs_config = FsConfig {
        ring_count: cfg.lanes.max(1),
    };

    let mut spawn = Local::new(0);
    let registry = s2n_quic_dc::counter::Registry::default();
    let clock = Clock::default();

    // Build the scheduler with the chosen backend (over a blocking runtime), then register the device.
    let scheduler = match cfg.backend {
        Backend::Syscall => {
            let backend = SyscallBackend::new(Runtime::new("fs-tester-io"));
            Scheduler::new(&fs_config, &backend, &mut spawn, &registry, clock)
        }
        Backend::Uring => build_uring(&fs_config, &mut spawn, &registry, clock, cfg.ring_depth),
    };
    let dev = scheduler
        .register_device("dev0", &device_cfg)
        .expect("register device");

    // Shared run state.
    let stop = Rc::new(AtomicBool::new(false));
    let counters: Rc<RunCounters> = Rc::new(RunCounters::default());

    // Spawn submitter streams.
    let blocks_per_stream = (cfg.file_size / cfg.op_size as u64).max(1);
    let mut tasks = Vec::with_capacity(cfg.streams);
    for s in 0..cfg.streams {
        let h = scheduler.handle();
        let dev = dev.clone();
        let fd = fd.clone();
        let stop = stop.clone();
        let counters = counters.clone();
        let op_size = cfg.op_size;
        let op_mix = cfg.op_mix;
        let direct = cfg.direct;
        tasks.push(tokio::task::spawn_local(async move {
            let mut i = 0u64;
            // Each stream walks a disjoint offset region to avoid trivial cache effects.
            let base = (s as u64) * blocks_per_stream / 8;
            while !stop.load(Ordering::Relaxed) {
                let block = (base + i) % blocks_per_stream;
                let offset = block * op_size as u64;
                let is_read = match op_mix {
                    OpMix::Read => true,
                    OpMix::Write => false,
                    OpMix::Mixed => i.is_multiple_of(2),
                };
                let start = Instant::now();
                let ok = run_one(&h, &dev, &fd, offset, op_size, is_read, direct).await;
                let elapsed_us = start.elapsed().as_micros() as u64;
                counters.record(ok, op_size as u64, elapsed_us);
                i += 1;
            }
        }));
    }

    // Run for the configured duration, then signal stop and drain.
    let wall_start = Instant::now();
    tokio::time::sleep(std::time::Duration::from_secs(cfg.duration_secs)).await;
    stop.store(true, Ordering::Relaxed);
    for t in tasks {
        let _ = t.await;
    }
    let elapsed = wall_start.elapsed();

    counters.report(elapsed);
    drop(file);
    let _ = std::fs::remove_file(&path);
}

/// Issue a single op of the requested kind, returning whether it succeeded.
async fn run_one(
    h: &SubmitHandle,
    dev: &Arc<Device>,
    fd: &s2n_quic_dc::fs::op::Fd,
    offset: u64,
    op_size: usize,
    is_read: bool,
    direct: bool,
) -> bool {
    if direct {
        use s2n_quic_dc::fs::direct::AlignedBuf;
        if is_read {
            let buf = AlignedBuf::new(op_size);
            h.read_direct(dev.clone(), fd.clone(), offset, buf, Priority::Medium)
                .await
                .is_ok()
        } else {
            let buf = AlignedBuf::new(op_size);
            h.write_direct(dev.clone(), fd.clone(), offset, buf, Priority::Medium)
                .await
                .is_ok()
        }
    } else if is_read {
        h.read(dev.clone(), fd.clone(), offset, op_size as u32, Priority::Medium)
            .await
            .is_ok()
    } else {
        let data = bytes::Bytes::from(vec![0u8; op_size]);
        h.write(dev.clone(), fd.clone(), offset, data, Priority::Medium)
            .await
            .is_ok()
    }
}

#[cfg(target_os = "linux")]
fn build_uring(
    fs_config: &FsConfig,
    spawn: &mut Local,
    registry: &s2n_quic_dc::counter::Registry,
    clock: Clock,
    ring_depth: u32,
) -> Scheduler {
    use s2n_quic_dc::fs::backend::uring::{self, UringBackend};
    // Detect whether io_uring is actually usable here (kernel too old, the `io_uring_disabled`
    // sysctl, a seccomp filter, or a low memlock rlimit). If not, fall back to the blocking syscall
    // pool rather than panicking — and report why, so a misconfigured host is obvious.
    if let Err(e) = uring::probe() {
        eprintln!("io_uring unavailable ({e}); falling back to the blocking syscall backend");
        let backend = SyscallBackend::new(Runtime::new("fs-tester-io"));
        return Scheduler::new(fs_config, &backend, spawn, registry, clock);
    }
    let backend = UringBackend::new(Runtime::new("fs-tester-uring"), ring_depth);
    Scheduler::new(fs_config, &backend, spawn, registry, clock)
}

#[cfg(not(target_os = "linux"))]
fn build_uring(
    _: &FsConfig,
    _: &mut Local,
    _: &s2n_quic_dc::counter::Registry,
    _: Clock,
    _: u32,
) -> Scheduler {
    panic!("the uring backend is only available on Linux");
}

/// Run counters: op count, bytes, errors, and a latency histogram (microseconds).
struct RunCounters {
    ops: Cell<u64>,
    bytes: Cell<u64>,
    errors: Cell<u64>,
    hist: std::cell::RefCell<hdrhistogram::Histogram<u64>>,
}

impl Default for RunCounters {
    fn default() -> Self {
        Self {
            ops: Cell::new(0),
            bytes: Cell::new(0),
            errors: Cell::new(0),
            // 1us..60s, 3 sig figs.
            hist: std::cell::RefCell::new(
                hdrhistogram::Histogram::new_with_bounds(1, 60_000_000, 3).unwrap(),
            ),
        }
    }
}

impl RunCounters {
    fn record(&self, ok: bool, bytes: u64, elapsed_us: u64) {
        self.ops.set(self.ops.get() + 1);
        if ok {
            self.bytes.set(self.bytes.get() + bytes);
        } else {
            self.errors.set(self.errors.get() + 1);
        }
        let _ = self.hist.borrow_mut().record(elapsed_us.max(1));
    }

    fn report(&self, elapsed: std::time::Duration) {
        let secs = elapsed.as_secs_f64().max(1e-9);
        let ops = self.ops.get();
        let bytes = self.bytes.get();
        let h = self.hist.borrow();
        let iops = ops as f64 / secs;
        let gbps = (bytes as f64 * 8.0) / secs / 1e9;
        let mibs = (bytes as f64) / secs / (1024.0 * 1024.0);
        println!("──────────────────────────────────────────────");
        println!("elapsed         {:.2} s", secs);
        println!("ops             {ops}  ({} errors)", self.errors.get());
        println!("IOPS            {iops:.0}");
        println!("throughput      {gbps:.2} Gbps  ({mibs:.0} MiB/s)");
        println!(
            "latency (us)    p50={} p90={} p99={} p999={} max={}",
            h.value_at_quantile(0.50),
            h.value_at_quantile(0.90),
            h.value_at_quantile(0.99),
            h.value_at_quantile(0.999),
            h.max(),
        );
        println!("──────────────────────────────────────────────");
    }
}
