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
//! Currently only `ADDR_TYPE_SOCKET_V6` (0x01) is defined; the 18-byte value
//! holds a 16-byte IPv6 address followed by a 2-byte big-endian port. IPv4
//! addresses are mapped to IPv4-mapped IPv6 form on encode and unmapped on
//! decode.

use s2n_codec::{DecoderBuffer, DecoderError, Encoder, EncoderValue};
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};

pub const VERSION: u8 = 0x01;
pub const ADDR_TYPE_SOCKET_V6: u8 = 0x01;

const SOCKET_V6_LEN: u16 = 18; // 16-byte IPv6 address + 2-byte port

struct Addrs<'a>(&'a [SocketAddr]);

impl EncoderValue for Addrs<'_> {
    fn encode<E: Encoder>(&self, buffer: &mut E) {
        buffer.encode(&VERSION);
        buffer.encode(&(self.0.len() as u8));
        for addr in self.0 {
            let v6 = match addr {
                SocketAddr::V6(v6) => *v6,
                SocketAddr::V4(v4) => SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0),
            };
            buffer.encode(&ADDR_TYPE_SOCKET_V6);
            buffer.encode(&SOCKET_V6_LEN);
            buffer.write_slice(&v6.ip().octets());
            buffer.encode(&v6.port());
        }
    }

    fn encoding_size(&self) -> usize {
        2 + self.0.len() * (1 + 2 + SOCKET_V6_LEN as usize)
    }
}

pub fn encode(addrs: &[SocketAddr]) -> bytes::Bytes {
    bytes::Bytes::from(Addrs(addrs).encode_to_vec())
}

pub fn decode(data: &[u8]) -> Result<Vec<SocketAddr>, DecoderError> {
    let buffer = DecoderBuffer::new(data);

    let (version, buffer) = buffer.decode::<u8>()?;
    if version != VERSION {
        return Err(DecoderError::InvariantViolation(
            "DcDataAddresses: unsupported version",
        ));
    }

    let (count, mut buffer) = buffer.decode::<u8>()?;
    let mut addrs = Vec::with_capacity(count as usize);

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
            addrs.push(addr);
        }
    }

    Ok(addrs)
}
