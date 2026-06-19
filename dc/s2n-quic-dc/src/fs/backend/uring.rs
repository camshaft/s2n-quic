// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! io_uring backend — one ring per lane (Linux only).
//!
//! Each execution lane owns one [`io_uring`](io_uring::IoUring) driven by a dedicated OS thread (from
//! the backend's blocking [`Runtime`](super::blocking::Runtime)). The thread drains the lane's
//! submission channel, builds SQEs straight from each op's fields (zero-copy: the SQE points at the
//! op's own buffer), submits, blocks in `submit_and_wait` for completions, reaps CQEs, and
//! **completes each finished op in place** ([`combinator::complete`](crate::fs::combinator::complete))
//! — releasing its credit to its own `Arc<Device>` pool and notifying its submitter on the ring
//! thread. There is no reaper task and no cross-thread completion bridge.
//!
//! # Dual-wait intake (eventfd)
//!
//! The ring thread must react to **two** independent events: a kernel completion *and* a producer
//! wanting to submit more work. A plain blocking `submit_and_wait(1)` only wakes on a CQE, so a
//! producer with spare ring capacity would wait until some unrelated completion happened to break the
//! wait — adding latency. The fix is an **eventfd self-pipe** folded into the ring as just another
//! completion source:
//!
//! * Each ring owns one `eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK)`.
//! * Intake is the shared [`sync`](crate::socket::channel::intrusive::sync) channel (the same channel
//!   type the rest of the pipeline uses), driven with a **custom [`Waker`] that writes the eventfd**.
//!   A producer `send`s an op and the channel fires that waker, which bumps the eventfd counter.
//! * A **multishot [`PollAdd`](opcode::PollAdd)** is armed on the eventfd with a reserved `user_data`
//!   token. When the eventfd becomes readable the poll yields a CQE, which breaks `submit_and_wait`.
//!   (Multishot needs kernel ≥ 5.13; the loop re-arms a fresh poll whenever the kernel reports the
//!   armed poll is no longer `more()`, so a one-shot fallback works on older kernels too.)
//!
//! So each loop wakes on a real CQE **or** new work, drains the channel into the ring's free SQE
//! slots, submits, and reaps — never blocked against a producer that has work to give it.
//!
//! # Buffer lifetime
//!
//! An op handed to the kernel is parked in an [`InFlightSlab`] keyed by the SQE's `user_data`; the
//! slab owns the `IoOp` (and thus its heap buffer) for the whole kernel operation, so the buffer the
//! kernel reads/writes stays pinned and alive until its CQE arrives — the io_uring memory-safety
//! contract. A submit error never frees in-flight buffers; they stay pinned and the loop keeps
//! reaping. Admission is credit-gated upstream, so the in-flight set is bounded by the device pool and
//! the hold-and-wait deadlock cannot form.
//!
//! Direct (`O_DIRECT`) vs. buffered is a per-op / per-file property exactly as in the syscall backend;
//! io_uring issues the same `read`/`write` opcodes either way and the kernel honors the file's flags.

use crate::{
    fs::{
        backend::{
            blocking::{JoinOnDrop, Runtime},
            inflight::{InFlight, InFlightSlab},
            Backend, LaneSetup,
        },
        combinator::complete,
        counters::Counters,
        op::{IoBuf, IoKind, IoOp, IoStatus},
    },
    intrusive::Entry,
    socket::channel::{intrusive::sync as sync_chan, Budget, UnboundedSender},
    sync::Arc,
};
use io_uring::{opcode, types, IoUring};
use std::{
    os::fd::RawFd,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc as StdArc,
    },
    task::{Context, Wake, Waker},
};

/// Default ring depth (entries per ring). Sized well above any reasonable credit-bounded in-flight
/// count so the SQ never backs up before the credit pool does.
pub const DEFAULT_RING_DEPTH: u32 = 256;

/// Reserved `user_data` token for the eventfd's armed [`PollAdd`](opcode::PollAdd). Distinct from any
/// real op's slab id (slab ids are dense from 0 and the slab never approaches `u64::MAX`).
const EVENTFD_TOKEN: u64 = u64::MAX;

/// Probe whether io_uring is usable in the current environment, returning the underlying
/// `io_uring_setup(2)` error if not — so the application can fall back to the blocking
/// [`SyscallBackend`](super::syscall::SyscallBackend) and log *why*.
///
/// The robust signal is to actually create a ring and observe the failure: `io_uring_setup` is the
/// one syscall that surfaces every disablement reason in a single check —
///
/// * **`ENOSYS`** — the kernel predates io_uring (< 5.1) or was built without it;
/// * **`EPERM`** — the `kernel.io_uring_disabled` sysctl (≥ 6.6: `1` = unprivileged-off, `2` =
///   fully off), or a seccomp filter blocking the syscall (common in hardened containers/sandboxes);
/// * **`ENOMEM`** — the locked-memory rlimit (`RLIMIT_MEMLOCK`) is too low to set up the ring.
///
/// On success the probe ring is created and immediately dropped — a cheap, side-effect-free
/// capability check. Run it once at startup:
///
/// ```ignore
/// let scheduler = match uring::probe() {
///     Ok(()) => Scheduler::new(&cfg, &UringBackend::default(), &mut spawn, &reg, clock),
///     Err(e) => {
///         tracing::warn!("io_uring unavailable ({e}); using the blocking syscall pool");
///         Scheduler::new(&cfg, &SyscallBackend::default(), &mut spawn, &reg, clock)
///     }
/// };
/// // then: let dev = scheduler.register_device("nvme0", &device_cfg)?;
/// ```
pub fn probe() -> std::io::Result<()> {
    // A minimal ring (8 entries — the smallest a depth ever rounds up to) is enough to exercise
    // `io_uring_setup`; the depth does not affect whether the syscall is permitted. Dropped at once.
    IoUring::new(8).map(drop)
}

/// What a reaped CQE means for its op.
enum Reaped {
    /// The op is finished (success or error); take it out of the slab and complete it.
    Done,
    /// A short read/write: more bytes remain; the op stays in the slab and its tail is re-submitted.
    Resubmit,
}

// ── eventfd ──────────────────────────────────────────────────────────────────

/// An owned `eventfd(2)` — the ring's "new work" wake source. **Shared ownership** (`StdArc<EventFd>`)
/// between the ring loop and the [`EventFdWaker`]: the channel can keep a registered waker alive past
/// ring teardown, so the fd must outlive every waker. Closing it here only when the last `Arc` drops
/// (ring loop + all wakers gone) prevents a use-after-free / fd-reuse where a late `wake` writes a
/// closed-and-recycled descriptor.
struct EventFd {
    fd: RawFd,
}

impl EventFd {
    /// Create a non-blocking, close-on-exec eventfd with an initial count of 0.
    fn new() -> std::io::Result<Self> {
        // SAFETY: `eventfd` is a plain syscall; the flags are valid.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    /// Bump the counter by 1 (an 8-byte write, the eventfd ABI), making the fd readable so the ring's
    /// armed `PollAdd` completes. Called from the producer thread via the waker.
    fn signal(&self) {
        let val: u64 = 1;
        // SAFETY: `&val` is a valid 8-byte source; `self` (and thus `fd`) is kept alive by the
        // `Arc<EventFd>` the waker holds, so the fd is open for the whole call — no UAF/reuse.
        let _ = unsafe {
            libc::write(self.fd, &val as *const u64 as *const libc::c_void, 8)
        };
    }

    /// Drain the eventfd's accumulated counter (one non-blocking 8-byte read). Called by the ring
    /// thread after the armed poll fires so the next write re-triggers it.
    fn drain(&self) {
        let mut buf = [0u8; 8];
        // SAFETY: `buf` is a valid 8-byte destination; a non-blocking eventfd read either returns 8
        // or fails with EAGAIN (nothing to drain) — both fine.
        let _ = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, 8) };
    }
}

impl Drop for EventFd {
    fn drop(&mut self) {
        // SAFETY: we own `fd` (last `Arc<EventFd>` reference) and it is not used after close.
        unsafe { libc::close(self.fd) };
    }
}

/// `ALIVE` bit of [`EventFdWaker::state`]: set while the ring thread is running; cleared on teardown
/// so a late wake skips the syscall.
const WAKER_ALIVE: usize = 0b01;
/// `NOTIFIED` bit of [`EventFdWaker::state`]: a wake has been signalled but the ring has not yet
/// consumed it. Coalesces a burst — only the producer that sets this bit writes the eventfd.
const WAKER_NOTIFIED: usize = 0b10;

/// The [`Waker`] the ring's intake channel fires when a producer sends an op: it writes the eventfd,
/// which completes the ring's armed [`PollAdd`](opcode::PollAdd) and breaks `submit_and_wait`. It
/// **owns a shared reference to the [`EventFd`]** (`StdArc<EventFd>`), so the descriptor stays open as
/// long as any waker exists — even after the ring loop has exited and dropped its own reference (the
/// `Arc`, not a flag, is what guarantees the fd is valid).
///
/// A single packed `state` word holds both control bits ([`WAKER_ALIVE`], [`WAKER_NOTIFIED`]), so a
/// producer does **one** atomic op to decide whether to write:
///
/// * **liveness** — after teardown the ring clears `ALIVE`, so a late wake on a stale registered waker
///   is a no-op (a cheap short-circuit; safety is the `Arc`).
/// * **burst coalescing** — only the producer that flips `NOTIFIED` off → on performs the eventfd
///   write; while a wake is still unconsumed the rest skip the syscall (the kernel counter would
///   coalesce them into one `POLLIN`, but this avoids even the `write`). The ring clears `NOTIFIED`
///   *before* draining the channel each loop, so a send that races the drain re-takes the transition
///   and re-signals — never lost. Same flag discipline as the thread-park waker in
///   [`blocking`](super::blocking).
struct EventFdWaker {
    efd: StdArc<EventFd>,
    state: AtomicUsize,
}

impl EventFdWaker {
    /// Clear `NOTIFIED`, called by the ring thread immediately before it drains the channel so a wake
    /// racing the drain re-signals.
    #[inline]
    fn clear_notified(&self) {
        self.state.fetch_and(!WAKER_NOTIFIED, Ordering::AcqRel);
    }

    /// Clear `ALIVE` on ring teardown so later wakes short-circuit.
    #[inline]
    fn mark_dead(&self) {
        self.state.fetch_and(!WAKER_ALIVE, Ordering::Release);
    }
}

impl Wake for EventFdWaker {
    fn wake(self: StdArc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &StdArc<Self>) {
        // One atomic: set NOTIFIED and read the prior state. Write the eventfd only if the ring is
        // ALIVE and NOTIFIED was not already set (this producer won the burst's first wake).
        let prev = self.state.fetch_or(WAKER_NOTIFIED, Ordering::AcqRel);
        if prev & WAKER_ALIVE != 0 && prev & WAKER_NOTIFIED == 0 {
            self.efd.signal();
        }
    }
}

// ── Ring loop ─────────────────────────────────────────────────────────────────

/// Build an SQE that arms a **multishot** poll on the eventfd for `POLLIN`, tagged with
/// [`EVENTFD_TOKEN`]. Multishot stays armed across firings (kernel ≥ 5.13); the loop re-arms if the
/// kernel reports it is no longer `more()`.
fn arm_eventfd_poll(efd: RawFd) -> io_uring::squeue::Entry {
    opcode::PollAdd::new(types::Fd(efd), libc::POLLIN as u32)
        .multi(true)
        .build()
        .user_data(EVENTFD_TOKEN)
}

/// The ring thread: drain submitted ops from the intake channel, build SQEs, submit, reap CQEs, and
/// complete each finished op in place. Runs on a dedicated blocking thread (it blocks in
/// `submit_and_wait`, so it cannot be a cooperative future). Returns when the intake channel closes
/// (every lane sender dropped) and nothing remains in flight.
///
/// `rx` is the lane's submission channel; `efd` its dual-wait eventfd; `waker` the eventfd-writing
/// waker the channel fires on `send` (kept alive here, marked dead on exit). `counters` is where each
/// completed op records its disposition.
fn ring_loop(
    mut ring: IoUring,
    mut rx: sync_chan::Receiver<IoOp>,
    efd: StdArc<EventFd>,
    waker: StdArc<EventFdWaker>,
    depth: usize,
    counters: Arc<Counters>,
) {
    let mut slab = InFlightSlab::<IoOp>::with_capacity(depth);
    // Scratch buffers RETAINED across iterations (cleared, never reallocated): ids whose tail must be
    // re-submitted after a short completion, and ids completed this pass.
    let mut to_resubmit: Vec<usize> = Vec::with_capacity(depth);
    let mut completed: Vec<usize> = Vec::with_capacity(depth);

    // Build a std `Waker`/`Context` over the eventfd waker, used to drive the (non-async) intake
    // channel: `poll_recv` registers this waker, so a producer's `send` fires it → writes the eventfd
    // → completes the armed poll → breaks our `submit_and_wait`. One unit of budget per drain pass is
    // irrelevant (we pass a large budget); the ring-space bound governs how many we pull.
    let std_waker: Waker = waker.clone().into();
    let mut cx = Context::from_waker(&std_waker);
    let mut budget = Budget::new(depth.max(1));

    // Arm the eventfd poll once up front. If the SQ is somehow full (impossible on an empty ring) the
    // push is retried at the top of the loop via `poll_armed`.
    let mut poll_armed = false;
    poll_armed |= push_eventfd_poll(&mut ring, &efd, poll_armed);

    let mut rx_open = true;

    loop {
        // 1. Re-arm the eventfd poll if a previous firing consumed it (oneshot fallback / kernel said
        //    not-more). Idempotent: skipped when already armed.
        poll_armed |= push_eventfd_poll(&mut ring, &efd, poll_armed);

        // 2. Re-submit the tails of short-completed ops first (already pinned in the slab and counted
        //    outstanding; only the SQE needs re-pushing). Drop those the SQ accepts; keep the rest.
        to_resubmit.retain(|&id| {
            let Some(slot) = slab.get(id) else {
                return false;
            };
            let entry = build_sqe(&slot.op, slot.done, id as u64);
            // SAFETY: the op (and its buffer) is pinned in the slab until its final CQE.
            match unsafe { ring.submission().push(&entry) } {
                Ok(()) => false, // submitted; drop from the resubmit list
                Err(_) => true,  // SQ full; retry next iteration
            }
        });

        // 3. Drain newly-submitted ops from the intake channel into the SQ, bounded by free ring
        //    space. `poll_recv` with our eventfd waker registers it whenever the channel goes empty,
        //    so the next `send` wakes us. A closed channel (all senders dropped) flips `rx_open`.
        //
        // Clear NOTIFIED *before* draining: a producer that enqueues after this point (racing the
        // drain) re-takes the wake transition and re-signals the eventfd, so its op is never stranded
        // waiting for a wake we already consumed.
        waker.clear_notified();
        budget.reset();
        while slab.outstanding() < depth {
            // Disambiguate: the `sync` channel's `Receiver<IoOp>` implements
            // `channel::Receiver<Entry<IoOp>>` (single-entry) and `..<Queue<IoOp>>` (batch); we want
            // the single-entry form so each `Ready(Some(_))` is one `Entry<IoOp>`.
            let polled = <sync_chan::Receiver<IoOp> as crate::socket::channel::Receiver<
                Entry<IoOp>,
            >>::poll_recv(&mut rx, &mut cx, &mut budget);
            match polled {
                core::task::Poll::Ready(Some(op)) => {
                    // Park the op (and its buffer) in the slab — it owns the op for the whole kernel
                    // operation — then build the SQE from the now-stable buffer pointer.
                    let want = transfer_len(&op);
                    let id = slab.insert(op, want);
                    let entry = build_sqe(&slab.get(id).unwrap().op, 0, id as u64);
                    // SAFETY: the op (and its buffer) lives in the slab until its CQE is reaped, so the
                    // buffer pointer in the SQE stays valid for the kernel operation's duration.
                    match unsafe { ring.submission().push(&entry) } {
                        Ok(()) => {}
                        Err(_) => {
                            // SQ full despite the outstanding<depth bound: keep the op pinned in the
                            // slab and re-push its SQE on a later iteration (done=0). Stop draining
                            // this pass — the SQ has no room.
                            to_resubmit.push(id);
                            break;
                        }
                    }
                    budget.reset();
                }
                core::task::Poll::Ready(None) => {
                    rx_open = false;
                    break;
                }
                core::task::Poll::Pending => break,
            }
        }

        // 4. Exit check: channel closed and nothing in flight → tear down. Mark the waker dead so any
        //    late wake on a still-registered copy short-circuits (the fd stays valid via the `Arc`).
        if !rx_open && slab.is_idle() {
            waker.mark_dead();
            return;
        }

        // 5. Submit and wait for at least one completion (a real CQE or the eventfd poll). A submit
        //    error NEVER frees in-flight buffers — they stay pinned and we fall through to reap
        //    whatever completions exist (treating the error like EINTR).
        match ring.submit_and_wait(1) {
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {}
        }

        // 6. Reap completions. The eventfd token is drained + (if no longer multishot) re-armed; a
        //    short read/write advances progress and is marked for re-submit; otherwise the op is
        //    completed in place and its slab slot (and buffer) freed.
        completed.clear();
        {
            let mut cq = ring.completion();
            cq.sync();
            for cqe in &mut cq {
                let token = cqe.user_data();
                if token == EVENTFD_TOKEN {
                    // The poll fired (a producer wrote the eventfd). If the kernel says it is no
                    // longer armed, re-arm next iteration.
                    if !io_uring::cqueue::more(cqe.flags()) {
                        poll_armed = false;
                    }
                    continue;
                }
                let id = token as usize;
                let Some(slot) = slab.get_mut(id) else {
                    // Orphan CQE for an already-completed/reaped id — harmless tombstone.
                    continue;
                };
                match apply_cqe(slot, cqe.result()) {
                    Reaped::Resubmit => to_resubmit.push(id),
                    Reaped::Done => completed.push(id),
                }
            }
        }
        // Drain the eventfd counter AFTER reaping so a write that raced the drain re-fires the poll.
        efd.drain();

        for &id in &completed {
            if let Some(mut op) = slab.take(id) {
                finalize(&mut op);
                // Complete the op in place: release its credit to its own device pool, notify its
                // submitter. Runs on this ring thread — no completion bridge.
                complete(op, &counters);
            }
        }
    }
}

/// Push the eventfd poll SQE if not currently armed. Returns whether it is armed after the call.
fn push_eventfd_poll(ring: &mut IoUring, efd: &EventFd, already_armed: bool) -> bool {
    if already_armed {
        return false;
    }
    let entry = arm_eventfd_poll(efd.fd);
    // SAFETY: the eventfd outlives the ring (owned by `ring_loop`); the poll SQE references only its
    // fd, no buffer.
    matches!(unsafe { ring.submission().push(&entry) }, Ok(()))
}

/// Apply a CQE result to an in-flight op, returning whether it is finished or needs its tail
/// re-submitted. `result` is `-errno` on failure, else the bytes transferred (0 for fsync/trim).
fn apply_cqe(slot: &mut InFlight<IoOp>, result: i32) -> Reaped {
    if result < 0 {
        let err = std::io::Error::from_raw_os_error(-result);
        slot.op.status = IoStatus::Failed(err.kind());
        return Reaped::Done;
    }
    let n = result as usize;
    let kind = slot.op.kind;
    match kind {
        IoKind::Write => {
            slot.done += n;
            if slot.done >= slot.want {
                slot.op.status = IoStatus::Done(slot.done);
                Reaped::Done
            } else if n == 0 {
                // A 0-byte non-error write makes no progress — mirror pwrite_all's WriteZero guard
                // rather than spin forever.
                slot.op.status = IoStatus::Failed(std::io::ErrorKind::WriteZero);
                Reaped::Done
            } else {
                // Short write: re-submit the unwritten tail.
                Reaped::Resubmit
            }
        }
        IoKind::Read => {
            // A short read means EOF (or a partial that the file simply had no more for): the op is
            // done with `done + n` bytes. We do NOT re-issue reads — a short read is a legitimate
            // end, not an error, matching the buffered backend's read-exact-to-EOF behavior.
            slot.done += n;
            slot.op.status = IoStatus::Done(slot.done);
            Reaped::Done
        }
        // fsync / fdatasync / trim: single completion, no byte progress.
        _ => {
            slot.op.status = IoStatus::Done(0);
            Reaped::Done
        }
    }
}

/// Finalize a completed op's buffer so the delivered length reflects the bytes actually transferred:
/// publish a buffered read's bytes via `set_len`, or shrink a direct read's `AlignedBuf` logical
/// length. (Writes leave their source buffer untouched.)
fn finalize(op: &mut IoOp) {
    if op.kind != IoKind::Read {
        return;
    }
    let n = match op.status {
        IoStatus::Done(n) => n,
        _ => return,
    };
    match &mut op.buf {
        // The buffered-read `BytesMut` arrived logically EMPTY (len 0); the kernel wrote `n` bytes
        // into its spare capacity, so we publish them with `set_len(n)` (NOT `truncate`, which would
        // leave len 0). `n <= op.len <= capacity` by the read contract.
        // SAFETY: the kernel initialized exactly the first `n` bytes of the spare capacity the SQE
        // pointed at (`read_ptr` used `op.len`, and `n` is the CQE byte count, so `n <= op.len <=
        // capacity`); `set_len(n)` exposes only those initialized bytes.
        IoBuf::Read(b) => unsafe { b.set_len(n) },
        IoBuf::Direct(b) => b.set_len(n),
        _ => {}
    }
}

/// The total transfer length of a read/write op (0 for control ops).
fn transfer_len(op: &IoOp) -> usize {
    match op.kind {
        IoKind::Read | IoKind::Write => op.len as usize,
        _ => 0,
    }
}

/// Build an SQE for `op` starting at byte offset `done` into its buffer (0 for the first submission,
/// `>0` when re-submitting a short-completed tail), tagged with `user_data`. The op stays pinned in
/// the slab so the buffer pointer is valid until the final CQE.
///
/// For a buffered read the destination is the op's `BytesMut` **uninitialized spare capacity** (the
/// buffer arrives pre-sized + logically empty per the [`IoBuf::Read`] contract); the kernel fills it
/// and the CQE's byte count is applied via `set_len` in `finalize`. No `resize`/zero-fill.
fn build_sqe(op: &IoOp, done: usize, user_data: u64) -> io_uring::squeue::Entry {
    let fd = types::Fd(op.fd as RawFd);
    let off = op.offset + done as u64;
    match op.kind {
        IoKind::Read => {
            let (ptr, len) = read_ptr(op, done);
            opcode::Read::new(fd, ptr, len)
                .offset(off)
                .build()
                .user_data(user_data)
        }
        IoKind::Write => {
            let (ptr, len) = write_ptr(op, done);
            opcode::Write::new(fd, ptr.cast_mut(), len)
                .offset(off)
                .build()
                .user_data(user_data)
        }
        IoKind::Fsync => opcode::Fsync::new(fd).build().user_data(user_data),
        IoKind::Fdatasync => opcode::Fsync::new(fd)
            .flags(types::FsyncFlags::DATASYNC)
            .build()
            .user_data(user_data),
        // Trim is a true no-op in v1 on both backends (the syscall backend also no-ops it), kept
        // interchangeable; a real block-discard (FALLOCATE+PUNCH_HOLE) is future work. Submit a
        // zero-length read at the current offset, which completes immediately as Done(0).
        IoKind::Trim => opcode::Read::new(fd, core::ptr::null_mut(), 0)
            .offset(off)
            .build()
            .user_data(user_data),
    }
}

/// Read destination pointer/len for the tail starting at `done`. For a buffered read the destination
/// is the buffer's uninitialized spare capacity (the op carries `len` and an empty pre-sized
/// `BytesMut`); for a direct read it is the aligned buffer. The op is pinned in the slab until the
/// final CQE, so the pointer is valid for the kernel operation.
fn read_ptr(op: &IoOp, done: usize) -> (*mut u8, u32) {
    let (base, total): (*mut u8, usize) = match &op.buf {
        IoBuf::Read(b) => {
            // Write into spare capacity: base is the buffer's start (len() is 0), total is `op.len`
            // (the requested transfer; capacity is >= len by the buffer contract).
            (b.as_ptr() as *mut u8, op.len as usize)
        }
        IoBuf::Direct(b) => {
            let s = b.as_slice();
            (s.as_ptr() as *mut u8, s.len())
        }
        _ => return (core::ptr::null_mut(), 0),
    };
    let done = done.min(total);
    // SAFETY: `done <= total`, so the offset pointer is within the buffer (or one-past-the-end).
    unsafe { (base.add(done), (total - done) as u32) }
}

/// Write source pointer/len for the tail starting at `done`.
fn write_ptr(op: &IoOp, done: usize) -> (*const u8, u32) {
    let (base, total): (*const u8, usize) = match &op.buf {
        IoBuf::Write(b) => (b.as_ptr(), b.len()),
        IoBuf::Direct(b) => {
            let s = b.as_slice();
            (s.as_ptr(), s.len())
        }
        _ => return (core::ptr::null(), 0),
    };
    let done = done.min(total);
    // SAFETY: `done <= total`, so the offset pointer is within the buffer (or one-past-the-end).
    unsafe { (base.add(done), (total - done) as u32) }
}

// ── Backend wiring ──────────────────────────────────────────────────────────

/// An io_uring backend: one ring (and dedicated thread, from the blocking [`Runtime`]) per lane.
pub struct UringBackend {
    runtime: Runtime,
    depth: u32,
}

impl UringBackend {
    /// Build an io_uring backend over `runtime` with the given ring depth (entries per ring).
    pub fn new(runtime: Runtime, depth: u32) -> Self {
        Self {
            runtime,
            depth: depth.max(8),
        }
    }

    /// Build with a fresh blocking runtime (threads named `s2n-dc-fs-uring`) and the given depth.
    pub fn with_depth(depth: u32) -> Self {
        Self::new(Runtime::new("s2n-dc-fs-uring"), depth)
    }
}

impl Default for UringBackend {
    fn default() -> Self {
        Self::with_depth(DEFAULT_RING_DEPTH)
    }
}

impl Backend for UringBackend {
    type Lane = RingSubmit;

    fn spawn_lanes(&self, setup: LaneSetup) -> Vec<RingSubmit> {
        let mut senders = Vec::with_capacity(setup.lane_count);
        let mut joins = Vec::with_capacity(setup.lane_count);
        for idx in 0..setup.lane_count {
            let (tx, rx) = sync_chan::new::<IoOp>();
            // Shared ownership of the eventfd: the ring loop and the waker each hold an `Arc`, so the
            // fd outlives every registered waker (the channel can keep one past teardown) — no UAF.
            let efd = StdArc::new(EventFd::new().expect("failed to create eventfd for io_uring lane"));
            let waker = StdArc::new(EventFdWaker {
                efd: efd.clone(),
                state: AtomicUsize::new(WAKER_ALIVE),
            });
            let ring_depth = self.depth;
            let depth = self.depth as usize;
            let counters = setup.counters.clone();
            // The ring loop blocks in `submit_and_wait`, so it cannot be a cooperative future — spawn
            // it as a plain blocking closure on the runtime (one dedicated thread per ring). The ring
            // is built ON the ring thread (an `IoUring` is not `Send`).
            let join = self.runtime.spawn_blocking(idx, move || {
                let ring = IoUring::new(ring_depth).expect("failed to create io_uring for lane");
                ring_loop(ring, rx, efd, waker, depth, counters);
            });
            senders.push(tx);
            joins.push(join);
        }
        let joins: Arc<[JoinOnDrop]> = joins.into();

        senders
            .into_iter()
            .map(|tx| RingSubmit {
                tx,
                _joins: joins.clone(),
            })
            .collect()
    }
}

/// The lane's submit handle: pushes each op onto its ring's intake `sync` channel. The channel fires
/// the ring's eventfd-writing waker, which breaks the ring thread's `submit_and_wait` so it picks the
/// op up immediately (dual-wait). Holds an `Arc` to the shared join set so the ring threads outlive
/// every lane handle and are joined when the last one drops.
pub struct RingSubmit {
    tx: sync_chan::Sender<IoOp>,
    _joins: Arc<[JoinOnDrop]>,
}

impl UnboundedSender<Entry<IoOp>> for RingSubmit {
    fn send(&mut self, op: Entry<IoOp>) -> Result<(), Entry<IoOp>> {
        self.tx.send_entry(op)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        counter::Registry,
        fs::{
            config::{Config, CostModel, DeviceConfig, OpWeights, PoolMode},
            direct::{File, Options},
            scheduler::Scheduler,
        },
        runtime::tokio::Local,
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

    fn config(ring_count: usize) -> Config {
        Config { ring_count }
    }

    /// End-to-end through io_uring: write then read back, verifying bytes.
    #[test]
    fn uring_write_then_read_roundtrip() {
        let path = temp_path("rt");
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
        run_local(|mut spawn, registry| {
            let path = path.clone();
            async move {
                let scheduler = Scheduler::new(
                    &config(1),
                    &UringBackend::default(),
                    &mut spawn,
                    &registry,
                    clock(),
                );
                let dev = scheduler.register_device("test", &byte_device(1 << 20)).unwrap();
                let h = scheduler.handle();

                let payload = bytes::Bytes::from_static(b"io_uring round trip payload");
                let n = h
                    .write(&dev, fd, 0, payload.clone(), TierPriority::Medium)
                    .await
                    .unwrap();
                assert_eq!(n, payload.len());

                let buf = h
                    .read(&dev, fd, 0, payload.len() as u32, TierPriority::High)
                    .await
                    .unwrap();
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
        run_local(|mut spawn, registry| {
            let path = path.clone();
            async move {
                let scheduler = Scheduler::new(
                    &config(1),
                    &UringBackend::default(),
                    &mut spawn,
                    &registry,
                    clock(),
                );
                let dev = scheduler.register_device("test", &byte_device(4096)).unwrap();
                let completed = Rc::new(std::cell::Cell::new(0usize));
                let mut tasks = Vec::new();
                for w in 0..32u64 {
                    let h = scheduler.handle();
                    let dev = dev.clone();
                    let completed = completed.clone();
                    tasks.push(tokio::task::spawn_local(async move {
                        for i in 0..4u64 {
                            let off = (w * 4 + i) * 512;
                            let data = bytes::Bytes::from(vec![(w & 0xff) as u8; 256]);
                            if h.write(&dev, fd, off, data, TierPriority::Medium)
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
                assert_eq!(
                    completed.get(),
                    32 * 4,
                    "every op must complete through the ring"
                );
                for pool in dev.pools.all() {
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
        run_local(|mut spawn, registry| async move {
            let scheduler = Scheduler::new(
                &config(1),
                &UringBackend::default(),
                &mut spawn,
                &registry,
                clock(),
            );
            let dev = scheduler.register_device("test", &byte_device(1 << 16)).unwrap();
            let h = scheduler.handle();
            let result = h.read(&dev, -1, 0, 4096, TierPriority::Medium).await;
            assert!(result.is_err(), "bad fd read must fail, not hang");
            for pool in dev.pools.all() {
                assert_eq!(pool.debug_free_total(), pool.debug_capacity() as i64);
            }
        });
    }

    /// Dual-wait: a producer that submits a single op to an otherwise-idle ring (the ring is parked in
    /// `submit_and_wait` with nothing else in flight) must have it picked up promptly via the eventfd
    /// wake — not stalled waiting for some unrelated completion. With one op and a generous pool, the
    /// only thing that can break the ring's wait is the eventfd the channel waker writes; if that path
    /// were broken the read would hang and the test would time out (nextest enforces the timeout).
    #[test]
    fn uring_producer_wakes_idle_ring() {
        let path = temp_path("dualwait");
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
        file.write_at(b"dual-wait payload", 0).unwrap();
        file.sync().unwrap();
        run_local(|mut spawn, registry| {
            let path = path.clone();
            async move {
                let scheduler = Scheduler::new(
                    &config(1),
                    &UringBackend::default(),
                    &mut spawn,
                    &registry,
                    clock(),
                );
                let dev = scheduler.register_device("test", &byte_device(1 << 20)).unwrap();
                let h = scheduler.handle();
                // Let the ring thread reach its idle `submit_and_wait` park before we submit, so this
                // exercises the producer-wakes-idle-ring path rather than a busy ring.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let buf = h
                    .read(&dev, fd, 0, b"dual-wait payload".len() as u32, TierPriority::High)
                    .await
                    .expect("idle-ring read must complete via eventfd wake");
                assert_eq!(&buf[..], b"dual-wait payload");
                drop(file);
                let _ = std::fs::remove_file(&path);
            }
        });
    }

    /// Lazy registration on the io_uring backend: a device registered *after* `Scheduler::new` (the
    /// config had no eager devices) is fully functional — its distributor was spawned via the
    /// registrar and credit conserves.
    #[test]
    fn uring_lazy_registration_works() {
        let path = temp_path("lazyreg");
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
        run_local(|mut spawn, registry| {
            let path = path.clone();
            async move {
                // No devices at construction; register one lazily afterwards.
                let scheduler = Scheduler::new(
                    &config(1),
                    &UringBackend::default(),
                    &mut spawn,
                    &registry,
                    clock(),
                );
                let dev = scheduler
                    .register_device("lazy", &byte_device(1 << 20))
                    .expect("lazy registration");
                let h = scheduler.handle();
                let payload = bytes::Bytes::from_static(b"lazily registered device");
                h.write(&dev, fd, 0, payload.clone(), TierPriority::Medium)
                    .await
                    .unwrap();
                let buf = h
                    .read(&dev, fd, 0, payload.len() as u32, TierPriority::High)
                    .await
                    .unwrap();
                assert_eq!(&buf[..], &payload[..]);
                for pool in dev.pools.all() {
                    assert_eq!(pool.debug_free_total(), pool.debug_capacity() as i64);
                }
                drop(file);
                let _ = std::fs::remove_file(&path);
            }
        });
    }

    /// Zero-copy `O_DIRECT` roundtrip through io_uring: write a page-aligned buffer then read it back
    /// into another, in place. `O_DIRECT` is a property of the *fd* (set at open), not the opcode — so
    /// the ring issues the same `Read`/`Write` and the kernel honors the file's flags. Falls back to
    /// buffered on tmpfs (the `direct` open flag is a no-op there), so the test stays portable while
    /// exercising the `IoBuf::Direct` path on real `O_DIRECT` storage when available.
    #[test]
    fn uring_direct_zero_copy_roundtrip() {
        use crate::fs::direct::{AlignedBuf, ALIGNMENT};
        let path = temp_path("direct");
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
        run_local(|mut spawn, registry| {
            let path = path.clone();
            async move {
                let scheduler = Scheduler::new(
                    &config(1),
                    &UringBackend::default(),
                    &mut spawn,
                    &registry,
                    clock(),
                );
                let dev = scheduler.register_device("test", &byte_device(1 << 20)).unwrap();
                let h = scheduler.handle();

                let mut wbuf = AlignedBuf::new(ALIGNMENT);
                for (i, b) in wbuf.as_mut_slice().iter_mut().enumerate() {
                    *b = (i & 0xff) as u8;
                }
                let (wbuf, n) = h
                    .write_direct(&dev, fd, 0, wbuf, TierPriority::Medium)
                    .await
                    .expect("direct write");
                assert_eq!(n, ALIGNMENT);
                drop(wbuf);

                let rbuf = AlignedBuf::new(ALIGNMENT);
                let (rbuf, rn) = h
                    .read_direct(&dev, fd, 0, rbuf, TierPriority::High)
                    .await
                    .expect("direct read");
                assert_eq!(rn, ALIGNMENT, "direct read returned wrong byte count");
                assert_eq!(rbuf.len(), ALIGNMENT, "direct read buffer len not set to n");
                for (i, b) in rbuf.as_slice().iter().enumerate() {
                    assert_eq!(*b, (i & 0xff) as u8, "direct read byte {i} mismatch");
                }

                for pool in dev.pools.all() {
                    assert_eq!(pool.debug_free_total(), pool.debug_capacity() as i64);
                }
                drop(file);
                let _ = std::fs::remove_file(&path);
            }
        });
    }
}
