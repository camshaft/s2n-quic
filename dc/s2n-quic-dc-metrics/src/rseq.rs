// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    mem::MaybeUninit,
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
        Mutex, MutexGuard,
    },
};

// std's `AtomicU128` is unstable (feature `integer_atomics`); this crate builds on stable, so we
// use `portable-atomic`, which lowers a 128-bit CAS to `cmpxchg16b` (x86_64) / `casp` (aarch64
// LSE). See the `TaggedHead` docs for why we need a double-word CAS on the SPMC pool.
use portable_atomic::AtomicU128;

#[cfg(target_os = "linux")]
use std::{cell::Cell, ffi::CStr, ptr::NonNull};

/// The fixed size of a whole [`Page`], including the trailing `length` and `next` fields. A power
/// of two keeps it friendly to the allocator's size classes and free of the dead alignment padding
/// an over-a-boundary struct would carry (the page spilled past this when the intrusive-stack
/// `next` field was added).
const PAGE_SIZE: usize = 64 * 1024;

/// Number of event slots per [`Page`], derived so `slots` plus the trailing `length` and `next`
/// fields fill [`PAGE_SIZE`] exactly. `next` is a pointer, so its width — and hence the slot count —
/// varies by target; we fix the *page size* and let the slot count adapt, rather than the reverse
/// (which would leave a 32-bit page over- or under-sized). Integer division floors, so on a target
/// whose pointer is narrower than a `u64` the few leftover bytes become padding the `align(128)`
/// rounding would absorb anyway.
const SLOTS: usize =
    (PAGE_SIZE - std::mem::size_of::<AtomicU64>() - std::mem::size_of::<*mut Page>())
        / std::mem::size_of::<u64>();

#[repr(C, align(128))]
struct Page {
    // assembly assumes slots is at index 0
    slots: [MaybeUninit<u64>; SLOTS],
    length: AtomicU64,
    // Intrusive stack link. Atomic (not a plain `*mut Page`) because a page can be concurrently
    // read as a stack `head` by one thread while another thread that just popped it writes its
    // `next`; a plain-pointer read/write there would be a data race (UB) even though the CAS is
    // what actually guards correctness. `Relaxed` is sufficient: the head CAS carries the
    // happens-before, `next` only needs to be readable without tearing.
    next: AtomicPtr<Page>,
}

// Lock in the exact page size so a future field addition can't silently push the struct over a
// boundary and reintroduce alignment padding (adjust the slot derivation to compensate instead).
const _: () = assert!(std::mem::size_of::<Page>() == PAGE_SIZE);

/// Test-only counter of every `Page::new` allocation. The whole point of the pool is that once it
/// is warm, recording should recycle pages and stop calling `Page::new`; a conservation test
/// watches this stop growing. (The production leak was exactly this count climbing without bound.)
#[cfg(test)]
static PAGES_ALLOCATED: AtomicU64 = AtomicU64::new(0);

#[cfg(any(test, target_os = "linux"))]
impl Page {
    fn new() -> Box<Page> {
        #[cfg(test)]
        PAGES_ALLOCATED.fetch_add(1, Ordering::Relaxed);
        Box::new(Page {
            slots: [const { MaybeUninit::uninit() }; SLOTS],
            length: AtomicU64::new(0),
            next: AtomicPtr::new(ptr::null_mut()),
        })
    }
}

/// The head cell of a [`Stack`]. This is the only thing that differs between the two page pools,
/// and abstracting it is what lets the compiler *statically* forbid the unsafe operation on each:
///
/// - [`PtrHead`] is a plain `AtomicPtr` — cheap, but its `pop` would be ABA-unsafe, so [`Stack`]
///   does not provide `pop` for it. Used by the MPSC `full_pages` pool (many pushers, one drainer,
///   never popped), whose hot push path stays exactly as fast as before.
/// - [`TaggedHead`] packs a `(generation, pointer)` pair CAS'd as one 128-bit word. The generation
///   is bumped on every mutation, so a recurring pointer value no longer makes a stale CAS succeed
///   — defeating ABA. Used by the SPMC `empty_pages` pool (one pusher under the aggregate lock,
///   many concurrent poppers in `send_event_slow`), the only pool that is `pop`ped.
///
/// A "snapshot" is an opaque observation of the head that carries whatever version information the
/// implementation needs to detect reuse; callers treat it as an all-or-nothing CAS token.
trait Head {
    type Snapshot: Copy;

    fn new() -> Self;

    /// Observe the current head.
    fn load(&self, order: Ordering) -> Self::Snapshot;

    /// The page pointer a snapshot refers to.
    fn ptr(snap: Self::Snapshot) -> *mut Page;

    /// Replace `current` with a head pointing at `new_ptr`, advancing the version on success (for
    /// tagged heads). Returns `Err` on mismatch or spurious failure; callers re-`load` and retry.
    fn compare_exchange_weak(
        &self,
        current: Self::Snapshot,
        new_ptr: *mut Page,
        success: Ordering,
        failure: Ordering,
    ) -> Result<(), ()>;

    /// Unconditionally take the whole chain, leaving the head empty; returns the previous head
    /// pointer. Only used by drain (MPSC).
    fn swap_null(&self, order: Ordering) -> *mut Page;
}

/// Plain-pointer head for the MPSC pool. `push`+`drain` only — never `pop` (ABA-unsafe).
struct PtrHead {
    head: AtomicPtr<Page>,
}

impl Head for PtrHead {
    type Snapshot = *mut Page;

    fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    #[inline]
    fn load(&self, order: Ordering) -> *mut Page {
        self.head.load(order)
    }

    #[inline]
    fn ptr(snap: *mut Page) -> *mut Page {
        snap
    }

    #[inline]
    fn compare_exchange_weak(
        &self,
        current: *mut Page,
        new_ptr: *mut Page,
        success: Ordering,
        failure: Ordering,
    ) -> Result<(), ()> {
        self.head
            .compare_exchange_weak(current, new_ptr, success, failure)
            .map(|_| ())
            .map_err(|_| ())
    }

    #[inline]
    fn swap_null(&self, order: Ordering) -> *mut Page {
        self.head.swap(ptr::null_mut(), order)
    }
}

/// Tagged `(generation, pointer)` head for the SPMC pool: ABA-safe `pop` via a double-word CAS.
struct TaggedHead {
    /// High 64 bits: generation counter. Low 64 bits: the head `*mut Page` (as `u64`). Both are
    /// CAS'd together, so any change to either — including a pointer that recurs to an earlier
    /// value — is observed as a change to the whole word.
    head: AtomicU128,
}

impl TaggedHead {
    #[inline]
    fn pack(generation: u64, ptr: *mut Page) -> u128 {
        ((generation as u128) << 64) | (ptr as u64 as u128)
    }

    #[inline]
    fn unpack(word: u128) -> (u64, *mut Page) {
        ((word >> 64) as u64, (word as u64) as *mut Page)
    }
}

impl Head for TaggedHead {
    type Snapshot = u128;

    fn new() -> Self {
        Self {
            head: AtomicU128::new(0),
        }
    }

    #[inline]
    fn load(&self, order: Ordering) -> u128 {
        self.head.load(order)
    }

    #[inline]
    fn ptr(snap: u128) -> *mut Page {
        Self::unpack(snap).1
    }

    #[inline]
    fn compare_exchange_weak(
        &self,
        current: u128,
        new_ptr: *mut Page,
        success: Ordering,
        failure: Ordering,
    ) -> Result<(), ()> {
        let (generation, _) = Self::unpack(current);
        // Bump the generation on every successful mutation. A u64 counter at the pool's ~hundreds-
        // of-ops/sec rate does not wrap in any realistic process lifetime, so a pointer value never
        // recurs with the same generation.
        let new = Self::pack(generation.wrapping_add(1), new_ptr);
        self.head
            .compare_exchange_weak(current, new, success, failure)
            .map(|_| ())
            .map_err(|_| ())
    }

    #[inline]
    fn swap_null(&self, order: Ordering) -> *mut Page {
        // Not used in production (the SPMC pool is drained by repeated `pop`), but the trait
        // requires it. Preserve the generation-bump discipline for correctness anyway.
        loop {
            let current = self.head.load(order);
            let (generation, ptr) = Self::unpack(current);
            let new = Self::pack(generation.wrapping_add(1), ptr::null_mut());
            if self
                .head
                .compare_exchange_weak(current, new, order, Ordering::Relaxed)
                .is_ok()
            {
                return ptr;
            }
        }
    }
}

/// Lock-free intrusive stack of Pages, generic over its [`Head`] cell.
///
/// `push` is common to both modes (a single CAS; pushing can never cause ABA because it only ever
/// links a new node above the observed head — always a valid chain). `pop` and `drain` are provided
/// only for the head type that can perform them safely, via separate inherent impls below.
struct Stack<H: Head> {
    head: H,
}

impl<H: Head> Stack<H> {
    fn new() -> Self {
        Self { head: H::new() }
    }

    fn push(&self, page: Box<Page>) {
        let raw = Box::into_raw(page);
        loop {
            let current = self.head.load(Ordering::Relaxed);
            unsafe { (*raw).next.store(H::ptr(current), Ordering::Relaxed) };
            if self
                .head
                .compare_exchange_weak(current, raw, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }
}

/// `pop` is available only on the ABA-safe tagged head (the SPMC `empty_pages` pool).
impl Stack<TaggedHead> {
    #[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
    fn pop(&self) -> Option<Box<Page>> {
        loop {
            let current = self.head.load(Ordering::Acquire);
            let head_ptr = TaggedHead::ptr(current);
            if head_ptr.is_null() {
                return None;
            }
            let next = unsafe { (*head_ptr).next.load(Ordering::Relaxed) };

            // Test-only seam: fire a caller-supplied interleaving in the exact window between
            // reading `next` and the CAS below — the classic ABA window. With the tagged head the
            // generation has moved when the head pointer recurs, so the CAS fails and we retry
            // instead of installing a stale `next`. The test asserts that conservation holds.
            #[cfg(test)]
            aba_hook::fire();

            if self
                .head
                .compare_exchange_weak(current, next, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                unsafe { (*head_ptr).next.store(ptr::null_mut(), Ordering::Relaxed) };
                return Some(unsafe { Box::from_raw(head_ptr) });
            }
        }
    }
}

/// `drain` (single `swap` of the whole chain) is available only on the plain-pointer head (the
/// MPSC `full_pages` pool).
impl Stack<PtrHead> {
    fn drain(&self, mut f: impl FnMut(Box<Page>)) {
        let mut cursor = self.head.swap_null(Ordering::Acquire);
        while !cursor.is_null() {
            let next = unsafe { (*cursor).next.load(Ordering::Relaxed) };
            unsafe { (*cursor).next.store(ptr::null_mut(), Ordering::Relaxed) };
            f(unsafe { Box::from_raw(cursor) });
            cursor = next;
        }
    }
}

/// Test-only injection point used to reproduce the `pop` ABA window deterministically. A test
/// installs a one-shot closure that runs between `pop`'s read of `next` and its CAS.
#[cfg(test)]
mod aba_hook {
    use std::cell::RefCell;

    thread_local! {
        static HOOK: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
    }

    /// Install a one-shot hook; it disarms itself the first time it fires so the interleaving is
    /// injected exactly once (nested `pop`s inside the hook do not re-enter it).
    pub(super) fn arm(f: impl FnMut() + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
    }

    pub(super) fn fire() {
        // Take the hook out before running it so a `pop` performed *inside* the hook sees an empty
        // slot and does not recurse.
        let hook = HOOK.with(|h| h.borrow_mut().take());
        if let Some(mut f) = hook {
            f();
        }
    }
}

const PAGE_POOL_SHARDS: usize = 2;
const PAGE_POOL_MASK: usize = PAGE_POOL_SHARDS - 1;

/// Sharded page pool. Each shard is a lock-free intrusive [`Stack`]. Producers pick a shard via
/// `cpu_hint & MASK`, distributing contention across shards so the CAS loop almost never retries.
///
/// Generic over the [`Head`] type so the two pools get exactly the operations that are safe for
/// their access pattern:
/// - `full_pages: ShardedPagePool<PtrHead>` — `push` + `drain`, never `pop`.
/// - `empty_pages: ShardedPagePool<TaggedHead>` — `push` + `pop`, ABA-safe.
struct ShardedPagePool<H: Head> {
    shards: [Stack<H>; PAGE_POOL_SHARDS],
}

impl<H: Head> ShardedPagePool<H> {
    fn new() -> Self {
        Self {
            shards: [Stack::new(), Stack::new()],
        }
    }

    #[inline]
    fn push(&self, page: Box<Page>, cpu_hint: usize) {
        self.shards[cpu_hint & PAGE_POOL_MASK].push(page);
    }
}

impl ShardedPagePool<TaggedHead> {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn pop(&self, cpu_hint: usize) -> Option<Box<Page>> {
        let start = cpu_hint & PAGE_POOL_MASK;
        for i in 0..PAGE_POOL_SHARDS {
            let shard = &self.shards[(start + i) & PAGE_POOL_MASK];
            if let Some(page) = shard.pop() {
                return Some(page);
            }
        }
        None
    }
}

impl ShardedPagePool<PtrHead> {
    fn drain(&self, mut f: impl FnMut(Box<Page>)) {
        for shard in &self.shards {
            shard.drain(&mut f);
        }
    }
}

fn possible_cpus() -> usize {
    let Ok(content) = fs::read_to_string("/sys/devices/system/cpu/possible") else {
        // As a fallback, ask Rust to provide us how much parallelism we have. This is **not**
        // the best option because there's no guarantee the parallelism matches up with the CPU
        // indices returned by the kernel, but in practice in our environments it's fairly likely
        // to do what we want.
        if let Ok(parallelism) = std::thread::available_parallelism() {
            return parallelism.get();
        } else {
            static PRINTED_WARNING: AtomicBool = AtomicBool::new(false);
            if !PRINTED_WARNING.swap(true, Ordering::Relaxed) {
                eprintln!("failed to identify CPU count, falling back to 4 fast CPUs");
            }
            // If neither option worked, default to 4 CPU cores. This essentially means that we
            // will have great performance on those 4 cores and terrible performance elsewhere.
            //
            // Our critical section will bail out to the fallback path if we're on a CPU with index
            // larger than this, so this really just an arbitrary value.
            return 4;
        }
    };

    let max_cpu = content
        .trim()
        .split(',')
        .map(|range| {
            if let Some((_start, end)) = range.split_once('-') {
                end.parse::<usize>().unwrap_or(0)
            } else {
                range.parse::<usize>().unwrap_or(0)
            }
        })
        .max()
        .unwrap_or(0);

    max_cpu.max(1)
}

fn init_per_cpu() -> Box<[AtomicPtr<Page>]> {
    (0..=possible_cpus())
        .map(|_| AtomicPtr::new(std::ptr::null_mut()))
        .collect()
}

/// Each CPU core populates a `page` until it fills up, and then pushes events into aggregate.
///
/// We also support stealing pages from all CPUs (`steal_pages`). Without that mechanism we'd leave
/// behind metrics on a CPU core that did some work and then went idle.
///
/// `T` is a type which aggregates incoming events. `T`'s are allocated typically per registered
/// metric, and when we're absorbing a page of events we'll do so under the `aggregate` lock.
///
/// This does mean that if events are flowing at a very high rate there may be contention on
/// `aggregate`; we retain the lock for now for simplicity. We could have a background thread that
/// aggregates events, but then we'd either need to drop events or have unbounded memory. We could
/// also have a shared-atomic aggregate, but that increases memory usage or requires another
/// somewhat complicated data structure (per-index Arc / Vec with lock-free access respectively).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct Channels<T: Absorb> {
    per_cpu: Box<[AtomicPtr<Page>]>,

    /// If true, it's not possible for us to use the per_cpu events. This primarily happens if we
    /// fail to register to use membarrier. Note that even if false fallback may still be used
    /// (e.g. because we fail to register rseq).
    must_use_fallback: bool,

    fallback: crossbeam_queue::SegQueue<u64>,

    // Recycled empty pages waiting to be handed back to recording CPUs. SPMC: one pusher (whoever
    // holds `aggregate` during a steal) recycles into it; many recorders `pop` from it in
    // `send_event_slow`. Its `pop` is the ABA-sensitive operation, so it uses the tagged head.
    empty_pages: ShardedPagePool<TaggedHead>,

    // Filled pages handed off by recorders, awaiting aggregation. MPSC: many recorders `push`, a
    // single stealer `drain`s. Never `pop`ped, so its plain-pointer head keeps the hot push path
    // as cheap as before.
    full_pages: ShardedPagePool<PtrHead>,

    // What we aggregate events into.
    aggregate: Mutex<Vec<T>>,
}

impl<T: Absorb> Drop for Channels<T> {
    fn drop(&mut self) {
        // Make sure we don't leak pages.
        self.steal_pages();
    }
}

pub(crate) trait Absorb: Sized + Default {
    fn handle(slots: &mut [Self], events: &mut [u64]);
}

#[cfg(target_os = "linux")]
static PRINTED_MEMBARRIER_WARNING: AtomicBool = AtomicBool::new(false);

impl<T: Absorb> Channels<T> {
    #[cfg_attr(not(target_os = "linux"), allow(unused_assignments))]
    pub(crate) fn new() -> Self {
        let mut must_use_fallback = false;

        #[cfg(target_os = "linux")]
        {
            let ret = unsafe {
                libc::syscall(
                    libc::SYS_membarrier,
                    libc::MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED,
                    0,
                )
            };
            if ret != 0 {
                if !PRINTED_MEMBARRIER_WARNING.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "failed to register membarrier: {:?}, {:?}",
                        ret,
                        std::io::Error::last_os_error()
                    );
                }
                must_use_fallback = true;
            }

            #[cfg(target_arch = "aarch64")]
            if !std::arch::is_aarch64_feature_detected!("lse") {
                must_use_fallback = true;
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            must_use_fallback = true;
        }

        Channels {
            must_use_fallback,
            per_cpu: init_per_cpu(),
            fallback: Default::default(),
            empty_pages: ShardedPagePool::new(),
            full_pages: ShardedPagePool::new(),

            aggregate: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn get_mut<R>(&self, idx: u32, mut cb: impl FnMut(&mut T) -> R) -> R {
        let mut guard = self.aggregate.lock().unwrap();
        cb(&mut guard[idx as usize])
    }

    pub(crate) fn allocate(&self) -> u32 {
        let mut guard = self.aggregate.lock().unwrap();
        let len = u32::try_from(guard.len()).unwrap();
        guard.push(T::default());
        len
    }

    /// Folds only the *full pages* (those handed off by `send_event_slow`) and the fallback queue
    /// into the aggregate. Deliberately does **not** touch the per-CPU pages, so it needs no
    /// `membarrier` and no per-CPU swap — it is the cheap periodic compaction path behind
    /// [`Registry::absorb`].
    ///
    /// This is sound without a membarrier: `full_pages` entries were published by the recorder's
    /// Release CAS in `Stack::push`, and `drain`'s Acquire load synchronizes with it, so we observe
    /// their event writes. The per-CPU pages, which *are* still being written under a relaxed store
    /// and would need the cross-core fence, are left in place until the next `steal_pages`. Leaving
    /// them also keeps their footprint bounded to one page per CPU and avoids the pop/push churn of
    /// forcing every active CPU onto a fresh page each interval.
    pub(crate) fn absorb_full_pages(&self) {
        let mut aggregate = self.aggregate.lock().unwrap();
        self.fold_full_pages(&mut aggregate);
        self.drain_fallback(&mut aggregate);
    }

    /// Drains `full_pages` into the aggregate, recycling each folded page into `empty_pages`. Shared
    /// by the absorb and the full-steal paths.
    fn fold_full_pages(&self, aggregate: &mut [T]) {
        // Spread recycled pages across the empty-pool shards (round-robin) rather than funneling
        // them all onto shard 0, so concurrent poppers in `send_event_slow` land on different
        // shards and contend less. `pop` scans every shard, so this is purely a contention win.
        let mut recycle_hint = 0usize;
        self.full_pages.drain(|page| {
            Self::aggregate_page(aggregate, page, &self.empty_pages, recycle_hint);
            recycle_hint = recycle_hint.wrapping_add(1);
        });
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn steal_pages(&self) {
        let mut aggregate = self.aggregate.lock().unwrap();
        self.fold_full_pages(&mut aggregate);
        self.drain_fallback(&mut aggregate);
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn steal_pages(&self) {
        let mut aggregate = self.aggregate.lock().unwrap();

        // Drain any full pages enqueued by send_event_slow
        self.fold_full_pages(&mut aggregate);

        if self.must_use_fallback {
            self.drain_fallback(&mut aggregate);

            // Don't look at per_cpu structures if we're in the only-fallback path.
            return;
        }

        let pages = self
            .per_cpu
            .iter()
            .map(|cpu| cpu.swap(std::ptr::null_mut(), Ordering::Relaxed))
            .collect::<Vec<_>>();

        // In theory this is infallible because we successfully registered membarrier above.
        //
        // If this fails it's UB to read from the stolen pages since we don't actually own them.
        // For now treat failure as a fatal condition and abort the process since there's no good
        // recovery path. If we're willing to leak a few sets of pages, it's probably possible to
        // leak these pages and then instruct all CPUs to use a stronger memory ordering (primarily
        // on aarch64) when finishing writes to the pages. But that doesn't seem obviously better
        // given the assumption this is infallible.
        //
        // FIXME: We should confirm via benchmarks that we actually need this. On x86_64 if we
        // preallocate all Pages the `mov` to increment length is implicitly a Release we could
        // Acquire synchronize with here. On aarch64 though we'd need to use a stronger memory
        // ordering instruction which is more expensive (maybe unaffordably so).
        let ret = unsafe {
            libc::syscall(
                libc::SYS_membarrier,
                libc::MEMBARRIER_CMD_PRIVATE_EXPEDITED,
                0,
            )
        };
        if ret != 0 {
            eprintln!(
                "failed to membarrier: {:?}, {:?}",
                ret,
                std::io::Error::last_os_error()
            );
            std::process::abort();
        }

        // All other CPUs have now flushed their memory stores and are guaranteed to
        // exit any ongoing RSEQ sections too due to membarrier. This means that (a) our thread
        // will see any events they've written and (b) they are no longer writing to the pages in
        // `PER_CPU`, which is sufficient to allow us to process all the events they've sent.

        for (recycle_hint, page) in pages.into_iter().enumerate() {
            if !page.is_null() {
                Self::aggregate_page(
                    &mut aggregate,
                    unsafe { Box::from_raw(page) },
                    &self.empty_pages,
                    recycle_hint,
                );
            }
        }

        self.drain_fallback(&mut aggregate);
    }

    fn aggregate_page(
        aggregate: &mut [T],
        mut page: Box<Page>,
        empty_pages: &ShardedPagePool<TaggedHead>,
        recycle_hint: usize,
    ) {
        let length = *page.length.get_mut() as usize;
        let filled = unsafe { &mut *(&mut page.slots[..length] as *mut [_] as *mut [u64]) };
        T::handle(aggregate, filled);

        *page.length.get_mut() = 0;
        empty_pages.push(page, recycle_hint);
    }

    pub(crate) fn lock_aggregate(&self) -> MutexGuard<'_, Vec<T>> {
        self.aggregate.lock().expect("propagate panic")
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn send_event(&self, event: u64) {
        self.fallback_push(event)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn send_event(&self, event: u64) {
        if self.must_use_fallback {
            return self.fallback_push(event);
        }

        let rseq_ptr = rseq();
        self.send_event_inner(event, rseq_ptr);
    }

    // Separate function for unit testing.
    #[cfg(target_os = "linux")]
    fn send_event_inner(&self, event: u64, rseq_ptr: NonNull<Rseq>) {
        unsafe {
            #[cfg(target_arch = "x86_64")]
            std::arch::asm!(
                "
                .pushsection __rseq_cs, \"aw\"
                .balign 32
                9:
                .long 0
                .long 0
                .quad 2f
                .quad (6f-2f)
                .quad 7f
                .popsection

                // The kernel ABI requires that the address we abort to is prefixed with the
                // RSEQ_SIG. This reduces the likelihood that writes to the descriptor block
                // (declared above) allow branching to arbitrary addresses in the code which makes
                // things easier for attackers.
                //
                // In the future we can optimize by moving the abort sequence out of the
                // primary instruction stream somehow so that we don't need this unconditional
                // jump.
                jmp 7f
                .long {RSEQ_SIG}
                7:
                mov {cpu_id:e}, [{rseq_ptr}+{cpu_id_offset_start}]

                // If the CPU index returned by rseq is too high, then we bail out
                // to our fallback path. This also handles the case that rseq failed (-1 or
                // u32::MAX is definitely out of range).
                cmp {cpu_id}, {per_cpu_len}
                jge {fallback}

                // Only attempt looping through rseq a limited number of times to make progress if
                // we're continuously aborting for some reason.
                dec {loop_count}
                jz {fallback}

                lea {tmp}, [rip+9b]
                mov [{rseq_ptr}+{rseq_cs_offset}], {tmp}

                // Everything following this is the critical section. It must be capable of
                // restarting after any instruction except the last one with no harmful effects.
                2:

                // Check that the CPU ID matches with the one loaded above.
                //
                // See https://google.github.io/tcmalloc/rseq.html#cpu-ids for good docs on why
                // there's two cpu_id fields.
                cmp {cpu_id:e}, [{rseq_ptr}+{cpu_id_offset}]
                jnz 7b

                mov {page_ptr}, [{per_cpu_base}+{cpu_id}*8]
                test {page_ptr}, {page_ptr}
                jz {needs_new_page}

                mov {tmp}, [{page_ptr}+{length_offset}]
                cmp {tmp}, {SLOTS}
                jge {needs_new_page}

                // page is non-null + non-empty

                // write the event to the current slot
                mov [{page_ptr}+{tmp}*8], {event}

                // increment the length
                inc {tmp}

                // length update must be the last instruction,
                // and must be a relaxed atomic store.
                mov [{page_ptr}+{length_offset}], {tmp}
                6:

                // Clear the rseq block as being in the critical section.
                // AFAICT, this isn't required, but tcmalloc's docs recommend it and it's
                // relatively cheap.
                mov QWORD PTR [{rseq_ptr}+{rseq_cs_offset}], 0
                ",
                rseq_ptr = in(reg) rseq_ptr.as_ptr(),
                cpu_id = out(reg) _,
                page_ptr = out(reg) _,
                tmp = out(reg) _,
                loop_count = inout(reg) 5u64 => _,
                per_cpu_base = in(reg) self.per_cpu.as_ptr(),
                per_cpu_len = in(reg) self.per_cpu.len(),
                event = in(reg) event,
                cpu_id_offset = const std::mem::offset_of!(Rseq, cpu_id),
                cpu_id_offset_start = const std::mem::offset_of!(Rseq, cpu_id_start),
                rseq_cs_offset = const std::mem::offset_of!(Rseq, rseq_cs),
                length_offset = const std::mem::offset_of!(Page, length),
                RSEQ_SIG = const RSEQ_SIG,
                SLOTS = const SLOTS,
                needs_new_page = label {
                    self.send_event_slow(rseq_ptr, event);
                },
                fallback = label {
                    self.fallback_push(event);
                },
                options(nostack)
            );

            #[cfg(target_arch = "aarch64")]
            std::arch::asm!(
                "
                .pushsection __rseq_cs, \"aw\"
                .balign 32
                9:
                .long 0
                .long 0
                .quad 2f
                .quad (6f-2f)
                .quad 7f
                .popsection

                b 7f
                .long {RSEQ_SIG}
                7:
                ldr {cpu_id:w}, [{rseq_ptr}, #{cpu_id_offset_start}]

                cmp {cpu_id:w}, {per_cpu_len:w}
                b.hs {fallback}

                subs {loop_count}, {loop_count}, #1
                b.eq {fallback}

                adrp {tmp}, 9b
                add {tmp}, {tmp}, :lo12:9b
                str {tmp}, [{rseq_ptr}, #{rseq_cs_offset}]

                2:
                ldr {tmp:w}, [{rseq_ptr}, #{cpu_id_offset}]
                cmp {cpu_id:w}, {tmp:w}
                b.ne 7b

                ldr {page_ptr}, [{per_cpu_base}, {cpu_id}, lsl #3]
                cbz {page_ptr}, {needs_new_page}

                ldr {tmp}, [{page_ptr}, {length_offset}]
                cmp {tmp}, {SLOTS}
                b.ge {needs_new_page}

                str {event}, [{page_ptr}, {tmp}, lsl #3]

                add {tmp}, {tmp}, #1

                str {tmp}, [{page_ptr}, {length_offset}]
                6:
                str xzr, [{rseq_ptr}, #{rseq_cs_offset}]
                ",
                rseq_ptr = in(reg) rseq_ptr.as_ptr(),
                cpu_id = out(reg) _,
                page_ptr = out(reg) _,
                tmp = out(reg) _,
                loop_count = inout(reg) 5u64 => _,
                per_cpu_base = in(reg) self.per_cpu.as_ptr(),
                per_cpu_len = in(reg) self.per_cpu.len(),
                event = in(reg) event,
                cpu_id_offset = const std::mem::offset_of!(Rseq, cpu_id),
                cpu_id_offset_start = const std::mem::offset_of!(Rseq, cpu_id_start),
                rseq_cs_offset = const std::mem::offset_of!(Rseq, rseq_cs),
                // too large for constant offset
                length_offset = in(reg) std::mem::offset_of!(Page, length),
                RSEQ_SIG = const RSEQ_SIG,
                // too large for constant
                SLOTS = in(reg) SLOTS,
                needs_new_page = label {
                    // SAFETY: lse is feature detected above and fallback is forced if it's not
                    // present, which means we never hit this code.
                    //
                    // lse is present on all aarch64 CPUs we'd expect run on (Graviton 2 has it).
                    #[allow(unused_unsafe)]
                    unsafe {
                        self.send_event_slow(rseq_ptr, event);
                    }
                },
                fallback = label {
                    self.fallback_push(event);
                },
                options(nostack)
            );
        }
    }

    #[inline(never)]
    fn fallback_push(&self, event: u64) {
        self.fallback.push(event);
    }

    fn drain_fallback(&self, aggregate: &mut [T]) {
        let mut buffer = Vec::with_capacity(SLOTS * 2);
        while let Some(event) = self.fallback.pop() {
            buffer.push(event);
        }
        if !buffer.is_empty() {
            T::handle(aggregate, &mut buffer);
        }
    }

    #[cold]
    #[cfg(target_os = "linux")]
    #[cfg_attr(target_arch = "aarch64", target_feature(enable = "lse"))]
    fn send_event_slow(&self, rseq_ptr: NonNull<Rseq>, serialized_event: u64) {
        let cpu_hint = unsafe { (*rseq_ptr.as_ptr()).cpu_id_start } as usize;
        let mut new_page = self.empty_pages.pop(cpu_hint).unwrap_or_else(Page::new);

        new_page.slots[0].write(serialized_event);
        new_page.length.store(1, Ordering::Relaxed);

        let mut taken: *mut Page = Box::into_raw(new_page);

        let mut fallback: u8 = 0;
        unsafe {
            #[cfg(target_arch = "x86_64")]
            std::arch::asm!(
                "
                .pushsection __rseq_cs, \"aw\"
                .balign 32
                12:
                .long 0
                .long 0
                .quad 3f
                .quad (7f-3f)
                .quad 8f
                .popsection

                jmp 8f
                .long {RSEQ_SIG}
                8:
                mov {cpu_id:e}, [{rseq_ptr}+{cpu_id_offset_start}]

                // If the CPU index returned by rseq is too high, then we bail out
                // to our fallback path. This also handles the case that rseq failed (-1 or
                // u32::MAX is definitely out of range).
                cmp {cpu_id}, {per_cpu_len}
                setge {fallback}
                jge 7f

                // Only attempt looping through rseq a limited number of times to make progress if
                // we're continuously aborting for some reason.
                dec {loop_count}
                setz {fallback}
                jz 7f

                lea {tmp}, [rip+12b]
                mov [{rseq_ptr}+{rseq_cs_offset}], {tmp}

                3:
                cmp {cpu_id:e}, [{rseq_ptr}+{cpu_id_offset}]
                jnz 8b

                xchg {taken}, [{per_cpu_base}+{cpu_id}*8]

                7:
                mov QWORD PTR [{rseq_ptr}+{rseq_cs_offset}], 0
                ",
                rseq_ptr = in(reg) rseq_ptr.as_ptr(),
                cpu_id = out(reg) _,
                tmp = out(reg) _,
                loop_count = inout(reg) 5u64 => _,
                per_cpu_base = in(reg) self.per_cpu.as_ptr(),
                per_cpu_len = in(reg) self.per_cpu.len(),
                taken = inout(reg) taken,
                cpu_id_offset = const std::mem::offset_of!(Rseq, cpu_id),
                cpu_id_offset_start = const std::mem::offset_of!(Rseq, cpu_id_start),
                rseq_cs_offset = const std::mem::offset_of!(Rseq, rseq_cs),
                RSEQ_SIG = const RSEQ_SIG,
                fallback = inout(reg_byte) fallback,
                options(nostack)
            );

            #[cfg(target_arch = "aarch64")]
            std::arch::asm!(
                "
                .pushsection __rseq_cs, \"aw\"
                .balign 32
                12:
                .long 0
                .long 0
                .quad 3f
                .quad (7f-3f)
                .quad 8f
                .popsection

                b 8f
                .long {RSEQ_SIG}
                8:
                ldr {cpu_id:w}, [{rseq_ptr}, #{cpu_id_offset_start}]

                cmp {cpu_id:w}, {per_cpu_len:w}
                cset {fallback:w}, hs
                b.hs 7f

                subs {loop_count}, {loop_count}, #1
                cset {fallback:w}, eq
                b.eq 7f

                adrp {tmp}, 12b
                add {tmp}, {tmp}, :lo12:12b
                str {tmp}, [{rseq_ptr}, #{rseq_cs_offset}]

                3:
                ldr {tmp2:w}, [{rseq_ptr}, #{cpu_id_offset}]
                cmp {cpu_id:w}, {tmp2:w}
                b.ne 8b

                add {tmp}, {per_cpu_base}, {cpu_id}, lsl #3
                swp {taken}, {taken}, [{tmp}]

                7:
                str xzr, [{rseq_ptr}, #{rseq_cs_offset}]
                ",
                rseq_ptr = in(reg) rseq_ptr.as_ptr(),
                cpu_id = out(reg) _,
                tmp = out(reg) _,
                tmp2 = out(reg) _,
                loop_count = inout(reg) 5u64 => _,
                per_cpu_base = in(reg) self.per_cpu.as_ptr(),
                per_cpu_len = in(reg) self.per_cpu.len(),
                taken = inout(reg) taken,
                cpu_id_offset = const std::mem::offset_of!(Rseq, cpu_id),
                cpu_id_offset_start = const std::mem::offset_of!(Rseq, cpu_id_start),
                rseq_cs_offset = const std::mem::offset_of!(Rseq, rseq_cs),
                RSEQ_SIG = const RSEQ_SIG,
                fallback = inout(reg) fallback,
                options(nostack)
            );
        }
        let fallback = fallback != 0;

        if fallback {
            // Because we hit fallback there shouldn't have been opportunity for the code to
            // exchange, so it shouldn't be possible for it to be null.
            assert!(!taken.is_null());

            // We failed to xchg `taken` with the page in the per_cpu[current] slot. As such
            // `taken` is still owned by us. Enqueue for background aggregation.
            self.full_pages
                .push(unsafe { Box::from_raw(taken) }, cpu_hint);
        } else {
            if taken.is_null() {
                return;
            }

            // The old page was swapped out successfully. Enqueue for background aggregation
            // rather than blocking the recording thread on the aggregate mutex.
            self.full_pages
                .push(unsafe { Box::from_raw(taken) }, cpu_hint);
        }
    }
}

#[cfg(target_os = "linux")]
thread_local! {
    static RSEQ: Cell<Option<NonNull<Rseq>>> = const { Cell::new(None) };

    static RSEQ_ALLOC: Cell<Option<RseqStorage>> = const { Cell::new(None) };
}

#[cfg(target_os = "linux")]
struct RseqStorage {
    slot: Box<Rseq>,
    registered: bool,
}

#[cfg(target_os = "linux")]
impl Drop for RseqStorage {
    fn drop(&mut self) {
        let Some(taken_address) = RSEQ.take() else {
            return;
        };

        if !self.registered {
            return;
        }

        // Unregister rseq before we free the memory. This avoids the kernel writing to it while
        // the thread is dying and clobbering something else that happens to get allocated there.
        if let Err(e) = sys_rseq(
            taken_address.as_ptr(),
            1i32, /* RSEQ_FLAGS_UNREGISTER */
        ) {
            eprintln!("failed to deregister rseq on thread death: {e:?}");
        }
    }
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[repr(align(32))]
pub(crate) struct Rseq {
    cpu_id_start: u32,
    cpu_id: u32,
    rseq_cs: u64,
    flags: u32,
}

#[cfg(target_os = "linux")]
// Note that NonNull is !Send + !Sync, so the compiler protects us against accessing this
// cross-thread.
pub(crate) fn rseq() -> NonNull<Rseq> {
    if let Some(ptr) = RSEQ.get() {
        return ptr;
    }

    rseq_init()
}

// Note that this is defined by glibc for both x86_64 and aarch64 as it owns rseq registration on
// AL2023+ (see /usr/include/bits/rseq.h on an AL2023 system).
//
// For AL2 for simplicity we use the same RSEQ constant.
//
// This is part of the glibc ABI, we can't influence this value in any way.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const RSEQ_SIG: u32 = 0x53053053;

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const RSEQ_SIG: u32 = 0xd428bc00;

#[cfg(all(test, target_os = "linux"))]
static RSEQ_FAILED: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "linux")]
#[cold]
fn rseq_init() -> NonNull<Rseq> {
    // If we successfully fetch rseq from libc, we **don't** touch the RSEQ_ALLOC thread local at
    // all, which means there's nothing to deregister or drop.
    //
    // Note that caching the value of rseq like this is possibly error prone if there's access to
    // RSEQ during thread death, when the thread locals are dropped in ~random order. But there's
    // not too much we can do about that, glibc doesn't provide an interface that lets us check
    // whether the thread local it allocates is still around. So just assume that's not an issue.
    // A working theory is that glibc doesn't reuse the thread local memory while the thread is
    // still alive.
    if let Ok(libc_rseq) = from_libc() {
        let ptr = NonNull::new(libc_rseq).unwrap();
        RSEQ.set(Some(ptr));
        return ptr;
    }

    // Register the main thread with rseq.
    let mut rseq_ptr = RseqStorage {
        slot: Box::new(Rseq {
            cpu_id_start: 0,
            cpu_id: 0,
            rseq_cs: 0,
            flags: 0,
        }),
        registered: false,
    };

    RSEQ.set(Some(NonNull::new(&raw mut *rseq_ptr.slot).unwrap()));
    RSEQ_ALLOC.set(Some(rseq_ptr));
    let rseq_ptr = RSEQ.get().unwrap();

    match sys_rseq(rseq_ptr.as_ptr(), 0) {
        Ok(()) => {
            RSEQ_ALLOC.with(|c| {
                let mut v = c.take().expect("just set above");
                v.registered = true;
                c.set(Some(v));
            });
        }
        Err(e) => {
            eprintln!("rseq failed to register: {e:?}");
            // Mark the structure as unregistered.
            //
            // In theory the kernel has done this but this helps make that a stronger
            // guarantee. This is necessary so that our assembly will bail out to the fallback
            // path rather than e.g. all threads thinking they are on CPU 0.
            #[cfg(test)]
            RSEQ_FAILED.store(true, Ordering::Relaxed);
            unsafe {
                (*rseq_ptr.as_ptr()).cpu_id_start = u32::MAX;
            }
        }
    };

    rseq_ptr
}

#[cfg(target_os = "linux")]
fn dlsym(symbol: &CStr) -> std::io::Result<*mut std::ffi::c_void> {
    unsafe {
        // clear previous errors
        let _ = libc::dlerror();
        let address = libc::dlsym(libc::RTLD_DEFAULT, symbol.as_ptr());
        if let Some(ptr) = NonNull::new(libc::dlerror()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "failed to dlsym {symbol:?}: {:?}",
                    std::ffi::CStr::from_ptr(ptr.as_ptr())
                ),
            ));
        }
        Ok(address)
    }
}

#[cfg(target_os = "linux")]
fn thread_plus_offset(offset: libc::ptrdiff_t) -> *mut std::ffi::c_void {
    let output: *mut std::ffi::c_void;
    // As far as I can tell, both of these should work in the most general case.
    //
    // Online references suggest you need e.g. __tls_get_addr and similar, but it seems to me that
    // glibc's __rseq_offset already takes care of calling that for us if needed, so we just need
    // the base pointer which is already conveniently stored in a register for us.
    unsafe {
        #[cfg(target_arch = "aarch64")]
        std::arch::asm!("mrs {output}, tpidr_el0", output = out(reg) output);

        #[cfg(target_arch = "x86_64")]
        std::arch::asm!("mov {output}, fs:0", output = out(reg) output);
    }
    output.wrapping_offset(offset)
}

#[cfg(target_os = "linux")]
fn from_libc() -> std::io::Result<*mut Rseq> {
    let _size = dlsym(c"__rseq_size")?.cast::<u32>();
    let offset = dlsym(c"__rseq_offset")?.cast::<libc::ptrdiff_t>(); // ptrdiff_t
    let _flags = dlsym(c"__rseq_flags")?.cast::<u32>();

    Ok(thread_plus_offset(unsafe { offset.read() }).cast())
}

#[cfg(target_os = "linux")]
fn sys_rseq(rseq_abi: *mut Rseq, flags: i32) -> std::io::Result<()> {
    let ret = unsafe {
        libc::syscall(
            libc::SYS_rseq,
            rseq_abi,
            std::mem::size_of::<Rseq>() as u32,
            flags,
            RSEQ_SIG,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Platform-independent tests for the page-pool stacks themselves. These exercise
/// `PageStack`/`ShardedPagePool` directly (no rseq asm), so they run on every target — including
/// the macOS dev machines where the rseq fast path is compiled out.
#[cfg(test)]
mod stack_tests {
    use super::*;

    /// Reproduces the ABA hazard in `PageStack::pop` deterministically and asserts page
    /// conservation. Every page created must end up in exactly one of two disjoint sets: *owned* by
    /// a caller, or *reachable* from the stack head. A page in neither is leaked; a page in both is
    /// double-owned (a latent double-free). The ABA interleaving produces both at once.
    ///
    /// Interleaving (mirrors production: one pusher, many poppers). Stack starts `head -> A -> B`:
    ///   1. Popper P1 enters `pop`, reads `head = A`, caches `next = A.next = B`, then stalls in
    ///      the window (via the test hook) before its CAS.
    ///   2. While stalled: P2 pops A (head -> B), then P2 pops B (head -> null). Both A and B are
    ///      now owned outside the stack.
    ///   3. The pusher recycles a fresh page C (head -> C), then recycles A on top (head -> A -> C),
    ///      so A's address is back at the top but `A.next` is now C, not the stale B.
    ///   4. P1 resumes and CASes (expected = A, new = stale B). Under the bug, head *is* A again so
    ///      the CAS succeeds and installs B as head: C is orphaned (leaked) and B is double-owned.
    ///      Under the fix, the generation moved, P1's CAS fails and it retries onto C -> conserved.
    ///
    /// Pages are tracked by raw pointer and intentionally never freed here (the buggy path would
    /// otherwise double-free the double-owned page and abort instead of failing the assertion). A
    /// few leaked pages in a unit test is fine; the point is the conservation check.
    #[test]
    fn pop_aba_conserves_pages() {
        // Distinct id per page, written into slot 0; survives push/pop (which only touch `.next`).
        fn tag(p: *const Page) -> u64 {
            unsafe { (*p).slots[0].assume_init() }
        }
        fn make(id: u64) -> *mut Page {
            let mut p = Page::new();
            p.slots[0].write(id);
            Box::into_raw(p)
        }

        // The SPMC pool's stack is the only one that is `pop`ped, so it is the one that must be
        // ABA-safe. Under the buggy plain-pointer head this test failed; under the tagged head it
        // passes.
        let stack = Stack::<TaggedHead>::new();

        // Seed the stack: head -> A(1) -> B(2).
        let a = make(1);
        let b = make(2);
        stack.push(unsafe { Box::from_raw(b) });
        stack.push(unsafe { Box::from_raw(a) });

        // Pages popped out of the stack during the interleaving ("owned by a caller").
        let mut owned: Vec<*mut Page> = Vec::new();

        // Arm the hook that fires inside P1's pop, in the window after it cached `next` and before
        // its CAS. Single-threaded, so the raw-pointer captures never alias concurrently; `fire()`
        // also removes the hook before running it, so the reentrant pops below don't recurse.
        let stack_ptr: *const Stack<TaggedHead> = &stack;
        let owned_ptr: *mut Vec<*mut Page> = &mut owned;
        aba_hook::arm(move || {
            let stack = unsafe { &*stack_ptr };
            let owned = unsafe { &mut *owned_ptr };
            // P2 pops A and B out of the stack.
            owned.push(Box::into_raw(stack.pop().expect("P2 pops A")));
            owned.push(Box::into_raw(stack.pop().expect("P2 pops B")));
            // Pusher recycles a fresh C, then recycles A on top -> head -> A -> C.
            stack.push(unsafe { Box::from_raw(make(3)) });
            let i = owned.iter().position(|&p| tag(p) == 1).expect("A popped");
            let a = owned.remove(i);
            stack.push(unsafe { Box::from_raw(a) });
        });

        // P1's pop resumes after the hook and does its (possibly stale) CAS.
        owned.push(Box::into_raw(stack.pop().expect("P1 pops something")));

        // Walk the stack chain read-only to collect the pages still reachable from head.
        let mut reachable: Vec<*mut Page> = Vec::new();
        let mut cur = TaggedHead::ptr(stack.head.load(Ordering::Acquire));
        while !cur.is_null() {
            reachable.push(cur);
            cur = unsafe { (*cur).next.load(Ordering::Relaxed) };
        }

        // Conservation: {owned} and {reachable} must together be exactly {A, B, C} = ids {1,2,3},
        // with no overlap. Under the ABA bug, C (id 3) is in neither (leaked) and B (id 2) is in
        // both (double-owned) -> this fails. Under the fix -> [1,2,3], disjoint.
        let mut owned_ids: Vec<u64> = owned.iter().map(|&p| tag(p)).collect();
        let mut reach_ids: Vec<u64> = reachable.iter().map(|&p| tag(p)).collect();
        owned_ids.sort_unstable();
        reach_ids.sort_unstable();

        let overlap: Vec<u64> = owned_ids
            .iter()
            .filter(|id| reach_ids.contains(id))
            .copied()
            .collect();
        assert!(
            overlap.is_empty(),
            "page double-owned via ABA: ids {overlap:?} are both owned and reachable \
             (owned={owned_ids:?}, reachable={reach_ids:?})"
        );

        let mut all: Vec<u64> = owned_ids.iter().chain(reach_ids.iter()).copied().collect();
        all.sort_unstable();
        assert_eq!(
            all,
            vec![1, 2, 3],
            "page leaked via ABA: expected all of {{1,2,3}} accounted for, got \
             owned={owned_ids:?} reachable={reach_ids:?}"
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    // See comments in possible_cpus if this fails -- it's possible the failure just indicates we
    // need to ignore that particular test environment.
    #[test]
    fn check_per_cpu() {
        let per_cpu = init_per_cpu();
        assert!(per_cpu.len() >= std::thread::available_parallelism().unwrap().get());
    }

    #[derive(Default)]
    struct TestAbsorber {
        value: u64,
    }

    impl super::Absorb for TestAbsorber {
        fn handle(slots: &mut [Self], events: &mut [u64]) {
            slots[0].value += events.len() as u64;
        }
    }

    #[test]
    fn test_send_event_local() {
        let mut channels = Channels::<TestAbsorber>::new();

        channels.allocate();

        for idx in 0..30 {
            channels.send_event(idx);
        }

        // We don't expect this to pass if we're not using rseq. We check this after sending the
        // events since that happens later.
        if channels.must_use_fallback || RSEQ_FAILED.load(Ordering::Relaxed) {
            return;
        }

        let local_filled = channels
            .per_cpu
            .iter_mut()
            .filter_map(|cpu| std::ptr::NonNull::new(*cpu.get_mut()))
            .map(|cpu| unsafe { &mut *cpu.as_ptr() })
            .map(|cpu| *cpu.length.get_mut())
            .sum::<u64>();
        assert_eq!(local_filled, 30);

        channels.steal_pages();

        // After stealing all per-CPU pages are empty and the aggregate value is populated.
        assert!(channels.per_cpu.iter_mut().all(|c| c.get_mut().is_null()));
        assert_eq!(channels.get_mut(0, |x| x.value), 30);
    }

    #[test]
    fn test_send_event_overflow() {
        let channels = Channels::<TestAbsorber>::new();

        channels.allocate();

        // Guarantee enough writes that at least one page needs to overflow.
        let total_events = channels.per_cpu.len() * SLOTS;
        for _ in 0..total_events {
            channels.send_event(0);
        }

        channels.steal_pages();

        let count = channels.get_mut(0, |v| v.value);
        assert_eq!(count, total_events as u64);
    }

    #[test]
    fn test_thread_ctor_dtor() {
        // Confirms we are able to register + unregister for new threads
        std::thread::spawn(move || {
            rseq();
        })
        .join()
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn check_send_branches() {
        let mut rseq = Rseq {
            // cpu index changed should force us to branch out to fallback.
            cpu_id_start: 0u32,
            cpu_id: 0u32,
            rseq_cs: 0u64,
            flags: 0u32,
        };

        // With a mismatched CPU index, we branch out to fallback.
        //
        // Note that this happens after N loops through the code, but we don't have any good way to
        // determine that unfortunately. But this does ensure we don't hang if the kernel isn't
        // cooperating.
        let channels = Channels::<TestAbsorber>::new();
        channels.allocate();
        rseq.cpu_id_start = 0;
        rseq.cpu_id = 1;
        assert_eq!(channels.fallback.len(), 0);
        channels.send_event_inner(0u64, NonNull::from(&mut rseq));
        assert_eq!(channels.fallback.len(), 1);
        drop(channels);

        // An index that's one out of bounds (but consistent) also causes us to fallback.
        let channels = Channels::<TestAbsorber>::new();
        channels.allocate();
        rseq.cpu_id_start = channels.per_cpu.len() as u32;
        rseq.cpu_id = channels.per_cpu.len() as u32;
        assert_eq!(channels.fallback.len(), 0);
        channels.send_event_inner(0u64, NonNull::from(&mut rseq));
        assert_eq!(channels.fallback.len(), 1);
        drop(channels);

        // We allocate the page at the right CPU index.
        let mut channels = Channels::<TestAbsorber>::new();
        channels.allocate();
        // Force 64 CPUs regardless of what hardware we're running on.
        channels.per_cpu = (0..=64)
            .map(|_| AtomicPtr::new(std::ptr::null_mut()))
            .collect();
        for idx in 0..64 {
            rseq.cpu_id_start = idx;
            rseq.cpu_id = idx;
            assert_eq!(channels.fallback.len(), 0);
            channels.send_event_inner(0u64, NonNull::from(&mut rseq));

            // Didn't fallback.
            assert_eq!(channels.fallback.len(), 0);

            // Only allocated exactly one CPU, at the right index.
            let taken = std::mem::take(channels.per_cpu[idx as usize].get_mut());
            assert!(!taken.is_null());
            drop(unsafe { Box::from_raw(taken) });
            for cpu in channels.per_cpu.iter_mut() {
                assert!(cpu.get_mut().is_null());
            }
        }
        drop(channels);
    }

    #[cfg(target_os = "linux")]
    #[test]
    // `lse` enablement on aarch64
    #[allow(unused_unsafe)]
    fn check_send_slow_branches() {
        #[cfg(target_arch = "aarch64")]
        if !std::arch::is_aarch64_feature_detected!("lse") {
            return;
        }

        let mut rseq = Rseq {
            // cpu index changed should force us to branch out to fallback.
            cpu_id_start: 0u32,
            cpu_id: 0u32,
            rseq_cs: 0u64,
            flags: 0u32,
        };

        // With a mismatched CPU index, we branch out to fallback.
        //
        // Note that this happens after N loops through the code, but we don't have any good way to
        // determine that unfortunately. But this does ensure we don't hang if the kernel isn't
        // cooperating.
        let channels = Channels::<TestAbsorber>::new();
        channels.allocate();
        rseq.cpu_id_start = 0;
        rseq.cpu_id = 1;
        unsafe {
            channels.send_event_slow(NonNull::from(&mut rseq), 0u64);
        }
        channels.steal_pages();
        assert_eq!(channels.get_mut(0, std::mem::take).value, 1);
        drop(channels);

        // An index that's one out of bounds (but consistent) also causes us to fallback.
        let channels = Channels::<TestAbsorber>::new();
        channels.allocate();
        rseq.cpu_id_start = channels.per_cpu.len() as u32;
        rseq.cpu_id = channels.per_cpu.len() as u32;
        unsafe {
            channels.send_event_slow(NonNull::from(&mut rseq), 0u64);
        }
        channels.steal_pages();
        assert_eq!(channels.get_mut(0, std::mem::take).value, 1);
        drop(channels);

        // We allocate the page at the right CPU index.
        let mut channels = Channels::<TestAbsorber>::new();
        channels.allocate();
        // Force 64 CPUs regardless of what hardware we're running on.
        channels.per_cpu = (0..=64)
            .map(|_| AtomicPtr::new(std::ptr::null_mut()))
            .collect();
        for idx in 0..64 {
            rseq.cpu_id_start = idx;
            rseq.cpu_id = idx;
            unsafe {
                channels.send_event_slow(NonNull::from(&mut rseq), 0u64);
            }

            // Didn't fallback.
            assert_eq!(channels.fallback.len(), 0);

            // Only allocated exactly one CPU, at the right index.
            assert!(!channels.per_cpu[idx as usize].get_mut().is_null());
            for (cpu_idx, cpu) in channels.per_cpu.iter_mut().enumerate() {
                if cpu_idx == idx as usize {
                    continue;
                }
                assert!(cpu.get_mut().is_null());
            }

            channels.steal_pages();
            assert_eq!(channels.get_mut(0, std::mem::take).value, 1);

            // Repeating the slow-send on the same CPU *will* persist exactly one event.
            unsafe {
                channels.send_event_slow(NonNull::from(&mut rseq), 0u64);
            }

            // Didn't fallback.
            assert_eq!(channels.fallback.len(), 0);

            channels.steal_pages();
            assert_eq!(channels.get_mut(0, std::mem::take).value, 1);

            // Pages are consumed when we aggregate.
            for cpu in channels.per_cpu.iter_mut() {
                assert!(cpu.get_mut().is_null());
            }
            assert!(channels.empty_pages.pop(0).is_some());
            assert!(channels.empty_pages.pop(0).is_none());
        }
        drop(channels);
    }

    /// End-to-end steady-state conservation on the real rseq recording path: once the pool is warm,
    /// a high-rate recording load driven concurrently with periodic `absorb_full_pages` (the cheap
    /// compaction) and occasional full `steal_pages` (report) must stop allocating fresh pages. The
    /// production leak was exactly `Page::new` climbing without bound here; this asserts it plateaus.
    ///
    /// This is inherently a *concurrency* test (the ABA it guards against needs a real popper vs.
    /// re-pusher race), so it uses threads rather than bach. It is tolerant of scheduling: it only
    /// asserts that allocations in a late window are bounded well below the events processed, not an
    /// exact count. On the fallback path (no rseq/membarrier) pages are never used, so it early-outs.
    #[test]
    fn steady_state_stops_allocating_pages() {
        use std::sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        };

        let channels = Arc::new(Channels::<TestAbsorber>::new());
        channels.allocate();

        // The fallback path (rseq/membarrier unavailable) never touches the page pool; nothing to
        // assert about page allocation there.
        if channels.must_use_fallback {
            return;
        }

        let stop = Arc::new(AtomicBool::new(false));
        let recorded = Arc::new(AtomicU64::new(0));

        // A few recorder threads hammering send_event — this is what pops from empty_pages and
        // pushes to full_pages, at a rate high enough to fill many pages.
        let mut recorders = Vec::new();
        for _ in 0..4 {
            let channels = channels.clone();
            let stop = stop.clone();
            let recorded = recorded.clone();
            recorders.push(std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..10_000 {
                        channels.send_event(1);
                    }
                    n += 10_000;
                }
                recorded.fetch_add(n, Ordering::Relaxed);
            }));
        }

        // A "reporter" thread that mostly absorbs (cheap, full_pages only) and occasionally does a
        // full steal (report), mirroring the intended production cadence.
        let reporter = {
            let channels = channels.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    channels.absorb_full_pages();
                    if i.is_multiple_of(10) {
                        channels.steal_pages();
                    }
                    i += 1;
                    std::thread::yield_now();
                }
            })
        };

        // Warm the pool: let it run, then snapshot the allocation count. After warm-up, additional
        // allocations should be near-zero because pages recycle through empty_pages.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let allocated_after_warmup = PAGES_ALLOCATED.load(Ordering::Relaxed);

        // Measurement window: keep recording hard, then compare fresh allocations against the huge
        // number of events processed in the same window.
        std::thread::sleep(std::time::Duration::from_millis(400));
        let allocated_end = PAGES_ALLOCATED.load(Ordering::Relaxed);

        stop.store(true, Ordering::Relaxed);
        for r in recorders {
            r.join().unwrap();
        }
        reporter.join().unwrap();

        let fresh_allocations = allocated_end - allocated_after_warmup;
        let total_recorded = recorded.load(Ordering::Relaxed);

        // The pool has a fixed working set (roughly one page per active CPU plus a small recycle
        // buffer). Fresh allocations in the measurement window must be a tiny fraction of the
        // events processed — under the ABA leak this instead grew ~linearly with events. We use a
        // generous bound (allocations < events / SLOTS, i.e. fewer than one fresh page per page's
        // worth of events) so the test is robust to scheduler variance but still fails hard on a
        // linear leak, which would allocate on the order of events/SLOTS pages *plus* the escapees.
        //
        // Concretely: with the leak, ~2-3% of filled pages escaped, so fresh allocations tracked
        // ~2-3% of (events / SLOTS) and grew every window. Post-fix the recycle is complete, so
        // once warm this is a small constant.
        let filled_pages_estimate = total_recorded / SLOTS as u64;
        assert!(
            fresh_allocations <= filled_pages_estimate / 20 + 64,
            "page pool kept allocating after warm-up: {fresh_allocations} fresh Page::new in the \
             measurement window vs ~{filled_pages_estimate} pages' worth of events recorded \
             (total events {total_recorded}); expected recycling to keep this near-constant"
        );
    }
}
