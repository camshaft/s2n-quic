// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A non-allocating reassembly queue for owned storage segments.
//!
//! [`Segments`] stores received segments in stream order without copying their bytes.
//! Data is only copied when the application drains the buffer via
//! [`reader::Storage::copy_into`].  This eliminates the extra receive-time copy that the
//! allocation-based [`super::Reassembler`] incurs when data arrives out of order.
//!
//! ## Write path (receive side)
//!
//! Call [`Segments::insert`] with the owned segment and its stream offset.  Duplicate or
//! already-consumed bytes are silently discarded; overlapping data is trimmed so that no
//! two segments in the queue ever overlap.
//!
//! ## Read path (application side)
//!
//! The type implements [`reader::Storage`] and [`Reader`], so callers can use the usual
//! `infallible_copy_into` / `copy_into` helpers to drain contiguous data into an
//! application buffer.

use super::{slot_storage::SlotStorage, Cursors};
use crate::{
    buffer::{
        reader::{storage::Chunk, Reader},
        writer,
        Error,
    },
    varint::VarInt,
};
use alloc::collections::VecDeque;

// ────────────────────────────────────────────────────────────────────────────
// Internal segment wrapper
// ────────────────────────────────────────────────────────────────────────────

struct Segment<S> {
    /// Logical stream offset of the first unconsumed byte inside `data`.
    ///
    /// This is kept in sync with `data.len()`:
    ///   segment.end() == segment.start + segment.data.len() as u64
    start: u64,
    data: S,
}

impl<S: SlotStorage> Segment<S> {
    #[inline]
    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Public type
// ────────────────────────────────────────────────────────────────────────────

/// A non-allocating, ordered reassembly buffer for segments of pre-existing owned storage.
///
/// Unlike [`super::Reassembler`], `Segments` does **not** allocate any backing memory.
/// Callers insert owned segments via [`Segments::insert`] and data is only copied into
/// the application buffer at read time.
///
/// The `write_at` / `write_reader` family of methods from `Reassembler` is intentionally
/// absent: storage types used with `Segments` are not required to support allocation.
#[derive(Debug)]
pub struct Segments<S> {
    inner: VecDeque<Segment<S>>,
    cursors: Cursors,
}

impl<S> Default for Segments<S> {
    fn default() -> Self {
        Self {
            inner: VecDeque::new(),
            cursors: Cursors::default(),
        }
    }
}

impl<S> Segments<S> {
    /// Creates a new, empty `Segments` buffer.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }
}

impl<S: SlotStorage> Segments<S> {
    // ── State queries ────────────────────────────────────────────────────────

    /// Returns the final size of the stream, if known.
    #[inline]
    pub fn final_size(&self) -> Option<u64> {
        self.cursors.final_size()
    }

    /// Returns the number of contiguous bytes available for reading from the
    /// current position.
    #[inline]
    pub fn len(&self) -> usize {
        let mut count: usize = 0;
        let mut expected = self.cursors.start_offset;
        for seg in &self.inner {
            if seg.start != expected || seg.data.is_empty() {
                break;
            }
            count = count.saturating_add(seg.data.len());
            expected = seg.end();
        }
        count
    }

    /// Returns `true` if no contiguous bytes are available for reading at the
    /// current position.
    #[inline]
    pub fn is_empty(&self) -> bool {
        match self.inner.front() {
            Some(s) => s.start != self.cursors.start_offset || s.data.is_empty(),
            None => true,
        }
    }

    /// Returns the number of bytes that have already been consumed by the
    /// application.
    #[inline]
    pub fn consumed_len(&self) -> u64 {
        self.cursors.start_offset
    }

    /// Returns the total number of contiguously received bytes (consumed +
    /// currently buffered and readable).
    #[inline]
    pub fn total_received_len(&self) -> u64 {
        let mut offset = self.cursors.start_offset;
        for seg in &self.inner {
            if seg.start != offset || seg.data.is_empty() {
                break;
            }
            offset = seg.end();
        }
        offset
    }

    /// Returns `true` if all data up to the final size has been received.
    #[inline]
    pub fn is_writing_complete(&self) -> bool {
        self.final_size()
            .is_some_and(|len| self.total_received_len() == len)
    }

    /// Returns `true` if all data up to the final size has been consumed by
    /// the application.
    #[inline]
    pub fn is_reading_complete(&self) -> bool {
        self.final_size() == Some(self.cursors.start_offset)
    }

    // ── Write path ───────────────────────────────────────────────────────────

    /// Inserts an owned segment into the reassembly queue at `offset`.
    ///
    /// No bytes are copied.  Duplicate or already-consumed bytes are silently
    /// discarded.  Overlapping bytes are trimmed so that no two segments in the
    /// queue overlap.
    ///
    /// If `is_fin` is `true`, the end of `data` is recorded as the final
    /// stream offset.  A subsequent call with a different final offset returns
    /// [`Error::InvalidFin`].
    pub fn insert(
        &mut self,
        offset: VarInt,
        mut data: S,
        is_fin: bool,
    ) -> Result<(), Error> {
        let new_start = offset.as_u64();
        let new_end = new_start
            .checked_add(data.len() as u64)
            .ok_or(Error::OutOfRange)?;

        // ── Validate / record the final offset ──────────────────────────────
        if is_fin {
            match self.cursors.final_size() {
                Some(existing) => {
                    if existing != new_end {
                        return Err(Error::InvalidFin);
                    }
                }
                None => {
                    if self.cursors.max_recv_offset > new_end {
                        return Err(Error::InvalidFin);
                    }
                    self.cursors.final_offset = new_end;
                }
            }
        } else if let Some(final_size) = self.cursors.final_size() {
            if new_end > final_size {
                return Err(Error::InvalidFin);
            }
        }

        // Track the furthest byte ever received.
        self.cursors.max_recv_offset = self.cursors.max_recv_offset.max(new_end);

        let cur = self.cursors.start_offset;

        // Discard fully stale data (already consumed).
        if new_end <= cur {
            return Ok(());
        }

        // Trim prefix that was already consumed.
        if new_start < cur {
            data.advance((cur - new_start) as usize);
        }
        let mut start = new_start.max(cur);

        if data.is_empty() {
            return Ok(());
        }

        // ── Find insertion position ──────────────────────────────────────────
        // First index where the existing segment's start >= our start.
        let mut idx = self.inner.partition_point(|s| s.start < start);

        // ── Handle left-neighbour overlap ────────────────────────────────────
        // The segment at idx-1 (if any) may extend past our start.
        if let Some(left) = idx.checked_sub(1).and_then(|i| self.inner.get(i)) {
            let left_end = left.end();
            if left_end > start {
                let trim = (left_end - start) as usize;
                if trim >= data.len() {
                    return Ok(()); // Entirely covered by the left neighbour.
                }
                data.advance(trim);
                start = left_end;
            }
        }

        if data.is_empty() {
            return Ok(());
        }

        // ── Handle right-neighbour overlaps ──────────────────────────────────
        // Walk forward through existing segments that overlap with our data,
        // inserting gap portions as we go.
        loop {
            if data.is_empty() {
                break;
            }

            let our_end = start + data.len() as u64;

            // Read the right neighbour's bounds before potentially mutating the
            // VecDeque (insertions invalidate the reference).
            let (right_start, right_end) = match self.inner.get(idx) {
                None => {
                    // No more existing segments — insert whatever remains.
                    self.inner.insert(idx, Segment { start, data });
                    self.debug_invariants();
                    return Ok(());
                }
                Some(right) => (right.start, right.end()),
            };

            if right_start >= our_end {
                // No overlap with right — insert before it.
                break;
            }

            if right_start <= start {
                // The right segment starts at or before our current start —
                // it covers a prefix of our remaining data.  Advance past it.
                if right_end >= our_end {
                    return Ok(()); // Entirely covered.
                }
                let trim = (right_end - start) as usize;
                data.advance(trim);
                start = right_end;
                idx += 1;
            } else {
                // `right_start` is strictly inside `[start, our_end)`.
                // Insert the gap `[start, right_start)` now and then skip over
                // the right segment.
                let gap_len = (right_start - start) as usize;
                let prefix = data.split_off_prefix(gap_len);
                self.inner.insert(idx, Segment { start, data: prefix });
                idx += 1; // `idx` now points at `right`.

                // `data` now covers `[right_start, our_end)`.
                let right_len = right_end - right_start;
                if right_len >= data.len() as u64 {
                    // `right` covers all remaining data.
                    self.debug_invariants();
                    return Ok(());
                }
                data.advance(right_len as usize);
                start = right_end;
                idx += 1; // Skip past `right`.
            }
        }

        if data.is_empty() {
            return Ok(());
        }

        self.inner.insert(idx, Segment { start, data });

        self.debug_invariants();

        Ok(())
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    /// Pops and discards any empty segments that ended up at the front due to
    /// a partial `read_chunk` drain.  Must be called before borrowing a slice
    /// from the front segment.
    #[inline]
    fn drain_empty_front(&mut self) {
        while self.inner.front().is_some_and(|s| s.data.is_empty()) {
            self.inner.pop_front();
        }
    }

    #[inline(always)]
    fn debug_invariants(&self) {
        if cfg!(debug_assertions) {
            let mut prev_end = self.cursors.start_offset;
            for seg in &self.inner {
                debug_assert!(
                    seg.start >= prev_end,
                    "segments overlap or are out of order: start={} prev_end={}",
                    seg.start,
                    prev_end
                );
                debug_assert!(!seg.data.is_empty(), "empty segment in queue");
                prev_end = seg.end();
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// reader::Storage implementation (read / copy-out path)
// ────────────────────────────────────────────────────────────────────────────

impl<S: SlotStorage> crate::buffer::reader::Storage for Segments<S> {
    type Error = core::convert::Infallible;

    #[inline]
    fn buffered_len(&self) -> usize {
        self.len()
    }

    #[inline]
    fn buffer_is_empty(&self) -> bool {
        self.is_empty()
    }

    /// Returns a borrowed slice of the next chunk of available data.
    ///
    /// # Safety
    ///
    /// The returned `Chunk::Slice` borrows from the front segment's backing
    /// allocation.  The front segment is **not** popped after advancing so
    /// that the backing allocation remains live for the duration of the borrow.
    /// Empty front segments left behind are cleaned up at the start of the
    /// next call (see `drain_empty_front`).
    ///
    /// This pattern is safe for storage types whose backing memory is
    /// reference-counted (e.g. `descriptor::Filled`): `advance()` only
    /// updates bookkeeping fields and does not free the underlying allocation.
    #[inline]
    fn read_chunk(&mut self, watermark: usize) -> Result<Chunk<'_>, Self::Error> {
        // Clean up any empty segments left from a previous partial drain.
        self.drain_empty_front();

        let Some(front) = self.inner.front_mut() else {
            return Ok(Chunk::empty());
        };

        if front.start != self.cursors.start_offset {
            return Ok(Chunk::empty()); // Gap before this segment.
        }

        let len = front.data.len().min(watermark);
        if len == 0 {
            return Ok(Chunk::empty());
        }

        // SAFETY: `SlotStorage::advance` is guaranteed to only update
        // bookkeeping (e.g. an offset/length pair) without freeing the backing
        // allocation.  `front` remains in `self.inner`, keeping the refcount
        // alive for the lifetime of this `&mut self` borrow.  We therefore
        // extend the lifetime of the raw pointer to `'_` (tied to `self`),
        // which is safe because `self` will not be mutated while the returned
        // `Chunk::Slice` is live (Rust's borrow checker enforces this).
        let slice: &[u8] = unsafe {
            core::slice::from_raw_parts(front.data.as_slice().as_ptr(), len)
        };

        front.data.advance(len);
        front.start += len as u64;
        self.cursors.start_offset += len as u64;

        // NOTE: we intentionally leave an empty `front` in the VecDeque.
        // Dropping it here would decrement the refcount and might free the
        // backing allocation while `slice` still borrows from it.  The empty
        // segment is removed by `drain_empty_front` on the next call.

        Ok(Chunk::Slice(slice))
    }

    #[inline]
    fn partial_copy_into<Dest>(
        &mut self,
        dest: &mut Dest,
    ) -> Result<Chunk<'_>, Self::Error>
    where
        Dest: writer::Storage + ?Sized,
    {
        // For destinations that want a trailing chunk: return the entire front
        // segment's data as a slice if it fits, otherwise fill dest and return
        // an empty trailing chunk.
        self.drain_empty_front();

        let Some(front) = self.inner.front_mut() else {
            return Ok(Chunk::empty());
        };

        if front.start != self.cursors.start_offset {
            return Ok(Chunk::empty());
        }

        let data_len = front.data.len();
        let remaining = dest.remaining_capacity();

        if remaining >= data_len {
            // Dest can hold all of front's data — return it as a trailing chunk.
            // SAFETY: same as `read_chunk` — advance is bookkeeping-only.
            let slice: &[u8] = unsafe {
                core::slice::from_raw_parts(front.data.as_slice().as_ptr(), data_len)
            };
            front.data.advance(data_len);
            front.start += data_len as u64;
            self.cursors.start_offset += data_len as u64;
            Ok(Chunk::Slice(slice))
        } else {
            // Dest is too small — copy what fits and return an empty chunk.
            dest.put_slice(&front.data.as_slice()[..remaining]);
            front.data.advance(remaining);
            front.start += remaining as u64;
            self.cursors.start_offset += remaining as u64;
            Ok(Chunk::empty())
        }
    }

    #[inline]
    fn copy_into<Dest>(&mut self, dest: &mut Dest) -> Result<(), Self::Error>
    where
        Dest: writer::Storage + ?Sized,
    {
        loop {
            ensure!(dest.has_remaining_capacity(), Ok(()));

            self.drain_empty_front();

            let Some(front) = self.inner.front_mut() else {
                return Ok(());
            };

            if front.start != self.cursors.start_offset {
                return Ok(()); // Gap — nothing contiguous to copy.
            }

            let to_write = front.data.len().min(dest.remaining_capacity());
            if to_write == 0 {
                return Ok(());
            }

            dest.put_slice(&front.data.as_slice()[..to_write]);
            front.data.advance(to_write);
            front.start += to_write as u64;
            self.cursors.start_offset += to_write as u64;
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Reader trait (offset metadata)
// ────────────────────────────────────────────────────────────────────────────

impl<S: SlotStorage> Reader for Segments<S> {
    #[inline]
    fn current_offset(&self) -> VarInt {
        unsafe {
            // SAFETY: start_offset is derived from VarInt additions and never
            // exceeds VarInt::MAX in practice.
            VarInt::new_unchecked(self.cursors.start_offset)
        }
    }

    #[inline]
    fn final_offset(&self) -> Option<VarInt> {
        self.final_size().map(|v| unsafe {
            // SAFETY: same as above.
            VarInt::new_unchecked(v)
        })
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Debug formatting
// ────────────────────────────────────────────────────────────────────────────

impl<S: core::fmt::Debug> core::fmt::Debug for Segment<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Segment")
            .field("start", &self.start)
            .field("data", &self.data)
            .finish()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::reader::storage::Infallible as _;

    /// Minimal in-memory `SlotStorage` implementation for unit tests.
    ///
    /// Uses an internal offset so that `advance` is bookkeeping-only (the backing
    /// `Vec` is never modified in-place).  This upholds the `SlotStorage::advance`
    /// contract required by the unsafe raw-pointer borrow in `Segments::read_chunk`.
    #[derive(Clone, Debug, PartialEq)]
    struct BytesStorage {
        data: alloc::vec::Vec<u8>,
        offset: usize,
    }

    impl BytesStorage {
        fn new(data: impl Into<alloc::vec::Vec<u8>>) -> Self {
            Self {
                data: data.into(),
                offset: 0,
            }
        }
    }

    impl SlotStorage for BytesStorage {
        fn len(&self) -> usize {
            self.data.len() - self.offset
        }

        fn as_slice(&self) -> &[u8] {
            &self.data[self.offset..]
        }

        fn advance(&mut self, len: usize) {
            assert!(len <= self.len());
            self.offset += len;
        }

        fn truncate(&mut self, len: usize) {
            let new_end = self.offset + len;
            if new_end < self.data.len() {
                self.data.truncate(new_end);
            }
        }

        fn split_off_prefix(&mut self, at: usize) -> Self {
            assert!(at <= self.len());
            let prefix = Self {
                data: self.data[self.offset..self.offset + at].to_vec(),
                offset: 0,
            };
            self.offset += at;
            prefix
        }
    }

    fn seg(data: &[u8]) -> BytesStorage {
        BytesStorage::new(data.to_vec())
    }

    // Helper: drain all readable bytes from `s` into a Vec.
    fn drain_all(s: &mut Segments<BytesStorage>) -> Vec<u8> {
        let mut out = Vec::new();
        s.infallible_copy_into(&mut out);
        out
    }

    #[test]
    fn in_order_single_segment() {
        let mut s: Segments<BytesStorage> = Segments::new();
        s.insert(VarInt::ZERO, seg(b"hello"), false).unwrap();
        assert_eq!(s.len(), 5);
        assert!(!s.is_empty());
        let bytes = drain_all(&mut s);
        assert_eq!(bytes, b"hello");
        assert_eq!(s.consumed_len(), 5);
        assert!(s.is_empty());
    }

    #[test]
    fn out_of_order_two_segments() {
        let mut s: Segments<BytesStorage> = Segments::new();
        // Insert [5,10) first, then [0,5).
        s.insert(VarInt::from_u8(5), seg(b"world"), false).unwrap();
        assert_eq!(s.len(), 0, "gap before 5 means nothing readable yet");

        s.insert(VarInt::ZERO, seg(b"hello"), false).unwrap();
        assert_eq!(s.len(), 10);
        let bytes = drain_all(&mut s);
        assert_eq!(bytes, b"helloworld");
    }

    #[test]
    fn exact_duplicate_discarded() {
        let mut s: Segments<BytesStorage> = Segments::new();
        s.insert(VarInt::ZERO, seg(b"abc"), false).unwrap();
        s.insert(VarInt::ZERO, seg(b"abc"), false).unwrap(); // duplicate
        assert_eq!(drain_all(&mut s), b"abc");
    }

    #[test]
    fn partial_overlap_left() {
        // [0,5) already in; insert [3,8).  Only [5,8) should be kept.
        let mut s: Segments<BytesStorage> = Segments::new();
        s.insert(VarInt::ZERO, seg(b"abcde"), false).unwrap();
        s.insert(VarInt::from_u8(3), seg(b"defgh"), false).unwrap();
        // We should have [0,5) + [5,8)
        let bytes = drain_all(&mut s);
        assert_eq!(bytes, b"abcdefgh");
    }

    #[test]
    fn partial_overlap_right() {
        // [5,10) already in; insert [0,8).  Only [0,5) should be kept from the new one.
        let mut s: Segments<BytesStorage> = Segments::new();
        s.insert(VarInt::from_u8(5), seg(b"fghij"), false).unwrap();
        s.insert(VarInt::ZERO, seg(b"abcdefgh"), false).unwrap();
        let bytes = drain_all(&mut s);
        assert_eq!(bytes, b"abcdefghij");
    }

    #[test]
    fn superset_covered_by_existing() {
        // [2,4) already in; insert [0,10).
        // Result: [0,2) + [2,4) + [4,10).
        let mut s: Segments<BytesStorage> = Segments::new();
        s.insert(VarInt::from_u8(2), seg(b"cd"), false).unwrap();
        s.insert(VarInt::ZERO, seg(b"abcdefghij"), false).unwrap();
        let bytes = drain_all(&mut s);
        assert_eq!(bytes, b"abcdefghij");
    }

    #[test]
    fn stale_data_discarded() {
        let mut s: Segments<BytesStorage> = Segments::new();
        s.insert(VarInt::ZERO, seg(b"hello"), false).unwrap();
        drain_all(&mut s); // consume [0,5)
        // Insert something fully stale.
        s.insert(VarInt::ZERO, seg(b"hello"), false).unwrap();
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn partially_stale_trimmed() {
        let mut s: Segments<BytesStorage> = Segments::new();
        s.insert(VarInt::ZERO, seg(b"abcde"), false).unwrap();
        drain_all(&mut s); // consume [0,5)
        // Insert [3,8) — bytes 3-4 are stale, only [5,8) is new.
        s.insert(VarInt::from_u8(3), seg(b"defgh"), false).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(drain_all(&mut s), b"fgh");
    }

    #[test]
    fn fin_sets_final_size() {
        let mut s: Segments<BytesStorage> = Segments::new();
        s.insert(VarInt::ZERO, seg(b"hello"), true).unwrap();
        assert_eq!(s.final_size(), Some(5));
        assert!(s.is_writing_complete());
        drain_all(&mut s);
        assert!(s.is_reading_complete());
    }

    #[test]
    fn fin_conflict_is_error() {
        let mut s: Segments<BytesStorage> = Segments::new();
        s.insert(VarInt::ZERO, seg(b"hello"), true).unwrap(); // final_size = 5
        // A different final size should be rejected.
        let result = s.insert(VarInt::ZERO, seg(b"helloworld"), true);
        assert!(matches!(result, Err(Error::InvalidFin)));
    }

    #[test]
    fn gap_prevents_reading() {
        let mut s: Segments<BytesStorage> = Segments::new();
        s.insert(VarInt::from_u8(5), seg(b"world"), false).unwrap();
        assert_eq!(s.len(), 0);
        assert!(s.is_empty());
        assert_eq!(drain_all(&mut s), b"");
    }

    #[test]
    fn multi_segment_chain() {
        let mut s: Segments<BytesStorage> = Segments::new();
        s.insert(VarInt::from_u8(8), seg(b"ij"), false).unwrap();
        s.insert(VarInt::from_u8(4), seg(b"efgh"), false).unwrap();
        s.insert(VarInt::ZERO, seg(b"abcd"), false).unwrap();
        assert_eq!(drain_all(&mut s), b"abcdefghij");
    }

    #[test]
    fn read_chunk_watermark() {
        let mut s: Segments<BytesStorage> = Segments::new();
        s.insert(VarInt::ZERO, seg(b"abcde"), false).unwrap();
        let chunk = s.infallible_read_chunk(3);
        assert_eq!(&chunk[..], b"abc");
        assert_eq!(s.consumed_len(), 3);
        let chunk = s.infallible_read_chunk(usize::MAX);
        assert_eq!(&chunk[..], b"de");
        assert_eq!(s.consumed_len(), 5);
    }
}
