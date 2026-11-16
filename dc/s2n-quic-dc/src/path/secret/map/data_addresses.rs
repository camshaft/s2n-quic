// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Encoding and decoding of the `DcDataAddresses` transport parameter payload.
//!
//! Format (version 0x01):
//! ```text
//! version  : u8
//! count    : u8
//! entries  : [Entry; count]
//! ```
//! where each `Entry` is:
//! ```text
//! addr_type  : u8
//! value_len  : u16 (big-endian)
//! value      : [u8; value_len]
//! ```
//! `count` counts the entries of every type.
//!
//! `ADDR_TYPE_SOCKET_V6` (0x01) is an address entry: the 18-byte value
//! holds a 16-byte IPv6 address followed by a 2-byte big-endian port. IPv4
//! addresses are mapped to IPv4-mapped IPv6 form on encode and unmapped on
//! decode.
//!
//! `ADDR_TYPE_INTERFACE` (0x02) tags the address entry just before it with the
//! local interface the address is on: the 2-byte value holds the interface's
//! big-endian id. An address without an interface entry after it is untagged. A
//! decoder that doesn't know this type skips its entries, as it skips any type it
//! doesn't know, so it still reads every address.

use s2n_codec::{DecoderBuffer, DecoderError, Encoder, EncoderValue};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6};

#[cfg(test)]
mod tests;

pub const VERSION: u8 = 0x01;
pub const ADDR_TYPE_SOCKET_V6: u8 = 0x01;
pub const ADDR_TYPE_INTERFACE: u8 = 0x02;

const SOCKET_V6_LEN: u16 = 18; // 16-byte IPv6 address + 2-byte port
const INTERFACE_LEN: u16 = 2; // 2-byte big-endian interface id

/// The most entries a payload's u8 count holds.
const MAX_ENTRIES: usize = u8::MAX as usize;

/// An advertised data address, with the local interface it is on where the
/// payload tags it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataAddr {
    pub addr: SocketAddr,
    pub interface: Option<u16>,
}

impl DataAddr {
    /// Returns `addr` without an interface.
    #[inline]
    pub fn untagged(addr: SocketAddr) -> Self {
        Self {
            addr,
            interface: None,
        }
    }
}

struct Addrs<'a>(&'a [DataAddr]);

impl Addrs<'_> {
    fn tag_count(&self) -> usize {
        self.0.iter().filter_map(|addr| addr.interface).count()
    }
}

impl EncoderValue for Addrs<'_> {
    fn encode<E: Encoder>(&self, buffer: &mut E) {
        buffer.encode(&VERSION);
        buffer.encode(&((self.0.len() + self.tag_count()) as u8));
        for DataAddr { addr, interface } in self.0 {
            let v6 = match addr {
                SocketAddr::V6(v6) => *v6,
                SocketAddr::V4(v4) => SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0),
            };
            buffer.encode(&ADDR_TYPE_SOCKET_V6);
            buffer.encode(&SOCKET_V6_LEN);
            buffer.write_slice(&v6.ip().octets());
            buffer.encode(&v6.port());
            if let Some(interface) = interface {
                buffer.encode(&ADDR_TYPE_INTERFACE);
                buffer.encode(&INTERFACE_LEN);
                buffer.encode(interface);
            }
        }
    }

    fn encoding_size(&self) -> usize {
        2 + self.0.len() * (1 + 2 + SOCKET_V6_LEN as usize)
            + self.tag_count() * (1 + 2 + INTERFACE_LEN as usize)
    }
}

/// Encodes `addrs`, untagged.
pub fn encode(addrs: &[SocketAddr]) -> bytes::Bytes {
    let addrs: Vec<DataAddr> = addrs.iter().copied().map(DataAddr::untagged).collect();
    bytes::Bytes::from(Addrs(&addrs).encode_to_vec())
}

/// Encodes `addrs`, each with an interface entry after it where it has an interface.
///
/// The payload's u8 count holds at most 255 entries, so at most 127 tagged addresses fit.
/// Where the addresses and their interface entries come to more than 255 entries, every
/// interface entry is left out, and the addresses are encoded untagged.
pub fn encode_tagged(addrs: &[DataAddr]) -> bytes::Bytes {
    let payload = Addrs(addrs);
    if addrs.len() + payload.tag_count() <= MAX_ENTRIES {
        return bytes::Bytes::from(payload.encode_to_vec());
    }

    let addrs: Vec<SocketAddr> = addrs.iter().map(|addr| addr.addr).collect();
    encode(&addrs)
}

/// Decodes the addresses, each with the interface of the interface entry after it.
///
/// An interface entry is skipped where its value isn't 2 bytes, where it is the first entry,
/// or where the entry before it isn't an address this decoder read, such as another interface
/// entry or an entry of a type it doesn't know.
pub fn decode(data: &[u8]) -> Result<Vec<DataAddr>, DecoderError> {
    let buffer = DecoderBuffer::new(data);

    let (version, buffer) = buffer.decode::<u8>()?;
    if version != VERSION {
        return Err(DecoderError::InvariantViolation(
            "DcDataAddresses: unsupported version",
        ));
    }

    let (count, mut buffer) = buffer.decode::<u8>()?;
    let mut addrs: Vec<DataAddr> = Vec::with_capacity(count as usize);
    // Whether the entry just read is an address that an interface entry may tag.
    let mut taggable = false;

    for _ in 0..count {
        let (addr_type, next) = buffer.decode::<u8>()?;
        let (value_len, next) = next.decode::<u16>()?;
        let (entry, next) = next.decode_slice(value_len as usize)?;
        buffer = next;

        if addr_type == ADDR_TYPE_SOCKET_V6 && value_len == SOCKET_V6_LEN {
            let (ip_bytes, entry) = entry.decode::<[u8; 16]>()?;
            let (port, _) = entry.decode::<u16>()?;
            let ip = Ipv6Addr::from(ip_bytes);
            let addr = match ip.to_ipv4_mapped() {
                Some(v4) => SocketAddr::new(v4.into(), port),
                None => SocketAddr::new(ip.into(), port),
            };
            addrs.push(DataAddr::untagged(addr));
            taggable = true;
        } else if addr_type == ADDR_TYPE_INTERFACE && value_len == INTERFACE_LEN && taggable {
            let (interface, _) = entry.decode::<u16>()?;
            if let Some(addr) = addrs.last_mut() {
                addr.interface = Some(interface);
            }
            taggable = false;
        } else {
            taggable = false;
        }
    }

    Ok(addrs)
}

/// Returns `payload` with each address tagged with the interface of the first socket of
/// `local` that it matches, or `None` where `payload` doesn't decode.
///
/// An address matches a socket where their ports are equal and their IPs are equal, an
/// IPv4-mapped IPv6 address read as its IPv4 address, or where either IP is unspecified
/// (0.0.0.0 or ::). An address that doesn't match a socket keeps the interface it has.
pub fn tag_interfaces(payload: &[u8], local: &[(SocketAddr, u16)]) -> Option<bytes::Bytes> {
    let mut addrs = decode(payload).ok()?;
    for addr in &mut addrs {
        let matched = local
            .iter()
            .find(|(socket, _)| matches_socket(addr.addr, *socket));
        if let Some((_, interface)) = matched {
            addr.interface = Some(*interface);
        }
    }
    Some(encode_tagged(&addrs))
}

fn matches_socket(advertised: SocketAddr, local: SocketAddr) -> bool {
    let (advertised_ip, local_ip) = (unmapped(advertised.ip()), unmapped(local.ip()));
    advertised.port() == local.port()
        && (advertised_ip == local_ip
            || advertised_ip.is_unspecified()
            || local_ip.is_unspecified())
}

fn unmapped(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}
