// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Provides an [`io::Provider`](s2n_quic_core::io::Provider) that uses
//! [`bach::net::UdpSocket`] so that [`bach::net::monitor`] can intercept
//! packets from regular s2n-quic endpoints inside a `sim()` environment.

use crate::{
    io::testing::time::Clock,
    message::{simple::Message, Message as _},
    socket::{
        io::{rx, tx},
        ring::{self, Consumer, Producer},
        stats,
    },
};
use bach::net::UdpSocket;
use futures::future::poll_fn;
use s2n_quic_core::{
    endpoint::Endpoint,
    inet::{self, SocketAddress},
    io::event_loop::{select::Select, EventLoop},
    path::{self, mtu},
};
use std::{io, net::SocketAddr};

pub type PathHandle = path::Tuple;

/// An IO provider that uses [`bach::net::UdpSocket`].
///
/// Using this provider (instead of the default tokio IO) causes all
/// transmitted and received packets to pass through the bach network
/// registry, making them visible to [`bach::net::monitor::on_packet_sent`].
/// This allows the same [`bach::net::monitor`] callbacks used in DC-QUIC
/// tests to also intercept regular s2n-quic traffic within a `sim()`
/// environment.
pub struct Io {
    addr: SocketAddr,
}

impl Io {
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr }
    }

    /// Starts the endpoint and returns its local address.
    ///
    /// Two async tasks are spawned into the current bach executor:
    /// - An IO bridge task that shuttles datagrams between the bach UDP
    ///   socket (visible to `bach::net::monitor`) and the ring buffers used
    ///   by the QUIC event loop.
    /// - The QUIC event loop task itself.
    pub fn start<E: Endpoint<PathHandle = PathHandle>>(
        self,
        mut endpoint: E,
    ) -> io::Result<SocketAddress> {
        let mut opts = bach::net::socket::Options::default();
        opts.local_addr = self.addr;
        let socket = UdpSocket::new(&opts)?;
        let local_addr: SocketAddr = socket.local_addr()?.into();
        let local_addr_quic: inet::SocketAddress = local_addr.into();

        let mtu_config = mtu::Config::builder().build().unwrap();
        endpoint.set_mtu_config(mtu_config);

        let payload_len: usize = mtu_config.max_mtu().into();
        let payload_len = payload_len as u32;
        let entries = 1024;

        let (rx_ep, rx_producer) = {
            let (producer, consumer) = ring::pair(entries, payload_len);
            let rx = rx::Rx::new(vec![consumer], mtu_config.max_mtu(), local_addr_quic.into());
            (rx, producer)
        };

        let (tx_ep, tx_consumer) = {
            let (producer, consumer) = ring::pair(entries, payload_len);
            let gso = crate::features::Gso::default();
            // bach's virtual network does not support GSO
            gso.disable();
            let tx = tx::Tx::new(vec![producer], gso, mtu_config.max_mtu());
            (tx, consumer)
        };

        let (stats_sender, stats_recv) = stats::channel();

        bach::task::spawn(run_io(socket, rx_producer, tx_consumer, stats_sender));

        let clock = Clock::default();
        let event_loop = EventLoop {
            endpoint,
            clock,
            tx: tx_ep,
            rx: rx_ep,
            cooldown: Default::default(),
            stats: stats_recv,
        };
        bach::task::spawn(event_loop.start(local_addr_quic));

        Ok(local_addr_quic)
    }
}

/// Bridges the bach UDP socket with the ring buffers consumed by the QUIC
/// event loop.  Mirrors the approach used in the turmoil IO provider.
async fn run_io(
    socket: UdpSocket,
    mut rx_producer: Producer<Message>,
    mut tx_consumer: Consumer<Message>,
    stats: stats::Sender,
) {
    let mut poll_producer = false;

    loop {
        let socket_ready = socket.readable();
        let consumer_ready = poll_fn(|cx| tx_consumer.poll_acquire(u32::MAX, cx));
        let producer_ready = async {
            if poll_producer {
                poll_fn(|cx| rx_producer.poll_acquire(u32::MAX, cx)).await
            } else {
                core::future::pending().await
            }
        };
        let application_wakeup = core::future::pending();

        // Reuse Select from the event_loop module to wait for any of:
        // - data available to send (consumer_ready)
        // - rx ring has capacity (producer_ready, when needed)
        // - socket is readable (socket_ready, mapped to timeout_expired)
        let is_readable = Select::new(consumer_ready, producer_ready, application_wakeup, socket_ready)
            .await
            .unwrap()
            .timeout_expired;

        if is_readable {
            let mut count = 0;
            for entry in rx_producer.data() {
                let res = socket.try_recv_from(entry.payload_mut());
                stats.recv().on_operation_result(&res, |_| 1);
                if let Ok((len, addr)) = res {
                    let addr: SocketAddr = addr;
                    entry.set_remote_address(&addr.into());
                    unsafe {
                        entry.set_payload_len(len);
                    }
                    count += 1;
                } else {
                    break;
                }
            }
            rx_producer.release(count);
            poll_producer = rx_producer.data().is_empty();
        }

        {
            let mut count = 0;
            for entry in tx_consumer.data() {
                let addr: SocketAddr = (*entry.remote_address()).into();
                let payload = entry.payload_mut();
                let res = socket.try_send_to(payload, addr);
                stats.send().on_operation_result(&res, |_| 1);
                if res.is_ok() {
                    count += 1;
                } else {
                    break;
                }
            }
            tx_consumer.release(count);
        }

        if !(rx_producer.is_open() && tx_consumer.is_open()) {
            return;
        }
    }
}
