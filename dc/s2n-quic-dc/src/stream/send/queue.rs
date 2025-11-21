// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    clock::precision,
    event::{self, ConnectionPublisher},
    socket::pool::{self, descriptor},
    stream::{
        send::{application, state::transmission},
        shared,
        socket::Socket,
    },
};
use core::task::{Context, Poll};
use s2n_quic_core::{
    ensure,
    inet::SocketAddress,
    ready,
    recovery::bandwidth::Bandwidth,
    time::{timer, Clock, Timer},
    varint::VarInt,
};
use std::io::{self, IoSliceMut};

pub struct Message<'a, TransmissionAlloc> {
    queue: &'a mut Queue,
    max_segments: usize,
    remote_address: &'a SocketAddress,
    segment_alloc: &'a pool::Sharded,
    transmission_alloc: TransmissionAlloc,
}

impl<TransmissionAlloc> application::state::Message for Message<'_, TransmissionAlloc>
where
    TransmissionAlloc: Fn() -> transmission::Entry,
{
    #[inline]
    fn max_segments(&self) -> usize {
        self.max_segments
    }

    #[inline]
    fn push_with<P: FnOnce(IoSliceMut) -> transmission::Event>(&mut self, p: P) -> Option<usize> {
        let buffer = self.segment_alloc.alloc_or_grow();
        let mut event = None;
        let descriptor = buffer
            .fill_with(|addr, _cmsg, payload| {
                addr.set(*self.remote_address);
                let evt = p(payload);
                let len = evt.info.packet_len;
                event = Some(evt);
                <Result<_, core::convert::Infallible>>::Ok(len as _)
            })
            .ok()?;

        let descriptor = descriptor.take_filled();

        let accepted_len = self.queue.push_segment(
            &self.transmission_alloc,
            event.unwrap(),
            descriptor,
            self.max_segments,
        );

        Some(accepted_len)
    }

    #[inline]
    fn push(&mut self, event: transmission::Event, descriptor: descriptor::Filled) -> usize {
        self.queue.push_segment(
            &self.transmission_alloc,
            event,
            descriptor,
            self.max_segments,
        )
    }
}

#[derive(Debug, Default)]
struct Checker {
    #[cfg(debug_assertions)]
    last_packet_number: Option<VarInt>,
}

impl Checker {
    #[inline]
    fn on_transmission(&mut self, packet_number: VarInt) {
        let _ = packet_number;
        #[cfg(debug_assertions)]
        {
            if let Some(last_packet_number) = self.last_packet_number {
                assert!(
                    packet_number > last_packet_number,
                    "packet number must be greater than the last packet number"
                );
            }
            self.last_packet_number = Some(packet_number);
        }
    }
}

#[derive(Debug)]
pub struct Queue {
    /// Holds any segments that haven't been flushed to the socket
    builder: transmission::Builder,
    /// How many bytes we've accepted from the caller of `poll_write`, but actually returned
    /// `Poll::Pending` for. This many bytes will be skipped the next time `poll_write` is called.
    ///
    /// This functionality ensures that we don't return to the application until we've flushed all
    /// outstanding packets to the underlying socket. Experience has shown applications rely on
    /// TCP's behavior, which never really requires `flush` or `shutdown` to progress the stream.
    accepted_len: usize,
    /// The current bandwidth of the queue
    bandwidth: Bandwidth,
    /// The next time the queue is allowed to transmit
    next_transmission_time: Option<precision::Timestamp>,
    transmission_timer: Timer,
    checker: Checker,
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            builder: transmission::Builder::default(),
            accepted_len: 0,
            bandwidth: Bandwidth::INFINITY,
            next_transmission_time: None,
            transmission_timer: Default::default(),
            checker: Checker::default(),
        }
    }
}

impl Queue {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.builder.is_empty()
    }

    #[inline]
    pub fn accepted_len(&self) -> usize {
        self.accepted_len
    }

    #[inline]
    pub fn set_bandwidth(&mut self, bandwidth: Bandwidth) {
        self.bandwidth = bandwidth;
    }

    pub fn append(&mut self, other: &mut Self) {
        self.builder.append(&mut other.builder);
        self.accepted_len += other.accepted_len;
        self.bandwidth = self.bandwidth.max(other.bandwidth);
    }

    #[inline]
    pub fn push_buffer<A, F, O>(
        &mut self,
        remote_address: &SocketAddress,
        max_segments: usize,
        segment_alloc: &pool::Sharded,
        transmission_alloc: A,
        push: F,
    ) -> O
    where
        A: Fn() -> transmission::Entry,
        F: FnOnce(&mut Message<A>) -> O,
    {
        let mut message = Message {
            queue: self,
            max_segments,
            segment_alloc,
            transmission_alloc,
            remote_address,
        };

        push(&mut message)
    }

    fn push_segment(
        &mut self,
        transmission_alloc: impl Fn() -> transmission::Entry,
        event: transmission::Event,
        mut descriptor: descriptor::Filled,
        max_segments: usize,
    ) -> usize {
        let transmission::Event {
            packet_number,
            info,
            meta,
        } = event;

        let application_len = info.payload_len;

        descriptor.set_ecn(info.ecn);

        self.checker.on_transmission(packet_number);

        self.builder.push_segment(
            (packet_number, info),
            meta,
            application_len,
            descriptor,
            max_segments,
            transmission_alloc,
        );

        self.accepted_len += application_len as usize;

        application_len as usize
    }

    #[inline]
    pub fn poll_flush<S, C, Sub>(
        &mut self,
        cx: &mut Context,
        limit: usize,
        socket: &S,
        clock: &C,
        subscriber: &shared::Subscriber<Sub>,
    ) -> Poll<Result<usize, io::Error>>
    where
        S: ?Sized + Socket,
        C: ?Sized + Clock,
        Sub: event::Subscriber,
    {
        ready!(self.poll_flush_segments(cx, socket, clock, subscriber))?;

        // Consume accepted credits
        let accepted = limit.min(self.accepted_len);
        self.accepted_len -= accepted;
        Poll::Ready(Ok(accepted))
    }

    #[inline]
    fn poll_flush_segments<S, C, Sub>(
        &mut self,
        cx: &mut Context,
        socket: &S,
        clock: &C,
        subscriber: &shared::Subscriber<Sub>,
    ) -> Poll<Result<(), io::Error>>
    where
        S: ?Sized + Socket,
        C: ?Sized + Clock,
        Sub: event::Subscriber,
    {
        ensure!(!self.builder.is_empty(), Poll::Ready(Ok(())));

        if socket.features().is_stream() {
            self.poll_flush_segments_stream(cx, socket, clock, subscriber)
        } else {
            self.poll_flush_segments_datagram(cx, socket, clock, subscriber)
        }
    }

    #[inline]
    fn poll_flush_segments_stream<S, C, Sub>(
        &mut self,
        _cx: &mut Context,
        _socket: &S,
        _clock: &C,
        _subscriber: &shared::Subscriber<Sub>,
    ) -> Poll<Result<(), io::Error>>
    where
        S: ?Sized + Socket,
        C: ?Sized + Clock,
        Sub: event::Subscriber,
    {
        // let default_addr = addr::Addr::new(Default::default());

        // while !self.segments.is_empty() {
        //     let mut provided_len = 0;
        //     let segments = segment::Batch::new(
        //         self.segments.iter().map(|v| {
        //             let slice = v.as_slice();
        //             provided_len += slice.len();
        //             (v.ecn, v.as_slice())
        //         }),
        //         &socket.features(),
        //     );

        //     let ecn = segments.ecn();

        //     let result = socket.poll_send(cx, addr, ecn, &segments);

        //     let now = clock.get_time();

        //     drop(segments);

        //     match result {
        //         Poll::Ready(Ok(written_len)) => {
        //             subscriber.publisher(now).on_stream_write_socket_flushed(
        //                 event::builder::StreamWriteSocketFlushed {
        //                     provided_len,
        //                     committed_len: written_len,
        //                 },
        //             );

        //             self.consume_segments(written_len);

        //             // keep trying to drain the buffer
        //             continue;
        //         }
        //         Poll::Ready(Err(err)) => {
        //             subscriber.publisher(now).on_stream_write_socket_errored(
        //                 event::builder::StreamWriteSocketErrored {
        //                     provided_len,
        //                     errno: err.raw_os_error(),
        //                 },
        //             );

        //             // the socket encountered an error so clear everything out since we're shutting
        //             // down
        //             self.segments.clear();
        //             self.accepted_len = 0;
        //             return Err(err).into();
        //         }
        //         Poll::Pending => {
        //             subscriber.publisher(now).on_stream_write_socket_blocked(
        //                 event::builder::StreamWriteSocketBlocked { provided_len },
        //             );

        //             return Poll::Pending;
        //         }
        //     }
        // }

        // Ok(()).into()
        todo!()
    }

    #[inline]
    #[expect(dead_code)]
    fn consume_segments(&mut self, _consumed: usize) {
        // ensure!(consumed > 0);

        // let mut remaining = consumed;

        // while let Some(mut segment) = self.segments.pop_front() {
        //     if let Some(r) = remaining.checked_sub(segment.as_slice().len()) {
        //         remaining = r;

        //         // if we don't have any remaining bytes to pop then we're done
        //         ensure!(remaining > 0, break);

        //         continue;
        //     }

        //     segment.offset += core::mem::take(&mut remaining) as u16;

        //     debug_assert!(!segment.as_slice().is_empty());

        //     self.segments.push_front(segment);
        //     break;
        // }

        // debug_assert_eq!(
        //     remaining, 0,
        //     "consumed ({consumed}) with too many bytes remaining ({remaining})"
        // );
        todo!()
    }

    #[inline]
    fn poll_flush_segments_datagram<S, C, Sub>(
        &mut self,
        _cx: &mut Context,
        socket: &S,
        clock: &C,
        subscriber: &shared::Subscriber<Sub>,
    ) -> Poll<Result<(), io::Error>>
    where
        S: ?Sized + Socket,
        C: ?Sized + Clock,
        Sub: event::Subscriber,
    {
        while let Some((batch, application_len)) = self.builder.pop_front() {
            let now = clock.get_time();

            let time = if let Some(next) = self.next_transmission_time {
                next.max(now.into())
            } else {
                now.into()
            };

            let transmission_len = batch.total_len as u64;

            // schedule the transmission immediately
            if let Err((batch, wheel_time)) = socket.send_transmission_at(batch, time.into()) {
                // If the target time is in the past it means the wheel is overloaded so we need to back off a bit
                let time = wheel_time.max(time.into());

                self.next_transmission_time = Some(time.into());
                self.builder.push_front(batch, application_len);
                self.transmission_timer.set(time);

                return Poll::Pending;
            }

            // Compute the next transmission time given the amount of bytes we're transmitting and the bandwidth
            let delay = transmission_len / self.bandwidth;
            self.next_transmission_time = Some(time + delay);

            let provided_len = application_len as usize;
            subscriber.publisher(now).on_stream_write_socket_flushed(
                event::builder::StreamWriteSocketFlushed {
                    provided_len,
                    // if the syscall went through, then we wrote the whole thing
                    committed_len: provided_len,
                },
            );
        }

        Ok(()).into()
    }
}

impl timer::Provider for Queue {
    fn timers<Q: timer::Query>(&self, query: &mut Q) -> timer::Result {
        self.transmission_timer.timers(query)
    }
}
