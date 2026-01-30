// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[event("endpoint:initialized")]
#[subject(endpoint)]
struct EndpointInitialized<'a> {
    #[nominal_counter("acceptor.protocol")]
    acceptor_addr: SocketAddress<'a>,
    #[nominal_counter("handshake.protocol")]
    handshake_addr: SocketAddress<'a>,
    #[bool_counter("tcp")]
    tcp: bool,
    #[bool_counter("udp")]
    udp: bool,
}

#[event("endpoint:udp:immediate_transmission_scheduled")]
#[subject(endpoint)]
/// Called when a transmission is scheduled for immediate transmission
struct EndpointUdpImmediateTransmissionScheduled<'a> {
    #[nominal_counter("peer.protocol")]
    peer_address: SocketAddress<'a>,

    #[measure("buffer_size", Bytes)]
    buffer_size: u16,

    #[measure("segment_size", Bytes)]
    segment_size: u16,

    #[measure("segment_count")]
    segment_count: u16,
}

#[event("endpoint:udp:transmission_scheduled")]
#[subject(endpoint)]
/// Called when a transmission is scheduled in the future
struct EndpointUdpTransmissionScheduled<'a> {
    #[nominal_counter("peer.protocol")]
    peer_address: SocketAddress<'a>,

    #[measure("buffer_size", Bytes)]
    buffer_size: u16,

    #[measure("segment_size", Bytes)]
    segment_size: u16,

    #[measure("segment_count")]
    segment_count: u16,

    #[measure("delay", Duration)]
    delay: core::time::Duration,
}

#[event("endpoint:udp:transmission_rejected")]
#[subject(endpoint)]
/// Called when a transmission is rejected
struct EndpointUdpTransmissionRejected<'a> {
    #[nominal_counter("peer.protocol")]
    peer_address: SocketAddress<'a>,

    #[measure("buffer_size", Bytes)]
    buffer_size: u16,

    #[measure("segment_size", Bytes)]
    segment_size: u16,

    #[measure("segment_count")]
    segment_count: u16,

    #[measure("delay", Duration)]
    delay: core::time::Duration,

    #[measure("backoff", Duration)]
    backoff: core::time::Duration,
}

#[event("endpoint:udp:packet_transmitted")]
#[subject(endpoint)]
struct EndpointUdpPacketTransmitted<'a> {
    #[nominal_counter("peer.protocol")]
    peer_address: SocketAddress<'a>,

    #[measure("buffer_size", Bytes)]
    buffer_size: u16,

    #[measure("segment_size", Bytes)]
    segment_size: u16,

    #[measure("segment_count")]
    segment_count: u16,
}

#[event("endpoint:udp:transmit_errored")]
#[subject(endpoint)]
struct EndpointUdpTransmitErrored<'a> {
    #[nominal_counter("peer.protocol")]
    peer_address: SocketAddress<'a>,

    #[measure("buffer_size", Bytes)]
    buffer_size: u16,

    #[measure("segment_size", Bytes)]
    segment_size: u16,

    #[measure("segment_count")]
    segment_count: u16,

    error: &'a std::io::Error,
}

#[event("endpoint:udp:packet_received")]
#[subject(endpoint)]
struct EndpointUdpPacketReceived<'a> {
    #[nominal_counter("peer.protocol")]
    peer_address: SocketAddress<'a>,

    #[measure("buffer_size", Bytes)]
    buffer_size: u16,

    #[measure("segment_size", Bytes)]
    segment_size: u16,

    #[measure("segment_count")]
    segment_count: u16,
}

#[event("endpoint:udp:receive_errored")]
#[subject(endpoint)]
struct EndpointUdpReceiveErrored<'a> {
    error: &'a std::io::Error,
}
