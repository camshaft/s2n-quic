// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests for the fork-local `byte_vec` shim over [`etude_bytevec`].
//!
//! The chunked-buffer storage semantics are exercised in the `etude-bytevec` crate itself; these
//! tests pin the fork-local surface: the `s2n_quic_core::buffer` Storage bridge on [`ByteVec`], and
//! the [`Builder`] framing (`write_with_len_prefix`, `for_socket_read`) which is layered on the
//! etude buffer here.

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
