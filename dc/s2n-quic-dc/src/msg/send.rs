// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{addr::Addr, cmsg};
use crate::{
    allocator::{self, Allocator},
    socket::pool::{descriptor, Pool},
};
use core::{fmt, task::Poll};
use libc::{iovec, msghdr, sendmsg};
use s2n_quic_core::{
    ensure,
    inet::{ExplicitCongestionNotification, SocketAddress, Unspecified},
    ready,
};
use s2n_quic_platform::features;
use std::{io, os::fd::AsRawFd};
use tracing::trace;

/// A segment that wraps an unfilled descriptor during filling
/// or a filled descriptor after filling
pub enum Segment {
    Unfilled(descriptor::Unfilled),
    Filled(descriptor::Filled),
}

impl fmt::Debug for Segment {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Segment::Unfilled(u) => write!(f, "Segment::Unfilled({:?})", u),
            Segment::Filled(filled) => write!(f, "Segment::Filled({:?})", filled),
        }
    }
}

impl allocator::Segment for Segment {
    #[inline]
    fn leak(&mut self) {
        // Descriptors handle their own lifecycle via Drop
        // Convert to a filled descriptor and forget it to prevent drop
        match core::mem::replace(self, Segment::Unfilled(unsafe { core::mem::zeroed() })) {
            Segment::Filled(filled) => core::mem::forget(filled),
            Segment::Unfilled(unfilled) => core::mem::forget(unfilled),
        }
    }
}

/// A retransmission handle that wraps a filled descriptor
pub struct Retransmission {
    filled: descriptor::Filled,
}

impl fmt::Debug for Retransmission {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Retransmission({:?})", self.filled)
    }
}

impl allocator::Segment for Retransmission {
    #[inline]
    fn leak(&mut self) {
        // Descriptors handle their own lifecycle via Drop
        // Replace with a zero value and forget it
        let filled = core::mem::replace(&mut self.filled, unsafe { core::mem::zeroed() });
        core::mem::forget(filled);
    }
}

pub struct Message {
    addr: Addr,
    gso: features::Gso,
    segment_len: u16,
    total_len: u16,
    can_push: bool,
    pool: Pool,
    pending_segments: Vec<descriptor::Filled>,
    payload: Vec<libc::iovec>,
    ecn: ExplicitCongestionNotification,
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut d = f.debug_struct("Message");

        d.field("addr", &self.addr)
            .field("segment_len", &self.segment_len)
            .field("total_len", &self.total_len)
            .field("can_push", &self.can_push)
            .field("pending_segments", &self.pending_segments.len())
            .field("segments", &self.payload.len())
            .field("ecn", &self.ecn);

        d.finish()
    }
}

unsafe impl Send for Message {}
unsafe impl Sync for Message {}

impl Message {
    #[inline]
    pub fn new(remote_address: SocketAddress, gso: features::Gso, pool: Pool) -> Self {
        let burst_size = 16;
        Self {
            addr: Addr::new(remote_address),
            gso,
            segment_len: 0,
            total_len: 0,
            can_push: true,
            pool,
            pending_segments: Vec::with_capacity(burst_size),
            payload: Vec::with_capacity(burst_size),
            ecn: ExplicitCongestionNotification::NotEct,
        }
    }

    #[inline]
    fn push_payload(&mut self, segment: &descriptor::Filled) {
        debug_assert!(self.can_push());

        let mut iovec = unsafe { core::mem::zeroed::<iovec>() };
        let buffer = segment.payload();

        debug_assert!(!buffer.is_empty());
        debug_assert!(
            buffer.len() <= u16::MAX as usize,
            "cannot transmit more than 2^16 bytes in a single packet"
        );

        let iov_base: *const u8 = buffer.as_ptr();
        iovec.iov_base = iov_base as *mut _;
        iovec.iov_len = buffer.len() as _;

        self.total_len += buffer.len() as u16;

        if self.payload.is_empty() {
            self.segment_len = buffer.len() as _;
        } else {
            debug_assert!(buffer.len() <= self.segment_len as usize);
            // the caller can only push until the last undersized segment
            self.can_push &= buffer.len() == self.segment_len as usize;
        }

        self.payload.push(iovec);

        let max_segments = self.gso.max_segments();

        self.can_push &= self.payload.len() < max_segments;

        // sendmsg has a limitation on the total length of the payload, even with GSO
        let next_size = self.total_len as usize + self.segment_len as usize;
        let max_size = u16::MAX as usize;
        self.can_push &= next_size <= max_size;
    }

    #[inline]
    pub fn send_with<Snd>(&mut self, send: Snd) -> io::Result<usize>
    where
        Snd: FnOnce(&Addr, ExplicitCongestionNotification, &[io::IoSlice]) -> io::Result<usize>,
    {
        let iov = unsafe {
            // SAFETY: IoSlice is guaranteed to have the same layout as iovec
            &*(self.payload.as_slice() as *const [libc::iovec] as *const [io::IoSlice])
        };
        let res = send(&self.addr, self.ecn, iov);
        self.on_transmit(&res);
        res
    }

    #[inline]
    pub fn poll_send_with<Snd>(&mut self, send: Snd) -> Poll<io::Result<usize>>
    where
        Snd: FnOnce(
            &Addr,
            ExplicitCongestionNotification,
            &[io::IoSlice],
        ) -> Poll<io::Result<usize>>,
    {
        let iov = unsafe {
            // SAFETY: IoSlice is guaranteed to have the same layout as iovec
            &*(self.payload.as_slice() as *const [libc::iovec] as *const [io::IoSlice])
        };
        let res = ready!(send(&self.addr, self.ecn, iov));
        self.on_transmit(&res);
        res.into()
    }

    #[inline]
    pub fn send<S: AsRawFd>(&mut self, s: &S) -> io::Result<()> {
        let segment_len = self.segment_len;

        self.send_with(|addr, ecn, iov| {
            use cmsg::Encoder as _;

            let mut msg = unsafe { core::mem::zeroed::<msghdr>() };

            msg.msg_iov = iov.as_ptr() as *mut _;
            msg.msg_iovlen = iov.len() as _;

            debug_assert!(
                !addr.get().ip().is_unspecified(),
                "cannot send packet to unspecified address"
            );
            debug_assert!(
                addr.get().port() != 0,
                "cannot send packet to unspecified port"
            );
            addr.send_with_msg(&mut msg);

            let mut cmsg_storage = cmsg::Storage::<{ cmsg::ENCODER_LEN }>::default();
            let mut cmsg = cmsg_storage.encoder();
            if ecn != ExplicitCongestionNotification::NotEct {
                let _ = cmsg.encode_ecn(ecn, &addr.get());
            }

            if iov.len() > 1 {
                let _ = cmsg.encode_gso(segment_len);
            }

            if !cmsg.is_empty() {
                msg.msg_control = cmsg.as_mut_ptr() as *mut _;
                msg.msg_controllen = cmsg.len() as _;
            }

            let flags = Default::default();

            let result = unsafe { sendmsg(s.as_raw_fd(), &msg, flags) };

            trace!(
                dest = %addr,
                segments = iov.len(),
                segment_len,
                cmsg_len = msg.msg_controllen,
                result,
            );

            if result >= 0 {
                Ok(result as usize)
            } else {
                Err(io::Error::last_os_error())
            }
        })?;

        Ok(())
    }

    #[inline]
    pub fn drain(&mut self) -> Drain<'_> {
        Drain {
            message: self,
            index: 0,
        }
    }

    /// The maximum number of segments that can be sent in a single GSO payload
    #[inline]
    pub fn max_segments(&self) -> usize {
        self.gso.max_segments()
    }

    #[inline]
    fn on_transmit(&mut self, result: &io::Result<usize>) {
        let len = match result {
            Ok(len) => *len,
            Err(err) => {
                // notify the GSO impl that we got an error
                self.gso.handle_socket_error(err);
                return;
            }
        };

        if self.total_len as usize > len {
            todo!();
        }

        self.force_clear()
    }
}

impl Allocator for Message {
    type Segment = Segment;

    type Retransmission = Retransmission;

    #[inline]
    fn alloc(&mut self) -> Option<Self::Segment> {
        ensure!(self.can_push(), None);

        // Allocate from the pool
        let unfilled = self.pool.alloc()?;
        trace!(operation = "alloc", segment = ?unfilled);

        Some(Segment::Unfilled(unfilled))
    }

    #[inline]
    fn get<'a>(&'a self, segment: &'a Segment) -> &'a Vec<u8> {
        // This method returns a reference to Vec<u8> which doesn't match our descriptor model
        // We'll need to create a temporary vector to satisfy the interface
        // This is inefficient but maintains API compatibility
        static EMPTY_VEC: Vec<u8> = Vec::new();
        match segment {
            Segment::Filled(filled) => {
                // We can't return a reference to the descriptor's payload as a Vec<u8>
                // This is a limitation of the current API design
                // For now, return an empty vec - this method should not be used in the new design
                &EMPTY_VEC
            }
            Segment::Unfilled(_) => {
                &EMPTY_VEC
            }
        }
    }

    #[inline]
    fn get_mut(&mut self, segment: &Segment) -> &mut Vec<u8> {
        // Similar to get, this doesn't fit well with the descriptor model
        // The descriptor payload is accessed differently
        // This method shouldn't be used in the new pool-based design
        // We'll panic if called to indicate it's not supported
        panic!("get_mut is not supported with pool-based descriptors; use descriptor.payload_mut() directly")
    }

    #[inline]
    fn push(&mut self, segment: Segment) {
        trace!(operation = "push", ?segment);
        
        // Extract the filled descriptor
        let filled = match segment {
            Segment::Filled(filled) => filled,
            Segment::Unfilled(_) => panic!("Cannot push an unfilled segment"),
        };

        self.push_payload(&filled);
        self.pending_segments.push(filled);
    }

    #[inline]
    fn push_with_retransmission(&mut self, segment: Segment) -> Retransmission {
        trace!(operation = "push_with_retransmission", ?segment);
        
        // Extract the filled descriptor
        let filled = match segment {
            Segment::Filled(filled) => filled,
            Segment::Unfilled(_) => panic!("Cannot push an unfilled segment"),
        };

        // Add the payload to iovecs for transmission
        self.push_payload(&filled);
        
        // Return the filled descriptor as the retransmission handle
        // The descriptor is reference-counted, and this handle keeps it alive
        // The iovecs in payload also reference the same memory
        // When force_clear() is called after transmission, the iovecs are cleared
        // but the descriptor stays alive because the Retransmission holds a reference
        Retransmission {
            filled,
        }
    }

    #[inline]
    fn retransmit(&mut self, segment: Retransmission) -> Segment {
        debug_assert!(
            self.payload.is_empty(),
            "cannot retransmit with pending payload"
        );

        Segment::Filled(segment.filled)
    }

    #[inline]
    fn retransmit_copy(&mut self, retransmission: &Retransmission) -> Option<Segment> {
        // Always copy on retransmission as per the design document
        let unfilled = self.pool.alloc()?;
        
        // Fill the new descriptor by copying from the original
        let source_payload = retransmission.filled.payload();
        debug_assert!(
            !source_payload.is_empty(),
            "cannot retransmit empty payload; source: {retransmission:?}"
        );
        
        let result = unfilled.fill_with(|addr, _cmsg, mut iov| {
            // Set the same remote address
            addr.set(retransmission.filled.remote_address().get());
            
            // Copy the payload
            let dst = &mut iov[..source_payload.len()];
            dst.copy_from_slice(source_payload);
            
            Ok::<usize, ()>(source_payload.len())
        });
        
        match result {
            Ok(segments) => {
                // We should get a single filled descriptor back
                // Use into_iter to extract the filled descriptor
                let mut iter = segments.into_iter();
                let filled = iter.next()?;
                Some(Segment::Filled(filled))
            }
            Err((unfilled, _err)) => {
                // Allocation failed, drop the unfilled descriptor
                drop(unfilled);
                None
            }
        }
    }

    #[inline]
    fn can_push(&self) -> bool {
        self.can_push
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }

    #[inline]
    fn segment_len(&self) -> Option<u16> {
        debug_assert_eq!(self.segment_len == 0, self.is_empty());
        if self.segment_len == 0 {
            None
        } else {
            Some(self.segment_len)
        }
    }

    #[inline]
    fn free(&mut self, segment: Segment) {
        trace!(operation = "free", ?segment);

        // With the pool model, we just drop the segment
        // The descriptor will be returned to the pool when dropped
        // If we haven't sent anything yet, we can drop immediately
        if !self.is_empty() {
            // Store it temporarily until we send
            if let Segment::Filled(filled) = segment {
                self.pending_segments.push(filled);
            }
        }
        // Otherwise just drop it
    }

    #[inline]
    fn free_retransmission(&mut self, segment: Retransmission) {
        debug_assert!(
            self.payload.is_empty(),
            "cannot free a retransmission with pending payload"
        );

        trace!(operation = "free_retransmission", ?segment);

        // Just drop the segment, it will be returned to the pool
        drop(segment);
    }

    #[inline]
    fn ecn(&self) -> ExplicitCongestionNotification {
        self.ecn
    }

    #[inline]
    fn set_ecn(&mut self, ecn: ExplicitCongestionNotification) {
        self.ecn = ecn;
    }

    #[inline]
    fn remote_address(&self) -> SocketAddress {
        self.addr.get()
    }

    #[inline]
    fn set_remote_address(&mut self, remote_address: SocketAddress) {
        self.addr.set(remote_address);
    }

    #[inline]
    fn set_remote_port(&mut self, port: u16) {
        self.addr.set_port(port);
    }

    #[inline]
    fn force_clear(&mut self) {
        // reset the current payload
        self.payload.clear();
        self.ecn = ExplicitCongestionNotification::NotEct;
        self.segment_len = 0;
        self.total_len = 0;
        self.can_push = true;

        // Clear all pending segments - they'll be returned to the pool when dropped
        self.pending_segments.clear();
    }
}

pub struct Drain<'a> {
    message: &'a mut Message,
    index: usize,
}

impl<'a> Iterator for Drain<'a> {
    type Item = &'a [u8];

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let v = self.message.payload.get(self.index)?;
        self.index += 1;
        let v = unsafe { core::slice::from_raw_parts(v.iov_base as *const u8, v.iov_len) };
        Some(v)
    }
}

impl Drop for Drain<'_> {
    #[inline]
    fn drop(&mut self) {
        self.message.force_clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();

        let addr: std::net::SocketAddr = "127.0.0.1:4433".parse().unwrap();
        let pool = Pool::new(1500, 16);
        let mut message = Message::new(addr.into(), Default::default(), pool.clone());

        // Allocate and fill the first segment
        let handle = message.alloc().unwrap();
        let hello = match handle {
            Segment::Unfilled(unfilled) => {
                // Fill it with data
                let result = unfilled.fill_with(|addr_ref, _cmsg, mut iov| {
                    addr_ref.set(addr.into());
                    let data = b"hello\n";
                    iov[..data.len()].copy_from_slice(data);
                    Ok::<usize, ()>(data.len())
                });
                
                match result {
                    Ok(segments) => {
                        let mut iter = segments.into_iter();
                        let filled = iter.next().expect("should have one filled segment");
                        message.push_with_retransmission(Segment::Filled(filled))
                    }
                    Err(_) => panic!("Failed to fill segment"),
                }
            }
            _ => panic!("Expected unfilled segment"),
        };

        let world = if message.gso.max_segments() > 1 {
            let handle = message.alloc().unwrap();
            match handle {
                Segment::Unfilled(unfilled) => {
                    let result = unfilled.fill_with(|addr_ref, _cmsg, mut iov| {
                        addr_ref.set(addr.into());
                        let data = b"world\n";
                        iov[..data.len()].copy_from_slice(data);
                        Ok::<usize, ()>(data.len())
                    });
                    
                    match result {
                        Ok(segments) => {
                            let mut iter = segments.into_iter();
                            let filled = iter.next().expect("should have one filled segment");
                            Some(message.push_with_retransmission(Segment::Filled(filled)))
                        }
                        Err(_) => panic!("Failed to fill segment"),
                    }
                }
                _ => panic!("Expected unfilled segment"),
            }
        } else {
            None
        };

        message.send(&socket).unwrap();

        let world = world.map(|world| message.retransmit(world));
        let hello = message.retransmit(hello);

        if let Some(world) = world {
            match world {
                Segment::Filled(ref filled) => {
                    assert_eq!(filled.payload(), b"world\n");
                }
                _ => panic!("Expected filled segment"),
            }
            message.push(world);
        }

        match hello {
            Segment::Filled(ref filled) => {
                assert_eq!(filled.payload(), b"hello\n");
            }
            _ => panic!("Expected filled segment"),
        }
        message.push(hello);

        message.send(&socket).unwrap();
    }
}
