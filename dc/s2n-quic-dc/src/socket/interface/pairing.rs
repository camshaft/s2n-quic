// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The pairing of an endpoint's local interfaces with a peer's interfaces.
//!
//! Where an endpoint lists more than one local interface and a peer tags its data addresses
//! with its own interfaces, each local interface sends to the peer's interfaces it pairs with:
//! the k-th local interface that holds a send socket, in the order of their [`Id`]s, pairs with
//! the peer's k-th interface, in the order of its tags, and the pairing starts again from the
//! peer's first interface where the endpoint has more interfaces than the peer. Where the peer
//! has more interfaces than the endpoint, the k-th local interface also pairs with the peer's
//! interfaces at k plus each multiple of the endpoint's count, and its send sockets take those
//! interfaces in turn, so that each of the peer's interfaces receives from the endpoint where
//! the local interface holds a send socket for each of them. The send sockets that send to one
//! of the peer's interfaces take its addresses in turn, so that they spread over them.
//!
//! Where the peer doesn't tag its addresses, the endpoint doesn't list interfaces, or either side
//! has a single interface, a sender keeps the address it took before: `addrs[sender % len]`.
//!
//! A receiver sends the acknowledgements of a peer's packets from the local interface the
//! packets arrived at, on a sender of that interface that sends back to the peer's interface the
//! packets came from where one does, so that they cross the same interfaces as the packets: see
//! [`paired_ack_sender`].

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

        let ids = self.send_interfaces();

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

    /// Returns the rank of each interface among the interfaces that hold a send socket, in the
    /// order of their [`Id`]s, by interface, or `None` where the configuration doesn't list an
    /// interface. An interface that doesn't hold a send socket doesn't have a rank.
    pub fn send_ranks(&self) -> Option<Vec<Option<usize>>> {
        if self.interfaces().len() <= 1 {
            return None;
        }

        let ids = self.send_interfaces();
        let ranks = self
            .interfaces()
            .iter()
            .map(|interface| ids.binary_search(&interface.id).ok())
            .collect();
        Some(ranks)
    }

    /// Returns the interfaces that hold a send socket, in the order of their [`Id`]s.
    fn send_interfaces(&self) -> Vec<Id> {
        let mut ids: Vec<Id> = self.send_sockets().to_vec();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

/// Returns the address of the peer that a sender sends to.
///
/// `addrs` holds the peer's data addresses, and `interfaces` the interface tag of each, in the
/// same order, or nothing where the peer doesn't tag them. `place` is the sender's place where
/// the endpoint lists interfaces: the sender sends to one of the peer's interfaces paired with
/// its own, the one its index takes where the peer has more interfaces than the endpoint, and
/// takes that interface's addresses in turn.
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

    let (rank, turn) = paired_rank(place, remote.len());
    let paired = remote[rank];
    let on_paired: Vec<SocketAddr> = addrs
        .iter()
        .zip(interfaces)
        .filter(|(_, tag)| **tag == Some(paired))
        .map(|(addr, _)| *addr)
        .collect();
    on_paired[turn % on_paired.len()]
}

/// Returns the rank of the peer's interface that a sender at `place` sends to, among the
/// `peer_count` interfaces the peer tags, and the sender's turn among the senders of its
/// interface that send to it, which picks one of that interface's addresses.
///
/// Where the peer has no more interfaces than the endpoint, the sender at rank `r` sends to the
/// peer's interface at `r % peer_count`, and its turn is its index. Where the peer has more, the
/// interfaces at `r`, `r + count`, `r + 2 * count` and so on below `peer_count` pair with the
/// sender's interface, and the sender at index `i` sends to the one at `i % m` among them, with
/// turn `i / m`, where `m` is how many there are.
fn paired_rank(place: SendPlace, peer_count: usize) -> (usize, usize) {
    if place.count <= 1 || peer_count <= place.count || place.rank >= peer_count {
        return (place.rank % peer_count, place.index);
    }
    let paired = (peer_count - place.rank - 1) / place.count + 1;
    let rank = place.rank + place.count * (place.index % paired);
    (rank, place.index / paired)
}

/// Returns the sender that sends the acknowledgements of a peer's packets, so that they follow
/// the packets' route back: a sender of the local interface the packets arrived at that sends
/// to the peer's interface the packets came from.
///
/// `arrival` is the rank of that local interface among the local interfaces that hold a send
/// socket, in the order of their [`Id`]s, where the receive path records it. Where it is `None`,
/// the interface is inferred as the one the peer's interface sends to, where it sends to one
/// alone: the peer's interface at rank `k` among its tags sends to the local interface at rank
/// `k % count`, where `count` is how many local interfaces hold a send socket. Where the
/// endpoint has more interfaces than the peer, the peer's interface sends to each local
/// interface at `k` plus a multiple of the peer's count below `count`. Where that is more than
/// one interface, the packets may have arrived at any of them, and the interface isn't inferred.
///
/// Among the senders of that local interface, the sender is one whose paired peer interface, as
/// [`paired_peer_addr`] chooses it, is the one the packets came from. Where none of them sends
/// to it, as where the peer has more interfaces than the endpoint and the local interface holds
/// fewer send sockets than the peer's interfaces paired with it, the sender is any sender of
/// that local interface.
///
/// `addrs` and `interfaces` hold the peer's data addresses and their tags, as
/// [`paired_peer_addr`] reads them, `places` the place of each of the endpoint's send sockets,
/// by sender, `source` the address the packets came from, and `remote_sender` the peer's sender
/// that sent them. Where `source` is one of `addrs`, the sender's position among those senders
/// is the address's position among the addresses of its interface, so that each of the peer's
/// addresses maps to one sender. Where only the IP address of `source` matches, the peer's
/// senders map to those senders in turn.
///
/// Returns `None`, which keeps the caller's choice, where the peer doesn't tag its addresses,
/// either side has a single interface, `source` doesn't match the addresses of exactly one of
/// the peer's interfaces, `arrival` is `None` and the peer's interface sends to more than one
/// local interface, or `places` doesn't hold a send socket on that local interface.
pub fn paired_ack_sender(
    addrs: &[SocketAddr],
    interfaces: &[Option<u16>],
    places: &[SendPlace],
    source: SocketAddress,
    remote_sender: usize,
    arrival: Option<usize>,
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
    // The acknowledgements leave on the local interface the packets arrived at. Where the
    // receive path doesn't record it, that is inferred as the one the peer's interface at `rank`
    // sends to under the peer's pairing, the local interface at `rank % count`, where it is the
    // only one. Where the endpoint has more interfaces than the peer, the peer's interface
    // sends to each local interface at `rank` plus a multiple of the peer's count below `count`,
    // and the packets may have arrived at any of them.
    let arrived_at = |count: usize| match arrival {
        Some(at) => Some(at),
        None if rank + remote.len() >= count => Some(rank % count),
        None => None,
    };
    let on_arrival: Vec<(usize, SendPlace)> = places
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, place)| place.count > 1 && arrived_at(place.count) == Some(place.rank))
        .collect();
    // Of those senders, the ones that send to the peer's interface the packets came from carry
    // the acknowledgements back across it. Where none of them does, any sender of the interface
    // carries them.
    let paired: Vec<usize> = on_arrival
        .iter()
        .filter(|(_, place)| paired_rank(*place, remote.len()).0 == rank)
        .map(|(sender, _)| *sender)
        .collect();
    let senders: Vec<usize> = if paired.is_empty() {
        on_arrival.iter().map(|(sender, _)| *sender).collect()
    } else {
        paired
    };
    if senders.is_empty() {
        return None;
    }
    Some(senders[position.unwrap_or(remote_sender) % senders.len()])
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
            let sender = paired_ack_sender(&peer, &tags, &places, source.into(), 7, None)
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
        assert_eq!(
            paired_ack_sender(&peer, &tags, &places, source, 4, None),
            Some(1)
        );
        assert_eq!(
            paired_ack_sender(&peer, &tags, &places, source, 5, None),
            Some(3)
        );

        // An IPv4 address mapped into IPv6 matches the address it maps.
        let peer = addrs(4);
        let mapped: SocketAddr = "[::ffff:10.0.0.1]:4001".parse().unwrap();
        assert_eq!(
            paired_ack_sender(&peer, &tags, &places, mapped.into(), 7, None),
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
        assert_eq!(
            paired_ack_sender(&peer, &[], &places, source, 7, None),
            None
        );
        assert_eq!(
            paired_ack_sender(&peer, &[None; 4], &places, source, 7, None),
            None
        );
        // The tags don't match the addresses.
        assert_eq!(
            paired_ack_sender(&peer, &tags[..3], &places, source, 7, None),
            None
        );
        // The source's address doesn't have a tag.
        let gap = [Some(1), None, Some(2), Some(2)];
        assert_eq!(
            paired_ack_sender(&peer, &gap, &places, source, 7, None),
            None
        );
        // The peer has one interface.
        assert_eq!(
            paired_ack_sender(&peer, &[Some(1); 4], &places, source, 7, None),
            None
        );
        // The endpoint has one interface that holds a send socket.
        let one = [SendPlace {
            rank: 0,
            count: 1,
            index: 0,
        }];
        assert_eq!(paired_ack_sender(&peer, &tags, &one, source, 7, None), None);
        // The source's IP address isn't one of the peer's.
        let elsewhere = SocketAddress::from(SocketAddr::from(([10, 0, 0, 9], 4001)));
        assert_eq!(
            paired_ack_sender(&peer, &tags, &places, elsewhere, 7, None),
            None
        );
        // The source's port isn't a data address, and its IP address is on both of the peer's
        // interfaces.
        let other_port = SocketAddress::from(SocketAddr::from(([10, 0, 0, 1], 5555)));
        assert_eq!(
            paired_ack_sender(&peer, &tags, &places, other_port, 7, None),
            None
        );
        // `places` doesn't hold a send socket on the source's local interface.
        let second_only = [SendPlace {
            rank: 1,
            count: 2,
            index: 0,
        }];
        assert_eq!(
            paired_ack_sender(&peer, &tags, &second_only, source, 7, None),
            None
        );
    }

    #[test]
    fn an_acknowledgement_from_a_third_peer_interface_goes_back_across_it() {
        // The peer has three interfaces and the endpoint two: the peer's third interface sends to
        // the endpoint's first, where the pairing starts again, and the endpoint's first
        // interface spreads its two send sockets over the peer's first and third interfaces.
        let peer = addrs(6);
        let tags = [Some(1), Some(1), Some(2), Some(2), Some(3), Some(3)];
        let places = two_interfaces();
        // The endpoint's data addresses, each tagged with the rank of the interface its send
        // socket is on.
        let local: Vec<SocketAddr> = (0..4)
            .map(|port| SocketAddr::from(([10, 0, 0, 2], 5000 + port)))
            .collect();
        let local_tags = [Some(0), Some(1), Some(0), Some(1)];

        for (at, &source) in peer.iter().enumerate() {
            let sender = paired_ack_sender(&peer, &tags, &places, source.into(), 7, None)
                .expect("a packet from a tagged address has a sender");

            // The peer's send socket at `source` sits at place `(at / 2, at % 2)` among the
            // peer's three interfaces, and sends to `arrived`. The sender is on the local
            // interface of `arrived`, the one the packets arrived at.
            let arrived = paired_peer_addr(&local, &local_tags, at, Some(place(at / 2, at % 2)));
            let arrived_on = local_tags[local.iter().position(|addr| *addr == arrived).unwrap()];
            assert_eq!(arrived_on, Some(places[sender].rank as u16));

            // The sender sends back to the peer's interface the packets came from, the third
            // too. Each address of the peer's second interface has a send socket of its own, so
            // the acknowledgements of its packets go back to their source.
            let back = paired_peer_addr(&peer, &tags, sender, Some(places[sender]));
            assert_eq!(tag_of(&peer, &tags, back), tags[at]);
            if at / 2 == 1 {
                assert_eq!(back, source);
            }
        }
    }

    #[test]
    fn send_ranks_rank_the_interfaces_that_hold_a_send_socket() {
        let sockets = addrs(4);
        let configs = [
            Config {
                send_sockets: vec![0, 2],
                recv_sockets: vec![0],
                ..Default::default()
            },
            Config {
                recv_sockets: vec![1],
                ..Default::default()
            },
            Config {
                send_sockets: vec![1, 3],
                ..Default::default()
            },
        ];
        let table = Table::resolve(&configs, Rate::new(1.0), &sockets, &sockets[..2], &System);
        // The default interface and the second listed interface don't hold a send socket.
        let ranks = table
            .send_ranks()
            .expect("the configuration lists interfaces");
        assert_eq!(ranks, [None, Some(0), None, Some(1)]);
        // Each send socket's interface has the rank of the socket's place.
        let places = table
            .send_places()
            .expect("the configuration lists interfaces");
        for (socket, place) in places.iter().enumerate() {
            let id = table.send_sockets()[socket];
            assert_eq!(ranks[id.as_usize()], Some(place.rank));
        }

        let table = Table::resolve(&[], Rate::new(1.0), &sockets, &[], &System);
        assert_eq!(table.send_ranks(), None);
    }

    #[test]
    fn an_acknowledgement_leaves_on_the_interface_its_packet_arrived_at() {
        let peer = addrs(4);
        let tags = [Some(1), Some(1), Some(2), Some(2)];
        let places = two_interfaces();
        for (at, &source) in peer.iter().enumerate() {
            // Where the packets arrive at the interface the pairing sends them to, the sender is
            // the one the pairing infers.
            let inferred = paired_ack_sender(&peer, &tags, &places, source.into(), 7, None);
            assert_eq!(
                paired_ack_sender(&peer, &tags, &places, source.into(), 7, Some(at / 2)),
                inferred
            );

            // Where they arrive at the other interface, the sender is on that interface, at the
            // position the source's address has among the addresses of its peer interface.
            let other = 1 - at / 2;
            let sender = paired_ack_sender(&peer, &tags, &places, source.into(), 7, Some(other))
                .expect("a packet from a tagged address has a sender");
            assert_eq!(places[sender].rank, other);
            assert_eq!(places[sender].index, at % 2);
        }

        let source = SocketAddress::from(peer[1]);
        // `places` doesn't hold a send socket on the interface the packets arrived at.
        assert_eq!(
            paired_ack_sender(&peer, &tags, &places, source, 7, Some(2)),
            None
        );
        // The peer doesn't tag its addresses.
        assert_eq!(
            paired_ack_sender(&peer, &[], &places, source, 7, Some(0)),
            None
        );
    }

    /// The tag of `addr` among `addrs`, where it is one of them.
    fn tag_of(addrs: &[SocketAddr], tags: &[Option<u16>], addr: SocketAddr) -> Option<u16> {
        let at = addrs.iter().position(|other| *other == addr)?;
        tags[at]
    }

    /// The data addresses of a host with `count` interfaces and `sockets` addresses on each,
    /// interface by interface, and the tag of each: the rank of its interface.
    fn tagged(host: u8, count: u16, sockets: u16) -> (Vec<SocketAddr>, Vec<Option<u16>>) {
        let mut addrs = Vec::new();
        let mut tags = Vec::new();
        for at in 0..count * sockets {
            addrs.push(SocketAddr::from(([10, 0, 0, host], 4000 + at)));
            tags.push(Some(at / sockets));
        }
        (addrs, tags)
    }

    /// The places of `sockets` send sockets on each of `count` interfaces, interface by
    /// interface.
    fn in_runs(count: usize, sockets: usize) -> Vec<SendPlace> {
        (0..count * sockets)
            .map(|at| SendPlace {
                rank: at / sockets,
                count,
                index: at % sockets,
            })
            .collect()
    }

    #[test]
    fn senders_spread_over_the_peer_interfaces_their_interface_pairs_with() {
        // Host X has two interfaces and host Y three, with four send sockets on each interface,
        // and each data address is the source of the send socket at the same position.
        let (x, x_tags) = tagged(1, 2, 4);
        let x_places = in_runs(2, 4);
        let (y, y_tags) = tagged(2, 3, 4);
        let y_places = in_runs(3, 4);

        // X's first interface pairs with Y's first and third and its sockets send to them in
        // turn, and X's second sends to Y's second. Each socket takes the addresses of its peer
        // interface in turn.
        let x_sends_to = [0, 8, 1, 9, 4, 5, 6, 7];
        for (sender, &at) in x_sends_to.iter().enumerate() {
            let place = Some(x_places[sender]);
            assert_eq!(paired_peer_addr(&y, &y_tags, sender, place), y[at]);
            assert_eq!(
                paired_peer_interface(&y, &y_tags, sender, place),
                y_tags[at]
            );
        }
        // Y, which has more interfaces, sends from its interface at rank `r` to X's at `r % 2`.
        for (sender, place) in y_places.iter().enumerate() {
            let at = (place.rank % 2) * 4 + place.index;
            assert_eq!(paired_peer_addr(&x, &x_tags, sender, Some(*place)), x[at]);
        }

        // X acknowledges a packet from each of Y's addresses on a sender of the interface the
        // packet arrived at that sends back to the packet's peer interface: a packet from Y's
        // third interface arrives at X's first and goes back on its second or fourth socket.
        let x_acks_on = [0, 2, 0, 2, 4, 5, 6, 7, 1, 3, 1, 3];
        for (at, &source) in y.iter().enumerate() {
            let to = paired_peer_addr(&x, &x_tags, at, Some(y_places[at]));
            let arrival = tag_of(&x, &x_tags, to).expect("an address of X");
            for arrived in [Some(usize::from(arrival)), None] {
                let sender = paired_ack_sender(&y, &y_tags, &x_places, source.into(), 7, arrived);
                assert_eq!(sender, Some(x_acks_on[at]));
            }
            let sender = x_acks_on[at];
            let back = paired_peer_addr(&y, &y_tags, sender, Some(x_places[sender]));
            assert_eq!(tag_of(&y, &y_tags, back), y_tags[at]);
        }

        // Y acknowledges a packet from each of X's addresses on a sender of the interface the
        // packet arrived at, which sends back to the packet's source: a packet from X's first
        // interface that arrives at Y's third goes back across it.
        for (at, &source) in x.iter().enumerate() {
            let arrived = y_tags[x_sends_to[at]].map(usize::from);
            let sender = paired_ack_sender(&x, &x_tags, &y_places, source.into(), 7, arrived)
                .expect("a packet from a tagged address has a sender");
            assert_eq!(arrived, Some(y_places[sender].rank));
            let back = paired_peer_addr(&x, &x_tags, sender, Some(y_places[sender]));
            assert_eq!(back, source);
        }
    }

    #[test]
    fn an_interface_with_too_few_send_sockets_acknowledges_on_another_pair() {
        // The peer has three interfaces with two addresses each. The endpoint's first interface,
        // which pairs with the peer's first and third, holds one send socket, which sends to the
        // peer's first interface, so no sender sends to the peer's third.
        let (peer, tags) = tagged(2, 3, 2);
        let two = |rank, index| SendPlace {
            rank,
            count: 2,
            index,
        };
        let places = [two(0, 0), two(1, 0), two(1, 1)];
        for (sender, &at) in [0, 2, 3].iter().enumerate() {
            assert_eq!(
                paired_peer_addr(&peer, &tags, sender, Some(places[sender])),
                peer[at]
            );
        }

        // The acknowledgements of packets from the peer's third interface arrive at the
        // endpoint's first interface, whose sender doesn't send back to it, and leave on that
        // sender. Those of packets from the peer's first interface leave on the same sender,
        // which sends to it.
        for &source in &peer[4..] {
            for arrived in [Some(0), None] {
                assert_eq!(
                    paired_ack_sender(&peer, &tags, &places, source.into(), 7, arrived),
                    Some(0)
                );
            }
        }
        for &source in &peer[..2] {
            assert_eq!(
                paired_ack_sender(&peer, &tags, &places, source.into(), 7, None),
                Some(0)
            );
        }

        // With a second send socket, the endpoint's first interface sends to the peer's third
        // interface too, and acknowledges its packets on that socket.
        let places = [two(0, 0), two(1, 0), two(1, 1), two(0, 1)];
        for (sender, &at) in [0, 2, 3, 4].iter().enumerate() {
            assert_eq!(
                paired_peer_addr(&peer, &tags, sender, Some(places[sender])),
                peer[at]
            );
        }
        for &source in &peer[4..] {
            for arrived in [Some(0), None] {
                assert_eq!(
                    paired_ack_sender(&peer, &tags, &places, source.into(), 7, arrived),
                    Some(3)
                );
            }
        }
    }

    #[test]
    fn the_arrival_interface_is_inferred_only_where_the_peer_interface_sends_to_one() {
        // Host X has three interfaces and host Y two, with two send sockets on each interface,
        // and each data address is the source of the send socket at the same position.
        let (x, x_tags) = tagged(1, 3, 2);
        let x_places = in_runs(3, 2);
        let (y, y_tags) = tagged(2, 2, 2);
        let y_places = in_runs(2, 2);

        // Y's first interface pairs with X's first and third and its sockets send to them in
        // turn, and Y's second sends to X's second alone.
        let y_sends_to = [0, 4, 2, 3];
        for (sender, &at) in y_sends_to.iter().enumerate() {
            assert_eq!(
                paired_peer_addr(&x, &x_tags, sender, Some(y_places[sender])),
                x[at]
            );
        }

        // X acknowledges a packet from each of Y's addresses on a sender of the interface the
        // packet arrived at, which sends back to the packet's source.
        for (at, &source) in y.iter().enumerate() {
            let arrived = tag_of(&x, &x_tags, x[y_sends_to[at]]).map(usize::from);
            let sender = paired_ack_sender(&y, &y_tags, &x_places, source.into(), 7, arrived)
                .expect("a packet from a tagged address has a sender");
            assert_eq!(arrived, Some(x_places[sender].rank));
            let back = paired_peer_addr(&y, &y_tags, sender, Some(x_places[sender]));
            assert_eq!(back, source);

            // Where the arrival isn't recorded, the sender is inferred for a packet from Y's
            // second interface, which sends to X's second alone, and not for one from Y's first,
            // which sends to X's first or third.
            let inferred = paired_ack_sender(&y, &y_tags, &x_places, source.into(), 7, None);
            assert_eq!(inferred, (y_tags[at] == Some(1)).then_some(sender));
        }

        // A packet from Y's first interface leaves on the interface it arrived at, either of
        // the two its interface sends to, where the receive path records it.
        for (arrived, senders) in [(0, [0, 1]), (2, [4, 5])] {
            for (at, &source) in y[..2].iter().enumerate() {
                assert_eq!(
                    paired_ack_sender(&y, &y_tags, &x_places, source.into(), 7, Some(arrived)),
                    Some(senders[at])
                );
            }
        }
    }
}
