// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{event, path::secret};

pub fn new(capacity: usize) -> secret::Map {
    new_with_evict_on_unknown_path_secret(capacity, false)
}

/// Builds a map as [`new`] does, which evicts a path secret on a validated unknown path secret
/// packet where `evict` is true.
pub fn new_with_evict_on_unknown_path_secret(capacity: usize, evict: bool) -> secret::Map {
    crate::testing::init_tracing();

    let subscriber = event::tracing::Subscriber::default();

    let signer = secret::stateless_reset::Signer::random();

    if s2n_quic_platform::io::testing::is_in_env() {
        secret::Map::builder()
            .with_signer(signer)
            .with_capacity(capacity)
            .with_evict_on_unknown_path_secret(evict)
            .with_clock(crate::time::bach::Clock::default())
            .with_subscriber(subscriber)
            .build()
            .unwrap()
    } else {
        secret::Map::builder()
            .with_signer(signer)
            .with_capacity(capacity)
            .with_evict_on_unknown_path_secret(evict)
            .with_clock(s2n_quic_core::time::StdClock::default())
            .with_subscriber(subscriber)
            .build()
            .unwrap()
    }
}
