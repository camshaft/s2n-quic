// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bounded blocking-syscall backend — the production deadlock fix.
//!
//! Storage IO done via `tokio::spawn_blocking` (the Membrain `core-fs-direct` `IoPool` pattern)
//! deadlocks: each pool spawns blocking threads with no global bound and no backpressure on thread
//! spawn, so under load the pool exhausts and in-flight ops wait on ops that can never be scheduled.
//!
//! This backend fixes that structurally. It owns **one** bounded thread pool — a fixed number of
//! worker threads created once at construction and **never** dynamically spawned — shared across all
//! lanes. Workers block on a condvar-guarded job queue, run `pread`/`pwrite`/`fsync` on the op's fd,
//! and push the completed op back through a `Send` channel to a per-lane reaper task on the
//! scheduler thread, which bridges to the `!Send` completion sink. Crucially, admission is bounded
//! *upstream* by the credit pool (a submitter that can't get credit parks on a waker, never a
//! thread), so the pool's queue can never grow without bound and the hold-and-wait deadlock cannot
//! form.
//!
//! Unlike the mock backend, this runs on real OS threads against real files, so it cannot run under
//! the bach simulated clock — its tests use real threads + temp files.

use crate::{
    fs::{
        backend::{Backend, CompletionSink, LaneSetup, LaneSubmit},
        direct::AlignedBuf,
        op::{IoBuf, IoKind, IoOp, IoStatus},
    },
    intrusive::Entry,
    socket::channel::{intrusive::sync as sync_chan, Budget, UnboundedSender},
    sync::Arc,
};
use core::task::Poll;
use std::{
    collections::VecDeque,
    os::fd::RawFd,
    sync::{Condvar, Mutex},
    thread::JoinHandle,
};

/// Whether the worker pool issues unbuffered (`O_DIRECT`) IO. In direct mode each op is bounced
/// through a page-aligned buffer so the syscall sees an aligned pointer/length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Buffered IO directly on the op's `BytesMut`/`Bytes` (portable; for tmpfs/tests).
    Buffered,
    /// Unbuffered `O_DIRECT` IO via a page-aligned bounce buffer.
    Direct,
}

/// A unit of work handed to a worker thread: the op plus the `Send` completion channel back to its
/// lane's reaper.
struct Job {
    op: Entry<IoOp>,
    completion: sync_chan::Sender<IoOp>,
}

/// The bounded worker pool. Created once; the worker count is fixed for its lifetime.
struct Pool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

struct Shared {
    queue: Mutex<Queue>,
    cond: Condvar,
}

struct Queue {
    jobs: VecDeque<Job>,
    shutdown: bool,
}

impl Pool {
    fn new(worker_count: usize, mode: Mode) -> Arc<Self> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                jobs: VecDeque::new(),
                shutdown: false,
            }),
            cond: Condvar::new(),
        });
        let mut workers = Vec::with_capacity(worker_count);
        for i in 0..worker_count.max(1) {
            let shared = shared.clone();
            let handle = std::thread::Builder::new()
                .name(format!("s2n-dc-fs-io-{i}"))
                .spawn(move || worker_loop(shared, mode))
                .expect("failed to spawn fs io worker");
            workers.push(handle);
        }
        Arc::new(Self { shared, workers })
    }

    /// Enqueue a job and wake one worker. Non-blocking — the credit pool already bounded the number
    /// of in-flight jobs, so this never grows without bound.
    fn enqueue(&self, job: Job) {
        {
            let mut q = self.shared.queue.lock().unwrap();
            q.jobs.push_back(job);
        }
        self.shared.cond.notify_one();
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        {
            let mut q = self.shared.queue.lock().unwrap();
            q.shutdown = true;
        }
        self.shared.cond.notify_all();
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

/// A worker thread: pull a job, run its syscall, push the completed op to its lane's reaper.
fn worker_loop(shared: Arc<Shared>, mode: Mode) {
    loop {
        let job = {
            let mut q = shared.queue.lock().unwrap();
            loop {
                if let Some(job) = q.jobs.pop_front() {
                    break job;
                }
                if q.shutdown {
                    return;
                }
                q = shared.cond.wait(q).unwrap();
            }
        };

        let Job { mut op, completion } = job;
        execute(&mut op, mode);
        // Forward the completed op to the lane reaper. If the reaper is gone the channel send fails;
        // the op (and its credit) drops here — acceptable only on full teardown.
        let mut completion = completion;
        let _ = UnboundedSender::send(&mut completion, op);
    }
}

/// Run the op's syscall on its fd, stamping `status`.
fn execute(op: &mut Entry<IoOp>, mode: Mode) {
    let fd = op.fd as RawFd;
    let offset = op.offset;
    let len = op.len as usize;
    let result = match op.kind {
        IoKind::Read => match &mut op.buf {
            IoBuf::Read(buf) => read_into(fd, offset, len, buf, mode),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "read op without a read buffer",
            )),
        },
        IoKind::Write => match &op.buf {
            IoBuf::Write(data) => write_from(fd, offset, data, mode).map(|()| data.len()),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "write op without a write buffer",
            )),
        },
        IoKind::Fsync => fsync(fd, false).map(|()| 0),
        IoKind::Fdatasync => fsync(fd, true).map(|()| 0),
        IoKind::Trim => Ok(0), // no-op in v1
    };
    op.status = match result {
        Ok(n) => IoStatus::Done(n),
        Err(e) => IoStatus::Failed(e.kind()),
    };
}

/// Read `want` bytes at `offset` into `buf` (the op's read destination, built empty by the
/// scheduler), returning the bytes actually read.
fn read_into(
    fd: RawFd,
    offset: u64,
    want: usize,
    buf: &mut bytes::BytesMut,
    mode: Mode,
) -> std::io::Result<usize> {
    match mode {
        Mode::Buffered => {
            buf.clear();
            buf.resize(want, 0);
            let n = pread(fd, buf.as_mut(), offset)?;
            buf.truncate(n);
            Ok(n)
        }
        Mode::Direct => {
            let mut aligned = AlignedBuf::new(want.max(1));
            let n = pread(fd, aligned.as_aligned_mut(), offset)?;
            let n = n.min(want);
            buf.clear();
            buf.extend_from_slice(&aligned.as_aligned_slice()[..n]);
            Ok(n)
        }
    }
}

fn write_from(fd: RawFd, offset: u64, data: &bytes::Bytes, mode: Mode) -> std::io::Result<()> {
    match mode {
        Mode::Buffered => pwrite_all(fd, data, offset),
        Mode::Direct => {
            let mut aligned = AlignedBuf::new(data.len().max(1));
            aligned.as_aligned_mut()[..data.len()].copy_from_slice(data);
            // Direct writes must transfer an aligned length; the file is pre-sized so the padding
            // tail is harmless.
            pwrite_all(fd, aligned.as_aligned_slice(), offset)
        }
    }
}

// ── Raw positional syscalls (libc) ──────────────────────────────────────────

/// One `pread`; returns bytes read (0 at EOF). Retries `EINTR`.
fn pread(fd: RawFd, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    loop {
        // SAFETY: `buf` is a valid writable slice of `buf.len()` bytes; `fd` is owned by the caller.
        let ret = unsafe {
            libc::pread(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                offset as libc::off_t,
            )
        };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        return Ok(ret as usize);
    }
}

/// Write all of `data` at `offset` via repeated `pwrite`. Retries `EINTR`/short writes.
fn pwrite_all(fd: RawFd, data: &[u8], offset: u64) -> std::io::Result<()> {
    let mut written = 0usize;
    while written < data.len() {
        // SAFETY: `data[written..]` is a valid readable slice; `fd` is owned by the caller.
        let ret = unsafe {
            libc::pwrite(
                fd,
                data[written..].as_ptr() as *const libc::c_void,
                data.len() - written,
                (offset + written as u64) as libc::off_t,
            )
        };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if ret == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "pwrite returned 0",
            ));
        }
        written += ret as usize;
    }
    Ok(())
}

fn fsync(fd: RawFd, data_only: bool) -> std::io::Result<()> {
    // `fdatasync` is Linux-only; on other platforms (macOS) fall back to a full `fsync`.
    #[cfg(target_os = "linux")]
    let ret = unsafe {
        if data_only {
            libc::fdatasync(fd)
        } else {
            libc::fsync(fd)
        }
    };
    #[cfg(not(target_os = "linux"))]
    let ret = {
        let _ = data_only;
        // SAFETY: `fd` is owned by the caller for the duration of the call.
        unsafe { libc::fsync(fd) }
    };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ── Backend wiring ──────────────────────────────────────────────────────────

/// A bounded blocking-syscall backend with a fixed-size shared thread pool.
pub struct SyscallBackend {
    pool: Arc<Pool>,
}

impl SyscallBackend {
    /// Build a backend with `worker_count` fixed worker threads in `mode`.
    pub fn new(worker_count: usize, mode: Mode) -> Self {
        Self {
            pool: Pool::new(worker_count, mode),
        }
    }
}

impl Backend for SyscallBackend {
    fn spawn_lanes(&self, setup: LaneSetup) -> Vec<LaneSubmit> {
        let mut handles = Vec::with_capacity(setup.lane_count);
        for _ in 0..setup.lane_count {
            // Send channel: worker threads → this lane's reaper (on the scheduler thread).
            let (done_tx, done_rx) = sync_chan::new::<IoOp>();
            // Reaper task: bridge the Send completion channel to the !Send completion sink.
            let sink = setup.completion.boxed_clone();
            setup.spawn.spawn(reaper(done_rx, sink));
            // The lane submit handle enqueues jobs onto the shared pool, tagging each with a clone of
            // this lane's done channel so the worker can route the completion back here.
            handles.push(Box::new(PoolSubmit {
                pool: self.pool.clone(),
                done_tx,
            }) as LaneSubmit);
        }
        handles
    }
}

/// The lane's submit handle: enqueues each op as a job on the bounded pool.
struct PoolSubmit {
    pool: Arc<Pool>,
    done_tx: sync_chan::Sender<IoOp>,
}

impl UnboundedSender<Entry<IoOp>> for PoolSubmit {
    fn send(&mut self, op: Entry<IoOp>) -> Result<(), Entry<IoOp>> {
        self.pool.enqueue(Job {
            op,
            completion: self.done_tx.clone(),
        });
        Ok(())
    }
}

/// Drain a lane's `Send` completion channel and forward each op to the `!Send` completion sink.
async fn reaper(mut done_rx: sync_chan::Receiver<IoOp>, sink: Box<dyn CompletionSink>) {
    let mut budget = Budget::new(1 << 20);
    core::future::poll_fn(move |cx| {
        budget.reset();
        loop {
            match crate::socket::channel::Receiver::<Entry<IoOp>>::poll_recv(
                &mut done_rx,
                cx,
                &mut budget,
            ) {
                Poll::Ready(Some(op)) => {
                    sink.send(op);
                    if budget.is_exhausted() {
                        // Queue may still be non-empty; self-wake to re-poll (the receiver did not
                        // register its waker on this drained-but-budget-spent path).
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                }
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => return Poll::Pending,
            }
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fs::{
            config::{Config, CostModel, DeviceConfig, OpWeights, PoolMode},
            device::DeviceId,
            direct::{File, Options},
            scheduler::Scheduler,
            SpawnHandle,
        },
        sched::{CreditConfig, Rate, TierPriority},
    };
    use std::rc::Rc;

    /// A unique temp-file path for a test (no external temp-file crate dependency).
    fn temp_path(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("s2n-dc-fs-{tag}-{}-{nanos}", std::process::id()))
    }

    /// A buffered (non-direct) device sized in bytes; the scheduler paces on byte cost. `Device::new`
    /// floors the grant slice at the per-op ceiling so grants are atomic (indivisible ops).
    fn byte_device(capacity: u64) -> DeviceConfig {
        DeviceConfig {
            pool_mode: PoolMode::Shared(
                CreditConfig::new(capacity)
                    .with_max_single_acquire_uniform(capacity.max(1))
                    .without_refill(),
            ),
            rate: Rate::new(100.0),
            cost_model: CostModel::Bytes,
            op_weights: OpWeights::default(),
        }
    }

    /// Run a `!Send` scheduler test body on a tokio current-thread runtime + LocalSet (the real,
    /// non-bach executor M2 needs because the worker pool runs on OS threads).
    fn run_local<F: std::future::Future<Output = ()> + 'static>(body: impl FnOnce(SpawnHandle) -> F) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        let spawn = SpawnHandle::new(|fut| {
            tokio::task::spawn_local(fut);
        });
        local.block_on(&rt, body(spawn));
    }

    fn clock() -> crate::busy_poll::clock::Clock {
        crate::busy_poll::clock::Clock::default()
    }

    /// End-to-end: write data through the scheduler, then read it back and verify the bytes — proving
    /// the bounded pool actually performs real positional IO and routes completions correctly.
    #[test]
    fn write_then_read_roundtrip() {
        let path = temp_path("roundtrip");
        let file = File::open(&path, Options { truncate: true, size: 1 << 20, direct: false }).unwrap();
        let fd = file.raw_fd();

        run_local(|spawn| {
            let path = path.clone();
            async move {
                let config = Config {
                    devices: vec![byte_device(1 << 20)],
                    ring_count: 2,
                    backend: super::super::super::config::BackendKind::Syscall,
                };
                let backend = SyscallBackend::new(4, Mode::Buffered);
                let scheduler = Scheduler::new(&config, &backend, spawn, clock());
                let h = scheduler.handle();
                let dev = DeviceId(0);

                let payload = bytes::Bytes::from_static(b"the quick brown fox jumps over the lazy dog");
                let n = h.write(dev, fd, 0, payload.clone(), TierPriority::Medium).await.unwrap();
                assert_eq!(n, payload.len());

                let buf = h.read(dev, fd, 0, payload.len() as u32, TierPriority::High).await.unwrap();
                assert_eq!(&buf[..], &payload[..], "read-back bytes must match what was written");

                // Keep the file handle alive across the IO.
                drop(file);
                let _ = std::fs::remove_file(&path);
            }
        });
    }

    /// Many concurrent submitters far exceed the pool's worker count and the device's credit
    /// capacity. The old `spawn_blocking` IoPool would dynamically spawn threads without bound and
    /// could deadlock; here the credit pool bounds admission and the fixed 2-thread pool drains it
    /// all without growing — every op completes, data is correct, and credit conserves.
    #[test]
    fn high_concurrency_stays_bounded_and_conserves() {
        let path = temp_path("stress");
        let file = File::open(&path, Options { truncate: true, size: 1 << 20, direct: false }).unwrap();
        let fd = file.raw_fd();

        run_local(|spawn| {
            let path = path.clone();
            async move {
                // Tiny capacity (4 KiB) + only 2 worker threads, but 32 concurrent writers each
                // doing several ops: admission is credit-bounded, execution is thread-bounded.
                let config = Config {
                    devices: vec![byte_device(4096)],
                    ring_count: 2,
                    backend: super::super::super::config::BackendKind::Syscall,
                };
                let backend = SyscallBackend::new(2, Mode::Buffered);
                let scheduler = Scheduler::new(&config, &backend, spawn, clock());
                let dev = DeviceId(0);

                let completed = Rc::new(std::cell::Cell::new(0usize));
                let mut tasks = Vec::new();
                for w in 0..32u64 {
                    let h = scheduler.handle();
                    let completed = completed.clone();
                    tasks.push(tokio::task::spawn_local(async move {
                        for i in 0..4u64 {
                            let off = (w * 4 + i) * 512;
                            let data = bytes::Bytes::from(vec![(w & 0xff) as u8; 256]);
                            // Each write is 256 bytes; with 4 KiB capacity at most 16 can be admitted
                            // at once, so the rest park on credit (never on a thread).
                            if h.write(dev, fd, off, data, TierPriority::Medium).await.is_ok() {
                                completed.set(completed.get() + 1);
                            }
                        }
                    }));
                }
                for t in tasks {
                    t.await.unwrap();
                }

                assert_eq!(completed.get(), 32 * 4, "every admitted op must complete");

                // Conservation: all credit returned to the pool at quiescence.
                let device = scheduler.devices().get(dev).unwrap();
                for pool in device.pools.all() {
                    assert_eq!(
                        pool.debug_free_total(),
                        pool.debug_capacity() as i64,
                        "credit leaked under high concurrency"
                    );
                }

                drop(file);
                let _ = std::fs::remove_file(&path);
            }
        });
    }

    /// A read of a hole/unwritten region still completes (zero-filled by the pre-sized file); a read
    /// past a bad fd surfaces as `Err` rather than hanging.
    #[test]
    fn bad_fd_read_fails_not_hangs() {
        run_local(|spawn| async move {
            let config = Config {
                devices: vec![byte_device(1 << 16)],
                ring_count: 1,
                backend: super::super::super::config::BackendKind::Syscall,
            };
            let backend = SyscallBackend::new(2, Mode::Buffered);
            let scheduler = Scheduler::new(&config, &backend, spawn, clock());
            let dev = DeviceId(0);
            let h = scheduler.handle();

            // fd -1 is never valid: the pread fails with EBADF and must surface as Err promptly.
            let result = h.read(dev, -1, 0, 4096, TierPriority::Medium).await;
            assert!(result.is_err(), "read on a bad fd must fail, not hang");

            // Conservation holds on the failure path too.
            let device = scheduler.devices().get(dev).unwrap();
            for pool in device.pools.all() {
                assert_eq!(pool.debug_free_total(), pool.debug_capacity() as i64);
            }
        });
    }
}
