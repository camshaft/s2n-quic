// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

struct ConnectionMeta {
    id: u64,
    timestamp: Timestamp,
}

struct EndpointMeta {
    timestamp: Timestamp,
}

struct ConnectionInfo<'a> {
    /// The credential ID (path secret identifier) for this stream
    #[snapshot("[HIDDEN]")]
    credential_id: &'a [u8],

    /// The key ID (per-stream counter derived from the path secret)
    key_id: u64,

    /// The remote peer address
    #[builder(&'a s2n_quic_core::inet::SocketAddress)]
    remote_address: SocketAddress<'a>,

    /// Whether this is the client or server side of the stream
    is_server: bool,
}
