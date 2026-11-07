// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The pairing of an endpoint's local interfaces with a peer's interfaces.
//!
//! Where an endpoint lists more than one local interface and a peer tags its data addresses
//! with its own interfaces, each local interface sends to one of the peer's interfaces: the k-th
//! local interface that holds a send socket, in the order of their [`Id`]s, pairs with the
//! peer's k-th interface, in the order of its tags, and the pairing starts again from the peer's
//! first interface where the endpoint has more interfaces than the peer. The send sockets of a
//! local interface take the addresses of its paired interface in turn, so that they spread over
//! them.
//!
//! Where the peer doesn't tag its addresses, the endpoint doesn't list interfaces, or either side
//! has a single interface, a sender keeps the address it took before: `addrs[sender % len]`.
//!
//! A receiver sends the acknowledgements of a peer's packets back along the pair they came on,
//! so that they cross the same interfaces as the packets: see [`paired_ack_sender`].

use super::{Id, Table};
use s2n_quic_core::inet::SocketAddress;
use std::net::SocketAddr;

/// Where a send socket sits among the endpoint's interfaces, for pairing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendPlace {
    /// The rank of the socket's interface among the interfaces that hold a send socket, in the
    /// order of their [`Id`]s.
    pub rank: usize,
    /// How many interfaces hold a send socket.
    pub count: usize,
    /// The socket's position among the send sockets of its interface.
    pub index: usize,
}

impl Table {
    /// Returns the place of each send socket, in the order of the send sockets, or `None` where
    /// the configuration doesn't list an interface.
    pub fn send_places(&self) -> Option<Vec<SendPlace>> {
        if self.interfaces().len() <= 1 {
            return None;
        }

        let mut ids: Vec<Id> = self.send_sockets().to_vec();
        ids.sort_unstable();
        ids.dedup();

        let mut taken = vec![0; ids.len()];
        let places = self
            .send_sockets()
            .iter()
            .map(|id| {
                let rank = ids
                    .binary_search(id)
                    .expect("each send socket's interface holds a send socket");
                let index = taken[rank];
                taken[rank] += 1;
                SendPlace {
                    rank,
                    count: ids.len(),
                    index,
                }
            })
            .collect();
        Some(places)
    }
}

/// Returns the address of the peer that a sender sends to.
///
/// `addrs` holds the peer's data addresses, and `interfaces` the interface tag of each, in the
/// same order, or nothing where the peer doesn't tag them. `place` is the sender's place where
/// the endpoint lists interfaces.
///
/// # Panics
///
/// Where `addrs` is empty.
pub fn paired_peer_addr(
    addrs: &[SocketAddr],
    interfaces: &[Option<u16>],
    sender_idx: usize,
    place: Option<SendPlace>,
) -> SocketAddr {
    assert!(!addrs.is_empty(), "a peer has at least one data address");
    let unpaired = addrs[sender_idx % addrs.len()];

    let Some(place) = place else {
        return unpaired;
    };
    if interfaces.len() != addrs.len() {
        return unpaired;
    }

    let mut remote: Vec<u16> = interfaces.iter().flatten().copied().collect();
    remote.sort_unstable();
    remote.dedup();
    if place.count <= 1 || remote.len() <= 1 {
        return unpaired;
    }

    let paired = remote[place.rank % remote.len()];
    let on_paired: Vec<SocketAddr> = addrs
        .iter()
        .zip(interfaces)
        .filter(|(_, tag)| **tag == Some(paired))
        .map(|(addr, _)| *addr)
        .collect();
    on_paired[place.index % on_paired.len()]
}

/// Returns the sender that sends the acknowledgements of a peer's packets, so that they follow
/// the packets' route back: a sender of the local interface paired with the peer's interface
/// that the packets came from.
///
/// `addrs` and `interfaces` hold the peer's data addresses and their tags, as
/// [`paired_peer_addr`] reads them, `places` the place of each of the endpoint's send sockets,
/// by sender, `source` the address the packets came from, and `remote_sender` the peer's sender
/// that sent them. Where `source` is one of `addrs`, the sender's position among the senders of
/// the paired interface is the address's position among the addresses of its interface, so
/// that each of the peer's addresses maps to one sender. Where only the IP address of `source`
/// matches, the peer's senders map to the senders of the paired interface in turn.
///
/// Returns `None`, which keeps the caller's choice, where the peer doesn't tag its addresses,
/// either side has a single interface, `source` doesn't match the addresses of exactly one of
/// the peer's interfaces, or the endpoint doesn't hold a send socket on the paired interface.
pub fn paired_ack_sender(
    addrs: &[SocketAddr],
    interfaces: &[Option<u16>],
    places: &[SendPlace],
    source: SocketAddress,
    remote_sender: usize,
) -> Option<usize> {
    if interfaces.len() != addrs.len() {
        return None;
    }

    let mut remote: Vec<u16> = interfaces.iter().flatten().copied().collect();
    remote.sort_unstable();
    remote.dedup();
    if remote.len() <= 1 {
        return None;
    }

    let exact = addrs
        .iter()
        .position(|addr| SocketAddress::from(*addr).unmapped_eq(&source));
    let (tag, position) = match exact {
        Some(at) => {
            let tag = interfaces[at]?;
            let position = interfaces[..at]
                .iter()
                .filter(|other| **other == Some(tag))
                .count();
            (tag, Some(position))
        }
        None => {
            let mut tags = addrs
                .iter()
                .zip(interfaces)
                .filter(|(addr, _)| SocketAddress::from(**addr).ip().unmapped_eq(&source.ip()))
                .map(|(_, tag)| *tag);
            let tag = tags.next().flatten()?;
            if tags.any(|other| other != Some(tag)) {
                return None;
            }
            (tag, None)
        }
    };

    let rank = remote.binary_search(&tag).ok()?;
    let on_rank: Vec<usize> = places
        .iter()
        .enumerate()
        .filter(|(_, place)| place.count > 1 && place.rank == rank)
        .map(|(sender, _)| sender)
        .collect();
    if on_rank.is_empty() {
        return None;
    }
    Some(on_rank[position.unwrap_or(remote_sender) % on_rank.len()])
}

/// Returns the interface tag of the peer's address that a sender sends to, as
/// [`paired_peer_addr`] chooses that address, or `None` where the peer doesn't tag it or doesn't
/// have a data address. The senders of one tag send to one of the peer's interfaces, so a down
/// peer interface stops the pairs of all of them.
pub fn paired_peer_interface(
    addrs: &[SocketAddr],
    interfaces: &[Option<u16>],
    sender_idx: usize,
    place: Option<SendPlace>,
) -> Option<u16> {
    if addrs.is_empty() || interfaces.len() != addrs.len() {
        return None;
    }
    let addr = paired_peer_addr(addrs, interfaces, sender_idx, place);
    addrs
        .iter()
        .zip(interfaces)
        .find(|(candidate, _)| **candidate == addr)
        .and_then(|(_, tag)| *tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::{
        interface::{Config, System},
        rate::Rate,
    };

    fn addrs(count: u16) -> Vec<SocketAddr> {
        (0..count)
            .map(|port| SocketAddr::from(([10, 0, 0, 1], 4000 + port)))
            .collect()
    }

    /// The place of a socket among three interfaces.
    fn place(rank: usize, index: usize) -> SendPlace {
        SendPlace {
            rank,
            count: 3,
            index,
        }
    }

    #[test]
    fn send_places_rank_the_interfaces_by_id() {
        let sockets = addrs(4);
        let configs = [
            Config {
                send_sockets: vec![0, 2],
                ..Default::default()
            },
            Config {
                send_sockets: vec![1],
                ..Default::default()
            },
        ];
        let table = Table::resolve(&configs, Rate::new(1.0), &sockets, &[], &System);
        // Socket 3, which no interface lists, is on the default interface, which ranks first.
        assert_eq!(
            table.send_places(),
            Some(vec![place(1, 0), place(2, 0), place(1, 1), place(0, 0)])
        );

        let table = Table::resolve(&[], Rate::new(1.0), &sockets, &[], &System);
        assert_eq!(table.send_places(), None);
    }

    #[test]
    fn a_sender_sends_to_the_interface_its_own_is_paired_with() {
        let peer = addrs(4);
        let tags = [Some(1), Some(1), Some(2), Some(2)];
        let two = |rank, index| SendPlace {
            rank,
            count: 2,
            index,
        };

        assert_eq!(paired_peer_addr(&peer, &tags, 7, Some(two(0, 0))), peer[0]);
        assert_eq!(paired_peer_addr(&peer, &tags, 7, Some(two(0, 1))), peer[1]);
        assert_eq!(paired_peer_addr(&peer, &tags, 7, Some(two(1, 0))), peer[2]);
        assert_eq!(paired_peer_addr(&peer, &tags, 7, Some(two(1, 3))), peer[3]);
        // A third local interface starts again from the peer's first interface.
        assert_eq!(
            paired_peer_addr(&peer, &tags, 7, Some(place(2, 0))),
            peer[0]
        );
        // An address without a tag among tagged ones isn't paired with any interface.
        let gap = [Some(1), None, Some(2), Some(2)];
        for index in 0..3 {
            assert_eq!(
                paired_peer_addr(&peer, &gap, 7, Some(two(0, index))),
                peer[0]
            );
        }
    }

    #[test]
    fn a_sender_keeps_its_address_where_the_interfaces_dont_pair() {
        let peer = addrs(4);
        let tags = [Some(1), Some(1), Some(2), Some(2)];
        let unpaired = peer[5 % 4];

        // The peer doesn't tag its addresses.
        assert_eq!(paired_peer_addr(&peer, &[], 5, Some(place(1, 0))), unpaired);
        assert_eq!(
            paired_peer_addr(&peer, &[None; 4], 5, Some(place(1, 0))),
            unpaired
        );
        // The tags don't match the addresses.
        assert_eq!(
            paired_peer_addr(&peer, &tags[..3], 5, Some(place(1, 0))),
            unpaired
        );
        // The endpoint doesn't list interfaces.
        assert_eq!(paired_peer_addr(&peer, &tags, 5, None), unpaired);
        // The endpoint has one interface that holds a send socket.
        let one = SendPlace {
            rank: 0,
            count: 1,
            index: 0,
        };
        assert_eq!(paired_peer_addr(&peer, &tags, 5, Some(one)), unpaired);
        // The peer has one interface.
        assert_eq!(
            paired_peer_addr(&peer, &[Some(1); 4], 5, Some(place(1, 0))),
            unpaired
        );
    }

    #[test]
    fn the_first_flagged_interface_that_holds_a_send_socket_carries_the_highest_priority() {
        let sockets = addrs(2);
        let configs = [
            Config {
                carries_highest_priority: true,
                ..Default::default()
            },
            Config {
                send_sockets: vec![0],
                ..Default::default()
            },
            Config {
                send_sockets: vec![1],
                carries_highest_priority: true,
                ..Default::default()
            },
        ];
        let table = Table::resolve(&configs, Rate::new(1.0), &sockets, &[], &System);
        assert_eq!(table.highest_priority(), Some(Id(3)));

        let table = Table::resolve(&configs[1..2], Rate::new(1.0), &sockets, &[], &System);
        assert_eq!(table.highest_priority(), None);
    }

    #[test]
    fn lists_the_send_sockets_of_the_highest_priority_interface() {
        let sockets = addrs(4);
        let configs = |carries_highest_priority| {
            [
                Config {
                    send_sockets: vec![0, 2],
                    ..Default::default()
                },
                Config {
                    send_sockets: vec![1, 3],
                    carries_highest_priority,
                    ..Default::default()
                },
            ]
        };
        let table = Table::resolve(&configs(true), Rate::new(1.0), &sockets, &[], &System);
        assert_eq!(table.highest_priority_send_sockets(), Some(vec![1, 3]));

        let table = Table::resolve(&configs(false), Rate::new(1.0), &sockets, &[], &System);
        assert_eq!(table.highest_priority_send_sockets(), None);

        // One interface holds every send socket.
        let one = [Config {
            send_sockets: vec![0, 1, 2, 3],
            carries_highest_priority: true,
            ..Default::default()
        }];
        let table = Table::resolve(&one, Rate::new(1.0), &sockets, &[], &System);
        assert_eq!(table.highest_priority_send_sockets(), None);
    }

    #[test]
    fn paired_peer_interface_is_the_tag_of_the_paired_address() {
        let peer = addrs(4);
        let tags = [Some(1), Some(2), Some(1), Some(2)];

        // A sender's peer interface is the tag of the address it sends to.
        assert_eq!(
            paired_peer_interface(&peer, &tags, 7, Some(place(0, 0))),
            Some(1)
        );
        assert_eq!(
            paired_peer_interface(&peer, &tags, 7, Some(place(1, 1))),
            Some(2)
        );
        assert_eq!(
            paired_peer_interface(&peer, &tags, 7, Some(place(2, 0))),
            Some(1)
        );
        // Where the endpoint doesn't list interfaces, it is the tag of `addrs[sender % len]`.
        assert_eq!(paired_peer_interface(&peer, &tags, 3, None), Some(2));
        // An address without a tag, a peer that doesn't tag its addresses, and a peer without
        // a data address don't have a peer interface.
        let gap = [Some(1), None, Some(2), Some(2)];
        assert_eq!(paired_peer_interface(&peer, &gap, 1, None), None);
        assert_eq!(paired_peer_interface(&peer, &[], 1, None), None);
        assert_eq!(paired_peer_interface(&[], &[], 1, None), None);
    }

    /// The places of the four send sockets of an endpoint with two interfaces: sockets 0 and 2
    /// on the first, 1 and 3 on the second.
    fn two_interfaces() -> [SendPlace; 4] {
        let two = |rank, index| SendPlace {
            rank,
            count: 2,
            index,
        };
        [two(0, 0), two(1, 0), two(0, 1), two(1, 1)]
    }

    #[test]
    fn an_acknowledgement_goes_back_along_the_pair_its_packet_came_on() {
        let peer = addrs(4);
        let tags = [Some(1), Some(1), Some(2), Some(2)];
        let places = two_interfaces();
        for (at, &source) in peer.iter().enumerate() {
            let sender = paired_ack_sender(&peer, &tags, &places, source.into(), 7)
                .expect("a packet from a tagged address is paired");
            // The sender is on the local interface paired with the source's interface, and
            // sends back to the source.
            assert_eq!(places[sender].rank, at / 2);
            assert_eq!(
                paired_peer_addr(&peer, &tags, sender, Some(places[sender])),
                source
            );
        }
    }

    #[test]
    fn an_acknowledgement_of_a_packet_from_another_port_stays_on_its_pair() {
        let peer = [
            SocketAddr::from(([10, 0, 0, 1], 4000)),
            SocketAddr::from(([10, 0, 0, 1], 4001)),
            SocketAddr::from(([10, 0, 0, 2], 4000)),
            SocketAddr::from(([10, 0, 0, 2], 4001)),
        ];
        let tags = [Some(1), Some(1), Some(2), Some(2)];
        let places = two_interfaces();
        // A packet from a port that isn't one of the peer's data addresses goes back on a
        // sender of the paired interface, the peer's senders in turn.
        let source = SocketAddress::from(SocketAddr::from(([10, 0, 0, 2], 5555)));
        assert_eq!(paired_ack_sender(&peer, &tags, &places, source, 4), Some(1));
        assert_eq!(paired_ack_sender(&peer, &tags, &places, source, 5), Some(3));

        // An IPv4 address mapped into IPv6 matches the address it maps.
        let peer = addrs(4);
        let mapped: SocketAddr = "[::ffff:10.0.0.1]:4001".parse().unwrap();
        assert_eq!(
            paired_ack_sender(&peer, &tags, &places, mapped.into(), 7),
            Some(2)
        );
    }

    #[test]
    fn an_acknowledgement_keeps_the_callers_sender_where_the_pair_isnt_known() {
        let peer = addrs(4);
        let tags = [Some(1), Some(1), Some(2), Some(2)];
        let places = two_interfaces();
        let source = SocketAddress::from(peer[1]);

        // The peer doesn't tag its addresses.
        assert_eq!(paired_ack_sender(&peer, &[], &places, source, 7), None);
        assert_eq!(paired_ack_sender(&peer, &[None; 4], &places, source, 7), None);
        // The tags don't match the addresses.
        assert_eq!(paired_ack_sender(&peer, &tags[..3], &places, source, 7), None);
        // The source's address doesn't have a tag.
        let gap = [Some(1), None, Some(2), Some(2)];
        assert_eq!(paired_ack_sender(&peer, &gap, &places, source, 7), None);
        // The peer has one interface.
        assert_eq!(paired_ack_sender(&peer, &[Some(1); 4], &places, source, 7), None);
        // The endpoint has one interface that holds a send socket.
        let one = [SendPlace {
            rank: 0,
            count: 1,
            index: 0,
        }];
        assert_eq!(paired_ack_sender(&peer, &tags, &one, source, 7), None);
        // The source's IP address isn't one of the peer's.
        let elsewhere = SocketAddress::from(SocketAddr::from(([10, 0, 0, 9], 4001)));
        assert_eq!(paired_ack_sender(&peer, &tags, &places, elsewhere, 7), None);
        // The source's port isn't a data address, and its IP address is on both of the peer's
        // interfaces.
        let other_port = SocketAddress::from(SocketAddr::from(([10, 0, 0, 1], 5555)));
        assert_eq!(paired_ack_sender(&peer, &tags, &places, other_port, 7), None);
        // The source is on the peer's third interface, which no local interface sends to.
        let three = [Some(1), Some(1), Some(2), Some(3)];
        let on_third = SocketAddress::from(peer[3]);
        assert_eq!(paired_ack_sender(&peer, &three, &places, on_third, 7), None);
        // A source on the peer's second interface still has a pair.
        let on_second = SocketAddress::from(peer[2]);
        assert_eq!(
            paired_ack_sender(&peer, &three, &places, on_second, 7),
            Some(1)
        );
    }
}
