// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[event("handshake_queue:start_limit_increased")]
#[subject(endpoint)]
/// Emitted when the handshake queue's start limit, which bounds the handshakes it starts at
/// once, grows after a clean answer
struct HandshakeStartLimitIncreased {
    /// The limit's size after the increase
    #[measure("size")]
    size: usize,
}

#[event("handshake_queue:start_limit_decreased")]
#[subject(endpoint)]
/// Emitted when the handshake queue's start limit shrinks after an answer that signals
/// congestion
struct HandshakeStartLimitDecreased {
    /// The limit's size after the decrease
    #[measure("size")]
    size: usize,
}

#[event("handshake_queue:inflight_limit_increased")]
#[subject(endpoint)]
/// Emitted when the handshake queue's inflight limit, which bounds the connections it keeps
/// open at once, grows after a clean answer
struct HandshakeInflightLimitIncreased {
    /// The limit's size after the increase
    #[measure("size")]
    size: usize,
}

#[event("handshake_queue:inflight_limit_decreased")]
#[subject(endpoint)]
/// Emitted when the handshake queue's inflight limit shrinks after an answer that signals
/// congestion
struct HandshakeInflightLimitDecreased {
    /// The limit's size after the decrease
    #[measure("size")]
    size: usize,
}
