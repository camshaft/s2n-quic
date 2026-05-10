// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

/// A trait for types that can be stored as segments in a [`super::Segments`] reassembly queue.
///
/// Implementations provide zero-copy split-and-trim semantics: the backing storage is
/// reference-counted (or otherwise stable in memory), so `advance` and `truncate` only
/// update bookkeeping without copying data.
///
/// This trait is intentionally minimal — it does **not** require allocation.  Types that
/// cannot allocate new storage (e.g. `descriptor::Filled` from the socket pool) implement
/// only this trait and are ordered inside [`super::Segments`] without ever copying bytes.
/// The one permitted copy happens at read-time when the application drains the buffer.
///
/// # Safety requirements for `advance`
///
/// After calling `advance(n)`, the memory region from which the previous `as_slice()`
/// obtained its pointer **must remain valid and unchanged** for the entire duration of
/// the borrow that called `as_slice()`.  In other words, `advance` is permitted to update
/// internal bookkeeping (e.g. an offset counter), but it **must not** move, reallocate,
/// or overwrite the bytes that `as_slice()` previously returned a pointer to.
///
/// Types backed by reference-counted allocations (e.g. `Bytes`, `Arc<[u8]>`,
/// `descriptor::Filled`) naturally satisfy this requirement.
pub trait SlotStorage: Sized {
    /// Returns the number of bytes available in this segment.
    fn len(&self) -> usize;

    /// Returns `true` if there are no bytes available.
    #[inline]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a view of the data as a contiguous byte slice.
    fn as_slice(&self) -> &[u8];

    /// Advances the start of this segment by `len` bytes, discarding the prefix.
    ///
    /// # Contract
    ///
    /// After this call the bytes that were visible through `as_slice()` **before**
    /// the call must still be present at the same memory address.  Only the internal
    /// bookkeeping (e.g. an offset field) may change.  See the trait-level safety
    /// requirement for details.
    ///
    /// # Panics
    ///
    /// Panics if `len > self.len()`.
    fn advance(&mut self, len: usize);

    /// Shortens this segment, keeping only the first `len` bytes.
    ///
    /// If `len >= self.len()` this is a no-op.
    fn truncate(&mut self, len: usize);

    /// Splits the first `at` bytes off as a new segment, advancing `self` to
    /// start at byte `at`.
    ///
    /// The returned segment contains bytes `[0, at)` and `self` is left containing
    /// bytes `[at, len)`.
    ///
    /// # Panics
    ///
    /// Panics if `at > self.len()`.
    fn split_off_prefix(&mut self, at: usize) -> Self;
}
