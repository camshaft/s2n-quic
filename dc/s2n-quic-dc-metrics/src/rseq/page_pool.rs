// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Lock-free page recycling pool for the rseq recorder.
//!
//! The recorder buffers events in fixed-size [`Page`]s (owned by [`super`], whose assembly reads
//! their layout at fixed offsets). This module owns the *recycling* of those pages between two
//! pools:
//!
//! - `full_pages` (MPSC): recorders push a filled page; a single stealer drains the chain. Never
//!   popped.
//! - `empty_pages` (SPMC): the stealer recycles folded pages back in; recorders pop a fresh one.
//!
//! Both are lock-free intrusive stacks. The subtlety is that a naive [Treiber
//! stack](https://en.wikipedia.org/wiki/Treiber_stack) `pop` has an ABA hazard: it reads `head`,
//! caches `head.next`, and if `head` is popped and re-pushed before its CAS, the CAS still sees the
//! same pointer and succeeds — installing a stale `next`, orphaning (leaking) one page and
//! double-owning another. `push` never has this problem (it only ever links a new node above the
//! observed head). So we make the stack generic over its [`Head`] cell and provide `pop` **only**
//! for the ABA-safe tagged head and `drain` **only** for the plain-pointer head, so the operation
//! that is unsafe for a given pool does not type-check.

use std::{
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

// std's `AtomicU128` is unstable (feature `integer_atomics`); this crate builds on stable, so we
// use `portable-atomic`, which lowers a 128-bit CAS to `cmpxchg16b` (x86_64) / `casp` (aarch64
// LSE). See the `TaggedHead` docs for why we need a double-word CAS on the SPMC pool.
use portable_atomic::AtomicU128;

use super::Page;

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
///
/// `pub(super)` only because it appears as a bound on the exported `ShardedPagePool<H>`; the trait
/// and both implementors are otherwise entirely internal to this module.
pub(super) trait Head {
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
pub(super) struct PtrHead {
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
pub(super) struct TaggedHead {
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
pub(super) struct ShardedPagePool<H: Head> {
    shards: [Stack<H>; PAGE_POOL_SHARDS],
}

impl<H: Head> ShardedPagePool<H> {
    pub(super) fn new() -> Self {
        Self {
            shards: [Stack::new(), Stack::new()],
        }
    }

    #[inline]
    pub(super) fn push(&self, page: Box<Page>, cpu_hint: usize) {
        self.shards[cpu_hint & PAGE_POOL_MASK].push(page);
    }
}

impl ShardedPagePool<TaggedHead> {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) fn pop(&self, cpu_hint: usize) -> Option<Box<Page>> {
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
    pub(super) fn drain(&self, mut f: impl FnMut(Box<Page>)) {
        for shard in &self.shards {
            shard.drain(&mut f);
        }
    }
}

/// Platform-independent tests for the page-pool stacks themselves. These exercise `Stack` /
/// `ShardedPagePool` directly (no rseq asm), so they run on every target — including the macOS dev
/// machines where the rseq fast path is compiled out.
#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduces the ABA hazard in a Treiber-stack `pop` deterministically and asserts page
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
    ///   4. P1 resumes and CASes (expected = A, new = stale B). Under a plain-pointer head, head
    ///      *is* A again so the CAS succeeds and installs B as head: C is orphaned (leaked) and B is
    ///      double-owned. Under the tagged head, the generation moved, P1's CAS fails and it retries
    ///      onto C -> conserved.
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
