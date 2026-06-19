// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Ordered streaming reads — spray + reassemble.
//!
//! [`MaterializeStream`] reads a logical object whose blocks scatter across devices and delivers
//! the bytes in **FIFO order** even though the block reads complete out of order, with bounded
//! read-ahead. It replaces the per-stream `futures::stream::iter(blocks).map(read).buffered(N)`
//! island that each Membrain materialization spins up today — the islanding (one `spawn_blocking`
//! storm per stream, no coordination) is what deadlocks and what loses fairness.
//!
//! The model is QUIC packet-spray: every block submits at the **same** priority (no mid-flight
//! reprioritization — re-tiering a parked acquire is painful and unnecessary), the read-ahead is
//! bounded by a fixed window, and the completions are reassembled in submission order by a
//! position-indexed window. Because every sprayed block is an ordinary credit-governed
//! [`SubmitHandle::submit`], it interleaves at the per-device pools with every other handle's work
//! — the cross-handle fairness `.buffered()` cannot provide.
//!
//! Concurrency is bounded the same way `.buffered(N)` bounds it: at most `read_ahead` block reads
//! are in flight (pulled lazily from the `impl Iterator` block source), and a slow consumer that
//! stops polling naturally stops the spray — credit for the next block is not even acquired until a
//! delivered block frees a window slot.

use crate::{
    fs::scheduler::{BlockRef, SubmitHandle},
    sched::TierPriority,
};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use std::collections::VecDeque;

type ReadFuture = Pin<Box<dyn Future<Output = std::io::Result<bytes::BytesMut>>>>;

/// One in-flight (or completed) block in the read-ahead window, kept in submission order.
enum Slot {
    /// The block read is in flight.
    Pending { fut: ReadFuture, block: BlockRef },
    /// The block read finished; holds the (trimmed) bytes awaiting in-order delivery.
    Ready(std::io::Result<bytes::BytesMut>),
}

/// An ordered, bounded-read-ahead stream over a scattered block list.
///
/// Poll it with [`MaterializeStream::next`]; each call resolves to the next block's bytes in FIFO
/// order, or `None` at end of stream.
pub struct MaterializeStream<I> {
    handle: SubmitHandle,
    blocks: I,
    priority: TierPriority,
    read_ahead: usize,
    /// In-flight / ready window, front = next block to deliver (the head).
    window: VecDeque<Slot>,
    /// Set once the block iterator is exhausted; the stream ends when the window then drains.
    source_done: bool,
}

impl<I> MaterializeStream<I>
where
    I: Iterator<Item = BlockRef>,
{
    pub(crate) fn new(
        handle: SubmitHandle,
        blocks: I,
        priority: TierPriority,
        read_ahead: usize,
    ) -> Self {
        Self {
            handle,
            blocks,
            priority,
            read_ahead: read_ahead.max(1),
            window: VecDeque::new(),
            source_done: false,
        }
    }

    /// Top up the read-ahead window: pull blocks from the source and submit each (sprayed across
    /// devices, governed by per-device credit) until the window is full or the source is exhausted.
    fn refill(&mut self) {
        while self.window.len() < self.read_ahead {
            let Some(block) = self.blocks.next() else {
                self.source_done = true;
                break;
            };
            let fut = Box::pin(self.handle.read(
                block.device,
                block.fd,
                block.offset,
                block.len,
                self.priority,
            )) as ReadFuture;
            self.window.push_back(Slot::Pending { fut, block });
        }
    }

    /// Poll for the next block in FIFO order.
    pub fn poll_next(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::io::Result<bytes::BytesMut>>> {
        // Keep the window full so reads behind the head make progress concurrently.
        self.refill();

        if self.window.is_empty() {
            // Nothing in flight and the source is drained → end of stream.
            debug_assert!(self.source_done);
            return Poll::Ready(None);
        }

        // Drive EVERY in-flight slot so out-of-order completions land — this is the spray: deeper
        // reads progress concurrently while the head is still outstanding. They all share this
        // task's waker (each `submit` future registers it on its completion channel), so any
        // completion re-polls the stream.
        for slot in self.window.iter_mut() {
            if let Slot::Pending { fut, block } = slot {
                if let Poll::Ready(result) = fut.as_mut().poll(cx) {
                    let result = result.map(|buf| trim(buf, *block));
                    *slot = Slot::Ready(result);
                }
            }
        }

        // Deliver only the head, preserving FIFO order regardless of completion order.
        if matches!(self.window.front(), Some(Slot::Ready(_))) {
            let Some(Slot::Ready(result)) = self.window.pop_front() else {
                unreachable!("front matched Ready but pop_front disagreed");
            };
            return Poll::Ready(Some(result));
        }

        Poll::Pending
    }

    /// Await the next block in FIFO order.
    pub async fn next(&mut self) -> Option<std::io::Result<bytes::BytesMut>> {
        core::future::poll_fn(|cx| self.poll_next(cx)).await
    }
}

/// Apply head/tail trim to a block's bytes (for sub-range reads whose edge blocks are partial).
fn trim(mut buf: bytes::BytesMut, block: BlockRef) -> bytes::BytesMut {
    use bytes::Buf as _;
    let len = buf.len();
    let tail = block.tail_trim as usize;
    if tail < len {
        buf.truncate(len - tail);
    } else if tail > 0 {
        buf.clear();
    }
    let head = (block.head_trim as usize).min(buf.len());
    if head > 0 {
        buf.advance(head);
    }
    buf
}
