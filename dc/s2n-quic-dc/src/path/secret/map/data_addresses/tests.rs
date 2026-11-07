// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::path::secret::Map;

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn tagged(s: &str, interface: u16) -> DataAddr {
    DataAddr {
        addr: addr(s),
        interface: Some(interface),
    }
}

fn untagged(s: &str) -> DataAddr {
    DataAddr::untagged(addr(s))
}

/// Builds a payload of `entries`, each an entry type and its value.
fn payload(entries: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut bytes = vec![VERSION, entries.len() as u8];
    for (addr_type, value) in entries {
        bytes.push(*addr_type);
        bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
        bytes.extend_from_slice(value);
    }
    bytes
}

/// The value of an address entry.
fn socket_v6(s: &str) -> Vec<u8> {
    let v6 = match addr(s) {
        SocketAddr::V6(v6) => v6,
        SocketAddr::V4(v4) => SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0),
    };
    let mut value = v6.ip().octets().to_vec();
    value.extend_from_slice(&v6.port().to_be_bytes());
    value
}

/// The value of an interface entry.
fn interface(id: u16) -> Vec<u8> {
    id.to_be_bytes().to_vec()
}

/// `count` addresses on ports 9000 and up, each tagged with its index.
fn tagged_localhost(count: u16) -> Vec<DataAddr> {
    (0..count)
        .map(|i| DataAddr {
            addr: SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 9000 + i),
            interface: Some(i),
        })
        .collect()
}

/// The decoder from before interface entries, as a peer that predates them decodes.
fn decode_without_interfaces(data: &[u8]) -> Result<Vec<SocketAddr>, DecoderError> {
    let buffer = DecoderBuffer::new(data);

    let (version, buffer) = buffer.decode::<u8>()?;
    if version != 0x01 {
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

        if addr_type == 0x01 && value_len == 18 {
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

fn new_map(advertised: Option<&[SocketAddr]>) -> Map {
    let builder = Map::builder()
        .with_signer(crate::path::secret::stateless_reset::Signer::new(b"secret"))
        .with_capacity(10)
        .with_clock(s2n_quic_core::time::NoopClock)
        .with_subscriber(crate::event::tracing::Subscriber::default());
    let builder = match advertised {
        Some(addrs) => builder.with_advertised_data_addrs(addrs),
        None => builder,
    };
    builder.build().unwrap()
}

#[test]
fn tagged_addresses_round_trip() {
    let addrs = vec![
        tagged("10.0.0.1:9000", 1),
        untagged("[2001:db8::1]:9001"),
        tagged("[2001:db8::2]:9002", u16::MAX),
    ];
    assert_eq!(decode(&encode_tagged(&addrs)).unwrap(), addrs);
}

#[test]
fn untagged_payload_decodes_untagged() {
    let bytes = encode(&[addr("10.0.0.1:9000"), addr("[2001:db8::1]:9001")]);
    let expected = payload(&[
        (ADDR_TYPE_SOCKET_V6, socket_v6("10.0.0.1:9000")),
        (ADDR_TYPE_SOCKET_V6, socket_v6("[2001:db8::1]:9001")),
    ]);
    assert_eq!(&bytes[..], &expected[..]);
    assert_eq!(
        decode(&bytes).unwrap(),
        vec![untagged("10.0.0.1:9000"), untagged("[2001:db8::1]:9001"),]
    );
    // Untagged addresses encode as they did before interface entries.
    assert_eq!(
        encode_tagged(&[untagged("10.0.0.1:9000"), untagged("[2001:db8::1]:9001")]),
        bytes
    );
}

#[test]
fn a_decoder_without_interfaces_reads_every_address_of_a_tagged_payload() {
    let addrs = vec![
        tagged("10.0.0.1:9000", 1),
        untagged("10.0.0.2:9001"),
        tagged("[2001:db8::1]:9002", 2),
    ];
    let bytes = encode_tagged(&addrs);
    assert_eq!(
        decode_without_interfaces(&bytes).unwrap(),
        vec![
            addr("10.0.0.1:9000"),
            addr("10.0.0.2:9001"),
            addr("[2001:db8::1]:9002"),
        ]
    );
}

#[test]
fn malformed_interface_entries_are_skipped() {
    let bytes = payload(&[
        // An interface entry before any address.
        (ADDR_TYPE_INTERFACE, interface(9)),
        (ADDR_TYPE_SOCKET_V6, socket_v6("10.0.0.1:9000")),
        // An interface entry of the wrong length.
        (ADDR_TYPE_INTERFACE, vec![0, 1, 2]),
        (ADDR_TYPE_SOCKET_V6, socket_v6("10.0.0.2:9001")),
        // An interface entry after an entry of a type this decoder doesn't know.
        (0x7f, vec![1, 2, 3, 4]),
        (ADDR_TYPE_INTERFACE, interface(8)),
        (ADDR_TYPE_SOCKET_V6, socket_v6("10.0.0.3:9002")),
        (ADDR_TYPE_INTERFACE, interface(3)),
        // A second interface entry after one address.
        (ADDR_TYPE_INTERFACE, interface(7)),
    ]);
    assert_eq!(
        decode(&bytes).unwrap(),
        vec![
            untagged("10.0.0.1:9000"),
            untagged("10.0.0.2:9001"),
            tagged("10.0.0.3:9002", 3),
        ]
    );
}

#[test]
fn tags_are_left_out_where_the_entries_pass_the_count() {
    // 127 tagged addresses come to 254 entries, which fit.
    let fits = tagged_localhost(127);
    assert_eq!(decode(&encode_tagged(&fits)).unwrap(), fits);

    // 128 tagged addresses come to 256 entries, so every interface entry is left out.
    let past = tagged_localhost(128);
    let expected: Vec<DataAddr> = past
        .iter()
        .map(|addr| DataAddr::untagged(addr.addr))
        .collect();
    assert_eq!(decode(&encode_tagged(&past)).unwrap(), expected);
}

#[test]
fn local_interfaces_tag_the_addresses_they_match() {
    let advertised = encode(&[
        addr("10.0.0.1:9000"),
        addr("[::]:9001"),
        addr("10.0.0.3:9002"),
        addr("10.0.0.4:9003"),
        addr("10.0.0.5:9004"),
    ]);
    let local = [
        // The same address.
        (addr("10.0.0.1:9000"), 1),
        // An unspecified advertised IP matches any IP of its port.
        (addr("10.0.0.2:9001"), 2),
        // An IPv4-mapped IPv6 address matches its IPv4 address.
        (addr("[::ffff:10.0.0.3]:9002"), 3),
        // An unspecified local IP matches any IP of its port.
        (addr("0.0.0.0:9003"), 4),
        // A socket of another port doesn't match.
        (addr("10.0.0.5:9999"), 5),
    ];
    let tagged_payload = tag_interfaces(&advertised, &local).unwrap();
    assert_eq!(
        decode(&tagged_payload).unwrap(),
        vec![
            tagged("10.0.0.1:9000", 1),
            tagged("[::]:9001", 2),
            tagged("10.0.0.3:9002", 3),
            tagged("10.0.0.4:9003", 4),
            untagged("10.0.0.5:9004"),
        ]
    );
}

#[test]
fn tagging_again_keeps_unmatched_tags_and_the_first_match_wins() {
    let advertised = encode_tagged(&[tagged("10.0.0.1:9000", 7), untagged("10.0.0.2:9001")]);
    let local = [(addr("10.0.0.2:9001"), 1), (addr("0.0.0.0:9001"), 2)];
    let tagged_payload = tag_interfaces(&advertised, &local).unwrap();
    assert_eq!(
        decode(&tagged_payload).unwrap(),
        vec![tagged("10.0.0.1:9000", 7), tagged("10.0.0.2:9001", 1)]
    );
}

#[test]
fn a_payload_that_doesnt_decode_isnt_tagged() {
    let local = [(addr("10.0.0.1:9000"), 1)];
    // An unknown version.
    assert!(tag_interfaces(&[0x02, 0], &local).is_none());
    // A count of one entry, without the entry.
    assert!(tag_interfaces(&[VERSION, 1], &local).is_none());
}

#[test]
fn a_map_tags_the_addresses_it_advertises() {
    let advertised = [addr("10.0.0.1:9000"), addr("10.0.0.1:9001")];
    let map = new_map(Some(&advertised[..]));
    map.set_advertised_data_interfaces(&[(advertised[0], 2), (advertised[1], 1)]);
    assert_eq!(
        decode(&map.advertised_data_addrs().unwrap()).unwrap(),
        vec![tagged("10.0.0.1:9000", 2), tagged("10.0.0.1:9001", 1)]
    );
}

#[test]
fn a_map_without_advertised_addresses_stays_without_them() {
    let map = new_map(None);
    map.set_advertised_data_interfaces(&[(addr("10.0.0.1:9000"), 1)]);
    assert!(map.advertised_data_addrs().is_none());
}
