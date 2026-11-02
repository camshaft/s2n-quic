// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use core::{fmt, mem::size_of};
#[cfg(unix)]
use libc::{msghdr, sockaddr_in, sockaddr_in6, AF_INET, AF_INET6};
use s2n_quic_core::{
    assume,
    inet::{self, SocketAddress},
};
#[cfg(windows)]
use sys::{sockaddr_in, sockaddr_in6, AF_INET, AF_INET6};

/// The `SOCKADDR_IN` and `SOCKADDR_IN6` layouts of Windows' `ws2def.h` and `ws2ipdef.h`, with the
/// field names that `libc` uses on Unix.
#[cfg(windows)]
#[allow(non_camel_case_types, dead_code)]
mod sys {
    pub const AF_INET: u16 = 2;
    pub const AF_INET6: u16 = 23;

    #[repr(C)]
    pub struct in_addr {
        pub s_addr: u32,
    }

    #[repr(C)]
    pub struct sockaddr_in {
        pub sin_family: u16,
        pub sin_port: u16,
        pub sin_addr: in_addr,
        pub sin_zero: [u8; 8],
    }

    #[repr(C)]
    pub struct in6_addr {
        pub s6_addr: [u8; 16],
    }

    #[repr(C)]
    pub struct sockaddr_in6 {
        pub sin6_family: u16,
        pub sin6_port: u16,
        pub sin6_flowinfo: u32,
        pub sin6_addr: in6_addr,
        pub sin6_scope_id: u32,
    }
}

const SIZE: usize = {
    let v4 = size_of::<sockaddr_in>();
    let v6 = size_of::<sockaddr_in6>();
    if v4 > v6 {
        v4
    } else {
        v6
    }
};

#[repr(align(8))]
pub struct Addr {
    msg_name: [u8; SIZE],
    msg_namelen: u8,
}

impl fmt::Debug for Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

impl PartialEq for Addr {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_slice().eq(other.as_slice())
    }
}

impl Eq for Addr {}

impl Default for Addr {
    #[inline]
    fn default() -> Self {
        Self::new(SocketAddress::default())
    }
}

impl Addr {
    #[inline]
    pub fn new(value: SocketAddress) -> Self {
        let mut v = Self {
            msg_name: Default::default(),
            msg_namelen: Default::default(),
        };
        v.set(value);
        v
    }

    #[inline]
    fn as_slice(&self) -> &[u8] {
        &self.msg_name[..self.msg_namelen as usize]
    }

    #[inline]
    pub fn get(&self) -> SocketAddress {
        unsafe { read(self.msg_name.as_ptr(), self.msg_namelen as _) }
    }

    #[inline]
    pub fn set(&mut self, remote_address: SocketAddress) {
        match remote_address {
            SocketAddress::IpV4(addr) => {
                let sockaddr: &mut sockaddr_in =
                    unsafe { &mut *(self.msg_name.as_mut_ptr() as *mut _) };
                sockaddr.sin_family = AF_INET as _;
                sockaddr.sin_port = addr.port().to_be();
                sockaddr.sin_addr.s_addr = u32::from_ne_bytes((*addr.ip()).into());
                self.msg_namelen = size_of::<sockaddr_in>() as _;
            }
            SocketAddress::IpV6(addr) => {
                let sockaddr: &mut sockaddr_in6 =
                    unsafe { &mut *(self.msg_name.as_mut_ptr() as *mut _) };
                sockaddr.sin6_family = AF_INET6 as _;
                sockaddr.sin6_port = addr.port().to_be();
                sockaddr.sin6_addr.s6_addr = (*addr.ip()).into();
                self.msg_namelen = size_of::<sockaddr_in6>() as _;
            }
        }
    }

    #[inline]
    pub fn set_port(&mut self, port: u16) {
        match self.msg_namelen as usize {
            size if size == size_of::<sockaddr_in>() => {
                let sockaddr: &mut sockaddr_in =
                    unsafe { &mut *(self.msg_name.as_mut_ptr() as *mut _) };
                sockaddr.sin_port = port.to_be();
            }
            size if size == size_of::<sockaddr_in6>() => {
                let sockaddr: &mut sockaddr_in6 =
                    unsafe { &mut *(self.msg_name.as_mut_ptr() as *mut _) };
                sockaddr.sin6_port = port.to_be();
            }
            _ => unsafe {
                assume!(false, "invalid remote address");
            },
        }
    }

    /// The address as a `sockaddr_in` or `sockaddr_in6`, for a send's message name.
    #[inline]
    pub(crate) fn name_ptr(&self) -> *const u8 {
        self.msg_name.as_ptr()
    }

    /// The length of the `sockaddr` [`Self::name_ptr`] points to.
    #[inline]
    pub(crate) fn name_len(&self) -> usize {
        self.msg_namelen as usize
    }

    /// The buffer a receive writes its source address into.
    #[inline]
    pub(crate) fn name_mut_ptr(&mut self) -> *mut u8 {
        self.msg_name.as_mut_ptr()
    }

    /// The capacity of the buffer [`Self::name_mut_ptr`] points to: the largest `sockaddr` the
    /// address holds.
    #[inline]
    pub(crate) fn name_capacity(&self) -> usize {
        self.msg_name.len()
    }

    /// Takes the length of the `sockaddr` a receive wrote into [`Self::name_mut_ptr`].
    #[inline]
    pub(crate) fn update_with_len(&mut self, len: usize) {
        match len {
            0 => {
                // set the address to unspecified
                self.set(SocketAddress::default());
            }
            size if size == size_of::<sockaddr_in>() => {
                self.msg_namelen = size as _;
            }
            size if size == size_of::<sockaddr_in6>() => {
                self.msg_namelen = size as _;
            }
            size => unreachable!("invalid address length {size}"),
        }
    }

    #[cfg(unix)]
    #[inline]
    pub fn send_with_msg(&self, msg: &mut msghdr) {
        msg.msg_name = self.name_ptr() as *mut u8 as *mut _;
        msg.msg_namelen = self.name_len() as _;
    }

    #[cfg(unix)]
    #[inline]
    pub fn recv_with_msg(&mut self, msg: &mut msghdr) {
        msg.msg_name = self.name_mut_ptr() as *mut _;
        // use the max size, in case the length changes
        msg.msg_namelen = self.name_capacity() as _;
    }

    #[cfg(unix)]
    #[inline]
    pub fn update_with_msg(&mut self, msg: &msghdr) {
        debug_assert_eq!(self.msg_name.as_ptr(), msg.msg_name as *const u8);
        self.update_with_len(msg.msg_namelen as usize);
    }
}

/// # Safety
///
/// * `ptr` must be a valid pointer
/// * `len` bytes must be readable from `ptr`
#[inline]
pub unsafe fn read(ptr: *const u8, len: usize) -> SocketAddress {
    match len {
        size if size == size_of::<sockaddr_in>() => {
            let sockaddr: &sockaddr_in = unsafe { &*(ptr as *const _) };
            let port = sockaddr.sin_port.to_be();
            let addr: inet::IpV4Address = sockaddr.sin_addr.s_addr.to_ne_bytes().into();
            inet::SocketAddressV4::new(addr, port).into()
        }
        size if size == size_of::<sockaddr_in6>() => {
            let sockaddr: &sockaddr_in6 = unsafe { &*(ptr as *const _) };
            let port = sockaddr.sin6_port.to_be();
            let addr: inet::IpV6Address = sockaddr.sin6_addr.s6_addr.into();
            inet::SocketAddressV6::new(addr, port).into()
        }
        size => unreachable!("invalid address length {size}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bolero::check;

    #[test]
    fn set_port_test() {
        check!().with_type().cloned().for_each(|(addr, port)| {
            let mut addr = Addr::new(addr);
            addr.set_port(port);
            assert_eq!(addr.get().port(), port);
        });
    }

    /// A receive's name length round-trips through `update_with_len`: the length of an IPv4 or an
    /// IPv6 `sockaddr` keeps the address, and 0 makes it unspecified.
    #[test]
    fn update_with_len_test() {
        check!().with_type().cloned().for_each(|value| {
            let mut addr = Addr::new(value);
            let len = addr.name_len();
            addr.update_with_len(len);
            assert_eq!(addr.get(), value);
            assert!(len <= addr.name_capacity());

            addr.update_with_len(0);
            assert_eq!(addr.get(), SocketAddress::default());
        });
    }
}
