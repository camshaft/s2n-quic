// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    event::{self, ConnectionPublisher},
    msg::addr,
    socket::{
        pool::{descriptor, Pool},
        send::wheel::{self, Wheel},
    },
    stream::{
        send::{
            application::{self, transmission},
            state::Transmission,
        },
        shared,
        socket::Socket,
    },
    sync::intrusive_queue::Queue as CompletionQueue,
};
use bytes::buf::UninitSlice;
use core::task::{Context, Poll};
use s2n_quic_core::{
    buffer::reader, inet::ExplicitCongestionNotification,
    time::{Clock, Timestamp},
};
use s2n_quic_platform::features::Gso;
use std::{io, sync::Weak};

pub struct Message<'a> {
    batch: &'a mut Option<Vec<Transmission>>,
    queue: &'a mut Queue,
    max_segments: usize,
    segment_alloc: &'a Pool,
    completion: Weak<CompletionQueue<wheel::Transmission<Vec<transmission::Info<descriptor::Filled>>>>>,
}

impl application::state::Message for Message<'_> {
    #[inline]
    fn max_segments(&self) -> usize {
        self.max_segments
    }

    #[inline]
    fn push<P: FnOnce(&mut UninitSlice) -> transmission::Event<()>>(
        &mut self,
        buffer_len: usize,
        p: P,
    ) -> Option<usize> {
        // If wheel is not available yet (Work Item 1 not completed), fall back to buffering
        if self.queue.wheel.is_none() {
            // Wheel not yet set up, cannot push to wheel
            // This will be properly handled once Work Item 1 is complete
            return None;
        }
        
        // Allocate descriptor from pool
        let unfilled = self.segment_alloc.alloc()?;
        
        // Fill the descriptor with packet data using the provided closure
        let segments = unfilled.fill_with(|_addr, _cmsg, mut iov| {
            // Set up a temporary uninitialized slice for the callback
            let capacity = iov.len().min(buffer_len);
            let slice = unsafe {
                // SAFETY: The slice comes from the IoSliceMut which is valid memory
                UninitSlice::from_raw_parts_mut(iov.as_mut_ptr(), capacity)
            };
            
            // Call the packet filling callback
            let _transmission_event = p(slice);
            
            // Return the packet length that was filled
            Ok::<_, std::convert::Infallible>(capacity)
        }).ok()?;
        
        // Get ECN from the segments
        // Note: We need to access ECN without going through the private descriptor field
        // For now, we'll use a default value until the proper API is exposed
        let ecn = ExplicitCongestionNotification::NotEct;
        
        // Create transmission info for wheel insertion
        // For now, create a single-item Vec (GSO support will be added in Work Item 13)
        let info = vec![transmission::Info {
            packet_len: segments.total_payload_len(),
            retransmission: None, // Will be populated when retransmission support is added
            stream_offset: s2n_quic_core::varint::VarInt::ZERO, // Will be provided by caller in future
            payload_len: segments.total_payload_len(),
            included_fin: false,
            time_sent: unsafe { Timestamp::from_duration(std::time::Duration::ZERO) },
            ecn,
        }];
        
        // Create wheel transmission entry
        let transmission = wheel::Transmission {
            descriptor: segments,
            transmission: None, // Will be set by the wheel
            info,
            completion: self.completion.clone(),
        };
        
        // Calculate transmission timestamp (current time for now, pacing will be added in Work Item 8)
        // TODO: Get current time from a clock reference
        let timestamp = unsafe { Timestamp::from_duration(std::time::Duration::from_micros(1)) };
        
        // Insert into the wheel (unwrap is safe because we checked is_some above)
        let entry = wheel::Entry::new(transmission);
        self.queue.wheel.as_ref().unwrap().insert(entry, timestamp);
        
        // Track accepted length for flow control
        self.queue.accepted_len += buffer_len;
        
        Some(buffer_len)
    }
}

#[derive(Debug)]
pub struct Queue {
    /// How many bytes we've accepted from the caller of `poll_write`, but actually returned
    /// `Poll::Pending` for. This many bytes will be skipped the next time `poll_write` is called.
    ///
    /// This functionality ensures that we don't return to the application until we've flushed all
    /// outstanding packets to the underlying socket. Experience has shown applications rely on
    /// TCP's behavior, which never really requires `flush` or `shutdown` to progress the stream.
    ///
    /// Note: With the wheel-based transmission model, flow control is now managed by tracking
    /// pending transmissions in the flow controller (Work Item 5). This field will be phased out
    /// as flow control is fully implemented.
    accepted_len: usize,
    
    /// Wheel for scheduling transmissions
    /// TODO: This should be provided via Work Item 1 (Pool Integration in Environment Layer)
    /// For now, it's an Option to maintain backward compatibility during the transition
    wheel: Option<Wheel<Vec<transmission::Info<descriptor::Filled>>>>,
    
    /// Completion queue for receiving transmission completions
    /// TODO: This should be provided via Work Item 2 (Stream Completion Queue Setup)
    /// For now, it's an Option to maintain backward compatibility during the transition
    completion: Option<Weak<CompletionQueue<wheel::Transmission<Vec<transmission::Info<descriptor::Filled>>>>>>,
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            accepted_len: 0,
            wheel: None,
            completion: None,
        }
    }
}

impl Queue {
    #[inline]
    pub fn is_empty(&self) -> bool {
        // With wheel-based transmission, the queue is always "empty" from the application's
        // perspective since packets are immediately inserted into the wheel
        true
    }

    #[inline]
    pub fn accepted_len(&self) -> usize {
        self.accepted_len
    }

    #[inline]
    pub fn push_buffer<B, F, E>(
        &mut self,
        buf: &mut B,
        batch: &mut Option<Vec<Transmission>>,
        max_segments: usize,
        segment_alloc: &Pool,
        push: F,
    ) -> Result<(), E>
    where
        B: reader::Storage,
        F: FnOnce(&mut Message, &mut reader::storage::Tracked<B>) -> Result<(), E>,
    {
        // Clone completion reference before creating Message
        let completion = self.completion.clone().unwrap_or_else(|| Weak::new());
        
        let mut message = Message {
            batch,
            queue: self,
            max_segments,
            segment_alloc,
            completion,
        };

        let mut buf = buf.track_read();

        push(&mut message, &mut buf)?;

        // Note: accepted_len is now tracked within Message::push
        // This maintains backward compatibility during the transition
        Ok(())
    }

    /// Poll to flush pending transmissions
    ///
    /// With the wheel-based transmission model, packets are inserted directly into the wheel
    /// by Message::push. This method now only manages flow control credits (accepted_len).
    /// The actual transmission is handled by the socket worker polling the wheel.
    ///
    /// Note: The socket/addr/gso/clock/subscriber parameters are kept for API compatibility
    /// during the transition but are no longer used for direct socket transmission.
    #[inline]
    pub fn poll_flush<S, C, Sub>(
        &mut self,
        _cx: &mut Context,
        limit: usize,
        _socket: &S,
        _addr: &addr::Addr,
        _gso: &Gso,
        _clock: &C,
        _subscriber: &shared::Subscriber<Sub>,
    ) -> Poll<Result<usize, io::Error>>
    where
        S: ?Sized + Socket,
        C: ?Sized + Clock,
        Sub: event::Subscriber,
    {
        // With wheel-based transmission, packets are immediately inserted into the wheel
        // and handled by the socket worker. We just need to release flow control credits.
        
        // Consume accepted credits up to the limit
        let accepted = limit.min(self.accepted_len);
        self.accepted_len -= accepted;
        
        // Always return Ready since transmission is handled asynchronously by the wheel
        Poll::Ready(Ok(accepted))
    }
}
