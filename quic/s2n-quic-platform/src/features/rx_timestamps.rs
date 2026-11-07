// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::c_int;
use core::time::Duration;

#[cfg(all(s2n_quic_platform_cmsg, target_os = "linux"))]
mod rx_timestamps_enabled {
    use super::*;
    use crate::message::cmsg::decode::value_from_bytes;
    use libc::{SCM_TIMESTAMPNS, SOL_SOCKET, SO_TIMESTAMPNS};

    pub const SOCKOPT: Option<(c_int, c_int)> = Some((SOL_SOCKET as _, SO_TIMESTAMPNS as _));
    pub const CMSG_SPACE: usize = crate::message::cmsg::size_of_cmsg::<libc::timespec>();

    #[inline]
    pub const fn is_match(level: c_int, ty: c_int) -> bool {
        level == SOL_SOCKET as c_int && ty == SCM_TIMESTAMPNS as c_int
    }

    /// Decodes the time the socket received a datagram, on the `CLOCK_REALTIME` clock
    #[inline]
    pub fn decode(value: &[u8]) -> Option<Duration> {
        // SAFETY: `value_from_bytes` checks the length, and any bytes are a valid `timespec`
        let time = unsafe { value_from_bytes::<libc::timespec>(value)? };
        to_duration(time)
    }

    /// Reads the `CLOCK_REALTIME` clock, which the socket uses for its receive times
    #[inline]
    #[cfg(not(kani))]
    pub fn now() -> Option<Duration> {
        let mut time: libc::timespec = unsafe { core::mem::zeroed() };
        let result = unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut time) };
        if result != 0 {
            return None;
        }
        to_duration(time)
    }

    /// Kani doesn't model the clock, so a proof doesn't read one
    #[inline]
    #[cfg(kani)]
    pub fn now() -> Option<Duration> {
        None
    }

    #[inline]
    fn to_duration(time: libc::timespec) -> Option<Duration> {
        let secs = u64::try_from(time.tv_sec).ok()?;
        let nanos = u32::try_from(time.tv_nsec).ok()?;
        if nanos >= 1_000_000_000 {
            return None;
        }
        Some(Duration::new(secs, nanos))
    }
}

#[cfg(any(not(all(s2n_quic_platform_cmsg, target_os = "linux")), test))]
mod rx_timestamps_disabled {
    #![cfg_attr(test, allow(dead_code))]
    use super::*;

    pub const SOCKOPT: Option<(c_int, c_int)> = None;
    pub const CMSG_SPACE: usize = 0;

    #[inline]
    pub const fn is_match(level: c_int, ty: c_int) -> bool {
        let _ = level;
        let _ = ty;
        false
    }

    #[inline]
    pub fn decode(value: &[u8]) -> Option<Duration> {
        let _ = value;
        None
    }

    #[inline]
    pub fn now() -> Option<Duration> {
        None
    }
}

mod rx_timestamps_impl {
    #[cfg(not(all(s2n_quic_platform_cmsg, target_os = "linux")))]
    pub use super::rx_timestamps_disabled::*;
    #[cfg(all(s2n_quic_platform_cmsg, target_os = "linux"))]
    pub use super::rx_timestamps_enabled::*;
}

pub use rx_timestamps_impl::*;
