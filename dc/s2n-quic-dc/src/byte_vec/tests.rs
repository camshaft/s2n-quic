// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests for the fork-local `byte_vec` shim over [`etude_bytevec`].
//!
//! The chunked-buffer storage semantics are exercised in the `etude-bytevec` crate itself; these
//! tests pin the fork-local surface: the `s2n_quic_core::buffer` Storage bridge on [`ByteVec`], and
//! the [`Builder`] framing (`write_with_len_prefix`, `for_socket_read`) and its [`Behavior`], which
//! are layered on the etude buffer here, and the methods and the [`Reader`] that the newtype
//! defines itself.

use super::*;
use s2n_quic_core::buffer::{reader::Storage as _, writer::Storage as _};

fn flatten(mut bv: ByteVec) -> Vec<u8> {
    let mut out = Vec::with_capacity(bv.len());
    while let Some(chunk) = bv.pop_front() {
        out.extend_from_slice(&chunk);
    }
    out
}

#[test]
fn writer_storage_bridge_put_slice() {
    let mut bv = ByteVec::new();
    bv.put_slice(b"hello");
    bv.put_slice(b" world");
    assert_eq!(bv.len(), 11);
    assert_eq!(flatten(bv), b"hello world");
}

#[test]
fn writer_storage_bridge_put_bytes() {
    let mut bv = ByteVec::new();
    bv.put_bytes(Bytes::from_static(b"chunk-a"));
    bv.put_bytes(Bytes::from_static(b"chunk-b"));
    assert_eq!(bv.len(), 14);
    assert_eq!(flatten(bv), b"chunk-achunk-b");
}

// Pin the writer-storage specialization invariant at compile time (the DC stack relies on
// `put_bytes` being specialized rather than copied through `put_slice`).
#[allow(clippy::assertions_on_constants)] // intentional: assert a const invariant of the impl
const _: () = assert!(<ByteVec as writer::Storage>::SPECIALIZES_BYTES);

#[test]
fn writer_storage_remaining_capacity_is_unbounded() {
    let bv = ByteVec::new();
    assert!(bv.remaining_capacity() >= usize::MAX - 1);
}

#[test]
fn reader_storage_copy_into_round_trips() {
    let mut src = ByteVec::from(&b"hello world"[..]);
    let mut dst = ByteVec::new();
    src.copy_into(&mut dst).unwrap();
    assert_eq!(src.buffered_len(), 0);
    assert_eq!(flatten(dst), b"hello world");
}

#[test]
fn reader_storage_partial_copy_into_respects_capacity() {
    // dest with a 5-byte write limit takes only the first 5 bytes; the rest stays readable.
    let mut src = ByteVec::from(&b"hello world"[..]);
    let mut dst = ByteVec::new();
    {
        let mut limited = dst.with_write_limit(5);
        let mut chunk = src.partial_copy_into(&mut limited).unwrap();
        chunk.infallible_copy_into(&mut limited);
    }
    assert_eq!(flatten(dst), b"hello");
    assert_eq!(flatten(src), b" world");
}

#[test]
fn builder_put_slice_and_finish() {
    let mut b = ByteVec::builder(1024);
    b.put_slice(b"hello");
    b.put_slice(b" world");
    let out = b.finish();
    assert_eq!(out, b"hello world");
}

#[test]
fn write_with_len_prefix_single() {
    let mut b = ByteVec::builder(1024);
    b.write_with_len_prefix(|b| {
        b.put_slice(b"payload");
    });
    let out = flatten(b.finish());
    // 8-byte big-endian length prefix, then the payload
    assert_eq!(&out[..8], &7u64.to_be_bytes());
    assert_eq!(&out[8..], b"payload");
}

#[test]
fn write_with_len_prefix_preserves_prior_data_and_frames() {
    let mut b = ByteVec::builder(1024);
    // some data written before the framed section must be preserved ahead of the prefix
    b.put_slice(b"PRE");
    b.write_with_len_prefix(|b| {
        b.put_slice(b"abc");
        b.put_slice(b"de");
    });
    let out = flatten(b.finish());
    let mut expected = Vec::new();
    expected.extend_from_slice(b"PRE");
    expected.extend_from_slice(&5u64.to_be_bytes());
    expected.extend_from_slice(b"abcde");
    assert_eq!(out, expected);
}

#[test]
fn write_with_len_prefix_back_to_back() {
    let mut b = ByteVec::builder(1024);
    b.write_with_len_prefix(|b| b.put_slice(b"aa"));
    b.write_with_len_prefix(|b| b.put_slice(b"bbb"));
    let out = flatten(b.finish());
    let mut expected = Vec::new();
    expected.extend_from_slice(&2u64.to_be_bytes());
    expected.extend_from_slice(b"aa");
    expected.extend_from_slice(&3u64.to_be_bytes());
    expected.extend_from_slice(b"bbb");
    assert_eq!(out, expected);
}

#[test]
fn write_with_len_prefix_empty_payload() {
    let mut b = ByteVec::builder(1024);
    b.write_with_len_prefix(|_b| {});
    let out = flatten(b.finish());
    assert_eq!(out, 0u64.to_be_bytes());
}

#[test]
fn split_to_splits_at_index() {
    let mut a = ByteVec::from(&b"hello world"[..]);
    let b = a.split_to(5).unwrap();
    assert_eq!(b, b"hello");
    assert_eq!(a, b" world");
}

#[test]
fn append_moves_other_empty() {
    let mut a = ByteVec::from(&b"hello"[..]);
    let mut b = ByteVec::from(&b" world"[..]);
    a.append(&mut b);
    assert_eq!(a, b"hello world");
    assert!(b.is_empty());
}

#[test]
fn for_socket_read_fills_head() {
    let mut b = ByteVec::builder(1024);
    b.for_socket_read(16, |slice| {
        let data = b"socket-bytes";
        slice[..data.len()].copy_from_slice(data);
        data.len()
    });
    let out = flatten(b.finish());
    assert_eq!(out, b"socket-bytes");
}

#[test]
fn len_and_is_empty_are_functions() {
    let buffers = [ByteVec::from(&b"hello"[..]), ByteVec::new()];
    let lens: Vec<usize> = buffers.iter().map(ByteVec::len).collect();
    assert_eq!(lens, [5, 0]);
    let empty: Vec<bool> = buffers.iter().map(ByteVec::is_empty).collect();
    assert_eq!(empty, [false, true]);
}

#[test]
fn new_is_const() {
    const fn empty() -> ByteVec {
        ByteVec::new()
    }
    assert!(empty().is_empty());
}

#[test]
fn truncate_takes_an_argument_that_reads_the_buffer() {
    let mut buf = ByteVec::from(&b"hello"[..]);
    buf.push_back(Bytes::from_static(b" world"));
    buf.truncate(buf.len() - 4);
    assert_eq!(buf, b"hello w");

    // a length past the end leaves the buffer as it is
    buf.truncate(100);
    assert_eq!(buf, b"hello w");

    buf.truncate(0);
    assert!(buf.is_empty());
}

#[test]
fn copy_to_bytes_flattens_the_whole_buffer() {
    assert!(ByteVec::new().copy_to_bytes().is_empty());

    // one chunk comes back as it is, without a copy
    let chunk = Bytes::from_static(b"hello");
    let one = ByteVec::from(chunk.clone());
    let bytes = one.copy_to_bytes();
    assert_eq!(bytes, chunk);
    assert_eq!(bytes.as_ptr(), chunk.as_ptr());

    let mut two = ByteVec::from(&b"hello"[..]);
    two.push_back(Bytes::from_static(b" world"));
    assert_eq!(two.copy_to_bytes(), &b"hello world"[..]);
    // the buffer keeps its bytes
    assert_eq!(two, b"hello world");
}

#[test]
fn buf_copy_to_bytes_still_takes_a_prefix() {
    let mut buf = ByteVec::from(&b"hello world"[..]);
    let head = bytes::Buf::copy_to_bytes(&mut buf, 5);
    assert_eq!(head, &b"hello"[..]);
    assert_eq!(buf, b" world");
}

#[test]
fn reader_clone_reads_on_from_the_same_place() {
    let mut buf = ByteVec::from(&b"hello"[..]);
    buf.push_back(Bytes::from_static(b" "));
    buf.push_back(Bytes::from_static(b"world"));

    let mut reader = buf.reader();
    assert_eq!(reader.len(), 11);
    let first = reader.next().unwrap();
    assert_eq!(reader.len(), 11 - first.len());

    let copy = reader.clone();
    let rest: Vec<Bytes> = reader.collect();
    let rest_of_copy: Vec<Bytes> = copy.collect();
    assert_eq!(rest, rest_of_copy);

    let mut read = first.to_vec();
    for chunk in &rest {
        read.extend_from_slice(chunk);
    }
    assert_eq!(read, b"hello world");
    // reading doesn't change the buffer
    assert_eq!(buf, b"hello world");
}

#[test]
fn reader_of_an_empty_buffer_is_empty() {
    let buf = ByteVec::new();
    let mut reader = buf.reader();
    assert!(reader.is_empty());
    assert!(reader.next().is_none());
}

#[test]
fn reader_fits_a_derived_clone() {
    #[derive(Clone)]
    struct Decoder<'a> {
        reader: Reader<'a>,
    }

    let buf = ByteVec::from(&b"hello"[..]);
    let decoder = Decoder {
        reader: buf.reader(),
    };
    let mut out = ByteVec::new();
    out.extend(decoder.clone().reader);
    assert_eq!(out, b"hello");
    // the clone's reads leave the original where it was
    assert_eq!(decoder.reader.len(), 5);
}

/// A buffer of the three chunks "012", "3456" and "7890", so that a read crosses the boundaries of
/// its chunks.
fn three_chunks() -> ByteVec {
    let mut buf = ByteVec::from(&b"012"[..]);
    buf.push_back(Bytes::from_static(b"3456"));
    buf.push_back(Bytes::from_static(b"7890"));
    // each test below reads across these boundaries
    assert_eq!(
        buf.chunks().map(|chunk| chunk.len()).collect::<Vec<_>>(),
        [3, 4, 4]
    );
    buf
}

#[test]
fn reader_advance_moves_on_and_refuses_past_the_end() {
    let buf = three_chunks();
    let mut reader = buf.reader();

    reader.advance(0).unwrap();
    assert_eq!(reader.len(), 11);

    // into the second chunk
    reader.advance(5).unwrap();
    assert_eq!(reader.len(), 6);
    // the iterator yields the rest of the chunk the reader is in
    assert_eq!(reader.next().as_deref(), Some(&b"56"[..]));

    // a length past the end is refused, and the reader stays where it was
    let error = reader.advance(5).unwrap_err();
    assert!(matches!(error, ByteVecError::OutOfBounds(5)));
    assert_eq!(reader.len(), 4);

    // to the end of the last chunk
    reader.advance(4).unwrap();
    assert!(reader.is_empty());
    assert!(reader.next().is_none());
}

#[test]
fn reader_buf_reads_across_chunks() {
    use bytes::Buf as _;

    let buf = three_chunks();
    // `Buf::reader` takes `buf` by value, so with `Buf` in scope `buf.reader()` would call it
    let mut reader = ByteVec::reader(&buf);
    assert_eq!(reader.remaining(), 11);
    assert_eq!(reader.chunk(), b"012");

    // `advance` inside a chunk leaves the rest of that chunk as `chunk`
    bytes::Buf::advance(&mut reader, 1);
    assert_eq!(reader.remaining(), 10);
    assert_eq!(reader.chunk(), b"12");

    // a u32 across the boundary of the first two chunks
    let value = reader.try_get_u32().unwrap();
    assert_eq!(value, u32::from_be_bytes(*b"1234"));
    assert_eq!(reader.remaining(), 6);
    assert_eq!(reader.chunk(), b"56");

    // `advance` to the end of a chunk moves on to the next chunk
    bytes::Buf::advance(&mut reader, 2);
    assert_eq!(reader.chunk(), b"7890");

    // `copy_to_bytes` of a prefix of one chunk shares that chunk's memory
    let last = buf.chunks().last().unwrap();
    let prefix = bytes::Buf::copy_to_bytes(&mut reader, 2);
    assert_eq!(prefix, &b"78"[..]);
    assert_eq!(prefix.as_ptr(), last.as_ptr());

    // too few bytes are left for a u32, and the reader stays where it was
    assert!(reader.try_get_u32().is_err());
    assert_eq!(reader.chunk(), b"90");

    bytes::Buf::advance(&mut reader, 2);
    assert_eq!(reader.remaining(), 0);
    assert!(reader.chunk().is_empty());
}

#[test]
#[should_panic]
fn reader_buf_advance_past_the_end_panics() {
    let buf = three_chunks();
    let mut reader = buf.reader();
    bytes::Buf::advance(&mut reader, 12);
}

#[test]
#[should_panic]
fn reader_buf_copy_to_bytes_past_the_end_panics() {
    let buf = three_chunks();
    let mut reader = buf.reader();
    let _ = bytes::Buf::copy_to_bytes(&mut reader, 12);
}

#[test]
fn reader_storage_reads_across_chunks() {
    let buf = three_chunks();
    let mut reader = buf.reader();
    assert_eq!(reader.buffered_len(), 11);

    // `read_chunk` returns at most the rest of the chunk the reader is in
    assert_eq!(&reader.read_chunk(2).unwrap()[..], b"01");
    assert_eq!(&reader.read_chunk(usize::MAX).unwrap()[..], b"2");
    assert_eq!(reader.buffered_len(), 8);

    // `infallible_copy_into` copies the rest across the boundary of the last two chunks
    let mut out: Vec<u8> = Vec::new();
    reader.infallible_copy_into(&mut out);
    assert_eq!(out, b"34567890");
    assert_eq!(reader.buffered_len(), 0);
    assert!(reader.read_chunk(usize::MAX).unwrap().is_empty());
}

#[test]
fn reader_partial_copy_into_returns_the_chunk_that_fills_dest() {
    let buf = three_chunks();
    let mut reader = buf.reader();
    let mut out: Vec<u8> = Vec::new();
    {
        // a dest with a 5-byte write limit takes the first chunk and the front of the second
        let mut limited = out.with_write_limit(5);
        let mut chunk = reader.partial_copy_into(&mut limited).unwrap();
        assert_eq!(&chunk[..], b"34");
        chunk.infallible_copy_into(&mut limited);
    }
    assert_eq!(out, b"01234");
    // the rest stays readable
    assert_eq!(reader.len(), 6);
    assert_eq!(reader.next().as_deref(), Some(&b"56"[..]));
}

#[test]
fn reader_io_read_reads_across_chunks() {
    use std::io::Read as _;

    let buf = three_chunks();
    let mut reader = buf.reader();

    // one read fills `out` across the boundary of the first two chunks
    let mut out = [0u8; 5];
    assert_eq!(reader.read(&mut out).unwrap(), 5);
    assert_eq!(&out, b"01234");
    assert_eq!(reader.len(), 6);

    // a read into a larger buffer reads the rest, and a read after it reads nothing
    let mut rest = [0u8; 16];
    assert_eq!(reader.read(&mut rest).unwrap(), 6);
    assert_eq!(&rest[..6], b"567890");
    assert_eq!(reader.read(&mut out).unwrap(), 0);
}

#[test]
fn reader_reads_leave_the_buffer_unchanged() {
    use std::io::Read as _;

    let buf = three_chunks();
    let mut reader = buf.reader();

    bytes::Buf::advance(&mut reader, 1);
    assert_eq!(bytes::Buf::copy_to_bytes(&mut reader, 5), &b"12345"[..]);
    let mut out: Vec<u8> = Vec::new();
    reader.clone().infallible_copy_into(&mut out);
    assert_eq!(out, b"67890");
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, b"67890");

    // the buffer keeps its bytes and its chunks, and a new reader reads all of them
    assert_eq!(buf, b"01234567890");
    assert_eq!(
        buf.chunks().map(|chunk| chunk.len()).collect::<Vec<_>>(),
        [3, 4, 4]
    );
    let mut all = Vec::new();
    buf.reader().read_to_end(&mut all).unwrap();
    assert_eq!(all, b"01234567890");
}

#[test]
fn reader_skips_empty_chunks() {
    use bytes::Buf as _;

    // where the buffer keeps empty chunks, the reader moves past them
    let buf = ByteVec::from(vec![
        Bytes::new(),
        Bytes::from_static(b"012"),
        Bytes::new(),
        Bytes::from_static(b"3456"),
    ]);
    // `Buf::reader` takes `buf` by value, so with `Buf` in scope `buf.reader()` would call it
    let mut reader = ByteVec::reader(&buf);
    assert_eq!(reader.remaining(), 7);
    assert_eq!(reader.chunk(), b"012");
    assert_eq!(reader.next().as_deref(), Some(&b"012"[..]));
    assert_eq!(reader.chunk(), b"3456");
    assert_eq!(reader.next().as_deref(), Some(&b"3456"[..]));
    assert!(reader.next().is_none());
}

/// A [`Behavior`] that stores each chunk in upper case and counts its freezes, so that a test can
/// tell which chunks went through it.
#[derive(Clone, Debug, Default)]
struct Upper {
    capacity: Option<usize>,
    freezes: std::rc::Rc<core::cell::Cell<usize>>,
}

impl Behavior for Upper {
    fn default_capacity(&self) -> usize {
        self.capacity.unwrap_or(Builder::DEFAULT_CAPACITY)
    }

    fn freeze(&self, buf: BytesMut) -> Bytes {
        self.freezes.set(self.freezes.get() + 1);
        Bytes::from(buf.to_ascii_uppercase())
    }
}

#[test]
fn default_behavior_is_the_plain_builder() {
    assert_eq!(
        DefaultBehavior.default_capacity(),
        Builder::DEFAULT_CAPACITY
    );

    // freeze keeps the buffer in place
    let buf = BytesMut::from(&b"hello"[..]);
    let ptr = buf.as_ptr();
    let frozen = DefaultBehavior.freeze(buf);
    assert_eq!(frozen.as_ptr(), ptr);
    assert_eq!(frozen, &b"hello"[..]);

    let mut b: Builder = Builder::default();
    b.put_slice(b"hello");
    assert_eq!(b.finish(), b"hello");
}

#[test]
fn from_behavior_takes_the_behavior_capacity() {
    let upper = Upper {
        capacity: Some(4),
        ..Default::default()
    };
    let mut b = Builder::from_behavior(upper.clone());
    b.put_slice(b"ab");
    b.put_slice(b"cd");
    assert_eq!(upper.freezes.get(), 0);
    // the 4-byte head buffer is full, so this write flushes it as a chunk
    b.put_slice(b"ef");
    assert_eq!(upper.freezes.get(), 1);
    assert_eq!(b.finish(), b"ABCDEF");
    assert_eq!(upper.freezes.get(), 2);
}

#[test]
fn with_behavior_keeps_the_builder_capacity() {
    let upper = Upper {
        capacity: Some(4),
        ..Default::default()
    };
    let mut b = ByteVec::builder(1024).with_behavior(upper.clone());
    b.put_slice(b"ab");
    b.put_slice(b"cd");
    // the 1024-byte head buffer holds all three writes
    b.put_slice(b"ef");
    assert_eq!(upper.freezes.get(), 0);
    assert_eq!(b.finish(), b"ABCDEF");
    assert_eq!(upper.freezes.get(), 1);
}

#[test]
fn behavior_freezes_each_buffer_the_builder_completes() {
    let upper = Upper::default();
    let mut b = ByteVec::builder(1024).with_behavior(upper.clone());

    // split flushes the head buffer
    b.put_slice(b"split");
    assert_eq!(b.split(), b"SPLIT");

    // split_to moves the front of the head buffer into the chunks
    b.put_slice(b"head tail");
    assert_eq!(b.split_to(4).unwrap(), b"HEAD");

    // append flushes the head buffer, and keeps the chunks it appends as they are
    let mut other = ByteVec::from(&b" appended"[..]);
    b.append(&mut other);

    // a `BytesMut` goes through the behavior, and `Bytes` don't
    b.put_bytes_mut(BytesMut::from(&b" mut"[..]));
    b.put_bytes(Bytes::from_static(b" frozen"));

    // the payload is flushed from the head buffer, and the length prefix is made as `Bytes`
    b.write_with_len_prefix(|b| b.put_slice(b"framed"));

    let mut expected = b" TAIL appended MUT frozen".to_vec();
    expected.extend_from_slice(&6u64.to_be_bytes());
    expected.extend_from_slice(b"FRAMED");
    assert_eq!(flatten(b.finish()), expected);
    assert_eq!(upper.freezes.get(), 5);
}

#[test]
fn debug_prints_the_chunk_list() {
    assert_eq!(format!("{:?}", ByteVec::new()), "[]");
    assert_eq!(format!("{:#?}", ByteVec::new()), "[]");

    let one = ByteVec::from(&b"\x01\x02\x03"[..]);
    assert_eq!(format!("{one:?}"), "[b\"\\x01\\x02\\x03\"]");
    assert_eq!(format!("{one:#?}"), "[\n    b\"\\x01\\x02\\x03\",\n]");

    let mut two = ByteVec::new();
    two.push_back(Bytes::copy_from_slice(&3u64.to_be_bytes()));
    two.push_back(Bytes::from_static(b"\x01\x02\x03"));
    assert_eq!(
        format!("{two:?}"),
        "[b\"\\0\\0\\0\\0\\0\\0\\0\\x03\", b\"\\x01\\x02\\x03\"]"
    );
    assert_eq!(
        format!("{two:#?}"),
        "[\n    b\"\\0\\0\\0\\0\\0\\0\\0\\x03\",\n    b\"\\x01\\x02\\x03\",\n]"
    );
}

#[derive(Debug)]
struct DebugOwner;

#[derive(Clone, Debug)]
struct DebugHandle;

impl tagged::Owner for DebugOwner {
    type Handle = DebugHandle;

    fn tag(&self, _len: usize) -> Self::Handle {
        DebugHandle
    }
}

impl tagged::Handle for DebugHandle {
    fn increment(&mut self, _len: usize) {}

    fn decrement(&mut self, _len: usize) {}
}

#[test]
fn tagged_debug_prints_the_chunk_list() {
    let tagged = Tagged::new(ByteVec::from(&b"abc"[..]), &DebugOwner);
    assert_eq!(
        format!("{tagged:?}"),
        "Tagged { bytes: [b\"abc\"], tag: DebugHandle }"
    );
}
