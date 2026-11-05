// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::worker;
use std::{
    sync::{
        atomic::{AtomicPtr, AtomicU64, Ordering},
        Arc, Mutex,
    },
    task::{self, Wake, Waker},
};

/// Bits tracked per readiness segment (one [`AtomicU64`]).
const IDS_PER_SEGMENT: usize = 64;
/// Maximum number of segments, i.e. the id ceiling is `MAX_SEGMENTS * IDS_PER_SEGMENT`.
/// 1024 segments → 65_536 ids, far beyond any endpoint's sub-task count, for an 8 KiB pointer table.
const MAX_SEGMENTS: usize = 1024;

#[derive(Default)]
pub struct Set {
    state: Arc<State>,
    local_root: Option<Waker>,
}

impl Set {
    /// Called at the beginning of the `poll` function for the owner of [`Set`]
    #[inline]
    pub fn poll_start(&mut self, cx: &task::Context) {
        let new_waker = cx.waker();

        let root_task_requires_update = if let Some(waker) = self.local_root.as_ref() {
            !waker.will_wake(new_waker)
        } else {
            true
        };

        if root_task_requires_update {
            self.state.root.update(new_waker);
            self.local_root = Some(new_waker.clone());
        }
    }

    /// Registers a waker with the given ID
    pub fn waker(&mut self, id: usize) -> Waker {
        // Ensure the segment backing `id` exists before the `Slot` (and thus any wake for `id`) can be
        // observed by another thread.
        self.state.ready.ensure_segment(id);
        let state = self.state.clone();
        Waker::from(Arc::new(Slot { id, state }))
    }

    /// Returns all of the IDs that are woken
    #[inline]
    pub fn drain(&mut self) -> impl Iterator<Item = usize> + '_ {
        self.state.ready.drain()
    }
}

#[derive(Default)]
struct State {
    root: worker::Waker,
    ready: ReadySet,
}

struct Slot {
    id: usize,
    state: Arc<State>,
}

impl Wake for Slot {
    #[inline]
    fn wake(self: Arc<Self>) {
        // SAFETY: `waker(id)` allocated `id`'s segment before this `Slot` was handed out, so the
        // segment is present and never moves/frees while a `Slot` for it is alive.
        unsafe { self.state.ready.set(self.id) };
        // use `wake_forced` instead of `wake` since we don't use the sleeping status from `worker::Waker`
        self.state.root.wake_forced();
    }
}

/// A lock-free readiness bitset.
///
/// Replaces the previous `Mutex<BitSet>`: every sub-task wake and every drain took that one
/// endpoint-global mutex, so at concurrency the wake path serialized on it (lock-wait jitter between a
/// sub-task becoming ready and the root poll draining it — the `endpoint::waker::Drain` profile hotspot
/// and TTFB tail at c16+). Here `set` is a single `fetch_or` and `drain` a per-word `swap(0)`; no lock
/// on the wake/drain hot path.
///
/// # Layout & safety
///
/// Ids are partitioned into segments of [`IDS_PER_SEGMENT`] bits, each an [`AtomicU64`] boxed on first
/// use. The `segments` table is a fixed-length `Box<[AtomicPtr<…>]>` (never reallocated), so a
/// concurrent `set` that loads a segment pointer never races a table grow. A segment, once allocated,
/// is never moved or freed until the `ReadySet` is dropped — so a `set` on an already-registered id (the
/// only concurrent op; `ensure_segment`/`drain` are owner-only via `&mut Set`) can never touch freed
/// memory. `set` for id `N` is only reachable after `waker(N)` allocated `N`'s segment (it hands out the
/// `Slot`), establishing the required happens-before.
///
/// # No lost wakeups without a whole-set snapshot
///
/// `drain` clears words one at a time (not a single atomic snapshot across the whole set). That is safe
/// because every `set` is paired with `root.wake_forced()`: a wake landing in a word already drained
/// this pass stays set for the next drain AND forces another root poll, so it is never dropped — the
/// same guarantee the mutex version had (a wake during drain re-polls).
struct ReadySet {
    /// `segments[i]` covers ids `[i*IDS_PER_SEGMENT, (i+1)*IDS_PER_SEGMENT)`. Null until allocated.
    segments: Box<[AtomicPtr<AtomicU64>]>,
    /// Serializes segment allocation (cold: only on `waker()` registration). Never taken on the
    /// wake/drain hot path.
    grow: Mutex<()>,
}

impl Default for ReadySet {
    fn default() -> Self {
        let mut segments = Vec::with_capacity(MAX_SEGMENTS);
        for _ in 0..MAX_SEGMENTS {
            segments.push(AtomicPtr::new(core::ptr::null_mut()));
        }
        Self {
            segments: segments.into_boxed_slice(),
            grow: Mutex::new(()),
        }
    }
}

impl ReadySet {
    #[inline]
    fn locate(id: usize) -> (usize, u64) {
        (id / IDS_PER_SEGMENT, 1u64 << (id % IDS_PER_SEGMENT))
    }

    /// Allocate `id`'s segment if absent. Owner-only (`&mut Set`), cold path.
    fn ensure_segment(&self, id: usize) {
        let (seg, _) = Self::locate(id);
        assert!(
            seg < MAX_SEGMENTS,
            "waker id {id} exceeds ReadySet capacity ({} ids)",
            MAX_SEGMENTS * IDS_PER_SEGMENT
        );
        if self.segments[seg].load(Ordering::Acquire).is_null() {
            let _g = self.grow.lock().unwrap();
            // re-check under the lock
            if self.segments[seg].load(Ordering::Acquire).is_null() {
                let word = Box::into_raw(Box::new(AtomicU64::new(0)));
                self.segments[seg].store(word, Ordering::Release);
            }
        }
    }

    /// Mark `id` ready. Lock-free; safe only for an `id` whose segment was allocated by
    /// [`ensure_segment`] (guaranteed for any live `Slot`).
    #[inline]
    unsafe fn set(&self, id: usize) {
        let (seg, mask) = Self::locate(id);
        let ptr = self.segments[seg].load(Ordering::Acquire);
        debug_assert!(!ptr.is_null(), "set on unallocated segment for id {id}");
        (*ptr).fetch_or(mask, Ordering::Release);
    }

    /// Drain all ready ids, clearing the set. Owner-only (`&mut Set`).
    #[inline]
    fn drain(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.segments.len()).flat_map(move |seg| {
            let ptr = self.segments[seg].load(Ordering::Acquire);
            let mut word = if ptr.is_null() {
                0
            } else {
                // SAFETY: a non-null segment pointer is a live `Box<AtomicU64>` for the ReadySet's
                // lifetime. Atomically take-and-clear its ready bits.
                unsafe { (*ptr).swap(0, Ordering::Acquire) }
            };
            let base = seg * IDS_PER_SEGMENT;
            core::iter::from_fn(move || {
                if word == 0 {
                    return None;
                }
                let bit = word.trailing_zeros() as usize;
                word &= word - 1; // clear lowest set bit
                Some(base + bit)
            })
        })
    }
}

impl Drop for ReadySet {
    fn drop(&mut self) {
        for seg in self.segments.iter() {
            let ptr = seg.load(Ordering::Acquire);
            if !ptr.is_null() {
                // SAFETY: allocated via `Box::into_raw` in `ensure_segment`; freed once here on drop
                // when no `Slot` (and thus no concurrent `set`) can still reference it.
                unsafe { drop(Box::from_raw(ptr)) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn waker_set_test() {
        bolero::check!().with_type::<Vec<u8>>().for_each(|ops| {
            let mut root = Set::default();
            let mut wakers = vec![];

            if let Some(max) = ops.iter().cloned().max() {
                let len = max as usize + 1;
                for i in 0..len {
                    wakers.push(root.waker(i));
                }
            }

            for idx in ops {
                wakers[*idx as usize].wake_by_ref();
            }

            let actual = root.drain().collect::<BTreeSet<_>>();
            let expected = ops.iter().map(|v| *v as usize).collect::<BTreeSet<_>>();
            assert_eq!(actual, expected);
        })
    }

    /// A drain returns each woken id exactly once and leaves the set empty (a second drain is empty).
    #[test]
    fn drain_clears() {
        let mut set = Set::default();
        let w: Vec<_> = (0..200).map(|i| set.waker(i)).collect();
        for i in [0usize, 1, 63, 64, 65, 127, 128, 199] {
            w[i].wake_by_ref();
        }
        let got = set.drain().collect::<BTreeSet<_>>();
        assert_eq!(got, BTreeSet::from([0, 1, 63, 64, 65, 127, 128, 199]));
        assert_eq!(set.drain().count(), 0, "set must be empty after drain");
    }

    /// Concurrent wakes from many threads to distinct ids are all observed by a subsequent drain — the
    /// lock-free set does not drop wakeups under contention.
    #[test]
    fn concurrent_wakes_all_observed() {
        use std::sync::Barrier;
        const N: usize = 512;
        let mut set = Set::default();
        let wakers: Vec<_> = (0..N).map(|i| set.waker(i)).collect();
        let barrier = Arc::new(Barrier::new(N));
        std::thread::scope(|s| {
            for w in &wakers {
                let w = w.clone();
                let barrier = barrier.clone();
                s.spawn(move || {
                    barrier.wait();
                    w.wake_by_ref();
                });
            }
        });
        let got = set.drain().collect::<BTreeSet<_>>();
        let expected: BTreeSet<usize> = (0..N).collect();
        assert_eq!(got, expected, "every concurrent wake must be observed");
    }
}
