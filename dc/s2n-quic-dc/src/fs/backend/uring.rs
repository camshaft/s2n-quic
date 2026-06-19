// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! io_uring backend — one ring per lane (Linux only).
//!
//! Each execution lane owns one [`io_uring`](io_uring::IoUring) driven by a dedicated OS thread.
//! The thread drains the lane's submission channel, builds SQEs straight from the op's
//! fields (zero-copy: the SQE points at the op's own buffer), submits, blocks in
//! `submit_and_wait` for completions, reaps CQEs, and pushes the finished ops back through the same
//! `Send` channel → per-lane reaper → `!Send` completion sink bridge the syscall backend uses.
//!
//! Like the syscall backend, the thread (and therefore ring) count is **fixed** (one per lane,
//! created once, never dynamically spawned), and admission is credit-gated upstream — so the ring's
//! in-flight set is bounded by the device pool and the hold-and-wait deadlock cannot form. The ring
//! depth is sized to comfortably exceed the lane's credit-bounded in-flight count.
//!
//! An op handed to a worker is parked in a slab keyed by the SQE's `user_data`; the slab owns the
//! `IoOp` (and thus its heap buffer) for the entire kernel operation, so the buffer the kernel
//! reads/writes stays pinned and alive until its CQE arrives. This is the io_uring memory-safety
//! contract: a submitted buffer must outlive its completion.
//!
//! Direct (`O_DIRECT`) vs. buffered is a per-op / per-file property exactly as in the syscall
//! backend; io_uring issues the same `read`/`write` opcodes either way and the kernel honors the
//! file's open flags.

use crate::{
    fs::{
        backend::{Backend, CompletionSink, LaneSetup, LaneSubmit},
        op::{IoBuf, IoKind, IoOp, IoStatus},
    },
    intrusive::Entry,
    socket::channel::{intrusive::sync as sync_chan, Budget, UnboundedSender},
};
use core::task::Poll;
use io_uring::{opcode, types, IoUring};
use std::{
    collections::VecDeque,
    os::fd::RawFd,
    sync::{Condvar, Mutex},
    thread::JoinHandle,
};

/// Default ring depth (entries per ring). Sized well above any reasonable credit-bounded in-flight
/// count so the SQ never backs up before the credit pool does.
pub const DEFAULT_RING_DEPTH: u32 = 256;

/// A job handed to a ring thread: the op plus the `Send` completion channel back to its reaper.
struct Job {
    op: Entry<IoOp>,
    completion: sync_chan::Sender<IoOp>,
}

/// One ring + its dedicated worker thread, plus the queue feeding it. Each lane gets one.
struct Ring {
    shared: std::sync::Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

struct Shared {
    queue: Mutex<Queue>,
    cond: Condvar,
}

struct Queue {
    jobs: VecDeque<Job>,
    shutdown: bool,
}

impl Ring {
    fn new(depth: u32) -> std::io::Result<Self> {
        let shared = std::sync::Arc::new(Shared {
            queue: Mutex::new(Queue {
                jobs: VecDeque::new(),
                shutdown: false,
            }),
            cond: Condvar::new(),
        });
        // Build the ring on the worker thread (an IoUring is not Send) and signal readiness.
        let thread_shared = shared.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::io::Result<()>>();
        let worker = std::thread::Builder::new()
            .name("s2n-dc-fs-uring".to_string())
            .spawn(move || match IoUring::new(depth) {
                Ok(ring) => {
                    let _ = ready_tx.send(Ok(()));
                    ring_loop(ring, thread_shared);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })?;
        // Propagate a ring-construction failure as the backend's error rather than a dead thread.
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(std::io::Error::other(
                    "uring worker died before signalling readiness",
                ))
            }
        }
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    fn enqueue(&self, job: Job) {
        {
            let mut q = self.shared.queue.lock().unwrap();
            q.jobs.push_back(job);
        }
        self.shared.cond.notify_one();
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        {
            let mut q = self.shared.queue.lock().unwrap();
            q.shutdown = true;
        }
        self.shared.cond.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The ring thread: pull submitted jobs, build SQEs, submit, reap CQEs, forward completions.
///
/// Drains all newly-queued jobs into the SQ (parking each op in the in-flight slab keyed by
/// `user_data`), then submits and waits for at least one completion whenever work is outstanding,
/// reaping every available CQE. Blocks on the condvar only when both the queue and the in-flight set
/// are empty — so it sleeps when idle and never busy-spins.
fn ring_loop(mut ring: IoUring, shared: std::sync::Arc<Shared>) {
    // In-flight ops keyed by user_data; the slab owns each op (and its buffer) until its CQE lands.
    let mut inflight: Vec<Option<Job>> = Vec::with_capacity(DEFAULT_RING_DEPTH as usize);
    let mut free_ids: Vec<usize> = Vec::new();
    let mut outstanding = 0usize;

    loop {
        // 1. Pull queued jobs (block only when nothing is queued AND nothing is in flight).
        {
            let mut q = shared.queue.lock().unwrap();
            loop {
                if !q.jobs.is_empty() {
                    break;
                }
                if q.shutdown && outstanding == 0 {
                    return;
                }
                if outstanding > 0 {
                    // Work is in the kernel; don't block on the condvar — go reap its completions.
                    break;
                }
                q = shared.cond.wait(q).unwrap();
            }
            // Move queued jobs into the SQ, bounded by available ring space.
            while outstanding < DEFAULT_RING_DEPTH as usize {
                let Some(mut job) = q.jobs.pop_front() else { break };
                let id = free_ids.pop().unwrap_or_else(|| {
                    inflight.push(None);
                    inflight.len() - 1
                });
                // Prepare the op's buffer with exclusive access *before* parking it in the slab, then
                // build the SQE from the (now-stable) buffer pointer. The op then moves into the
                // slab and stays put until its CQE, keeping the pointer valid for the kernel.
                prepare_buf(&mut job.op);
                let entry = build_sqe(&job.op, id as u64);
                inflight[id] = Some(job);
                // SAFETY: the op (and its buffer) lives in `inflight[id]` until its CQE is reaped, so
                // the buffer pointer in the SQE stays valid for the kernel operation's duration.
                unsafe {
                    if ring.submission().push(&entry).is_err() {
                        // SQ full despite the bound check — put the job back and stop filling.
                        let job = inflight[id].take().unwrap();
                        free_ids.push(id);
                        q.jobs.push_front(job);
                        break;
                    }
                }
                outstanding += 1;
            }
        }

        if outstanding == 0 {
            continue;
        }

        // 2. Submit and wait for at least one completion.
        match ring.submit_and_wait(1) {
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                // A submit failure is unexpected; fail every outstanding op so submitters don't hang.
                for slot in inflight.iter_mut() {
                    if let Some(mut job) = slot.take() {
                        job.op.status = IoStatus::Failed(std::io::ErrorKind::Other);
                        let mut c = job.completion;
                        let _ = UnboundedSender::send(&mut c, job.op);
                    }
                }
                outstanding = 0;
                free_ids.clear();
                continue;
            }
        }

        // 3. Reap completions.
        let mut completed: Vec<usize> = Vec::new();
        {
            let mut cq = ring.completion();
            cq.sync();
            for cqe in &mut cq {
                let id = cqe.user_data() as usize;
                let Some(mut job) = inflight.get_mut(id).and_then(|s| s.take()) else {
                    continue;
                };
                stamp(&mut job.op, cqe.result());
                let mut c = job.completion;
                let _ = UnboundedSender::send(&mut c, job.op);
                completed.push(id);
            }
        }
        for id in completed {
            free_ids.push(id);
            outstanding -= 1;
        }
    }
}

/// Prepare an op's buffer for submission, with exclusive `&mut` access (called before the op is
/// parked in the slab). For a buffered read the scheduler hands an empty `BytesMut`; size it to the
/// op length here so the SQE points at a valid `op.len`-byte destination. Other variants are already
/// sized by the caller (direct buffer / write source).
fn prepare_buf(op: &mut IoOp) {
    if op.kind == IoKind::Read {
        if let IoBuf::Read(b) = &mut op.buf {
            let cap = op.len as usize;
            b.clear();
            b.resize(cap, 0);
        }
    }
}

/// Build an SQE for `op`, tagged with `user_data` so its CQE can be correlated. Reads the (already
/// prepared) buffer pointer; the op stays in the slab so the pointer is valid until the CQE.
fn build_sqe(op: &IoOp, user_data: u64) -> io_uring::squeue::Entry {
    let fd = types::Fd(op.fd as RawFd);
    match op.kind {
        IoKind::Read => {
            let (ptr, len) = read_ptr(op);
            opcode::Read::new(fd, ptr, len)
                .offset(op.offset)
                .build()
                .user_data(user_data)
        }
        IoKind::Write => {
            let (ptr, len) = write_ptr(op);
            opcode::Write::new(fd, ptr.cast_mut(), len)
                .offset(op.offset)
                .build()
                .user_data(user_data)
        }
        IoKind::Fsync => opcode::Fsync::new(fd).build().user_data(user_data),
        IoKind::Fdatasync => opcode::Fsync::new(fd)
            .flags(types::FsyncFlags::DATASYNC)
            .build()
            .user_data(user_data),
        // No trim opcode wired in v1; submit a no-op fsync that completes immediately and is stamped
        // Done(0) by `stamp` (result 0). Keeps the op moving rather than special-casing the slab.
        IoKind::Trim => opcode::Fsync::new(fd).build().user_data(user_data),
    }
}

/// Read destination pointer/len. The op (and its buffer) lives in the in-flight slab until the CQE,
/// so the pointer is valid for the kernel operation. For a buffered read the buffer was already
/// resized to `op.len` by [`prepare_buf`]; we hand the kernel a `*mut` to those bytes (the kernel is
/// the only writer until completion, so this raw write through is sound).
fn read_ptr(op: &IoOp) -> (*mut u8, u32) {
    match &op.buf {
        IoBuf::Read(b) => (b.as_ptr() as *mut u8, b.len() as u32),
        IoBuf::Direct(b) => (b.as_slice().as_ptr() as *mut u8, b.len() as u32),
        _ => (core::ptr::null_mut(), 0),
    }
}

/// Write source pointer/len.
fn write_ptr(op: &IoOp) -> (*const u8, u32) {
    match &op.buf {
        IoBuf::Write(b) => (b.as_ptr(), b.len() as u32),
        IoBuf::Direct(b) => {
            let s = b.as_slice();
            (s.as_ptr(), s.len() as u32)
        }
        _ => (core::ptr::null(), 0),
    }
}

/// Translate a CQE result into the op's status. io_uring returns `-errno` on failure, else the byte
/// count (or 0 for fsync).
fn stamp(op: &mut Entry<IoOp>, result: i32) {
    if result < 0 {
        let err = std::io::Error::from_raw_os_error(-result);
        op.status = IoStatus::Failed(err.kind());
    } else {
        // For a buffered read, trim the BytesMut to the bytes actually read.
        if op.kind == IoKind::Read {
            if let IoBuf::Read(b) = &mut op.buf {
                b.truncate(result as usize);
            }
        }
        op.status = IoStatus::Done(result as usize);
    }
}

// ── Backend wiring ──────────────────────────────────────────────────────────

/// An io_uring backend: one ring (and dedicated thread) per lane.
pub struct UringBackend {
    depth: u32,
}

impl UringBackend {
    /// Build an io_uring backend with the given ring depth (entries per ring).
    pub fn new(depth: u32) -> Self {
        Self {
            depth: depth.max(8),
        }
    }
}

impl Default for UringBackend {
    fn default() -> Self {
        Self::new(DEFAULT_RING_DEPTH)
    }
}

impl Backend for UringBackend {
    fn spawn_lanes(&self, setup: LaneSetup) -> Vec<LaneSubmit> {
        let mut handles = Vec::with_capacity(setup.lane_count);
        for _ in 0..setup.lane_count {
            // One ring (+ its thread) per lane.
            let ring = std::sync::Arc::new(
                Ring::new(self.depth).expect("failed to create io_uring for lane"),
            );
            // Send channel: ring thread → this lane's reaper (on the scheduler thread).
            let (done_tx, done_rx) = sync_chan::new::<IoOp>();
            let sink = setup.completion.boxed_clone();
            setup.spawn.spawn(reaper(done_rx, sink));
            handles.push(Box::new(RingSubmit { ring, done_tx }) as LaneSubmit);
        }
        handles
    }
}

/// The lane's submit handle: enqueues each op onto its ring's thread.
struct RingSubmit {
    ring: std::sync::Arc<Ring>,
    done_tx: sync_chan::Sender<IoOp>,
}

impl UnboundedSender<Entry<IoOp>> for RingSubmit {
    fn send(&mut self, op: Entry<IoOp>) -> Result<(), Entry<IoOp>> {
        self.ring.enqueue(Job {
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
            config::{BackendKind, Config, CostModel, DeviceConfig, OpWeights, PoolMode},
            device::DeviceId,
            direct::{File, Options},
            scheduler::Scheduler,
            SpawnHandle,
        },
        sched::{CreditConfig, Rate, TierPriority},
    };
    use std::rc::Rc;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("s2n-dc-uring-{tag}-{}-{nanos}", std::process::id()))
    }

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

    /// End-to-end through io_uring: write then read back, verifying bytes.
    #[test]
    fn uring_write_then_read_roundtrip() {
        let path = temp_path("rt");
        let file = File::open(&path, Options { truncate: true, size: 1 << 20, direct: false }).unwrap();
        let fd = file.raw_fd();
        run_local(|spawn| {
            let path = path.clone();
            async move {
                let config = Config {
                    devices: vec![byte_device(1 << 20)],
                    ring_count: 1,
                    backend: BackendKind::Uring,
                };
                let scheduler = Scheduler::new(&config, &UringBackend::default(), spawn, clock());
                let h = scheduler.handle();
                let dev = DeviceId(0);

                let payload = bytes::Bytes::from_static(b"io_uring round trip payload");
                let n = h.write(dev, fd, 0, payload.clone(), TierPriority::Medium).await.unwrap();
                assert_eq!(n, payload.len());

                let buf = h.read(dev, fd, 0, payload.len() as u32, TierPriority::High).await.unwrap();
                assert_eq!(&buf[..], &payload[..]);

                drop(file);
                let _ = std::fs::remove_file(&path);
            }
        });
    }

    /// High concurrency through one ring: admission is credit-bounded, the ring drains it all, every
    /// op completes, data is correct, credit conserves.
    #[test]
    fn uring_high_concurrency_conserves() {
        let path = temp_path("stress");
        let file = File::open(&path, Options { truncate: true, size: 1 << 20, direct: false }).unwrap();
        let fd = file.raw_fd();
        run_local(|spawn| {
            let path = path.clone();
            async move {
                let config = Config {
                    devices: vec![byte_device(4096)],
                    ring_count: 1,
                    backend: BackendKind::Uring,
                };
                let scheduler = Scheduler::new(&config, &UringBackend::default(), spawn, clock());
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
                            if h.write(dev, fd, off, data, TierPriority::Medium).await.is_ok() {
                                completed.set(completed.get() + 1);
                            }
                        }
                    }));
                }
                for t in tasks {
                    t.await.unwrap();
                }
                assert_eq!(completed.get(), 32 * 4, "every op must complete through the ring");
                let device = scheduler.devices().get(dev).unwrap();
                for pool in device.pools.all() {
                    assert_eq!(
                        pool.debug_free_total(),
                        pool.debug_capacity() as i64,
                        "credit leaked through io_uring backend"
                    );
                }
                drop(file);
                let _ = std::fs::remove_file(&path);
            }
        });
    }

    /// A read on a bad fd surfaces as Err (io_uring returns -EBADF in the CQE), not a hang.
    #[test]
    fn uring_bad_fd_fails() {
        run_local(|spawn| async move {
            let config = Config {
                devices: vec![byte_device(1 << 16)],
                ring_count: 1,
                backend: BackendKind::Uring,
            };
            let scheduler = Scheduler::new(&config, &UringBackend::default(), spawn, clock());
            let dev = DeviceId(0);
            let h = scheduler.handle();
            let result = h.read(dev, -1, 0, 4096, TierPriority::Medium).await;
            assert!(result.is_err(), "bad fd read must fail, not hang");
            let device = scheduler.devices().get(dev).unwrap();
            for pool in device.pools.all() {
                assert_eq!(pool.debug_free_total(), pool.debug_capacity() as i64);
            }
        });
    }
}
