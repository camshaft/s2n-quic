// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Local network interfaces: the interface each socket of an endpoint is on, and the most the
//! endpoint sends on each interface.
//!
//! An endpoint identifies its interfaces by [`Id`]. Interface 0, [`Id::DEFAULT`], is always
//! present: its send rate is the endpoint's overall send rate, and it holds each socket that the
//! configuration doesn't put on another interface. The interfaces that the configuration lists
//! are numbered from 1, in the order it lists them, so an endpoint whose configuration lists none
//! has the default interface alone, with every socket on it.
//!
//! The send rate of a listed interface is the link speed that the operating system reports for
//! the interface's name, where it reports one; else the rate that the configuration sets for the
//! interface; else the endpoint's overall send rate. Linux reports a link speed in
//! `/sys/class/net/<name>/speed`, and macOS in the link-layer `if_data` that `getifaddrs`
//! returns; other platforms don't report one. The send rate is the most the endpoint sends on
//! the interface: congestion control finds the rate each path allows below it.

use crate::{socket::rate::Rate, tracing::warn};
use core::num::NonZeroU64;
use std::{
    io,
    net::{IpAddr, SocketAddr},
};

#[cfg(test)]
mod tests;

/// The identity of one of an endpoint's local interfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(u16);

impl Id {
    /// The default interface, which holds each socket that the configuration doesn't put on
    /// another interface.
    pub const DEFAULT: Self = Self(0);

    /// Returns the identity of the interface at `position` in the configuration's list.
    #[inline]
    fn of_config(position: usize) -> Self {
        Self(position as u16 + 1)
    }

    /// Returns the interface's position in [`Table::interfaces`].
    #[inline]
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }
}

impl core::fmt::Display for Id {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

/// A local interface as an endpoint's configuration lists it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    /// The interface's name in the operating system, such as `eth0`, where it is known.
    ///
    /// The endpoint reads the interface's link speed by this name. It also puts on this interface
    /// each socket that no interface lists and whose local address the operating system puts on
    /// an interface of this name. Where two listed interfaces have the same name, the first one
    /// holds those sockets.
    /// On Linux, `getifaddrs` returns an IPv4 alias address with its label, such as `eth0:1`, as
    /// its interface's name, so a socket on such an address is placed on an interface of that
    /// name and not on `eth0`.
    pub name: Option<String>,
    /// The interface's send rate where the operating system doesn't report its link speed. Where
    /// this is `None` too, the interface's send rate is the endpoint's overall send rate.
    pub send_rate: Option<Rate>,
    /// The send sockets on this interface, each by its position in the list of send sockets that
    /// [`setup_endpoint`](crate::endpoint::setup_endpoint) takes.
    pub send_sockets: Vec<usize>,
    /// The recv sockets on this interface, each by its position in the list of recv sockets that
    /// [`setup_endpoint`](crate::endpoint::setup_endpoint) takes.
    pub recv_sockets: Vec<usize>,
}

/// The speed of an interface's link, as the operating system reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LinkSpeed {
    bits_per_second: NonZeroU64,
}

impl LinkSpeed {
    /// Returns a link speed of `bits_per_second`, the unit macOS reports, or `None` for 0.
    #[inline]
    pub fn from_bits_per_second(bits_per_second: u64) -> Option<Self> {
        NonZeroU64::new(bits_per_second)
            .map(|bits_per_second| Self { bits_per_second })
    }

    /// Returns a link speed of `megabits_per_second`, the unit Linux reports, or `None` for 0.
    #[inline]
    pub fn from_megabits_per_second(megabits_per_second: u64) -> Option<Self> {
        Self::from_bits_per_second(megabits_per_second.checked_mul(1_000_000)?)
    }

    /// Returns the link speed in bits per second.
    #[inline]
    pub fn bits_per_second(self) -> u64 {
        self.bits_per_second.get()
    }

    /// Returns the send rate of a link of this speed.
    #[inline]
    pub fn rate(self) -> Rate {
        Rate::new(self.bits_per_second() as f64 / 1e9)
    }
}

/// A local interface as an endpoint resolved it.
#[derive(Clone, Debug, PartialEq)]
pub struct Interface {
    /// The interface's identity, its position in [`Table::interfaces`].
    pub id: Id,
    /// The interface's name in the operating system, where it is known.
    pub name: Option<String>,
    /// The link speed that the operating system reported for the interface, where it reported
    /// one.
    pub link_speed: Option<LinkSpeed>,
    /// The most the endpoint sends on the interface.
    pub send_rate: Rate,
}

/// An endpoint's local interfaces, and the interface of each of its sockets.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    interfaces: Vec<Interface>,
    send_sockets: Vec<Id>,
    recv_sockets: Vec<Id>,
}

impl Table {
    /// Resolves the interfaces that `configs` lists, each with its send rate, and the interface
    /// of each socket, from the sockets' local addresses and what `os` reports.
    ///
    /// Where `configs` is empty, every socket is on the default interface, and `os` isn't read.
    /// `os` reports addresses only where they place a socket: one that no interface lists, bound
    /// to a specified address, while a listed interface has a name.
    ///
    /// # Panics
    ///
    /// Where an interface lists a socket past the end of its list of sockets, where two
    /// interfaces list the same socket, or where `configs` lists more interfaces than an [`Id`]
    /// numbers.
    pub(crate) fn resolve<O: Os>(
        configs: &[Config],
        overall_send_rate: Rate,
        send_addrs: &[SocketAddr],
        recv_addrs: &[SocketAddr],
        os: &O,
    ) -> Self {
        assert!(
            configs.len() < u16::MAX as usize,
            "an endpoint lists at most {} local interfaces",
            u16::MAX - 1
        );

        let mut interfaces = Vec::with_capacity(configs.len() + 1);
        interfaces.push(Interface {
            id: Id::DEFAULT,
            name: None,
            link_speed: None,
            send_rate: overall_send_rate,
        });
        for (position, config) in configs.iter().enumerate() {
            let link_speed = config.name.as_deref().and_then(|n| os.link_speed(n));
            let send_rate = link_speed
                .map(LinkSpeed::rate)
                .or(config.send_rate)
                .unwrap_or(overall_send_rate);
            interfaces.push(Interface {
                id: Id::of_config(position),
                name: config.name.clone(),
                link_speed,
                send_rate,
            });
        }

        let send_listed = listed(
            configs.iter().map(|c| c.send_sockets.as_slice()),
            send_addrs.len(),
            "send",
        );
        let recv_listed = listed(
            configs.iter().map(|c| c.recv_sockets.as_slice()),
            recv_addrs.len(),
            "recv",
        );

        let named = configs.iter().any(|config| config.name.is_some());
        let unplaced = send_addrs
            .iter()
            .zip(&send_listed)
            .chain(recv_addrs.iter().zip(&recv_listed))
            .any(|(addr, listed)| listed.is_none() && is_specified(addr));
        let addresses = if named && unplaced {
            os.addresses().unwrap_or_else(|error| {
                warn!(
                    %error,
                    "could not read local addresses; unlisted sockets stay on the default interface"
                );
                Vec::new()
            })
        } else {
            Vec::new()
        };

        let send_sockets: Vec<Id> = send_addrs
            .iter()
            .zip(send_listed)
            .map(|(addr, listed)| place(listed, &interfaces, &addresses, addr))
            .collect();
        let recv_sockets: Vec<Id> = recv_addrs
            .iter()
            .zip(recv_listed)
            .map(|(addr, listed)| place(listed, &interfaces, &addresses, addr))
            .collect();

        Self {
            interfaces,
            send_sockets,
            recv_sockets,
        }
    }

    /// Returns the interfaces, each at the position of its [`Id`]: the default interface
    /// first, then the interfaces of the configuration, in the order it lists them.
    #[inline]
    pub fn interfaces(&self) -> &[Interface] {
        &self.interfaces
    }

    /// Returns the interface of `id`.
    #[inline]
    pub fn get(&self, id: Id) -> Option<&Interface> {
        self.interfaces.get(id.as_usize())
    }

    /// Returns the interface of each send socket, in the order of the send sockets that
    /// [`setup_endpoint`](crate::endpoint::setup_endpoint) took.
    #[inline]
    pub fn send_sockets(&self) -> &[Id] {
        &self.send_sockets
    }

    /// Returns the interface of each recv socket, in the order of the recv sockets that
    /// [`setup_endpoint`](crate::endpoint::setup_endpoint) took, which is the order of the
    /// endpoint's `data_addrs`.
    #[inline]
    pub fn recv_sockets(&self) -> &[Id] {
        &self.recv_sockets
    }
}

/// Returns, for each of `socket_count` sockets, the interface that lists it, where one does.
/// `lists` holds the list of sockets of each interface of the configuration, in its order.
///
/// # Panics
///
/// Where a list holds a socket past `socket_count`, or two lists hold the same socket.
fn listed<'a, L>(lists: L, socket_count: usize, socket_kind: &str) -> Vec<Option<Id>>
where
    L: Iterator<Item = &'a [usize]>,
{
    let mut listed = vec![None; socket_count];
    for (position, list) in lists.enumerate() {
        let id = Id::of_config(position);
        for &socket in list {
            let Some(slot) = listed.get_mut(socket) else {
                panic!(
                    "interface {id} lists {socket_kind} socket {socket} of {socket_count} sockets"
                );
            };
            if let Some(other) = slot.replace(id) {
                panic!(
                    "interfaces {other} and {id} both list {socket_kind} socket {socket}"
                );
            }
        }
    }
    listed
}

/// Returns whether `addr` is bound to a specified address, which can place it on an interface.
fn is_specified(addr: &SocketAddr) -> bool {
    !addr.ip().to_canonical().is_unspecified()
}

/// Returns the interface of the socket at `addr`: the interface that lists it, where one does;
/// else the first listed interface whose name `addresses` puts `addr` on, though an interface
/// the configuration doesn't list holds the address too; else the default interface.
fn place(
    listed: Option<Id>,
    interfaces: &[Interface],
    addresses: &[(String, IpAddr)],
    addr: &SocketAddr,
) -> Id {
    if let Some(id) = listed {
        return id;
    }
    if !is_specified(addr) {
        return Id::DEFAULT;
    }
    let ip = addr.ip();
    interfaces
        .iter()
        .find(|interface| {
            interface
                .name
                .as_deref()
                .is_some_and(|name| interfaces_of(addresses, ip).any(|holder| holder == name))
        })
        .map_or(Id::DEFAULT, |interface| interface.id)
}

/// Returns the name of each interface that `addresses`, each local address with the name of its
/// interface, puts `ip` on, in the order of `addresses`.
pub(crate) fn interfaces_of(
    addresses: &[(String, IpAddr)],
    ip: IpAddr,
) -> impl Iterator<Item = &str> + '_ {
    let ip = ip.to_canonical();
    addresses
        .iter()
        .filter(move |(_, local)| local.to_canonical() == ip)
        .map(|(name, _)| name.as_str())
}

/// What an endpoint reads from the operating system about its local interfaces.
pub(crate) trait Os {
    /// Returns the link speed of the interface of `name`, where the operating system reports
    /// one.
    fn link_speed(&self, name: &str) -> Option<LinkSpeed>;

    /// Returns each local address, with the name of the interface it is on.
    fn addresses(&self) -> io::Result<Vec<(String, IpAddr)>>;
}

/// The operating system the process runs on.
pub(crate) struct System;

impl Os for System {
    #[inline]
    fn link_speed(&self, name: &str) -> Option<LinkSpeed> {
        platform::link_speed(name)
    }

    #[inline]
    fn addresses(&self) -> io::Result<Vec<(String, IpAddr)>> {
        platform::addresses()
    }
}

/// Reads a link speed from the text of a Linux `/sys/class/net/<name>/speed` file, in megabits
/// per second. Where the file can't be read, or holds an empty, zero or negative value, the
/// speed is unknown: Linux holds -1 there where it doesn't know the speed, such as for a link
/// that is down, and refuses the read for an interface that has no speed, such as loopback.
#[cfg(any(target_os = "linux", test))]
fn sysfs_speed(text: io::Result<String>) -> Option<LinkSpeed> {
    let megabits_per_second: i64 = text.ok()?.trim().parse().ok()?;
    LinkSpeed::from_megabits_per_second(u64::try_from(megabits_per_second).ok()?)
}

/// Returns whether `name` is a single path component, so that it names a directory of
/// `/sys/class/net` and no other path.
#[cfg(any(target_os = "linux", test))]
fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

/// Reads a link speed from the `ifi_baudrate` of a macOS link-layer address, in bits per second.
/// 0 is an unknown speed. The field holds 32 bits, so it can't hold the speed of a faster link:
/// `u32::MAX` is an unknown speed too, and the interface takes its configured rate.
#[cfg(any(target_vendor = "apple", test))]
fn baudrate_speed(baudrate: u32) -> Option<LinkSpeed> {
    if baudrate == u32::MAX {
        return None;
    }
    LinkSpeed::from_bits_per_second(baudrate.into())
}

/// The list of local addresses that `getifaddrs` returns.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
mod ifaddrs {
    use std::{
        ffi::CStr,
        io,
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
    };

    /// The list of local addresses that `getifaddrs` returns, freed on drop.
    pub(super) struct IfAddrs(*mut libc::ifaddrs);

    impl IfAddrs {
        pub(super) fn new() -> io::Result<Self> {
            let mut head = core::ptr::null_mut();
            // SAFETY: `head` is a valid place for `getifaddrs` to write the list's head to, and
            // `Drop` frees the list once.
            if unsafe { libc::getifaddrs(&mut head) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(head))
        }

        /// Returns each entry of the list.
        pub(super) fn iter(&self) -> impl Iterator<Item = &libc::ifaddrs> + '_ {
            let mut next = self.0;
            core::iter::from_fn(move || {
                // SAFETY: each entry is null or valid until `Drop` frees the list, which the
                // borrow of `self` rules out while the iterator lives.
                let entry = unsafe { next.as_ref() }?;
                next = entry.ifa_next;
                Some(entry)
            })
        }
    }

    impl Drop for IfAddrs {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the list came from `getifaddrs`, and this frees it once.
                unsafe { libc::freeifaddrs(self.0) }
            }
        }
    }

    /// Returns the name of the interface of an entry.
    pub(super) fn name(entry: &libc::ifaddrs) -> Option<&str> {
        if entry.ifa_name.is_null() {
            return None;
        }
        // SAFETY: `getifaddrs` sets `ifa_name` to a string that ends in a NUL and lives as long
        // as the entry.
        unsafe { CStr::from_ptr(entry.ifa_name) }.to_str().ok()
    }

    /// Returns the address family of an entry, where it has an address.
    pub(super) fn family(entry: &libc::ifaddrs) -> Option<libc::c_int> {
        // SAFETY: `ifa_addr` is null or points to a socket address that lives as long as the
        // entry.
        let addr = unsafe { entry.ifa_addr.as_ref() }?;
        Some(addr.sa_family as libc::c_int)
    }

    /// Returns the IP address of an entry, where it has one.
    fn ip(entry: &libc::ifaddrs) -> Option<IpAddr> {
        match family(entry)? {
            libc::AF_INET => {
                // SAFETY: the address of an `AF_INET` entry is a `sockaddr_in`.
                let addr = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in>() };
                Some(Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)).into())
            }
            libc::AF_INET6 => {
                // SAFETY: the address of an `AF_INET6` entry is a `sockaddr_in6`.
                let addr = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in6>() };
                Some(Ipv6Addr::from(addr.sin6_addr.s6_addr).into())
            }
            _ => None,
        }
    }

    /// Returns each local IP address, with the name of the interface it is on.
    pub(super) fn addresses() -> io::Result<Vec<(String, IpAddr)>> {
        let list = IfAddrs::new()?;
        let addresses = list
            .iter()
            .filter_map(|entry| Some((name(entry)?.to_owned(), ip(entry)?)))
            .collect();
        Ok(addresses)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{is_plain_name, sysfs_speed, LinkSpeed};

    pub(super) use super::ifaddrs::addresses;

    /// Reads the link speed of the interface of `name` from `/sys/class/net/<name>/speed`.
    pub(super) fn link_speed(name: &str) -> Option<LinkSpeed> {
        if !is_plain_name(name) {
            return None;
        }
        let path = format!("/sys/class/net/{name}/speed");
        sysfs_speed(std::fs::read_to_string(path))
    }
}

#[cfg(target_vendor = "apple")]
mod platform {
    use super::{baudrate_speed, ifaddrs, LinkSpeed};

    pub(super) use super::ifaddrs::addresses;

    /// Reads the link speed of the interface of `name` from the `if_data` of its link-layer
    /// address.
    pub(super) fn link_speed(name: &str) -> Option<LinkSpeed> {
        let list = ifaddrs::IfAddrs::new().ok()?;
        list.iter().find_map(|entry| {
            if ifaddrs::name(entry)? != name || ifaddrs::family(entry)? != libc::AF_LINK {
                return None;
            }
            // SAFETY: the data of an `AF_LINK` entry is null or points to the interface's
            // `if_data`, which lives as long as the entry.
            let data = unsafe { entry.ifa_data.cast::<libc::if_data>().as_ref() }?;
            baudrate_speed(data.ifi_baudrate)
        })
    }
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
mod platform {
    use super::LinkSpeed;
    use std::{io, net::IpAddr};

    /// This platform doesn't report a link speed.
    pub(super) fn link_speed(_name: &str) -> Option<LinkSpeed> {
        None
    }

    /// This platform doesn't report its local addresses, so each socket that no interface lists
    /// is on the default interface.
    pub(super) fn addresses() -> io::Result<Vec<(String, IpAddr)>> {
        Err(io::ErrorKind::Unsupported.into())
    }
}
