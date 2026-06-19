// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bounded blocking-syscall backend — the production deadlock fix.
//!
//! Storage IO done via `tokio::spawn_blocking` (the Membrain `core-fs-direct` `IoPool` pattern)
//! deadlocks: each pool spawns blocking threads with no global bound and no backpressure on thread
//! spawn, so under load the pool exhausts and in-flight ops wait on ops that can never be scheduled.
//!
//! This backend fixes that structurally. It owns a **fixed** set of worker threads — one per
//! execution lane, created once at construction and **never** dynamically spawned. Each worker owns
//! its lane's [`sync`](crate::socket::channel::intrusive::sync) submission-channel receiver and
//! drives it with a thread-park waker ([`blocking::run`]): it runs `pread`/`pwrite`/`fsync` on each
//! op's fd, then forwards the completed op through a shared `Send` channel to a single reaper task on
//! the scheduler thread, which bridges to the `!Send` completion sink. Per-lane channels let the
//! dispatch EDT pick-two balance work across lanes. Crucially, admission is bounded *upstream* by the
//! credit pool (a submitter that can't get credit parks on a waker, never a thread), so a lane's
//! queue can never grow without bound and the hold-and-wait deadlock cannot form.
//!
//! Reusing the ordinary `sync` channel (rather than a bespoke `Mutex+Condvar` queue) means the same
//! channel and the same `Waker` wake-path serve both the async pipeline and these blocking threads —
//! a producer's plain `waker.wake()` unparks a blocked worker with no special-casing at the send
//! site. The thread-park driver lives in [`blocking`](super::blocking).
//!
//! Unlike the bach backend, this runs on real OS threads against real files, so it cannot run under
//! the bach simulated clock — its tests use real threads + temp files.

use crate::{
    fs::{
        backend::{
            blocking, {Backend, LaneSetup},
        },
        combinator::CompletionSink,
        op::{IoBuf, IoKind, IoOp, IoStatus},
    },
    intrusive::Entry,
    runtime::Spawner,
    socket::channel::{intrusive::sync as sync_chan, Map, ReceiverExt as _, UnboundedSender},
    sync::Arc,
};
use std::{os::fd::RawFd, thread::JoinHandle};

/// One execution lane: a `sync` submission channel feeding a dedicated blocking worker thread that
/// `pread`/`pwrite`s each op and forwards the completed op to the lane's reaper. The worker owns the
/// channel's `Receiver` and drives it with a thread-park waker ([`blocking::run`]) — the same channel
/// and `Waker` mechanism the async pipeline uses, just consumed on a thread that can block on a
/// syscall. Dropping the lane's `Sender` (scheduler teardown) closes the channel, so `run` returns
/// and the worker exits; the handle then joins it.
struct Worker {
    handle: Option<JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn one lane: a `sync` channel + a worker thread draining it via [`blocking::run`], completing
/// each op onto `done_tx` (this lane's reaper bridge to the `!Send` completion sink). Returns the
/// lane's submission `Sender` and the worker handle (which joins the thread on drop).
fn spawn_worker(idx: usize, done_tx: sync_chan::Sender<IoOp>) -> (sync_chan::Sender<IoOp>, Worker) {
    let (tx, rx) = sync_chan::new::<IoOp>();
    let handle = std::thread::Builder::new()
        .name(format!("s2n-dc-fs-io-{idx}"))
        .spawn(move || {
            blocking::run(rx, |mut op: Entry<IoOp>| {
                execute(&mut op);
                // Forward the completed op to the reaper. If the reaper is gone the send fails and
                // the op (and its credit) drops here — acceptable only on full teardown. `send_entry`
                // takes `&self`, so no per-op clone of the sender.
                let _ = done_tx.send_entry(op);
            });
        })
        .expect("failed to spawn fs io worker");
    (
        tx,
        Worker {
            handle: Some(handle),
        },
    )
}

/// Run the op's syscall on its fd, stamping `status`.
///
/// **Zero-copy:** the op operates directly on its own buffer — a buffered read fills the op's
/// `BytesMut` in place, a buffered write `pwrite`s from the op's `Bytes` in place, and a `Direct`
/// op reads into / writes from the caller's page-aligned [`AlignedBuf`] in place. There is no
/// bounce-buffer copy on any path. (`O_DIRECT` alignment of the offset is validated at submit time;
/// the buffer pointer/length are aligned by `AlignedBuf` construction.)
fn execute(op: &mut Entry<IoOp>) {
    let fd = op.fd as RawFd;
    let offset = op.offset;
    let want = op.len as usize;
    let result = match op.kind {
        IoKind::Read => match &mut op.buf {
            IoBuf::Read(buf) => {
                // Buffered read: grow the scheduler-allocated dst to the op length and read in place.
                buf.clear();
                buf.resize(want, 0);
                let r = pread(fd, buf.as_mut(), offset);
                if let Ok(n) = r {
                    buf.truncate(n);
                }
                r
            }
            IoBuf::Direct(buf) => {
                // Zero-copy direct read: read straight into the caller's aligned buffer, then set the
                // logical length to the bytes actually read so a short read at EOF exposes no
                // stale/zero tail.
                let r = pread(fd, buf.as_mut_slice(), offset);
                if let Ok(n) = r {
                    buf.set_len(n);
                }
                r
            }
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "read op without a read buffer",
            )),
        },
        IoKind::Write => match &op.buf {
            IoBuf::Write(data) => pwrite_all(fd, data, offset).map(|()| data.len()),
            // Zero-copy direct write: write straight from the caller's aligned buffer.
            IoBuf::Direct(buf) => pwrite_all(fd, buf.as_slice(), offset).map(|()| buf.len()),
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

/// A bounded blocking-syscall backend: one dedicated worker thread per execution lane, each draining
/// its own `sync` submission channel (so the dispatch EDT pick-two routes work across lanes). The
/// thread count is therefore the lane count — fixed at construction, never dynamically spawned, which
/// is what structurally removes the `spawn_blocking` deadlock.
///
/// Direct vs. buffered IO is a per-op property (the [`IoBuf`] variant the submitter chose) and a
/// property of how each file was opened ([`crate::fs::direct::File`] with `direct: true/false`), not
/// a backend-wide mode — so a single backend serves both buffered and `O_DIRECT` files. The op's
/// buffer variant and the file's open flags must agree; a mismatch surfaces as the kernel's `EINVAL`
/// on the syscall (stamped `Failed`), never silent corruption.
pub struct SyscallBackend;

impl SyscallBackend {
    /// Build a syscall backend. One worker thread is spawned per execution lane when the scheduler
    /// calls [`Backend::spawn_lanes`] (lane count = `Config::ring_count`).
    pub fn new() -> Self {
        Self
    }
}

impl Default for SyscallBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for SyscallBackend {
    type Lane = LaneSubmit;

    fn spawn_lanes<S: Spawner>(&self, setup: LaneSetup, spawner: &mut S) -> Vec<LaneSubmit> {
        // One shared completion channel (not per-lane): every worker pushes completed ops here, and
        // ONE reaper drains it into the completion sink. Completions need no per-op routing state, so
        // they flow as bare intrusive `Entry<IoOp>`; the reaper bridges the `Send` worker threads to
        // the `!Send` completion sink on the scheduler task.
        let (done_tx, done_rx) = sync_chan::new::<IoOp>();
        spawner.spawn_named("fs.syscall.reaper", reaper(done_rx, setup.completion));

        // One worker thread per lane, each owning its lane's submission-channel receiver. Workers are
        // wrapped in a shared `Arc<[Worker]>` so the threads are joined exactly once — when the last
        // lane handle drops — after the senders have closed the channels and the workers have exited.
        let mut senders = Vec::with_capacity(setup.lane_count);
        let mut workers = Vec::with_capacity(setup.lane_count);
        for idx in 0..setup.lane_count {
            let (tx, worker) = spawn_worker(idx, done_tx.clone());
            senders.push(tx);
            workers.push(worker);
        }
        let workers: Arc<[Worker]> = workers.into();

        senders
            .into_iter()
            .map(|tx| LaneSubmit {
                tx,
                _workers: workers.clone(),
            })
            .collect()
    }
}

/// The lane's submit handle: pushes each op onto its lane's `sync` channel (waking the lane's worker
/// thread via the thread-park waker). Holds an `Arc` to the worker set so the threads outlive every
/// lane handle and are joined when the last one drops.
pub struct LaneSubmit {
    tx: sync_chan::Sender<IoOp>,
    _workers: Arc<[Worker]>,
}

impl UnboundedSender<Entry<IoOp>> for LaneSubmit {
    fn send(&mut self, op: Entry<IoOp>) -> Result<(), Entry<IoOp>> {
        self.tx.send_entry(op)
    }
}

/// Drain the shared `Send` completion channel and forward each op to the completion sink. A `Map`
/// combinator over the completion receiver, driven by `drain_budgeted` — the channel layer's standard
/// task driver, not a hand-rolled poll loop. The explicit `Entry<IoOp>` closure parameter selects the
/// single-entry `Receiver` impl (the channel also impls the batch form).
async fn reaper(done_rx: sync_chan::Receiver<IoOp>, sink: CompletionSink) {
    Map::new(done_rx, move |op: Entry<IoOp>| {
        let _ = sink.send_entry(op);
    })
    .drain_budgeted(Some(crate::fs::combinator::DRAIN_BUDGET))
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        counter::Registry,
        fs::{
            config::{Config, CostModel, DeviceConfig, OpWeights, PoolMode},
            device::DeviceId,
            direct::{File, Options},
            scheduler::Scheduler,
        },
        runtime::tokio::Local,
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
    /// non-bach executor M2 needs because the worker pool runs on OS threads). The body receives a
    /// `Local` spawner (which `spawn_local`s onto the `LocalSet`) and a fresh counter `Registry`.
    fn run_local<F, B>(body: B)
    where
        F: std::future::Future<Output = ()> + 'static,
        B: FnOnce(Local, Registry) -> F,
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, body(Local::new(0), Registry::default()));
    }

    fn clock() -> crate::busy_poll::clock::Clock {
        crate::busy_poll::clock::Clock::default()
    }

    /// End-to-end: write data through the scheduler, then read it back and verify the bytes — proving
    /// the bounded pool actually performs real positional IO and routes completions correctly.
    #[test]
    fn write_then_read_roundtrip() {
        let path = temp_path("roundtrip");
        let file = File::open(
            &path,
            Options {
                truncate: true,
                size: 1 << 20,
                direct: false,
            },
        )
        .unwrap();
        let fd = file.raw_fd();

        run_local(|mut spawn, _registry| {
            let path = path.clone();
            async move {
                let config = Config {
                    devices: vec![byte_device(1 << 20)],
                    ring_count: 2,
                };
                let backend = SyscallBackend::new();
                let scheduler = Scheduler::new(&config, &backend, &mut spawn, &_registry, clock());
                let h = scheduler.handle();
                let dev = DeviceId(0);

                let payload =
                    bytes::Bytes::from_static(b"the quick brown fox jumps over the lazy dog");
                let n = h
                    .write(dev, fd, 0, payload.clone(), TierPriority::Medium)
                    .await
                    .unwrap();
                assert_eq!(n, payload.len());

                let buf = h
                    .read(dev, fd, 0, payload.len() as u32, TierPriority::High)
                    .await
                    .unwrap();
                assert_eq!(
                    &buf[..],
                    &payload[..],
                    "read-back bytes must match what was written"
                );

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
        let file = File::open(
            &path,
            Options {
                truncate: true,
                size: 1 << 20,
                direct: false,
            },
        )
        .unwrap();
        let fd = file.raw_fd();

        run_local(|mut spawn, _registry| {
            let path = path.clone();
            async move {
                // Tiny capacity (4 KiB) + only 2 worker threads, but 32 concurrent writers each
                // doing several ops: admission is credit-bounded, execution is thread-bounded.
                let config = Config {
                    devices: vec![byte_device(4096)],
                    ring_count: 2,
                };
                let backend = SyscallBackend::new();
                let scheduler = Scheduler::new(&config, &backend, &mut spawn, &_registry, clock());
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
                            if h.write(dev, fd, off, data, TierPriority::Medium)
                                .await
                                .is_ok()
                            {
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
        run_local(|mut spawn, _registry| async move {
            let config = Config {
                devices: vec![byte_device(1 << 16)],
                ring_count: 1,
            };
            let backend = SyscallBackend::new();
            let scheduler = Scheduler::new(&config, &backend, &mut spawn, &_registry, clock());
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

    /// Zero-copy direct IO roundtrip: write an aligned buffer in place, read it back into another
    /// aligned buffer in place — no bounce-buffer copy on either path. Uses `O_DIRECT` on Linux
    /// (the `is_tmpfs` guard falls back to buffered if the temp dir is tmpfs, so the test is
    /// portable while still exercising the in-place `IoBuf::Direct` path).
    #[test]
    fn direct_zero_copy_roundtrip() {
        use crate::fs::direct::{AlignedBuf, ALIGNMENT};
        let path = temp_path("direct");
        // Open in direct mode (falls back to buffered on tmpfs / macOS without O_DIRECT semantics).
        let file = File::open(
            &path,
            Options {
                truncate: true,
                size: 1 << 20,
                direct: true,
            },
        )
        .unwrap();
        let fd = file.raw_fd();

        run_local(|mut spawn, _registry| {
            let path = path.clone();
            async move {
                // Capacity in bytes large enough for one block; atomic_grant makes the grant whole.
                let config = Config {
                    devices: vec![byte_device(1 << 20)],
                    ring_count: 1,
                };
                let backend = SyscallBackend::new();
                let scheduler = Scheduler::new(&config, &backend, &mut spawn, &_registry, clock());
                let dev = DeviceId(0);
                let h = scheduler.handle();

                // Build a page-aligned write buffer with a known pattern.
                let mut wbuf = AlignedBuf::new(ALIGNMENT);
                for (i, b) in wbuf.as_mut_slice().iter_mut().enumerate() {
                    *b = (i & 0xff) as u8;
                }
                let (wbuf, n) = h
                    .write_direct(dev, fd, 0, wbuf, TierPriority::Medium)
                    .await
                    .expect("direct write");
                assert_eq!(n, ALIGNMENT);
                drop(wbuf);

                // Read it back into a fresh aligned buffer, in place.
                let rbuf = AlignedBuf::new(ALIGNMENT);
                let (rbuf, rn) = h
                    .read_direct(dev, fd, 0, rbuf, TierPriority::High)
                    .await
                    .expect("direct read");
                assert_eq!(rn, ALIGNMENT, "direct read returned wrong byte count");
                assert_eq!(rbuf.len(), ALIGNMENT, "direct read buffer len not set to n");
                for (i, b) in rbuf.as_slice().iter().enumerate() {
                    assert_eq!(*b, (i & 0xff) as u8, "direct read byte {i} mismatch");
                }

                // A misaligned offset must be rejected before any IO.
                let bad = AlignedBuf::new(ALIGNMENT);
                let err = h.read_direct(dev, fd, 1, bad, TierPriority::Medium).await;
                assert!(
                    matches!(
                        err.as_ref().map_err(|e| e.kind()),
                        Err(std::io::ErrorKind::InvalidInput)
                    ),
                    "misaligned direct offset must be rejected, got {:?}",
                    err.as_ref().map(|_| ())
                );

                // A misaligned direct LENGTH must also be rejected at submit (not deep-EINVAL).
                let bad_len = AlignedBuf::new(100); // len 100 is not block-aligned
                let err = h
                    .read_direct(dev, fd, 0, bad_len, TierPriority::Medium)
                    .await;
                assert!(
                    matches!(
                        err.as_ref().map_err(|e| e.kind()),
                        Err(std::io::ErrorKind::InvalidInput)
                    ),
                    "misaligned direct length must be rejected, got {:?}",
                    err.as_ref().map(|_| ())
                );

                drop(file);
                let _ = std::fs::remove_file(&path);
            }
        });
    }

    /// End-to-end `materialize_direct` over real `O_DIRECT` files: write block-aligned data, then
    /// stream it back through the direct-IO materialize path and verify FIFO order + bytes. Exercises
    /// the zero-copy `AlignedBuf` → `Bytes::from_owner` delivery and the per-device acquire reactor on
    /// the real syscall backend (not just the bach mock).
    #[test]
    fn materialize_direct_streams_in_order() {
        use crate::fs::{direct::ALIGNMENT, scheduler::BlockRef};
        let path = temp_path("materialize-direct");
        let file = File::open(
            &path,
            Options {
                truncate: true,
                size: 1 << 20,
                direct: true,
            },
        )
        .unwrap();
        let fd = file.raw_fd();

        // Pre-fill 8 aligned blocks with a per-block pattern (block k -> byte value k).
        let nblocks = 8usize;
        for k in 0..nblocks {
            let buf = vec![k as u8; ALIGNMENT];
            file.write_at(&buf, (k * ALIGNMENT) as u64).unwrap();
        }
        file.sync().unwrap();

        run_local(|mut spawn, registry| {
            let path = path.clone();
            async move {
                let config = Config {
                    devices: vec![byte_device(1 << 20)],
                    ring_count: 2,
                };
                let backend = SyscallBackend::new();
                let scheduler = Scheduler::new(&config, &backend, &mut spawn, &registry, clock());
                let dev = DeviceId(0);
                let h = scheduler.handle();

                let blocks: Vec<BlockRef> = (0..nblocks)
                    .map(|k| BlockRef::whole(dev, fd, (k * ALIGNMENT) as u64, ALIGNMENT as u32))
                    .collect();

                let mut stream = h.materialize_direct(blocks, TierPriority::High);
                let mut delivered = 0usize;
                while let Some(chunk) = stream.next().await {
                    let bytes = chunk.expect("direct block read failed");
                    assert_eq!(bytes.len(), ALIGNMENT, "block {delivered} wrong len");
                    assert!(
                        bytes.iter().all(|&b| b == delivered as u8),
                        "block {delivered} content/order mismatch"
                    );
                    delivered += 1;
                }
                assert_eq!(
                    delivered, nblocks,
                    "did not deliver all direct blocks in order"
                );

                let device = scheduler.devices().get(dev).unwrap();
                for pool in device.pools.all() {
                    assert_eq!(pool.debug_free_total(), pool.debug_capacity() as i64);
                }

                drop(file);
                let _ = std::fs::remove_file(&path);
            }
        });
    }

    /// A buffered read past EOF returns only the bytes that exist — the completion's byte count and
    /// the delivered buffer length both reflect the short read, never a zero-padded full length.
    #[test]
    fn short_read_at_eof_reports_actual_len() {
        let path = temp_path("eof");
        let file = File::open(
            &path,
            Options {
                truncate: true,
                size: 0,
                direct: false,
            },
        )
        .unwrap();
        let fd = file.raw_fd();
        // Write exactly 10 bytes, then ask for 4096.
        file.write_at(b"0123456789", 0).unwrap();
        file.sync().unwrap();

        run_local(|mut spawn, _registry| {
            let path = path.clone();
            async move {
                let config = Config {
                    devices: vec![byte_device(1 << 20)],
                    ring_count: 1,
                };
                let backend = SyscallBackend::new();
                let scheduler = Scheduler::new(&config, &backend, &mut spawn, &_registry, clock());
                let dev = DeviceId(0);
                let h = scheduler.handle();

                let buf = h
                    .read(dev, fd, 0, 4096, TierPriority::Medium)
                    .await
                    .unwrap();
                assert_eq!(
                    buf.len(),
                    10,
                    "short read must report only the bytes that exist"
                );
                assert_eq!(&buf[..], b"0123456789");

                drop(file);
                let _ = std::fs::remove_file(&path);
            }
        });
    }

    /// The scheduler is a single, process-wide, `Send + Sync` object. On ONE multi-threaded runtime,
    /// the scheduler's own (`!Send`) tasks run on a `LocalSet` while many concurrent submitter tasks
    /// are `tokio::spawn`ed onto the multi-thread pool — which *requires* the submit futures (and the
    /// `SubmitHandle` they capture) to be `Send`. They all share one scheduler via cheap handle
    /// clones, the model `stream::Client`/`Arc<Endpoint>` uses.
    #[test]
    fn handle_shared_across_threads() {
        let path = temp_path("mt");
        let file = File::open(
            &path,
            Options {
                truncate: true,
                size: 1 << 20,
                direct: false,
            },
        )
        .unwrap();
        let fd = file.raw_fd();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();

        let total = local.block_on(&rt, async move {
            let config = Config {
                devices: vec![byte_device(1 << 20)],
                ring_count: 4,
            };
            let backend = SyscallBackend::new();
            // The scheduler's internal tasks are !Send → the tokio `Local` spawner `spawn_local`s
            // them onto the surrounding `LocalSet`.
            let mut spawn = Local::new(0);
            let registry = Registry::default();
            let scheduler = Scheduler::new(&config, &backend, &mut spawn, &registry, clock());

            // Concurrent submitters via `tokio::spawn` (multi-thread pool): this only compiles if the
            // submit future + captured SubmitHandle are Send.
            let threads = 8usize;
            let per_task = 64u64;
            let mut tasks = Vec::new();
            for t in 0..threads {
                let h = scheduler.handle();
                tasks.push(tokio::spawn(async move {
                    let mut ok = 0u64;
                    for i in 0..per_task {
                        let off = (t as u64 * per_task + i) * 4096;
                        let data = bytes::Bytes::from(vec![(t & 0xff) as u8; 256]);
                        if h.write(DeviceId(0), fd, off, data, TierPriority::Medium)
                            .await
                            .is_ok()
                        {
                            ok += 1;
                        }
                    }
                    ok
                }));
            }
            let mut total = 0u64;
            for t in tasks {
                total += t.await.unwrap();
            }
            total
        });

        assert_eq!(total, 8 * 64, "all cross-thread submits must complete");
        drop(file);
        let _ = std::fs::remove_file(&path);
    }
}
