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
//! where a `Self` value crosses the boundary. A few methods are inherent on the newtype itself:
//! `len` and `is_empty`, so that `ByteVec::len` names a function; `truncate`, so that a
//! call such as `buf.truncate(buf.len() - 1)` compiles; and `copy_to_bytes`, so that a call
//! without a length isn't taken for `bytes::Buf::copy_to_bytes`. [`Reader`] is the newtype's own,
//! so that a reader can be cloned. The [`Builder`] (length-prefix framing, socket reads, and its
//! [`Behavior`]) and [`tagged`] (owner/handle tagging) modules keep their fork-local logic, now
//! layered on this newtype.

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

pub use builder::{Behavior, Builder, DefaultBehavior};
// etude-bytevec's chunk and drain iterators are re-exported so that callers can name them without
// a dependency on etude-bytevec.
pub use etude_bytevec::{ByteVecError, Bytes, BytesMut, ChunkIter, DrainIter};
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
    pub const fn new() -> Self {
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

    /// Returns the total number of bytes in the buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    ///
    /// let buffers = [ByteVec::from(&b"hello"[..]), ByteVec::new()];
    /// let lens: Vec<usize> = buffers.iter().map(ByteVec::len).collect();
    /// assert_eq!(lens, [5, 0]);
    /// ```
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if the buffer is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    ///
    /// assert!(ByteVec::new().is_empty());
    /// assert!(!ByteVec::from(&b"hello"[..]).is_empty());
    /// ```
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Shortens the buffer to its first `len` bytes. It does nothing where `len` is the buffer's
    /// length or more.
    ///
    /// # Examples
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    ///
    /// let mut buf = ByteVec::from(&b"hello world"[..]);
    /// buf.truncate(buf.len() - 6);
    /// assert_eq!(buf, b"hello");
    ///
    /// buf.truncate(10);
    /// assert_eq!(buf, b"hello");
    /// ```
    #[inline]
    pub fn truncate(&mut self, len: usize) {
        self.0.truncate(len);
    }

    /// Flattens the buffer into one [`Bytes`], and leaves the buffer as it is.
    ///
    /// A buffer of one chunk returns that chunk without a copy, and a buffer of more chunks copies
    /// them into one new buffer, so read the chunks one by one where one buffer isn't needed. To
    /// take bytes off the front of the buffer, call `bytes::Buf::copy_to_bytes(&mut buf, len)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    /// use bytes::Bytes;
    ///
    /// let mut buf = ByteVec::new();
    /// buf.push_back(Bytes::from("hello"));
    /// buf.push_back(Bytes::from(" world"));
    ///
    /// assert_eq!(buf.copy_to_bytes(), &b"hello world"[..]);
    /// assert_eq!(buf, b"hello world");
    /// ```
    #[inline]
    pub fn copy_to_bytes(&self) -> Bytes {
        let mut chunks = self.0.chunks();
        let Some(first) = chunks.next() else {
            return Bytes::new();
        };
        let Some(second) = chunks.next() else {
            // one chunk: hand it back without a copy
            return first.clone();
        };
        let mut out = BytesMut::with_capacity(self.0.len());
        out.extend_from_slice(first);
        out.extend_from_slice(second);
        for chunk in chunks {
            out.extend_from_slice(chunk);
        }
        out.freeze()
    }

    /// Returns a [`Reader`] over the buffer's chunks. The reader borrows the buffer, and a clone of
    /// it reads on from the same place.
    ///
    /// # Examples
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    ///
    /// let buf = ByteVec::from(&b"hello"[..]);
    /// let mut reader = buf.reader();
    /// let copy = reader.clone();
    ///
    /// assert_eq!(reader.next().as_deref(), Some(&b"hello"[..]));
    /// assert!(reader.next().is_none());
    /// assert_eq!(copy.len(), 5);
    /// ```
    #[inline]
    pub fn reader(&self) -> Reader<'_> {
        Reader::new(&self.0)
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

/// A reader over the chunks of a [`ByteVec`], which [`ByteVec::reader`] returns.
///
/// It yields each chunk as a [`Bytes`], a reference-counted handle to the chunk's memory, and
/// borrows the [`ByteVec`] instead of cloning its list of chunks. A clone of a reader reads on from
/// the place the reader had reached, and the two then read independently.
///
/// As the reader before the move to etude-bytevec did, it also implements [`bytes::Buf`],
/// s2n-quic-core's [`reader::Storage`] and [`io::Read`], and [`Reader::advance`] moves it on
/// without reading. A read through any of them can stop inside a chunk, and the next read starts
/// from that place. None of them changes the [`ByteVec`] the reader borrows.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    buffer: &'a etude_bytevec::ByteVec,
    /// The index in `buffer` of the chunk that holds the next byte to read.
    index: usize,
    /// The bytes of that chunk the reader has read already.
    offset: usize,
    /// The bytes left to read.
    remaining: usize,
}

impl<'a> Reader<'a> {
    #[inline]
    fn new(buffer: &'a etude_bytevec::ByteVec) -> Self {
        let mut reader = Self {
            buffer,
            index: 0,
            offset: 0,
            remaining: buffer.len(),
        };
        reader.skip_read_chunks();
        reader
    }

    /// Returns the number of bytes left to read.
    #[inline]
    pub fn len(&self) -> usize {
        self.remaining
    }

    /// Returns `true` if no bytes are left to read.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.remaining == 0
    }

    /// Moves the reader `len` bytes on without reading them.
    ///
    /// # Errors
    ///
    /// Returns [`ByteVecError::OutOfBounds`], and leaves the reader where it was, if `len` is
    /// greater than the number of bytes left to read.
    ///
    /// # Examples
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    /// use bytes::Bytes;
    ///
    /// let mut buf = ByteVec::from(&b"hello"[..]);
    /// buf.push_back(Bytes::from_static(b" world"));
    ///
    /// let mut reader = buf.reader();
    /// reader.advance(6).unwrap();
    /// assert_eq!(reader.len(), 5);
    /// assert!(reader.advance(6).is_err());
    /// assert_eq!(reader.next().as_deref(), Some(&b"world"[..]));
    /// ```
    #[inline]
    pub fn advance(&mut self, len: usize) -> Result<(), ByteVecError> {
        if len > self.remaining {
            return Err(ByteVecError::OutOfBounds(len));
        }

        let mut left = len;
        while left > 0 {
            let read = left.min(self.head().len());
            self.consume(read);
            left -= read;
        }

        Ok(())
    }

    /// Returns the bytes left to read in the chunk that holds the next byte to read. They are
    /// empty only once the reader has read every byte.
    #[inline]
    fn head(&self) -> &'a [u8] {
        if self.remaining == 0 {
            return &[];
        }
        let buffer: &'a etude_bytevec::ByteVec = self.buffer;
        &buffer[self.index][self.offset..]
    }

    /// Takes up to `watermark` bytes off the front of `head` as a [`Bytes`] that shares the
    /// chunk's memory, and moves the reader past them.
    #[inline]
    fn read_head(&mut self, watermark: usize) -> Bytes {
        let len = watermark.min(self.head().len());
        if len == 0 {
            return Bytes::new();
        }
        let chunk = &self.buffer[self.index];
        let bytes = chunk.slice(self.offset..self.offset + len);
        self.consume(len);
        bytes
    }

    /// Moves the reader `len` bytes on in `head`, where `len` is at most the length of `head`.
    #[inline]
    fn consume(&mut self, len: usize) {
        self.offset += len;
        self.remaining -= len;
        self.skip_read_chunks();
    }

    /// Moves the reader past each chunk it has read to its end and past each empty chunk, so that
    /// `head` isn't empty while bytes are left to read.
    #[inline]
    fn skip_read_chunks(&mut self) {
        while self.remaining > 0 && self.offset == self.buffer[self.index].len() {
            self.index += 1;
            self.offset = 0;
        }
    }
}

impl Iterator for Reader<'_> {
    type Item = Bytes;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        // the rest of the chunk the reader is in, which is empty only once every byte is read
        let chunk = self.read_head(usize::MAX);
        if chunk.is_empty() {
            None
        } else {
            Some(chunk)
        }
    }
}

impl bytes::Buf for Reader<'_> {
    #[inline]
    fn remaining(&self) -> usize {
        self.remaining
    }

    #[inline]
    fn chunk(&self) -> &[u8] {
        self.head()
    }

    #[inline]
    fn copy_to_bytes(&mut self, len: usize) -> Bytes {
        assert!(
            len <= self.remaining,
            "`len` is greater than the number of bytes left to read"
        );

        // a prefix of the chunk the reader is in shares the chunk's memory, without a copy
        let head = self.read_head(len);
        if head.len() == len {
            return head;
        }

        let mut out = BytesMut::with_capacity(len);
        out.extend_from_slice(&head);
        while out.len() < len {
            let chunk = self.read_head(len - out.len());
            out.extend_from_slice(&chunk);
        }
        out.freeze()
    }

    #[inline]
    fn advance(&mut self, len: usize) {
        Self::advance(self, len).unwrap();
    }
}

impl reader::Storage for Reader<'_> {
    type Error = core::convert::Infallible;

    #[inline]
    fn buffered_len(&self) -> usize {
        self.remaining
    }

    #[inline]
    fn read_chunk(&mut self, watermark: usize) -> Result<Chunk<'_>, Self::Error> {
        Ok(self.read_head(watermark).into())
    }

    #[inline]
    fn partial_copy_into<Dest>(&mut self, dest: &mut Dest) -> Result<Chunk<'_>, Self::Error>
    where
        Dest: writer::Storage + ?Sized,
    {
        loop {
            let head_len = self.head().len();
            if head_len == 0 {
                return Ok(Chunk::empty());
            }

            let remaining_capacity = dest.remaining_capacity();
            // the chunk that fills `dest` goes back to the caller, which copies it later
            if head_len >= remaining_capacity {
                return Ok(self.read_head(remaining_capacity).into());
            }

            // the chunk fits, so copy it into `dest` and go on with the next one
            let mut chunk = Chunk::from(self.read_head(head_len));
            chunk.infallible_copy_into(dest);
        }
    }
}

impl io::Read for Reader<'_> {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(self.remaining);
        bytes::Buf::copy_to_slice(self, &mut buf[..len]);
        Ok(len)
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
