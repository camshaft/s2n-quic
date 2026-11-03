// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use s2n_quic_core::buffer::writer::Storage;

const DEFAULT_CAPACITY: usize = 1 << 17;

/// The policy a [`Builder`] runs on: how it sizes its head buffer, and how a completed chunk buffer
/// becomes immutable [`Bytes`].
///
/// A [`Builder`] is *parameterized* over this behavior trait, so a caller can control every builder
/// knob by supplying one `Behavior` impl — without reimplementing the builder. Every method has a
/// default matching the plain builder, so an impl overrides only the knobs it cares about, and the
/// zero-sized [`DefaultBehavior`] (all defaults) leaves existing callers unchanged.
///
/// The motivating override is [`Behavior::freeze`] for secret hygiene: wrap the completed buffer in
/// a zeroize-on-drop owner and produce the chunk via [`Bytes::from_owner`] instead of
/// [`BytesMut::freeze`], so the backing allocation is wiped when the last chunk drops.
pub trait Behavior {
    /// The head-buffer capacity a builder uses when one is not given explicitly (e.g. via
    /// [`Builder::default`] or [`Builder::from_behavior`]). Defaults to 128 KiB.
    fn default_capacity(&self) -> usize {
        DEFAULT_CAPACITY
    }

    /// Converts a completed chunk buffer into the immutable [`Bytes`] stored in the rope. Defaults
    /// to [`BytesMut::freeze`] — reuse the buffer in place, no copy.
    fn freeze(&self, buf: BytesMut) -> Bytes {
        buf.freeze()
    }
}

/// The default [`Behavior`]: 128 KiB capacity and [`BytesMut::freeze`] — a zero-sized type, so
/// `Builder<DefaultBehavior>` costs and behaves exactly as the plain builder.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultBehavior;

impl Behavior for DefaultBehavior {}

/// A builder for efficiently constructing a [`ByteVec`] by buffering writes.
///
/// The builder maintains a head buffer for direct writes and a collection of
/// completed chunks. This allows for efficient buffering of writes while
/// maintaining the chunked nature of [`ByteVec`].
///
/// The type parameter `B` is the [`Behavior`] — the builder's capacity/freeze policy. It defaults
/// to [`DefaultBehavior`], so `Builder` (unparameterized) behaves exactly as before; use
/// [`Builder::with_behavior`] or [`Builder::from_behavior`] to select another.
///
/// # Examples
///
/// ```
/// use s2n_quic_dc::byte_vec::ByteVec;
/// use bytes::Bytes;
/// use s2n_quic_core::buffer::writer::Storage;
///
/// let mut builder = ByteVec::builder(1024);
/// builder.put_slice(b"hello");
/// builder.put_slice(b" world");
///
/// let byte_vec = builder.finish();
/// assert_eq!(byte_vec, b"hello world");
/// ```
#[derive(Debug)]
pub struct Builder<B = DefaultBehavior> {
    chunks: ByteVec,
    head: BytesMut,
    capacity: usize,
    /// The [`Behavior`] policy — the default capacity and how a completed [`BytesMut`] chunk becomes
    /// immutable [`Bytes`]. Defaults to [`DefaultBehavior`]; selected via
    /// [`Builder::with_behavior`]/[`Builder::from_behavior`].
    behavior: B,
}

impl Default for Builder<DefaultBehavior> {
    fn default() -> Self {
        Self::from_behavior(DefaultBehavior)
    }
}

impl Builder<DefaultBehavior> {
    pub const DEFAULT_CAPACITY: usize = DEFAULT_CAPACITY;

    /// Creates a new [`Builder`] with the specified capacity for the head buffer.
    ///
    /// The capacity determines the size of the internal buffer used for direct writes.
    /// When this buffer is full, it will be flushed to the chunks collection.
    ///
    /// The builder uses the [`DefaultBehavior`]; call [`Builder::with_behavior`] to select another.
    ///
    /// # Examples
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    ///
    /// let builder = ByteVec::builder(1024);
    /// ```
    pub fn new(capacity: usize) -> Self {
        Builder {
            chunks: ByteVec::new(),
            head: BytesMut::new(),
            capacity,
            behavior: DefaultBehavior,
        }
    }
}

impl<B: Behavior> Builder<B> {
    /// Creates a [`Builder`] driven by the given [`Behavior`], taking its head-buffer capacity from
    /// the behavior's [`default_capacity`](Behavior::default_capacity).
    ///
    /// This is the "control the builder with one impl" entry point. Use [`ByteVec::builder`] instead
    /// to pin an explicit capacity with the default behavior.
    pub fn from_behavior(behavior: B) -> Self {
        Builder {
            chunks: ByteVec::new(),
            head: BytesMut::new(),
            capacity: behavior.default_capacity(),
            behavior,
        }
    }

    /// Selects the [`Behavior`]: the builder's capacity/freeze policy, returning a builder
    /// parameterized over the new behavior (the buffered contents and current capacity are carried
    /// over — only the freeze behavior and future policy queries change).
    ///
    /// The behavior's [`freeze`](Behavior::freeze) is applied at every point the builder turns a
    /// [`BytesMut`] into a chunk: flushing the head buffer (on capacity overflow,
    /// [`split`](Builder::split), [`split_to`](Builder::split_to),
    /// [`append`](Builder::append)/[`extend`](Builder::extend),
    /// [`write_with_len_prefix`](Builder::write_with_len_prefix), and [`finish`](Builder::finish))
    /// and freezing a [`BytesMut`] handed to [`put_bytes_mut`](writer::Storage::put_bytes_mut). It
    /// is *not* applied to chunks supplied already-frozen as [`Bytes`]. To also adopt the behavior's
    /// default capacity, construct with [`Builder::from_behavior`] instead.
    ///
    /// The default is [`DefaultBehavior`] ([`BytesMut::freeze`], in place, no copy). Parameterizing
    /// over a trait keeps the builder generic while letting the caller add new behaviors — for
    /// example, wrapping secret-carrying buffers in a zeroize-on-drop owner and producing the chunk
    /// via [`Bytes::from_owner`].
    pub fn with_behavior<B2: Behavior>(self, behavior: B2) -> Builder<B2> {
        Builder {
            chunks: self.chunks,
            head: self.head,
            capacity: self.capacity,
            behavior,
        }
    }

    /// Converts a completed [`BytesMut`] chunk into immutable [`Bytes`] via the configured
    /// [`Behavior`] (see [`Builder::with_behavior`]).
    #[inline]
    fn freeze_chunk(&self, buf: BytesMut) -> Bytes {
        self.behavior.freeze(buf)
    }

    /// Returns the total number of bytes in the builder.
    ///
    /// This includes both the bytes in the completed chunks and the head buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    /// use s2n_quic_core::buffer::writer::Storage;
    ///
    /// let mut builder = ByteVec::builder(1024);
    /// builder.put_slice(b"hello");
    /// assert_eq!(builder.len(), 5);
    ///
    /// builder.put_slice(b" world");
    /// assert_eq!(builder.len(), 11);
    /// ```
    pub fn len(&self) -> usize {
        self.chunks.len() + self.head.len()
    }

    /// Returns `true` if the builder contains no bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    /// use s2n_quic_core::buffer::writer::Storage;
    ///
    /// let mut builder = ByteVec::builder(1024);
    /// assert!(builder.is_empty());
    ///
    /// builder.put_slice(b"hello");
    /// assert!(!builder.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.head.is_empty() && self.chunks.is_empty()
    }

    /// Appends the contents of another [`ByteVec`] to this builder.
    ///
    /// This operation flushes the current head buffer before appending
    /// the new bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    /// use s2n_quic_core::buffer::writer::Storage;
    ///
    /// let mut builder = ByteVec::builder(1024);
    /// builder.put_slice(b"hello");
    ///
    /// let mut other = ByteVec::from(b" world");
    /// builder.append(&mut other);
    ///
    /// let result = builder.finish();
    /// assert_eq!(result, b"hello world");
    /// ```
    pub fn append(&mut self, bytes: &mut ByteVec) {
        if bytes.is_empty() {
            return;
        }
        self.flush();
        self.chunks.append(bytes);
    }

    /// Appends the contents of another [`ByteVec`] to this builder.
    ///
    /// This operation flushes the current head buffer before appending
    /// the new bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    /// use s2n_quic_core::buffer::writer::Storage;
    ///
    /// let mut builder = ByteVec::builder(1024);
    /// builder.put_slice(b"hello");
    ///
    /// let other = ByteVec::from(b" world");
    /// builder.extend(&other);
    ///
    /// let result = builder.finish();
    /// assert_eq!(result, b"hello world");
    /// ```
    pub fn extend(&mut self, bytes: &ByteVec) {
        if bytes.is_empty() {
            return;
        }
        self.flush();
        self.chunks.extend(bytes.chunks().cloned());
    }

    /// Splits the builder, taking all bytes and leaving it empty.
    ///
    /// This operation flushes the current head buffer before splitting.
    ///
    /// # Examples
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    /// use s2n_quic_core::buffer::writer::Storage;
    ///
    /// let mut builder = ByteVec::builder(1024);
    /// builder.put_slice(b"hello world");
    ///
    /// let bytes = builder.split();
    /// assert_eq!(bytes, b"hello world");
    /// assert!(builder.is_empty());
    /// ```
    pub fn split(&mut self) -> ByteVec {
        self.flush();
        core::mem::take(&mut self.chunks)
    }

    /// Splits the bytes into two at the given index.
    ///
    /// After this operation, `self` contains elements `[at, len)`, and the
    /// returned [`ByteVec`] contains elements `[0, at)`.
    ///
    /// # Errors
    ///
    /// Returns [`ByteVecError::OutOfBounds`] if `at` is greater than the
    /// builder's length.
    ///
    /// # Examples
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    /// use s2n_quic_core::buffer::writer::Storage;
    ///
    /// let mut builder = ByteVec::builder(1024);
    /// builder.put_slice(b"hello world");
    ///
    /// let hello = builder.split_to(5).unwrap();
    /// assert_eq!(hello, b"hello");
    ///
    /// let result = builder.finish();
    /// assert_eq!(result, b" world");
    /// ```
    pub fn split_to(&mut self, at: usize) -> Result<ByteVec, ByteVecError> {
        let len = self.len();

        if len < at {
            return Err(ByteVecError::OutOfBounds(at));
        }

        if len == at {
            return Ok(self.split());
        }

        // check if we need to move some of the head into the chunks
        if let Some(remaining) = at.checked_sub(self.chunks.len()).filter(|v| *v > 0) {
            let buf = self.head.split_to(remaining);
            let chunk = self.freeze_chunk(buf);
            self.chunks.push_back(chunk);
        }

        self.chunks.split_to(at)
    }

    /// Finishes building and returns the constructed [`ByteVec`].
    ///
    /// This operation consumes the builder and returns the final [`ByteVec`]
    /// containing all written bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use s2n_quic_dc::byte_vec::ByteVec;
    /// use s2n_quic_core::buffer::writer::Storage;
    ///
    /// let mut builder = ByteVec::builder(1024);
    /// builder.put_slice(b"hello");
    /// builder.put_slice(b" world");
    ///
    /// let result = builder.finish();
    /// assert_eq!(result, b"hello world");
    /// ```
    pub fn finish(self) -> ByteVec {
        let Builder {
            mut chunks,
            head,
            behavior,
            ..
        } = self;
        if !head.is_empty() {
            chunks.push_back(behavior.freeze(head));
        }
        chunks
    }

    /// Calls the provided function and prefixes the written data with a `u64` length
    pub fn write_with_len_prefix<Body: FnOnce(&mut Self)>(&mut self, f: Body) {
        // flush any data we have buffered
        self.flush();

        // record the chunk index where to insert the length
        let chunk_index = self.chunks.chunks().len();

        // record the starting length
        let before_len = self.len();

        // have the caller write into the buffer
        f(self);

        // flush any data we have buffered from the caller write
        self.flush();

        // compute the amount of data written by the caller
        let written_len = (self.len() - before_len) as u64;

        // write the length into the `head` buffer ensuring it stays in one chunk
        let written_len_bytes = written_len.to_be_bytes();
        if written_len_bytes.len() > self.head.spare_capacity_mut().len() {
            self.flush_and_reserve(written_len_bytes.len());
        }
        self.head.put_slice(&written_len_bytes);

        // insert the length chunk where we recorded initially
        let buf = self.head.split();
        let len_chunk = self.freeze_chunk(buf);
        // make sure the length chunk is not torn
        debug_assert_eq!(len_chunk.len(), 8);
        self.chunks.insert(chunk_index, len_chunk);
    }

    /// Reserves buffer space for reading from a socket
    pub fn for_socket_read<Cb: FnOnce(&mut bytes::buf::UninitSlice) -> usize>(
        &mut self,
        preferred_read_size: usize,
        f: Cb,
    ) {
        if preferred_read_size > self.head.spare_capacity_mut().len() {
            self.flush_and_reserve(preferred_read_size);
        }

        let len = self
            .head
            .put_uninit_slice(preferred_read_size, |slice| {
                let len = f(slice);
                Err(len)
            })
            .unwrap_err();

        unsafe {
            use bytes::BufMut;
            self.head.advance_mut(len);
        }
    }

    // flushes and reserves at least the specified `min_len`
    fn flush_and_reserve(&mut self, min_len: usize) {
        let capacity = self.capacity.max(min_len);
        let head = core::mem::replace(&mut self.head, BytesMut::with_capacity(capacity));
        if !head.is_empty() {
            let chunk = self.freeze_chunk(head);
            self.chunks.push_back(chunk);
        }
    }

    // flushes the current buffer, if non-empty
    fn flush(&mut self) {
        if !self.head.is_empty() {
            let buf = self.head.split();
            let chunk = self.freeze_chunk(buf);
            self.chunks.push_back(chunk);
        }
    }
}

impl From<ByteVec> for Builder<DefaultBehavior> {
    fn from(chunks: ByteVec) -> Self {
        Builder {
            chunks,
            head: BytesMut::new(),
            capacity: DEFAULT_CAPACITY,
            behavior: DefaultBehavior,
        }
    }
}

impl<B: Behavior> From<Builder<B>> for ByteVec {
    fn from(writer: Builder<B>) -> Self {
        writer.finish()
    }
}

impl<B: Behavior> writer::Storage for Builder<B> {
    // we prefer direct writes into the head chunk rather than appending byte chunks
    const SPECIALIZES_BYTES: bool = false;
    const SPECIALIZES_BYTES_MUT: bool = false;

    fn put_slice(&mut self, bytes: &[u8]) {
        let remaining_capacity = self.head.spare_capacity_mut().len();
        let len = bytes.len().min(remaining_capacity);
        let (head, tail) = bytes.split_at(len);

        // append the head if it has capacity
        if !head.is_empty() {
            self.head.put_slice(head);
        }

        // if tail is non-empty then we need to allocate a new chunk
        if !tail.is_empty() {
            self.flush_and_reserve(tail.len());
            self.head.put_slice(tail);
        }
    }

    fn remaining_capacity(&self) -> usize {
        usize::MAX
    }

    fn put_uninit_slice<Fill, Error>(&mut self, payload_len: usize, f: Fill) -> Result<bool, Error>
    where
        Fill: FnOnce(&mut bytes::buf::UninitSlice) -> Result<(), Error>,
    {
        if payload_len > self.head.spare_capacity_mut().len() {
            self.flush_and_reserve(payload_len);
        }

        self.head.put_uninit_slice(payload_len, f)
    }

    fn has_remaining_capacity(&self) -> bool {
        true
    }

    fn put_bytes(&mut self, bytes: Bytes) {
        if bytes.is_empty() {
            return;
        }
        self.flush();
        self.chunks.push_back(bytes);
    }

    fn put_bytes_mut(&mut self, bytes: BytesMut) {
        if bytes.is_empty() {
            return;
        }
        self.flush();
        let chunk = self.freeze_chunk(bytes);
        self.chunks.push_back(chunk);
    }
}

impl<B: Behavior> reader::Storage for Builder<B> {
    type Error = core::convert::Infallible;

    fn buffered_len(&self) -> usize {
        self.head.buffered_len() + self.chunks.buffered_len()
    }

    fn buffer_is_empty(&self) -> bool {
        self.head.is_empty() && self.chunks.is_empty()
    }

    fn read_chunk(&mut self, watermark: usize) -> Result<Chunk<'_>, Self::Error> {
        if self.chunks.is_empty() {
            self.head.read_chunk(watermark)
        } else {
            self.chunks.read_chunk(watermark)
        }
    }

    fn partial_copy_into<Dest>(&mut self, dest: &mut Dest) -> Result<Chunk<'_>, Self::Error>
    where
        Dest: writer::Storage + ?Sized,
    {
        // First drain chunks into dest
        if !self.chunks.buffer_is_empty() {
            let chunk = self.chunks.partial_copy_into(dest)?;

            if !chunk.is_empty() {
                let mut should_return = false;

                // If dest matches the chunk, return it
                should_return |= dest.remaining_capacity() == chunk.len();

                // if the `head` is empty then return early as well
                should_return |= self.head.buffer_is_empty();

                if should_return {
                    return Ok(chunk);
                }

                // Otherwise, write the trailing chunk to dest and continue to head
                dest.put_chunk(chunk);
            }
        }

        // Then drain head into dest
        self.head.partial_copy_into(dest)
    }

    fn copy_into<Dest>(&mut self, dest: &mut Dest) -> Result<(), Self::Error>
    where
        Dest: writer::Storage + ?Sized,
    {
        self.chunks.copy_into(dest)?;
        self.head.copy_into(dest)
    }
}
