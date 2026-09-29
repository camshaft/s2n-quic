// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A chunked byte buffer for the DC stack.
//!
//! The chunked-buffer storage lives in the published [`etude_bytevec`] crate; this module is a thin
//! fork-local wrapper around it. [`ByteVec`] is a newtype over [`etude_bytevec::ByteVec`] that
//! carries the `s2n_quic_core::buffer::{writer,reader}::Storage` bridge (etude implements its own
//! `etude_buffer` Storage traits, so the fork-local newtype supplies the s2n-quic-core ones — a
//! newtype is required because both the trait and etude's type are foreign to this crate). Every
//! other method is either forwarded verbatim to etude via [`Deref`]/[`DerefMut`] or a thin wrapper
//! where a `Self` value crosses the boundary. The [`Builder`] (length-prefix framing, socket reads)
//! and [`tagged`] (owner/handle tagging) modules keep their fork-local logic, now layered on this
//! newtype.

use core::ops;
use s2n_quic_core::buffer::{
    reader::{
        self,
        storage::{Chunk, Infallible as _},
    },
    writer,
};
use std::io;

mod builder;
pub mod tagged;
#[cfg(test)]
mod tests;

pub use builder::Builder;
pub use etude_bytevec::{ByteVecError, Bytes, BytesMut};
pub use tagged::Tagged;

/// A vector of [`Bytes`] chunks, backed by [`etude_bytevec::ByteVec`].
///
/// Useful when you want to use a `Vec<Bytes>` in a "chunked" way, without using [`BytesMut`] to
/// build a new [`Bytes`] for each chunk.
///
/// ```
/// use s2n_quic_dc::byte_vec::ByteVec;
/// use bytes::Bytes;
///
/// let mut buf = ByteVec::default();
/// let data = Bytes::from(vec![0u8; 1000]);
/// buf.push_back(data);
///
/// for _ in 0..10 {
///     let chunk: ByteVec = buf.split_to(100).unwrap();
///     assert_eq!(chunk.len(), 100);
/// }
/// assert!(buf.pop_front().is_none());
/// ```
#[derive(Clone, Debug, Default)]
pub struct ByteVec(etude_bytevec::ByteVec);

impl ByteVec {
    /// Creates an empty [`ByteVec`].
    #[inline]
    pub fn new() -> Self {
        Self(etude_bytevec::ByteVec::new())
    }

    /// Creates a [`ByteVec`] with capacity for `cap` chunks.
    #[inline]
    pub fn with_capacity(cap: usize) -> Self {
        Self(etude_bytevec::ByteVec::with_capacity(cap))
    }

    /// Creates a [`Builder`] with the given head-buffer chunk capacity.
    #[inline]
    pub fn builder(chunk_capacity: usize) -> Builder {
        Builder::new(chunk_capacity)
    }

    /// Splits the bytes into two at the given index.
    ///
    /// Afterwards `self` contains elements `[at, len)`, and the returned [`ByteVec`] contains
    /// elements `[0, at)`.
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    ///
    /// let mut a = ByteVec::from(&b"hello world"[..]);
    /// let b = a.split_to(5).unwrap();
    ///
    /// assert_eq!(a, b" world");
    /// assert_eq!(b, b"hello");
    /// ```
    #[must_use = "consider ByteVec::advance if you don't need the other half"]
    #[inline]
    pub fn split_to(&mut self, at: usize) -> Result<Self, ByteVecError> {
        self.0.split_to(at).map(Self)
    }

    /// Moves all the elements of `other` into `self`, leaving `other` empty.
    #[inline]
    pub fn append(&mut self, other: &mut Self) {
        self.0.append(&mut other.0);
    }

    /// Flattens the [`ByteVec`] into a single [`BytesMut`] buffer, consuming it.
    #[inline]
    pub fn copy_to_bytes_mut(self) -> BytesMut {
        self.0.copy_to_bytes_mut()
    }

    /// Tags the buffer with an owner, producing a [`Tagged`].
    #[inline]
    pub fn tag<O: tagged::Owner>(self, owner: &O) -> tagged::Tagged<O> {
        tagged::Tagged::new(self, owner)
    }

    /// Returns a reference to the wrapped [`etude_bytevec::ByteVec`].
    #[inline]
    pub fn inner(&self) -> &etude_bytevec::ByteVec {
        &self.0
    }

    /// Consumes the wrapper, returning the wrapped [`etude_bytevec::ByteVec`].
    #[inline]
    pub fn into_inner(self) -> etude_bytevec::ByteVec {
        self.0
    }
}

impl ops::Deref for ByteVec {
    type Target = etude_bytevec::ByteVec;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ops::DerefMut for ByteVec {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl PartialEq for ByteVec {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for ByteVec {}

macro_rules! forward_partial_eq {
    ($($ty:ty),* $(,)?) => {
        $(
            impl PartialEq<$ty> for ByteVec {
                #[inline]
                fn eq(&self, other: &$ty) -> bool {
                    self.0.eq(other)
                }
            }
        )*
    };
}

forward_partial_eq!([u8], &[u8], str, &str, Vec<u8>, Bytes, [Bytes], &[Bytes],);

impl<const LEN: usize> PartialEq<[u8; LEN]> for ByteVec {
    #[inline]
    fn eq(&self, other: &[u8; LEN]) -> bool {
        self.0.eq(&other[..])
    }
}

impl<const LEN: usize> PartialEq<&[u8; LEN]> for ByteVec {
    #[inline]
    fn eq(&self, other: &&[u8; LEN]) -> bool {
        self.0.eq(&other[..])
    }
}

impl<const LEN: usize> PartialEq<[Bytes; LEN]> for ByteVec {
    #[inline]
    fn eq(&self, other: &[Bytes; LEN]) -> bool {
        self.0.eq(&other[..])
    }
}

impl<const LEN: usize> PartialEq<&[Bytes; LEN]> for ByteVec {
    #[inline]
    fn eq(&self, other: &&[Bytes; LEN]) -> bool {
        self.0.eq(&other[..])
    }
}

impl ops::Index<usize> for ByteVec {
    type Output = Bytes;

    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

macro_rules! forward_from {
    ($($ty:ty),* $(,)?) => {
        $(
            impl From<$ty> for ByteVec {
                #[inline]
                fn from(value: $ty) -> Self {
                    Self(etude_bytevec::ByteVec::from(value))
                }
            }
        )*
    };
}

forward_from!(
    Bytes,
    BytesMut,
    Vec<u8>,
    String,
    &'static [u8],
    &'static str,
    Vec<Bytes>,
);

impl<const LEN: usize> From<&'static [u8; LEN]> for ByteVec {
    #[inline]
    fn from(value: &'static [u8; LEN]) -> Self {
        Self(etude_bytevec::ByteVec::from(&value[..]))
    }
}

impl From<ByteVec> for Vec<Bytes> {
    #[inline]
    fn from(value: ByteVec) -> Self {
        value.0.into()
    }
}

impl Extend<Bytes> for ByteVec {
    #[inline]
    fn extend<T: IntoIterator<Item = Bytes>>(&mut self, iter: T) {
        self.0.extend(iter);
    }
}

impl Extend<Vec<u8>> for ByteVec {
    #[inline]
    fn extend<T: IntoIterator<Item = Vec<u8>>>(&mut self, iter: T) {
        self.0.extend(iter);
    }
}

impl Extend<ByteVec> for ByteVec {
    #[inline]
    fn extend<T: IntoIterator<Item = ByteVec>>(&mut self, iter: T) {
        for mut vec in iter {
            self.append(&mut vec);
        }
    }
}

impl FromIterator<Bytes> for ByteVec {
    #[inline]
    fn from_iter<T: IntoIterator<Item = Bytes>>(iter: T) -> Self {
        Self(etude_bytevec::ByteVec::from_iter(iter))
    }
}

impl IntoIterator for ByteVec {
    type Item = Bytes;
    type IntoIter = <etude_bytevec::ByteVec as IntoIterator>::IntoIter;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl io::Read for ByteVec {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut self.0, buf)
    }
}

impl io::Write for ByteVec {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut self.0, buf)
    }

    #[inline]
    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        io::Write::write_vectored(&mut self.0, bufs)
    }

    #[inline]
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        io::Write::write_all(&mut self.0, buf)
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.0)
    }
}

impl bytes::Buf for ByteVec {
    #[inline]
    fn remaining(&self) -> usize {
        bytes::Buf::remaining(&self.0)
    }

    #[inline]
    fn chunk(&self) -> &[u8] {
        bytes::Buf::chunk(&self.0)
    }

    #[inline]
    fn copy_to_bytes(&mut self, len: usize) -> Bytes {
        bytes::Buf::copy_to_bytes(&mut self.0, len)
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        bytes::Buf::advance(&mut self.0, cnt);
    }
}

impl writer::Storage for ByteVec {
    const SPECIALIZES_BYTES: bool = true;

    #[inline]
    fn put_slice(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // Binary write: push the slice as a chunk. NOT etude's `append_bytes`, which debug-asserts
        // UTF-8 (that method belongs to the str-rope variant); the DC stack stores arbitrary bytes.
        self.0.push_back(Bytes::copy_from_slice(bytes));
    }

    #[inline]
    fn put_bytes(&mut self, bytes: Bytes) {
        self.0.push_back(bytes);
    }

    #[inline]
    fn remaining_capacity(&self) -> usize {
        usize::MAX
    }
}

impl reader::Storage for ByteVec {
    type Error = core::convert::Infallible;

    #[inline]
    fn buffered_len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    fn read_chunk(&mut self, watermark: usize) -> Result<Chunk<'_>, Self::Error> {
        // Take up to `watermark` bytes off the front contiguous chunk as an owned `Bytes` (zero-copy
        // slice of the head), then hand it back as a reader chunk.
        let head_len = bytes::Buf::chunk(&self.0).len();
        let n = watermark.min(head_len);
        let bytes = bytes::Buf::copy_to_bytes(&mut self.0, n);
        Ok(bytes.into())
    }

    #[inline]
    fn partial_copy_into<Dest>(&mut self, dest: &mut Dest) -> Result<Chunk<'_>, Self::Error>
    where
        Dest: writer::Storage + ?Sized,
    {
        loop {
            let head_len = bytes::Buf::chunk(&self.0).len();
            if head_len == 0 {
                return Ok(Chunk::empty());
            }

            let remaining_capacity = dest.remaining_capacity();
            // If the head doesn't fit, return it for the caller to place (deferred copy).
            if head_len >= remaining_capacity {
                let chunk = self.read_chunk(remaining_capacity)?;
                return Ok(chunk);
            }

            // The head fits — copy it into `dest` and continue with the next chunk.
            let mut chunk = self.read_chunk(head_len)?;
            chunk.infallible_copy_into(dest);
        }
    }
}

#[cfg(any(test, feature = "testing", feature = "bolero-generator"))]
impl bolero_generator::TypeGenerator for ByteVec {
    #[inline]
    fn generate<D>(driver: &mut D) -> Option<Self>
    where
        D: bolero_generator::Driver,
    {
        // Generate the chunks directly rather than delegating to etude's `TypeGenerator` (which is
        // only available when etude's `bolero-generator` feature is on — this impl also compiles
        // under a bare `cfg(test)` where that feature may be off).
        use bolero_generator::ValueGenerator as _;

        let count = (1..4).generate(driver)?;
        let mut out = ByteVec::with_capacity(count);
        for _ in 0..count {
            let bytes: Vec<u8> = bolero_generator::TypeGenerator::generate(driver)?;
            out.push_back(bytes.into());
        }
        Some(out)
    }
}
