// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Provides an implementation of the [`io::Provider`](crate::provider::io::Provider) backed by
//! [`bach::net`], the same simulated network the s2n-quic-dc data plane uses in its deterministic
//! tests.
//!
//! Unlike the [`testing`](crate::io::testing) provider — which routes packets through its own
//! in-memory [`network`](crate::io::testing::network) abstraction — this provider binds real
//! [`bach::net::UdpSocket`]s. That lets a full QUIC/TLS handshake share the same simulated
//! addresses, latency, and packet monitors as the dc data plane running in the same `bach`
//! simulation.

use crate::{
    io::testing::time::Clock,
    message::{simple::Message, Message as _},
    socket::{
        io::{rx, tx},
        ring::{self, Consumer, Producer},
        stats,
    },
};
use bach::net::{
    socket::{Options, RecvOptions},
    SocketAddr, UdpSocket,
};
use core::future::poll_fn;
use s2n_quic_core::{
    endpoint::Endpoint,
    inet::{self, SocketAddress},
    io::event_loop::EventLoop,
    path::{self, mtu},
};
use std::{io, io::ErrorKind, sync::Arc};

mod builder;

pub use builder::Builder;
pub type PathHandle = path::Tuple;

#[derive(Default)]
pub struct Io {
    builder: Builder,
}

impl Io {
    pub fn builder() -> Builder {
        Builder::default()
    }

    pub fn new(addr: SocketAddr) -> io::Result<Self> {
        let builder = Builder::default().with_address(addr)?;
        Ok(Self { builder })
    }

    /// Binds the `bach::net` socket, wires up the RX/TX rings, and spawns the socket and event-loop
    /// tasks into the current `bach` scope.
    ///
    /// Unlike the turmoil provider, the `bach::net` socket binds synchronously, so the *real*
    /// local address is available immediately and returned here (rather than a placeholder). The dc
    /// handshake relies on this address for its data-address exchange.
    ///
    /// NOTE: this must be called from within a `bach` group task so that `UdpSocket::new` resolves
    /// the calling group and `bach::spawn` inherits it.
    pub fn start<E: Endpoint<PathHandle = PathHandle>>(
        self,
        mut endpoint: E,
    ) -> io::Result<(bach::task::JoinHandle<()>, SocketAddress)> {
        let Builder {
            socket,
            addr,
            mtu_config_builder,
        } = self.builder;

        let mtu_config = mtu_config_builder
            .build()
            .map_err(|err| io::Error::new(ErrorKind::InvalidInput, format!("{err}")))?;

        endpoint.set_mtu_config(mtu_config);

        let socket = if let Some(socket) = socket {
            socket
        } else if let Some(addr) = addr {
            let mut opts = Options::default();
            opts.local_addr = addr;
            UdpSocket::new(&opts)?
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing bind address",
            ));
        };

        let local_addr = socket.local_addr()?;
        let local_addr: inet::SocketAddress = local_addr.into();
        let payload_len: u32 = u32::from(u16::from(mtu_config.max_mtu()));

        // This number matches the turmoil provider: a decent number of in-flight messages without
        // consuming an unreasonable amount of memory.
        let entries = 1024;

        let (rx, rx_producer) = {
            let (producer, consumer) = ring::pair(entries, payload_len);
            let rx = rx::Rx::new(vec![consumer], mtu_config.max_mtu(), local_addr.into());
            (rx, producer)
        };

        let (tx, tx_consumer) = {
            let (producer, consumer) = ring::pair(entries, payload_len);

            let gso = crate::features::Gso::default();
            // bach::net does model GSO (via SendOptions::segment_len) and GRO (RecvOptions::gro),
            // but this provider exists to exercise the QUIC/TLS handshake under bach, not to
            // benchmark segmentation. The `simple::Message` type used here doesn't carry segment
            // info, so GSO is disabled and each datagram is sent individually. Wiring up GSO/GRO
            // would require a segment-aware message type and is left as future work.
            gso.disable();

            let tx = tx::Tx::new(vec![producer], gso, mtu_config.max_mtu());
            (tx, consumer)
        };

        let (stats_sender, stats_recv) = stats::channel();

        // Spawn tasks that do the actual socket calls and coordinate with the event loop through
        // the ring buffers. Unlike the turmoil provider, the `bach::net` socket does not implement
        // `poll_readable`/`poll_writable`, so readiness must be driven directly off `poll_recv_msg`
        // / `poll_send_msg` (which register wakers). Splitting RX and TX into separate tasks lets
        // each one park on its own ring + socket future without busy-looping.
        let socket = Arc::new(socket);
        bach::spawn(rx_io(socket.clone(), rx_producer, stats_sender.clone()));
        bach::spawn(tx_io(socket, tx_consumer, stats_sender));

        let event_loop = EventLoop {
            clock: Clock::default(),
            rx,
            tx,
            endpoint,
            cooldown: Default::default(),
            stats: stats_recv,
        }
        .start(local_addr);

        let task = bach::spawn(event_loop);

        Ok((task, local_addr))
    }
}

/// Receives datagrams from the `bach::net` socket into the RX ring for the event loop to consume.
///
/// Each iteration waits for ring capacity, then parks on `poll_recv_msg` (which registers a waker)
/// until at least one datagram arrives, draining any additional immediately-ready datagrams into
/// the remaining free entries before releasing them to the consumer.
async fn rx_io(
    socket: Arc<UdpSocket>,
    mut producer: Producer<Message>,
    stats: stats::Sender,
) -> io::Result<()> {
    use std::io::IoSliceMut;

    loop {
        // Wait for at least one free entry in the ring.
        poll_fn(|cx| producer.poll_acquire(u32::MAX, cx)).await;
        if !producer.is_open() {
            return Ok(());
        }

        // Fill as many entries as we have ready datagrams for, parking until the first one arrives.
        let count = {
            let data = producer.data();
            let mut count = 0usize;
            poll_fn(|cx| {
                while count < data.len() {
                    let entry = &mut data[count];
                    let mut bufs = [IoSliceMut::new(entry.payload_mut())];
                    match socket.poll_recv_msg(cx, &mut bufs, RecvOptions::default()) {
                        core::task::Poll::Ready(res) => {
                            stats.recv().on_operation_result(&res, |_len| 1);
                            match res {
                                Ok(res) => {
                                    entry.set_remote_address(&res.peer_addr.into());
                                    unsafe {
                                        entry.set_payload_len(res.len);
                                    }
                                    count += 1;
                                }
                                // A stateless UDP socket shouldn't surface non-WouldBlock errors;
                                // stop draining and release what we have.
                                Err(_) => break,
                            }
                        }
                        core::task::Poll::Pending => {
                            // Park only if we haven't received anything yet this iteration;
                            // otherwise release what we have and loop.
                            if count == 0 {
                                return core::task::Poll::Pending;
                            }
                            break;
                        }
                    }
                }
                core::task::Poll::Ready(())
            })
            .await;
            count as u32
        };

        producer.release(count);
    }
}

/// Sends datagrams queued by the event loop on the TX ring out through the `bach::net` socket.
///
/// Each iteration waits for queued messages, then parks on `poll_send_msg` (which registers a waker
/// when the simulated tx queue is full) until they are accepted, before releasing ring capacity.
async fn tx_io(
    socket: Arc<UdpSocket>,
    mut consumer: Consumer<Message>,
    stats: stats::Sender,
) -> io::Result<()> {
    use bach::net::socket::SendOptions;
    use std::io::IoSlice;

    loop {
        // Wait for at least one message to send.
        poll_fn(|cx| consumer.poll_acquire(u32::MAX, cx)).await;
        if !consumer.is_open() {
            return Ok(());
        }

        let count = {
            let data = consumer.data();
            let mut count = 0usize;
            poll_fn(|cx| {
                while count < data.len() {
                    let entry = &mut data[count];
                    let addr: std::net::SocketAddr = (*entry.remote_address()).into();
                    let bufs = [IoSlice::new(entry.payload_mut())];
                    match socket.poll_send_msg(cx, addr, &bufs, SendOptions::default()) {
                        core::task::Poll::Ready(res) => {
                            stats.send().on_operation_result(&res, |_len| 1);
                            if res.is_err() {
                                break;
                            }
                            count += 1;
                        }
                        core::task::Poll::Pending => {
                            // The simulated tx queue is full; park (the waker is registered).
                            if count == 0 {
                                return core::task::Poll::Pending;
                            }
                            break;
                        }
                    }
                }
                core::task::Poll::Ready(())
            })
            .await;
            count as u32
        };

        consumer.release(count);
    }
}
