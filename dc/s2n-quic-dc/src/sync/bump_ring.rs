// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A lock-free, bump-allocating ring buffer for low-overhead flight recording.
//!
//! Writers reserve space with a single `fetch_add` on a monotonic cursor and then memcpy their
//! record into the backing buffer (wrapping at the power-of-two boundary). There is no
//! coordination between writers beyond the atomic reservation, so the hot path is just an atomic
//! add plus a copy.
//!
//! Records are self-delimiting: each is stored as its payload bytes followed by a little-endian
//! [`RecLen`] suffix holding the *payload* length. A reader recovers records newest-first by
//! reading the suffix at the head, stepping back over the payload, and repeating
//! ([`walk`]). Because writers race ahead and wrap around, the oldest records are continuously
//! overwritten; the walk is therefore *best effort* — it stops as soon as it would step into a
//! region that has been (or is being) overwritten. Callers that need to detect torn records
//! should embed their own magic/version marker at the front of each payload.
//!
//! Atomics come from [`super`] (the crate's loom-swappable re-exports) so the structure can be
//! model-checked under `--features loom`.

use super::{AtomicUsize, Ordering};
use std::cell::UnsafeCell;

/// The integer type stored in each record's trailing length suffix.
type RecLen = u16;

/// Number of bytes occupied by the trailing length suffix.
const LEN_SUFFIX: usize = core::mem::size_of::<RecLen>();

/// The largest payload a single record may hold (bounded by [`RecLen`]).
pub const MAX_RECORD: usize = RecLen::MAX as usize;

/// A fixed-capacity, lock-free bump-allocating ring buffer.
///
/// See the [module docs](self) for the framing format and concurrency model.
pub struct BumpRing {
    /// Backing storage of length `capacity()` (a power of two). `UnsafeCell` because writers
    /// mutate disjoint (best-effort) byte ranges without holding a lock.
    buf: Box<[UnsafeCell<u8>]>,
    /// `capacity() - 1`; masks an absolute offset down to a physical index.
    mask: usize,
    /// Monotonically increasing count of bytes ever reserved. Never masked — the physical index
    /// for absolute offset `abs` is `abs & mask`. Wrapping the full `usize` would require writing
    /// exabytes, so we treat it as effectively unbounded.
    offset: AtomicUsize,
}

// SAFETY: writes go to disjoint byte ranges reserved by the atomic `offset`, and the `UnsafeCell`
// bytes are plain `u8` with no destructor or interior pointers. Readers observe best-effort
// snapshots and tolerate torn data (see module docs), so sharing across threads is sound.
unsafe impl Send for BumpRing {}
unsafe impl Sync for BumpRing {}

impl BumpRing {
    /// Creates a ring with capacity `requested` rounded up to the next power of two.
    ///
    /// # Panics
    /// Panics if `requested` is zero.
    pub fn new(requested: usize) -> Self {
        assert!(requested > 0, "BumpRing capacity must be non-zero");
        let capacity = requested.next_power_of_two();
        let mut buf = Vec::with_capacity(capacity);
        buf.resize_with(capacity, || UnsafeCell::new(0u8));
        Self {
            buf: buf.into_boxed_slice(),
            mask: capacity - 1,
            offset: AtomicUsize::new(0),
        }
    }

    /// The physical capacity in bytes (a power of two).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// The current write head: the total number of bytes ever reserved.
    ///
    /// Records occupying absolute offsets in `[head - capacity(), head)` are (best-effort) still
    /// resident; everything older has been overwritten.
    #[inline]
    pub fn head(&self) -> usize {
        self.offset.load(Ordering::Acquire)
    }

    /// Appends `payload` as a single record and returns its starting absolute offset.
    ///
    /// The record is `payload` followed by a little-endian [`RecLen`] suffix. This reserves space
    /// with one `fetch_add` and then copies the bytes in; it never blocks.
    ///
    /// # Panics
    /// Panics if `payload.len() > MAX_RECORD`.
    pub fn push(&self, payload: &[u8]) -> usize {
        assert!(
            payload.len() <= MAX_RECORD,
            "record payload {} exceeds MAX_RECORD {}",
            payload.len(),
            MAX_RECORD
        );

        let total = payload.len() + LEN_SUFFIX;
        // Reserve our slice of the ring. Relaxed is sufficient: writers never read each other's
        // bytes, and a reader establishes ordering via its own `head()` (Acquire) load plus the
        // park/unpark (or other) synchronization that delivered the dump request.
        let start = self.offset.fetch_add(total, Ordering::Relaxed);

        self.write_wrapping(start, payload);
        self.write_wrapping(
            start + payload.len(),
            &(payload.len() as RecLen).to_le_bytes(),
        );

        start
    }

    /// Copies `src` into the ring starting at absolute offset `abs`, splitting the copy if it
    /// straddles the power-of-two boundary.
    #[inline]
    fn write_wrapping(&self, abs: usize, src: &[u8]) {
        if src.is_empty() {
            return;
        }
        let cap = self.capacity();
        let begin = abs & self.mask;
        let first = (cap - begin).min(src.len());
        // SAFETY: `begin < cap` and `first <= cap - begin`, so this range is in bounds. The bytes
        // are plain `u8`; concurrent best-effort overwrites of the same range are tolerated by
        // readers (module docs).
        unsafe {
            let dst = self.buf.as_ptr().add(begin) as *mut u8;
            core::ptr::copy_nonoverlapping(src.as_ptr(), dst, first);
        }
        if first < src.len() {
            // Wrap to the start of the buffer for the remainder.
            // SAFETY: remaining length is `src.len() - first <= cap`, copied from index 0.
            unsafe {
                let dst = self.buf.as_ptr() as *mut u8;
                core::ptr::copy_nonoverlapping(src.as_ptr().add(first), dst, src.len() - first);
            }
        }
    }

    /// Copies the resident region into `dst` and returns the head snapshot.
    ///
    /// `dst` must be exactly [`capacity`](Self::capacity) bytes; on return `dst[i]` holds the byte
    /// at every absolute offset `abs` with `abs & mask == i`. Pair the returned head with `dst`
    /// when calling [`walk`].
    ///
    /// This is a best-effort snapshot: writers may be mutating the region concurrently, so a
    /// freshly overwritten record may appear torn. [`walk`]'s guards (and any caller-embedded
    /// magic) filter those out.
    pub fn snapshot_into(&self, dst: &mut [u8]) -> usize {
        let cap = self.capacity();
        assert_eq!(dst.len(), cap, "snapshot buffer must equal capacity");
        // Load the head first so the copied bytes are no older than `head - capacity`.
        let head = self.head();
        // SAFETY: `buf` and `dst` are both `cap` bytes, non-overlapping (separate allocations).
        unsafe {
            core::ptr::copy_nonoverlapping(self.buf.as_ptr() as *const u8, dst.as_mut_ptr(), cap);
        }
        head
    }
}

/// Walks the records in a snapshot newest-first, invoking `f` with each record's payload.
///
/// `region` is a raw snapshot of the ring (as produced by [`BumpRing::snapshot_into`] or read from
/// a dump file), `head` is the write head at snapshot time, and `capacity` is `region.len()` (must
/// be a power of two). The walk reads the trailing length suffix at the head, yields the payload
/// in front of it, steps back, and repeats until it reaches the oldest resident byte or hits a
/// guard.
///
/// The guards make the walk safe against partially-overwritten tails: it stops if a length is
/// impossible, would underflow the head, or would step below the oldest resident offset. This is
/// best-effort — a clobbered suffix can still yield a plausible-but-wrong length, so callers that
/// need certainty should validate a magic marker inside each payload.
pub fn walk(region: &[u8], head: usize, capacity: usize, mut f: impl FnMut(&[u8])) {
    if region.len() != capacity || !capacity.is_power_of_two() {
        return;
    }
    let mask = capacity - 1;
    // Oldest absolute offset still (best-effort) resident in the ring.
    let valid_low = head.saturating_sub(capacity);

    let read_wrapping = |abs: usize, len: usize, out: &mut [u8]| {
        let begin = abs & mask;
        let first = (capacity - begin).min(len);
        out[..first].copy_from_slice(&region[begin..begin + first]);
        if first < len {
            out[first..len].copy_from_slice(&region[..len - first]);
        }
    };

    let mut abs = head;
    while abs >= valid_low + LEN_SUFFIX {
        // Read the length suffix that ends at `abs`.
        let suffix_start = abs - LEN_SUFFIX;
        let mut len_bytes = [0u8; LEN_SUFFIX];
        read_wrapping(suffix_start, LEN_SUFFIX, &mut len_bytes);
        let payload_len = RecLen::from_le_bytes(len_bytes) as usize;
        let rec_total = payload_len + LEN_SUFFIX;

        // Guards against overwritten/torn tails.
        if rec_total > abs {
            break; // would underflow absolute 0
        }
        let payload_start = abs - rec_total;
        if payload_start < valid_low {
            break; // would read evicted bytes
        }

        let mut payload = vec![0u8; payload_len];
        read_wrapping(payload_start, payload_len, &mut payload);
        f(&payload);

        abs = payload_start;
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;
    use crate::testing::loom;
    use std::sync::Arc;

    /// Concurrent `push`es must receive disjoint, contiguous reservations and the head must end up
    /// at the exact sum of their sizes. This is the load-bearing concurrency property — the byte
    /// copies into those disjoint ranges are plain memory and need no further synchronization.
    ///
    /// Run via `LOOM_MAX_PREEMPTIONS=3 cargo test --features loom sync::bump_ring::loom_tests`.
    #[test]
    fn concurrent_push_reservations_are_disjoint() {
        loom::model(|| {
            // Capacity large enough that the three records don't wrap, so physical ranges are
            // also disjoint and we can sanity-check the bytes after the join.
            let ring = Arc::new(BumpRing::new(64));

            let payloads: [Vec<u8>; 3] = [vec![0xAA; 2], vec![0xBB; 4], vec![0xCC; 6]];
            let handles: Vec<_> = payloads
                .into_iter()
                .map(|p| {
                    let ring = ring.clone();
                    loom::thread::spawn(move || {
                        let len = p.len() + LEN_SUFFIX;
                        (ring.push(&p), len)
                    })
                })
                .collect();

            let mut spans: Vec<(usize, usize)> =
                handles.into_iter().map(|h| h.join().unwrap()).collect();

            // Total reserved equals the head.
            let total: usize = spans.iter().map(|(_, len)| len).sum();
            assert_eq!(ring.head(), total);

            // Reservations are pairwise disjoint and tile [0, total) with no gaps/overlaps.
            spans.sort_by_key(|(start, _)| *start);
            let mut expected = 0;
            for (start, len) in spans {
                assert_eq!(
                    start, expected,
                    "reservations must be contiguous and disjoint"
                );
                expected += len;
            }
            assert_eq!(expected, total);
        });
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;

    /// Collect a ring's records newest-first into owned Vecs.
    fn collect(ring: &BumpRing) -> Vec<Vec<u8>> {
        let mut region = vec![0u8; ring.capacity()];
        let head = ring.snapshot_into(&mut region);
        let mut out = Vec::new();
        walk(&region, head, ring.capacity(), |p| out.push(p.to_vec()));
        out
    }

    #[test]
    fn rounds_up_capacity() {
        assert_eq!(BumpRing::new(1).capacity(), 1);
        assert_eq!(BumpRing::new(3).capacity(), 4);
        assert_eq!(BumpRing::new(1000).capacity(), 1024);
    }

    #[test]
    fn single_thread_round_trip_newest_first() {
        let ring = BumpRing::new(256);
        ring.push(b"first");
        ring.push(b"second");
        ring.push(b"third");

        let got = collect(&ring);
        assert_eq!(
            got,
            vec![b"third".to_vec(), b"second".to_vec(), b"first".to_vec()]
        );
    }

    #[test]
    fn empty_payloads_round_trip() {
        let ring = BumpRing::new(64);
        ring.push(b"");
        ring.push(b"x");
        ring.push(b"");
        let got = collect(&ring);
        assert_eq!(got, vec![b"".to_vec(), b"x".to_vec(), b"".to_vec()]);
    }

    #[test]
    fn empty_ring_walks_nothing() {
        let ring = BumpRing::new(64);
        assert!(collect(&ring).is_empty());
    }

    #[test]
    fn wrap_keeps_most_recent_and_terminates() {
        // Small ring; push many records so it wraps several times.
        let ring = BumpRing::new(64);
        for i in 0u32..200 {
            ring.push(&i.to_le_bytes());
        }
        let got = collect(&ring);
        // The walk must terminate and the newest record must be the last one pushed.
        assert!(!got.is_empty());
        assert_eq!(got[0], 199u32.to_le_bytes().to_vec());
        // Every recovered record must be a valid 4-byte payload and strictly decreasing.
        let mut prev = 200u32;
        for rec in &got {
            assert_eq!(rec.len(), 4);
            let v = u32::from_le_bytes(rec.as_slice().try_into().unwrap());
            assert!(v < prev, "records should be newest-first and contiguous");
            prev = v;
        }
        // Recovered count can't exceed what physically fits.
        assert!(got.len() <= ring.capacity() / (4 + LEN_SUFFIX) + 1);
    }

    #[test]
    fn record_straddling_boundary_round_trips() {
        // Capacity 16. Advance the head so a record's start sits near the boundary and its body
        // wraps across index 0.
        let ring = BumpRing::new(16);
        // Push 13 bytes of payload (+2 suffix = 15) so head = 15, one short of the boundary.
        ring.push(&[0xAA; 13]);
        // Next record (payload 6 + suffix 2 = 8 bytes) starts at abs 15 and wraps.
        let straddler = [1u8, 2, 3, 4, 5, 6];
        ring.push(&straddler);

        let mut region = vec![0u8; ring.capacity()];
        let head = ring.snapshot_into(&mut region);
        let mut out = Vec::new();
        walk(&region, head, ring.capacity(), |p| out.push(p.to_vec()));
        // The straddling record is newest; assert it round-trips byte-for-byte.
        assert_eq!(out[0], straddler.to_vec());
    }

    #[test]
    fn walk_rejects_bad_region_len() {
        // capacity mismatch is a no-op, not a panic/OOB.
        let region = vec![0u8; 8];
        let mut called = false;
        walk(&region, 8, 16, |_| called = true);
        assert!(!called);
    }
}
