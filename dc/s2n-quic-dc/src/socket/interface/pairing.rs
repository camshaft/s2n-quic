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

use super::{Id, Table};
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
        assert_eq!(paired_peer_addr(&peer, &tags, 7, Some(place(2, 0))), peer[0]);
        // An address without a tag among tagged ones isn't paired with any interface.
        let gap = [Some(1), None, Some(2), Some(2)];
        for index in 0..3 {
            assert_eq!(paired_peer_addr(&peer, &gap, 7, Some(two(0, index))), peer[0]);
        }
    }

    #[test]
    fn a_sender_keeps_its_address_where_the_interfaces_dont_pair() {
        let peer = addrs(4);
        let tags = [Some(1), Some(1), Some(2), Some(2)];
        let unpaired = peer[5 % 4];

        // The peer doesn't tag its addresses.
        assert_eq!(paired_peer_addr(&peer, &[], 5, Some(place(1, 0))), unpaired);
        assert_eq!(paired_peer_addr(&peer, &[None; 4], 5, Some(place(1, 0))), unpaired);
        // The tags don't match the addresses.
        assert_eq!(paired_peer_addr(&peer, &tags[..3], 5, Some(place(1, 0))), unpaired);
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
        assert_eq!(paired_peer_addr(&peer, &[Some(1); 4], 5, Some(place(1, 0))), unpaired);
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
}
