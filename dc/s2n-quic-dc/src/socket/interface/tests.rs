// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
};

const OVERALL: Rate = Rate::new(10.0);

/// An operating system that reports the link speeds and the addresses that a test sets, and
/// records what the endpoint reads from it.
#[derive(Default)]
struct TestOs {
    speeds: HashMap<String, LinkSpeed>,
    /// The local addresses, or `None` for an operating system whose read of them fails.
    addresses: Option<Vec<(String, IpAddr)>>,
    speed_reads: RefCell<Vec<String>>,
    address_reads: Cell<usize>,
}

impl TestOs {
    fn with_speed(mut self, name: &str, mbps: u64) -> Self {
        self.speeds.insert(name.to_owned(), speed(mbps));
        self
    }

    fn with_address(mut self, name: &str, ip: &str) -> Self {
        let addresses = self.addresses.get_or_insert_with(Vec::new);
        addresses.push((name.to_owned(), ip.parse().unwrap()));
        self
    }
}

impl Os for TestOs {
    fn link_speed(&self, name: &str) -> Option<LinkSpeed> {
        self.speed_reads.borrow_mut().push(name.to_owned());
        self.speeds.get(name).copied()
    }

    fn addresses(&self) -> io::Result<Vec<(String, IpAddr)>> {
        self.address_reads.set(self.address_reads.get() + 1);
        match &self.addresses {
            Some(addresses) => Ok(addresses.clone()),
            None => Err(io::ErrorKind::Unsupported.into()),
        }
    }
}

fn speed(mbps: u64) -> LinkSpeed {
    LinkSpeed::from_megabits_per_second(mbps).unwrap()
}

fn addrs(addrs: &[&str]) -> Vec<SocketAddr> {
    addrs.iter().map(|addr| addr.parse().unwrap()).collect()
}

fn named(name: &str) -> Config {
    Config {
        name: Some(name.to_owned()),
        ..Default::default()
    }
}

fn interface(id: u16, name: Option<&str>, mbps: Option<u64>, send_rate: Rate) -> Interface {
    Interface {
        id: Id(id),
        name: name.map(str::to_owned),
        link_speed: mbps.map(speed),
        send_rate,
    }
}

#[test]
fn no_interfaces_put_every_socket_on_the_default_interface() {
    let os = TestOs::default()
        .with_speed("eth0", 1000)
        .with_address("eth0", "10.0.0.1");
    let send = addrs(&["10.0.0.1:1", "10.0.0.1:2", "0.0.0.0:3", "[::1]:4"]);
    let recv = addrs(&["10.0.0.1:5", "10.0.0.1:6"]);

    let table = Table::resolve(&[], OVERALL, &send, &recv, &os);

    assert_eq!(table.interfaces(), &[interface(0, None, None, OVERALL)]);
    assert_eq!(table.send_sockets(), &[Id::DEFAULT; 4]);
    assert_eq!(table.recv_sockets(), &[Id::DEFAULT; 2]);
    // A configuration without interfaces reads nothing from the operating system.
    assert!(os.speed_reads.borrow().is_empty());
    assert_eq!(os.address_reads.get(), 0);
}

#[test]
fn listed_sockets_stay_on_their_interfaces() {
    let os = TestOs::default();
    let configs = [
        Config {
            send_rate: Some(Rate::new(1.0)),
            send_sockets: vec![0, 2],
            recv_sockets: vec![1],
            ..Default::default()
        },
        Config {
            send_rate: Some(Rate::new(2.5)),
            send_sockets: vec![1],
            recv_sockets: vec![0],
            ..Default::default()
        },
    ];
    let send = addrs(&["10.0.0.1:1", "10.0.1.1:2", "10.0.0.1:3", "10.0.1.1:4"]);
    let recv = addrs(&["10.0.1.1:5", "10.0.0.1:6", "10.0.0.1:7"]);

    let table = Table::resolve(&configs, OVERALL, &send, &recv, &os);

    assert_eq!(
        table.interfaces(),
        &[
            interface(0, None, None, OVERALL),
            interface(1, None, None, Rate::new(1.0)),
            interface(2, None, None, Rate::new(2.5)),
        ]
    );
    assert_eq!(table.send_sockets(), &[Id(1), Id(2), Id(1), Id::DEFAULT]);
    assert_eq!(table.recv_sockets(), &[Id(2), Id(1), Id::DEFAULT]);
    assert_eq!(
        table.get(Id(2)),
        Some(&interface(2, None, None, Rate::new(2.5)))
    );
    assert_eq!(table.get(Id(3)), None);
    // An interface without a name isn't read from the operating system.
    assert!(os.speed_reads.borrow().is_empty());
    assert_eq!(os.address_reads.get(), 0);
}

#[test]
fn unlisted_sockets_are_placed_by_their_local_address() {
    let os = TestOs::default()
        .with_address("lo", "127.0.0.1")
        .with_address("eth1", "10.0.0.2")
        .with_address("eth2", "2001:db8::2");
    let mut eth2 = named("eth2");
    eth2.send_sockets = vec![3];
    let configs = [named("eth1"), eth2];
    let send = [
        // On eth1, by its address.
        "10.0.0.2:1",
        // Unspecified: on the default interface.
        "0.0.0.0:2",
        // On eth1, by its IPv4-mapped address.
        "[::ffff:10.0.0.2]:3",
        // Listed on eth2, though its address is on eth1.
        "10.0.0.2:4",
    ];
    let recv = [
        // On eth2, by its address.
        "[2001:db8::2]:5",
        // Unspecified: on the default interface.
        "[::]:6",
        // On lo, which the configuration doesn't list: on the default interface.
        "127.0.0.1:7",
        // On no interface: on the default interface.
        "10.0.0.9:8",
    ];

    let table = Table::resolve(&configs, OVERALL, &addrs(&send), &addrs(&recv), &os);

    assert_eq!(table.send_sockets(), &[Id(1), Id::DEFAULT, Id(1), Id(2)]);
    assert_eq!(
        table.recv_sockets(),
        &[Id(2), Id::DEFAULT, Id::DEFAULT, Id::DEFAULT]
    );
    // One read of the addresses places every socket.
    assert_eq!(os.address_reads.get(), 1);
}

#[test]
fn a_socket_goes_to_the_first_listed_interface_that_holds_its_address() {
    // The operating system puts 10.0.0.2 on lo first, which the configuration doesn't list, and
    // on eth1 and eth2 after it; it puts fe80::1 on eth2 first and on eth1 after it.
    let os = TestOs::default()
        .with_address("lo", "10.0.0.2")
        .with_address("eth1", "10.0.0.2")
        .with_address("eth2", "fe80::1")
        .with_address("eth1", "fe80::1")
        .with_address("eth2", "10.0.0.2");
    let configs = [named("eth1"), named("eth2"), named("eth1")];
    let send = addrs(&["10.0.0.2:1", "[fe80::1]:2"]);

    let table = Table::resolve(&configs, OVERALL, &send, &[], &os);

    // Each socket is on eth1, the first listed interface that holds its address, and not on
    // the interface the operating system lists first, nor on the second interface named eth1.
    assert_eq!(table.send_sockets(), &[Id(1), Id(1)]);
}

#[test]
fn addresses_are_read_only_where_they_can_place_a_socket() {
    // Each socket is listed or unspecified: the addresses aren't read.
    let os = TestOs::default();
    let mut eth1 = named("eth1");
    eth1.send_sockets = vec![0];
    let send = addrs(&["10.0.0.2:1", "0.0.0.0:2"]);
    let table = Table::resolve(&[eth1], OVERALL, &send, &[], &os);
    assert_eq!(table.send_sockets(), &[Id(1), Id::DEFAULT]);
    assert_eq!(os.address_reads.get(), 0);

    // No interface has a name to place a socket by: the addresses aren't read.
    let os = TestOs::default();
    let send = addrs(&["10.0.0.2:1"]);
    let table = Table::resolve(&[Config::default()], OVERALL, &send, &[], &os);
    assert_eq!(table.send_sockets(), &[Id::DEFAULT]);
    assert_eq!(os.address_reads.get(), 0);

    // The read of the addresses fails: the socket stays on the default interface.
    let os = TestOs::default();
    let table = Table::resolve(&[named("eth1")], OVERALL, &send, &[], &os);
    assert_eq!(table.send_sockets(), &[Id::DEFAULT]);
    assert_eq!(os.address_reads.get(), 1);
}

#[test]
fn send_rate_is_link_speed_then_configured_rate_then_overall_rate() {
    let os = TestOs::default().with_speed("eth1", 2500);
    let mut eth1 = named("eth1");
    eth1.send_rate = Some(Rate::new(1.0));
    let mut eth2 = named("eth2");
    eth2.send_rate = Some(Rate::new(1.0));
    let unnamed = Config {
        send_rate: Some(Rate::new(4.0)),
        ..Default::default()
    };
    let configs = [eth1, eth2, named("eth3"), unnamed];

    let table = Table::resolve(&configs, OVERALL, &[], &[], &os);

    assert_eq!(
        table.interfaces(),
        &[
            interface(0, None, None, OVERALL),
            // The reported link speed takes the place of the configured rate.
            interface(1, Some("eth1"), Some(2500), Rate::new(2.5)),
            // The link speed is unknown: the configured rate.
            interface(2, Some("eth2"), None, Rate::new(1.0)),
            // The link speed is unknown, and no rate is configured: the overall send rate.
            interface(3, Some("eth3"), None, OVERALL),
            // No name to read a link speed by: the configured rate.
            interface(4, None, None, Rate::new(4.0)),
        ]
    );
    assert_eq!(*os.speed_reads.borrow(), ["eth1", "eth2", "eth3"]);
}

#[test]
fn linux_speed_file_is_read_in_megabits_per_second() {
    let read = |text: &str| sysfs_speed(Ok(text.to_owned()));
    assert_eq!(read("1000\n"), Some(speed(1000)));
    assert_eq!(read("2500\n"), Some(speed(2500)));
    assert_eq!(read("2500\n").map(LinkSpeed::rate), Some(Rate::new(2.5)));
    // Linux holds -1 where it doesn't know the speed, such as for a link that is down.
    assert_eq!(read("-1\n"), None);
    assert_eq!(read(""), None);
    // A speed of 0 is unknown too.
    assert_eq!(read("0\n"), None);
    // Linux refuses the read for an interface that has no speed, such as loopback.
    assert_eq!(sysfs_speed(Err(io::ErrorKind::InvalidInput.into())), None);
    // An interface that doesn't exist has no file.
    assert_eq!(sysfs_speed(Err(io::ErrorKind::NotFound.into())), None);
}

#[test]
fn only_a_plain_name_is_read_from_sysfs() {
    assert!(is_plain_name("eth0"));
    assert!(is_plain_name("enp3s0f1"));
    for name in ["", ".", "..", "../eth0", "eth0/speed", "eth\0"] {
        assert!(!is_plain_name(name), "{name:?}");
    }
}

#[test]
fn macos_baudrate_is_read_in_bits_per_second() {
    assert_eq!(baudrate_speed(1_000_000_000), Some(speed(1000)));
    assert_eq!(baudrate_speed(0), None);
    // The field holds 32 bits, so `u32::MAX` may stand for a faster link: it reads as unknown.
    assert_eq!(baudrate_speed(u32::MAX), None);
}

/// Returns the name of the first interface that the operating system puts 127.0.0.1 on.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn loopback_name() -> String {
    let addresses = System.addresses().expect("read the local addresses");
    let mut names = interfaces_of(&addresses, std::net::Ipv4Addr::LOCALHOST.into());
    let name = names.next().expect("127.0.0.1 should be on an interface");
    name.to_owned()
}

#[test]
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn address_lookup_finds_the_interface_of_127_0_0_1() {
    // Linux names the loopback interface `lo`, and macOS names it `lo0`.
    let name = loopback_name();
    assert!(name.starts_with("lo"), "127.0.0.1 is on {name}");
}

#[test]
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn sockets_on_127_0_0_1_are_placed_on_the_loopback_interface() {
    let name = loopback_name();
    let send = addrs(&["127.0.0.1:1"]);
    let recv = addrs(&["127.0.0.1:2"]);

    let table = Table::resolve(&[named(&name)], OVERALL, &send, &recv, &System);

    assert_eq!(table.send_sockets(), &[Id(1)]);
    assert_eq!(table.recv_sockets(), &[Id(1)]);
    let loopback = &table.interfaces()[1];
    assert_eq!(loopback.name.as_deref(), Some(name.as_str()));
    // Linux refuses the read of a loopback interface's speed, and macOS may report one: the
    // probe answers either way, and the send rate follows from its answer.
    let expected = loopback.link_speed.map_or(OVERALL, LinkSpeed::rate);
    assert_eq!(loopback.send_rate, expected);
}

#[test]
#[should_panic(expected = "interfaces 1 and 2 both list send socket 0")]
fn a_socket_listed_on_two_interfaces_panics() {
    let listed = Config {
        send_sockets: vec![0],
        ..Default::default()
    };
    let configs = [listed.clone(), listed];
    let send = addrs(&["10.0.0.1:1"]);
    Table::resolve(&configs, OVERALL, &send, &[], &TestOs::default());
}

#[test]
#[should_panic(expected = "interface 1 lists recv socket 2 of 2 sockets")]
fn a_socket_past_the_end_of_its_list_panics() {
    let listed = Config {
        recv_sockets: vec![2],
        ..Default::default()
    };
    let recv = addrs(&["10.0.0.1:1", "10.0.0.1:2"]);
    Table::resolve(&[listed], OVERALL, &[], &recv, &TestOs::default());
}

/// Returns an endpoint configuration for the inspector runtime, with `interfaces`.
fn endpoint_config(interfaces: Vec<Config>) -> crate::endpoint::Config {
    use crate::{acceptor, endpoint, path::secret::map::testing, socket::pool::Pool};

    let mut ids = 1..;
    let layout = endpoint::WorkerLayout {
        frame_dispatch: 0,
        send: (&mut ids).take(2).collect(),
        recv_io: (&mut ids).take(1).collect(),
        recv_dispatch: (&mut ids).take(1).collect(),
        waker_drain: (&mut ids).take(1).collect(),
        background: ids.next().expect("background worker id should exist"),
    };

    endpoint::Config {
        counters: crate::counter::Registry::default(),
        layout,
        recv_backend: endpoint::RecvBackend::Syscall,
        send_pool: Pool::new(u16::MAX),
        recv_pool: Pool::new(u16::MAX),
        path_secret_map: testing::new(50_000),
        gso: endpoint::Gso::default(),
        acceptor_registry: acceptor::Registry::new(),
        overall_send_rate: OVERALL,
        interfaces,
        budgets: endpoint::Budgets::default(),
        submission_shards: 4,
        ups_rate: Rate::new(0.001),
        ups_dedup_capacity: 1024,
        ups_dedup_window: core::time::Duration::from_secs(1),
        dead_peer_cooldown: endpoint::DEFAULT_DEAD_PEER_COOLDOWN,
        initial_tx_descriptor_allocs: 0,
        initial_rx_descriptor_allocs: 0,
        send_credit_pool_config: crate::credit::Config::default(),
        recv_credit_pool_config: crate::credit::Config::default(),
    }
}

#[test]
fn an_endpoint_without_interfaces_puts_every_socket_on_the_default_interface() {
    let config = endpoint_config(Vec::new());
    let table = crate::runtime::inspector::endpoint_interfaces(config, 4, 2);

    assert_eq!(table.interfaces(), &[interface(0, None, None, OVERALL)]);
    assert_eq!(table.send_sockets(), &[Id::DEFAULT; 4]);
    assert_eq!(table.recv_sockets(), &[Id::DEFAULT; 2]);
}

#[test]
fn an_endpoint_keeps_its_configured_interfaces_and_socket_assignments() {
    let configs = vec![
        Config {
            send_rate: Some(Rate::new(1.0)),
            send_sockets: vec![0, 1],
            recv_sockets: vec![1],
            ..Default::default()
        },
        Config {
            send_rate: Some(Rate::new(2.5)),
            send_sockets: vec![3],
            recv_sockets: vec![0],
            ..Default::default()
        },
    ];
    let table = crate::runtime::inspector::endpoint_interfaces(endpoint_config(configs), 4, 2);

    assert_eq!(
        table.interfaces(),
        &[
            interface(0, None, None, OVERALL),
            interface(1, None, None, Rate::new(1.0)),
            interface(2, None, None, Rate::new(2.5)),
        ]
    );
    assert_eq!(table.send_sockets(), &[Id(1), Id(1), Id::DEFAULT, Id(2)]);
    assert_eq!(table.recv_sockets(), &[Id(2), Id(1)]);
}
