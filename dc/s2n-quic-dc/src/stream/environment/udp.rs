// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    packet::Packet,
    path::secret::Map,
    psk::io::DEFAULT_MTU,
    socket::pool::{self, descriptor},
    socket::send::wheel::DEFAULT_GRANULARITY_US,
    stream::{
        environment::{Environment, Peer, SetupResult, SocketSet},
        recv::{
            buffer,
            dispatch::{Control, Stream},
            shared::RecvBuffer,
        },
        server::accept,
        socket, TransportFeatures,
    },
    sync::mpsc::{self, Capacity},
};
use s2n_codec::{DecoderBufferMut, DecoderParameterizedValueMut};
use s2n_quic_core::{
    inet::{IpAddress, IpV4Address, IpV6Address, SocketAddress, Unspecified},
    recovery::MAX_BURST_PACKETS,
    time::token_bucket::TokenBucket,
};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Config {
    pub blocking: bool,
    pub reuse_port: bool,
    pub stream_recv_queue: Capacity,
    pub control_recv_queue: Capacity,
    pub max_packet_size: u16,
    pub packet_count: usize,
    pub accept_flavor: accept::Flavor,
    pub workers: Option<usize>,
    pub map: Map,
    // Send worker configuration
    pub send_wheel_horizon: Duration,
    pub max_bitrate_bps: u64,
    pub priority_levels: usize,
}

impl Config {
    pub fn new(map: Map) -> Self {
        Self {
            blocking: false,
            reuse_port: false,
            // TODO tune these defaults
            stream_recv_queue: Capacity {
                max: 4096,
                initial: 256,
            },

            // set the control queue depth shallow, since we really only need the most recent ones
            control_recv_queue: Capacity { max: 8, initial: 8 },

            // Allocate 1MB at a time
            max_packet_size: u16::MAX,
            packet_count: 16,

            accept_flavor: accept::Flavor::default(),

            workers: None,
            map,

            // Send worker defaults
            send_wheel_horizon: Duration::from_millis(100),
            max_bitrate_bps: 5_000_000_000, // 5 Gbps (EC2 per-flow limit)
            priority_levels: 2,             // 0 = control, 1+ = application
        }
    }

    pub(crate) fn token_bucket(&self) -> TokenBucket {
        // Convert bits per second to bytes per interval
        // bytes_per_interval = (bits_per_second * interval_microseconds) / 8_000_000
        let bytes_per_interval = (self.max_bitrate_bps * DEFAULT_GRANULARITY_US) / 8_000_000;
        // make sure at least one burst goes through each iteration
        let bytes_per_interval =
            bytes_per_interval.max(DEFAULT_MTU as u64 * MAX_BURST_PACKETS as u64);
        let refill_interval = Duration::from_micros(DEFAULT_GRANULARITY_US);
        let burst = bytes_per_interval; // 1x multiplier

        TokenBucket::builder()
            .with_max(burst)
            .with_refill_interval(refill_interval)
            .with_refill_amount(bytes_per_interval)
            .build()
    }

    pub fn unroutable_packets<S>(
        &self,
        socket: S,
    ) -> (
        mpsc::Sender<descriptor::Filled>,
        impl core::future::Future<Output = ()> + Send + Sync + 'static,
    )
    where
        S: Send + Sync + 'static + crate::stream::socket::Socket,
    {
        let (tx, rx) = mpsc::new::<descriptor::Filled>(4096);
        let map = self.map.clone();
        let task = async move {
            let mut out_buffer = [0u8; 1500];

            while let Ok(mut descriptor) = rx.recv_front().await {
                let peer = descriptor.remote_address().get().into();
                let buffer = DecoderBufferMut::new(descriptor.payload_mut());
                let Ok((packet, _)) = Packet::decode_parameterized_mut(16, buffer) else {
                    continue;
                };
                let params = match packet {
                    Packet::Stream(packet) => {
                        let credentials = *packet.credentials();
                        let stream_id = *packet.stream_id();
                        Some((credentials, stream_id.queue_id))
                    }
                    Packet::Datagram(packet) => {
                        // datagrams are not routable
                        let _ = packet;
                        None
                    }
                    Packet::Control(packet) => {
                        let credentials = *packet.credentials();
                        let stream_id = packet.stream_id();
                        stream_id.map(|stream_id| (credentials, stream_id.queue_id))
                    }
                    Packet::FlowReset(packet) => {
                        // Don't reply to flow reset packets to avoid looping
                        let _ = packet;
                        None
                    }
                    Packet::StaleKey(packet) => {
                        let _ = map.handle_stale_key_packet(&packet, &peer);
                        None
                    }
                    Packet::ReplayDetected(packet) => {
                        let _ = map.handle_replay_detected_packet(&packet, &peer);
                        None
                    }
                    Packet::UnknownPathSecret(packet) => {
                        let _ = map.handle_unknown_path_secret_packet(&packet, &peer);
                        None
                    }
                };

                let Some((credentials, queue_id)) = params else {
                    continue;
                };

                let packet = crate::packet::secret_control::FlowReset {
                    credentials,
                    wire_version: crate::packet::WireVersion::ZERO,
                    queue_id,
                    code: crate::stream::shared::ShutdownKind::ERRORED_CODE.into(),
                };

                let Some(len) = map.sign_flow_reset_packet(&packet, &mut out_buffer) else {
                    continue;
                };

                let remote_addr = descriptor.remote_address();
                let ecn = Default::default();
                let buffer = &out_buffer[..len];
                let buffer = &[std::io::IoSlice::new(buffer)];
                let _ = socket.try_send(remote_addr, ecn, buffer);
            }
        };
        (tx, task)
    }
}

pub struct Pooled<S: socket::application::Application, W: socket::Socket> {
    pub peer_addr: SocketAddress,
    pub control: Control,
    pub stream: Stream,
    pub application_socket: Arc<S>,
    pub worker_socket: Arc<W>,
    #[allow(dead_code)]
    pub(crate) pool: Arc<pool::Pool>,
}

impl<S: socket::application::Application, W: socket::Socket> std::fmt::Debug for Pooled<S, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pooled")
            .field("peer_addr", &self.peer_addr)
            .field("control", &self.control)
            .field("stream", &self.stream)
            .field("application_socket", &"<socket>")
            .field("worker_socket", &"<socket>")
            .field("pool", &"<pool>")
            .finish()
    }
}

impl<E, S, W> Peer<E> for Pooled<S, W>
where
    E: Environment,
    S: socket::application::Application + 'static,
    W: socket::Socket + 'static,
{
    type ReadWorkerSocket = Arc<W>;
    type WriteWorkerSocket = (Arc<W>, buffer::Channel<Control>);

    #[inline]
    fn features(&self) -> TransportFeatures {
        TransportFeatures::UDP
    }

    #[inline]
    fn setup(self, _env: &E) -> SetupResult<Self::ReadWorkerSocket, Self::WriteWorkerSocket> {
        let mut remote_addr = self.peer_addr;
        let control = self.control;
        let stream = self.stream;
        let queue_id = control.queue_id();

        let local_addr: SocketAddress = self.worker_socket.local_addr()?.into();
        let application = Box::new(self.application_socket);
        let read_worker = Some(self.worker_socket.clone());
        let write_worker = Some((self.worker_socket, buffer::Channel::new(control)));

        #[inline]
        fn ipv6_loopback() -> IpV6Address {
            std::net::Ipv6Addr::LOCALHOST.into()
        }

        match (remote_addr.ip(), local_addr.ip()) {
            (IpAddress::Ipv4(v4), IpAddress::Ipv4(_)) if v4.is_unspecified() => {
                // if remote addr is unspecified then it needs to be localhost instead
                remote_addr = IpV4Address::new([127, 0, 0, 1])
                    .with_port(remote_addr.port())
                    .into();
            }
            (IpAddress::Ipv4(v4), IpAddress::Ipv6(_)) if v4.is_unspecified() => {
                // if v4 is unspecified then use v6 loopback
                remote_addr = ipv6_loopback().with_port(remote_addr.port()).into();
            }
            (IpAddress::Ipv6(v6), IpAddress::Ipv6(_)) if v6.is_unspecified() => {
                // if v6 is unspecified then use v6 loopback
                remote_addr = ipv6_loopback().with_port(remote_addr.port()).into();
            }
            (IpAddress::Ipv4(_), IpAddress::Ipv4(_)) => {}
            (IpAddress::Ipv4(v4), IpAddress::Ipv6(_)) => {
                // use an IPv6-mapped addr if we're listening on a V6 socket
                remote_addr = v4.to_ipv6_mapped().with_port(remote_addr.port()).into();
            }
            (IpAddress::Ipv6(_), IpAddress::Ipv4(_)) => {
                return Err(std::io::Error::other("IPv6 not supported on a IPv4 socket"))
            }
            (IpAddress::Ipv6(_), IpAddress::Ipv6(_)) => {}
        }

        let socket = SocketSet {
            application,
            read_worker,
            write_worker,
            remote_addr,
            source_queue_id: Some(queue_id),
        };

        let recv_buffer = RecvBuffer::B(buffer::Channel::new(stream));

        Ok((socket, recv_buffer))
    }
}
