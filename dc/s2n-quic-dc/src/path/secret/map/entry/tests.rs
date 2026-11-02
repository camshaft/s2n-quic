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

/// A packet that clears a dead mark hands over the resets the entry owes the peer for the streams
/// whose frames failed when it found the peer unresponsive, each stream once; a packet that
/// doesn't clear a mark hands over none.
#[test]
fn touch_activity_that_clears_a_dead_mark_hands_over_the_owed_resets() {
    let entry = test_entry_with_senders(0);
    let cooldown = crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN;
    let t0: crate::time::precision::Timestamp =
        (unsafe { Timestamp::from_duration(Duration::from_secs(100)) }).into();
    let stream = |queue_id: u8, binding_id: u8| OwedReset {
        queue_pair: crate::packet::datagram::QueuePair {
            source_queue_id: VarInt::from_u8(queue_id),
            dest_queue_id: VarInt::from_u8(queue_id + 1),
        },
        binding_id: VarInt::from_u8(binding_id),
    };
    let (a, b) = (stream(1, 1), stream(3, 2));

    assert!(
        !entry.touch_activity(t0),
        "a packet clears nothing where the entry isn't marked dead"
    );

    // As the send idle wheel does: the streams are owed before the mark.
    entry.owe_resets([a, b]);
    entry.owe_resets([a]);
    assert!(
        entry.mark_dead_if_cooldown_elapsed(t0, cooldown),
        "first mark should take effect"
    );

    let t1 = t0 + Duration::from_secs(1);
    assert!(
        entry.touch_activity(t1),
        "the first packet after the mark clears it"
    );
    let owed = entry.take_owed_resets();
    assert_eq!(owed.len(), 2, "each stream is owed once: {owed:?}");
    assert!(owed.contains(&a) && owed.contains(&b), "{owed:?}");

    let t2 = t1 + Duration::from_secs(1);
    assert!(!entry.touch_activity(t2), "a later packet clears nothing");
    assert!(
        entry.take_owed_resets().is_empty(),
        "the owed resets are handed over once"
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

/// The dead mark owes the peer a reset of each stream it resets on this side, each once: each
/// stream whose frames the send idle wheel failed and each stream open on the path, which may
/// have none of its frames in the send context. A stream whose slot was freed isn't open, and
/// isn't owed.
#[test]
fn mark_dead_owing_resets_owes_each_failed_and_each_open_stream() {
    let entry = test_entry_with_senders(0);
    let cooldown = crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN;
    let t0: crate::time::precision::Timestamp =
        (unsafe { Timestamp::from_duration(Duration::from_secs(100)) }).into();

    let QueueState::Client(state) = entry.queue_state() else {
        panic!("a client entry holds client queue state");
    };
    let open = state
        .alloc_local(VarInt::from_u8(7))
        .expect("a fresh client queue state allocates a slot");
    // As the stream's writer does when it is made.
    open.control.set_peer_queue_id(open.dest_queue_id);
    let open_stream = OwedReset {
        queue_pair: crate::packet::datagram::QueuePair {
            source_queue_id: open.local_queue_id,
            dest_queue_id: open.dest_queue_id,
        },
        binding_id: open.binding_id,
    };
    let failed_stream = OwedReset {
        queue_pair: crate::packet::datagram::QueuePair {
            source_queue_id: VarInt::from_u8(9),
            dest_queue_id: VarInt::from_u8(10),
        },
        binding_id: VarInt::from_u8(9),
    };

    // The open stream's frames failed too: it is owed once all the same.
    assert_eq!(
        entry.mark_dead_owing_resets(t0, cooldown, [failed_stream, open_stream]),
        (2, true),
        "the first mark should take effect, and owe each of the two streams once"
    );
    let t1 = t0 + Duration::from_secs(1);
    assert!(
        entry.touch_activity(t1),
        "the first packet after the mark clears it"
    );
    let owed = entry.take_owed_resets();
    assert_eq!(owed.len(), 2, "each stream is owed once: {owed:?}");
    assert!(
        owed.contains(&failed_stream) && owed.contains(&open_stream),
        "{owed:?}"
    );

    // A stream open on the path is owed though none of its frames failed.
    let t2 = t1 + Duration::from_secs(1);
    let (recorded, marked) = entry.mark_dead_owing_resets(t2, cooldown, core::iter::empty());
    assert!(marked, "the next mark should take effect");
    assert_eq!(recorded, 1, "the open stream is owed");
    let t3 = t2 + Duration::from_secs(1);
    assert!(
        entry.touch_activity(t3),
        "the first packet after the next mark clears it"
    );
    assert_eq!(entry.take_owed_resets(), vec![open_stream]);

    // Once the stream's receivers are dropped, its slot is freed and the stream isn't owed.
    drop(open);
    let t4 = t3 + Duration::from_secs(1);
    let (recorded, marked) = entry.mark_dead_owing_resets(t4, cooldown, core::iter::empty());
    assert!(marked, "the next mark should take effect");
    assert_eq!(recorded, 0, "a freed slot's stream isn't owed");
    let t5 = t4 + Duration::from_secs(1);
    assert!(
        entry.touch_activity(t5),
        "the first packet after the next mark clears it"
    );
    assert!(
        entry.take_owed_resets().is_empty(),
        "a freed slot doesn't hold a stream"
    );
}

/// The send idle wheel records the resets it owes before it marks the peer dead
/// ([`Entry::mark_dead_owing_resets`]), so that a packet from the peer that arrives between the
/// two writes doesn't find a mark to clear, and the packet that clears the mark finds the
/// resets. The stream to record comes from an iterator that, as the record reads it, touches the
/// entry's activity, as a receive worker does for a packet that arrives while the reset is
/// recorded, and checks that the touch doesn't clear a mark. The test runs on one thread: with
/// the two writes the other way round, that touch clears the mark before the reset is recorded,
/// and the test fails on every run.
#[test]
fn the_packet_that_clears_a_dead_mark_finds_the_resets_owed_before_it() {
    let entry = test_entry_with_senders(0);
    let cooldown = crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN;
    let t0: crate::time::precision::Timestamp =
        (unsafe { Timestamp::from_duration(Duration::from_secs(100)) }).into();
    let stream = OwedReset {
        queue_pair: crate::packet::datagram::QueuePair {
            source_queue_id: VarInt::from_u8(1),
            dest_queue_id: VarInt::from_u8(2),
        },
        binding_id: VarInt::from_u8(1),
    };

    let failed = core::iter::once_with(|| {
        // A packet from the peer that arrives while the reset is recorded.
        assert!(
            !entry.touch_activity(t0),
            "a packet that arrives as the reset is recorded must not clear a dead mark"
        );
        stream
    });
    assert_eq!(
        entry.mark_dead_owing_resets(t0, cooldown, failed),
        (1, true),
        "the mark should take effect, and owe the stream once"
    );

    let t1 = t0 + Duration::from_secs(1);
    assert!(
        entry.touch_activity(t1),
        "the first packet after the mark clears it"
    );
    assert_eq!(entry.take_owed_resets(), vec![stream]);
}
