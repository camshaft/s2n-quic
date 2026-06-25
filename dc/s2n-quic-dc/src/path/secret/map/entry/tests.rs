// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::{
    endpoint::id::Id,
    packet::secret_control as control,
    path::secret::{receiver, schedule, sender},
};
use s2n_quic_core::time::Timestamp;
use std::time::Duration;

fn test_entry_with_senders(sender_count: usize) -> Entry {
    let peer = (std::net::Ipv4Addr::LOCALHOST, 4433).into();
    let secret = schedule::Secret::new(
        schedule::Ciphersuite::AES_GCM_128_SHA256,
        s2n_quic_core::dc::SUPPORTED_VERSIONS[0],
        s2n_quic_core::endpoint::Type::Client,
        &[7u8; 32],
    );

    Entry::new_with_socket_senders(
        peer,
        secret,
        sender::State::new([0; control::TAG_LEN]),
        receiver::State::new(),
        s2n_quic_core::dc::testing::TEST_APPLICATION_PARAMS,
        crate::time::DefaultClock::default().now().into(),
        None,
        Arc::from([peer]),
        sender_count,
    )
}

#[test]
fn entry_size() {
    let mut should_check = true;

    should_check &= cfg!(target_pointer_width = "64");
    should_check &= cfg!(target_os = "linux");
    should_check &= std::env::var("S2N_QUIC_RUN_VERSION_SPECIFIC_TESTS").is_ok();

    // This gates to running only on specific GHA to reduce false positives.
    if should_check {
        assert_eq!(
            Entry::builder((std::net::Ipv4Addr::LOCALHOST, 0).into())
                .build(None)
                .size(),
            // Includes per-entry sender scheduling storage metadata (Box<[AtomicU64]>) and the
            // default one-element peer_data_addrs list (one std SocketAddr, 32 bytes).
            337
        );
    }
}

/// A peer marked dead must claw back out of the cooldown as soon as we receive proof it is alive.
///
/// `dead_at` is otherwise only ever *set* (by [`Entry::mark_dead_if_cooldown_elapsed`]) and never
/// cleared, so a transient blip (e.g. a deploy restart) traps the peer pair in hard-refused flows
/// for the whole cooldown — and an idle probe re-arms the window, so it never recovers. An
/// authenticated, de-duped packet is proof of life, and that is exactly the path that calls
/// [`Entry::touch_activity`], so touching activity must clear the mark.
#[test]
fn touch_activity_clears_dead_cooldown() {
    let entry = test_entry_with_senders(0);
    let cooldown = crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN;

    let t0: crate::time::precision::Timestamp =
        (unsafe { Timestamp::from_duration(Duration::from_secs(100)) }).into();

    // Mark the peer dead (simulating the idle wheel observing unacked inflight).
    assert!(
        entry.mark_dead_if_cooldown_elapsed(t0, cooldown),
        "first mark should take effect"
    );
    assert!(
        entry.is_dead_during_cooldown(t0, cooldown),
        "peer should be in the dead cooldown right after marking"
    );

    // A packet authenticated from the peer 1s later is proof of life — touch_activity runs on that
    // path and must release the cooldown so flows are no longer refused.
    let t1 = t0 + Duration::from_secs(1);
    entry.touch_activity(t1);

    assert!(
        !entry.is_dead_during_cooldown(t1, cooldown),
        "an authenticated packet (proof of life) must clear the dead cooldown"
    );
}

#[test]
fn allocates_sender_schedule_slots() {
    let entry = test_entry_with_senders(4);
    assert_eq!(entry.socket_sender_count(), 4);
}

#[test]
fn empty_sender_schedule_is_supported() {
    let entry = test_entry_with_senders(0);
    assert_eq!(entry.socket_sender_count(), 0);
    assert_eq!(
        entry.sender_load_score(crate::endpoint::id::LocalSenderId::from_index(0)),
        0
    );
}

#[test]
fn sender_with_more_queued_bytes_has_higher_load_score() {
    let entry = test_entry_with_senders(2);
    let now = unsafe { Timestamp::from_duration(Duration::from_micros(10)) };

    entry.update_sender_load_score(
        crate::endpoint::id::LocalSenderId::from_index(0),
        now,
        4_000,
        s2n_quic_core::recovery::bandwidth::Bandwidth::new(1_000, Duration::from_millis(1)),
    );
    entry.update_sender_load_score(
        crate::endpoint::id::LocalSenderId::from_index(1),
        now,
        2_000,
        s2n_quic_core::recovery::bandwidth::Bandwidth::new(1_000, Duration::from_millis(1)),
    );

    let score0 = entry.sender_load_score(crate::endpoint::id::LocalSenderId::from_index(0));
    let score1 = entry.sender_load_score(crate::endpoint::id::LocalSenderId::from_index(1));
    assert!(
        score0 > score1,
        "sender 0 has more bytes queued so should have a higher load score"
    );
}
