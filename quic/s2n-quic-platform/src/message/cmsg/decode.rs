// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::Storage;
use crate::features;
use core::mem::{align_of, size_of};
use libc::cmsghdr;
use s2n_quic_core::{ensure, inet::AncillaryData};

/// Decodes a value of type `T` from the given `cmsghdr`
/// # Safety
///
/// `cmsghdr` must refer to a cmsg containing a payload of type `T`
#[inline]
pub unsafe fn value_from_bytes<T: Copy>(value: &[u8]) -> Option<T> {
    use core::mem;

    ensure!(value.len() == size_of::<T>(), None);

    debug_assert!(mem::align_of::<T>() <= mem::align_of::<cmsghdr>());

    let mut v = mem::zeroed::<T>();

    core::ptr::copy_nonoverlapping(value.as_ptr(), &mut v as *mut T as *mut u8, size_of::<T>());

    Some(v)
}

/// Decodes all recognized control messages in the given `iter` into `AncillaryData`
#[inline]
pub fn collect(iter: Iter) -> AncillaryData {
    collect_with_clock(iter, features::rx_timestamps::now)
}

/// Decodes all recognized control messages in the given `iter` into `AncillaryData`, and reads
/// `now` for the time of the read where a message holds the time the socket received the
/// datagram
#[inline]
fn collect_with_clock<Now>(iter: Iter, now: Now) -> AncillaryData
where
    Now: FnOnce() -> Option<core::time::Duration>,
{
    let mut data = AncillaryData::default();
    let mut received = None;

    for (cmsg, value) in iter {
        unsafe {
            // SAFETY: `Iter` ensures values are aligned
            collect_item(&mut data, &mut received, cmsg, value);
        }
    }

    // The socket's receive time and `now` are both read on `CLOCK_REALTIME`, which can move back
    // between the two reads, so `saturating_sub` reports a wait of zero in that case
    data.receive_wait_nanos = received.and_then(|time| {
        let wait = now()?.saturating_sub(time);
        Some(u64::try_from(wait.as_nanos()).unwrap_or(u64::MAX))
    });

    data
}

#[inline]
unsafe fn collect_item(
    data: &mut AncillaryData,
    received: &mut Option<core::time::Duration>,
    cmsg: &cmsghdr,
    value: &[u8],
) {
    macro_rules! decode_error {
        ($error:expr) => {
            #[cfg(all(test, feature = "tracing", not(any(kani, miri, fuzz))))]
            tracing::debug!(
                error = $error,
                level = cmsg.cmsg_level,
                r#type = cmsg.cmsg_type,
                value = ?value,
            );
        }
    }

    match (cmsg.cmsg_level, cmsg.cmsg_type) {
        (level, ty) if features::tos::is_match(level, ty) => {
            if let Some(ecn) = features::tos::decode(value) {
                data.ecn = ecn;
            } else {
                decode_error!("invalid TOS value");
            }
        }
        (level, ty) if features::pktinfo_v4::is_match(level, ty) => {
            if let Some((local_address, local_interface)) = features::pktinfo_v4::decode(value) {
                // The port should be specified by a different layer that has that information
                let port = 0;
                let local_address = s2n_quic_core::inet::SocketAddressV4::new(local_address, port);
                data.local_address = local_address.into();
                data.local_interface = Some(local_interface);
            } else {
                decode_error!("invalid pktinfo_v4 value");
            }
        }
        (level, ty) if features::pktinfo_v6::is_match(level, ty) => {
            if let Some((local_address, local_interface)) = features::pktinfo_v6::decode(value) {
                // The port should be specified by a different layer that has that information
                let port = 0;
                let local_address = s2n_quic_core::inet::SocketAddressV6::new(local_address, port);
                data.local_address = local_address.into();
                data.local_interface = Some(local_interface);
            } else {
                decode_error!("invalid pktinfo_v6 value");
            }
        }
        (level, ty) if features::gso::is_match(level, ty) => {
            // ignore GSO settings when reading
        }
        (level, ty) if features::gro::is_match(level, ty) => {
            if let Some(segment_size) = value_from_bytes::<features::gro::Cmsg>(value) {
                data.segment_size = segment_size as _;
            } else {
                decode_error!("invalid gro value");
            }
        }
        (level, ty) if features::rx_timestamps::is_match(level, ty) => {
            if let Some(time) = features::rx_timestamps::decode(value) {
                *received = Some(time);
            } else {
                decode_error!("invalid rx timestamp value");
            }
        }
        _ => {
            decode_error!("unexpected cmsghdr");
        }
    }
}

pub struct Iter<'a> {
    cursor: *const u8,
    len: usize,
    contents: core::marker::PhantomData<&'a [u8]>,
}

impl<'a> Iter<'a> {
    /// Creates a new cmsg::Iter used for iterating over control message headers in the given
    /// [`Storage`].
    #[inline]
    pub fn new<const L: usize>(contents: &'a Storage<L>) -> Iter<'a> {
        let cursor = contents.as_ptr();
        let len = contents.len();

        Self {
            cursor,
            len,
            contents: Default::default(),
        }
    }

    /// Creates a new cmsg::Iter used for iterating over control message headers in the given slice
    /// of bytes.
    ///
    /// # Safety
    ///
    /// * `contents` must be aligned to cmsghdr
    #[inline]
    pub unsafe fn from_bytes(contents: &'a [u8]) -> Self {
        let cursor = contents.as_ptr();
        let len = contents.len();

        debug_assert_eq!(
            cursor.align_offset(align_of::<cmsghdr>()),
            0,
            "contents must be aligned to cmsghdr"
        );

        Self {
            cursor,
            len,
            contents: Default::default(),
        }
    }

    /// Creates a new cmsg::Iter used for iterating over control message headers in the given
    /// msghdr.
    ///
    /// # Safety
    ///
    /// * `contents` must be aligned to cmsghdr
    /// * `msghdr` must point to a valid control buffer
    #[inline]
    pub unsafe fn from_msghdr(msghdr: &'a libc::msghdr) -> Self {
        let cursor = msghdr.msg_control as *const u8;
        let len = msghdr.msg_controllen as usize;

        debug_assert_eq!(
            cursor.align_offset(align_of::<cmsghdr>()),
            0,
            "contents must be aligned to cmsghdr"
        );

        Self {
            cursor,
            len,
            contents: Default::default(),
        }
    }

    #[inline]
    pub fn collect(self) -> AncillaryData {
        collect(self)
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a cmsghdr, &'a [u8]);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            let cursor = self.cursor;

            // make sure we can decode a cmsghdr
            self.len.checked_sub(size_of::<cmsghdr>())?;
            let cmsg = &*(cursor as *const cmsghdr);
            let data_ptr = cursor.add(size_of::<cmsghdr>());

            let cmsg_len = cmsg.cmsg_len as usize;

            // make sure we have capacity to decode the provided cmsg_len
            self.len.checked_sub(cmsg_len)?;

            // the cmsg_len includes the header itself so it needs to be subtracted off
            let data_len = cmsg_len.checked_sub(size_of::<cmsghdr>())?;
            // construct a slice with the provided data len
            let data = core::slice::from_raw_parts(data_ptr, data_len);

            // empty messages are invalid
            if data.is_empty() {
                return None;
            }

            // calculate the next message and update the cursor/len
            {
                let space = libc::CMSG_SPACE(data_len as _) as usize;
                debug_assert!(
                    space >= data_len,
                    "space ({space}) should be at least of size len ({data_len})"
                );
                self.len = self.len.saturating_sub(space);
                self.cursor = cursor.add(space);
            }

            Some((cmsg, data))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::cmsg::Encoder as _;
    use core::{cell::Cell, time::Duration};

    /// Decodes `iter` on a clock that answers `now`, and counts the clock's reads in `reads`
    fn collect_at(iter: Iter, now: Duration, reads: &Cell<usize>) -> AncillaryData {
        collect_with_clock(iter, || {
            reads.set(reads.get() + 1);
            Some(now)
        })
    }

    /// Encodes an `SCM_TIMESTAMPNS` message that holds `received`, and decodes it on a clock
    /// that answers `now`
    #[cfg(target_os = "linux")]
    fn collect_with_receive_time(received: Duration, now: Duration) -> (AncillaryData, usize) {
        let mut storage = Storage::<128>::default();
        let mut encoder = storage.encoder();
        let mut time: libc::timespec = unsafe { core::mem::zeroed() };
        time.tv_sec = received.as_secs() as _;
        time.tv_nsec = received.subsec_nanos() as _;
        let (level, ty) = (libc::SOL_SOCKET, libc::SCM_TIMESTAMPNS);
        assert!(encoder.encode_cmsg(level, ty, time).is_ok());

        let reads = Cell::new(0);
        let data = collect_at(encoder.iter(), now, &reads);
        (data, reads.get())
    }

    /// A message without a receive time doesn't hold a wait, and the decode doesn't read the clock
    #[test]
    fn without_receive_time() {
        let mut storage = Storage::<128>::default();
        let mut encoder = storage.encoder();
        assert!(encoder.encode_cmsg(0, 0, 1u32).is_ok());

        let reads = Cell::new(0);
        let data = collect_at(encoder.iter(), Duration::from_secs(10), &reads);

        assert_eq!(data.receive_wait_nanos, None);
        assert_eq!(reads.get(), 0);
    }

    /// A message with a receive time holds the time from the socket's receive to the read
    #[test]
    #[cfg(target_os = "linux")]
    fn with_receive_time() {
        let received = Duration::new(10, 500);
        let now = Duration::new(10, 2_000_500);
        let (data, reads) = collect_with_receive_time(received, now);

        assert_eq!(data.receive_wait_nanos, Some(2_000_000));
        assert_eq!(reads, 1);
    }

    /// A receive time after the read, as where the clock moved back, holds a wait of zero
    #[test]
    #[cfg(target_os = "linux")]
    fn with_receive_time_after_the_read() {
        let received = Duration::from_secs(10);
        let now = Duration::from_secs(9);
        let (data, reads) = collect_with_receive_time(received, now);

        assert_eq!(data.receive_wait_nanos, Some(0));
        assert_eq!(reads, 1);
    }

    /// `MAX_LEN` holds every control message one receive can return with receive timestamps on:
    /// the TOS, the segment offload, the packet info of both address families, and the receive
    /// time
    #[test]
    #[cfg(all(s2n_quic_platform_cmsg, target_os = "linux"))]
    fn max_len_holds_every_receive_message() {
        use crate::message::cmsg::MAX_LEN;
        use s2n_quic_core::inet::{
            ExplicitCongestionNotification, IpV4Address, IpV6Address, SocketAddress,
            SocketAddressV4, SocketAddressV6,
        };

        let mut storage = Storage::<MAX_LEN>::default();
        let mut encoder = storage.encoder();
        let mut messages = 0;

        if features::tos_v4::IS_SUPPORTED {
            let remote = SocketAddressV4::new(IpV4Address::new([192, 0, 2, 1]), 443);
            encoder
                .encode_ecn(ExplicitCongestionNotification::Ect0, &SocketAddress::IpV4(remote))
                .expect("the TOS message fits in MAX_LEN");
            messages += 1;
        }

        // `MAX_LEN` keeps the larger of the GSO and GRO spaces, so the GSO message fills that
        // space where the platform doesn't support GRO
        if let (Some(level), Some(ty)) = (features::gro::LEVEL, features::gro::TYPE) {
            encoder
                .encode_cmsg(level, ty, 1200 as features::gro::Cmsg)
                .expect("the GRO message fits in MAX_LEN");
            messages += 1;
        } else if let (Some(level), Some(ty)) = (features::gso::LEVEL, features::gso::TYPE) {
            encoder
                .encode_cmsg(level, ty, 1200 as features::gso::Cmsg)
                .expect("the GSO message fits in MAX_LEN");
            messages += 1;
        }

        // A dual-stack socket returns the packet info of both address families
        if features::pktinfo_v4::IS_SUPPORTED {
            let local = SocketAddressV4::new(IpV4Address::new([192, 0, 2, 2]), 4433);
            encoder
                .encode_local_address(&SocketAddress::IpV4(local))
                .expect("the IPv4 packet info fits in MAX_LEN");
            messages += 1;
        }
        if features::pktinfo_v6::IS_SUPPORTED {
            let ip = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
            let local = SocketAddressV6::new(IpV6Address::new(ip), 4433);
            encoder
                .encode_local_address(&SocketAddress::IpV6(local))
                .expect("the IPv6 packet info fits in MAX_LEN");
            messages += 1;
        }

        let received = Duration::new(10, 500);
        let mut time: libc::timespec = unsafe { core::mem::zeroed() };
        time.tv_sec = received.as_secs() as _;
        time.tv_nsec = received.subsec_nanos() as _;
        encoder
            .encode_cmsg(libc::SOL_SOCKET, libc::SCM_TIMESTAMPNS, time)
            .expect("the receive time fits in MAX_LEN");
        messages += 1;

        assert_eq!(encoder.iter().count(), messages);

        let reads = Cell::new(0);
        let data = collect_at(encoder.iter(), Duration::new(10, 2_000_500), &reads);

        assert_eq!(data.receive_wait_nanos, Some(2_000_000));
        assert_eq!(reads.get(), 1);
        if features::tos_v4::IS_SUPPORTED {
            assert_eq!(data.ecn, ExplicitCongestionNotification::Ect0);
        }
        if features::gro::IS_SUPPORTED {
            assert_eq!(data.segment_size, 1200);
        }
    }
}
