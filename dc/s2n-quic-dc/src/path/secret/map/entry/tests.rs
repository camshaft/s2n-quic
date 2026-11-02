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

/// Where two receive workers take packets from the peer at once, both may find the dead mark set,
/// and only one of them may report that it cleared it, so that the owed resets are sent once.
/// Each round marks the peer dead and then releases both workers together: each spins on the
/// round counter, so the two call `touch_activity` at nearly the same moment. A clear that stores
/// -1 after its load and returns `true`, instead of the swap, reports the mark twice in each round
/// where both workers load it before either clears it.
#[test]
fn touch_activity_reports_a_dead_mark_two_workers_clear_at_once_to_one_of_them() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Barrier,
    };

    const ROUNDS: usize = 1000;

    let entry = test_entry_with_senders(0);
    let cooldown = crate::endpoint::DEFAULT_DEAD_PEER_COOLDOWN;
    let t0: crate::time::precision::Timestamp =
        (unsafe { Timestamp::from_duration(Duration::from_secs(100)) }).into();
    let t1 = t0 + Duration::from_secs(1);

    // `start` holds the round the workers run, and `done` waits for both of them to return before
    // the next round marks the peer dead again.
    let start = AtomicUsize::new(0);
    let done = Barrier::new(3);
    let reports = AtomicUsize::new(0);

    // The results are checked once every worker has returned, so that a failed round doesn't
    // leave a worker waiting on a round that never starts.
    let (marked, reported) = std::thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| {
                for round in 1..=ROUNDS {
                    while start.load(Ordering::Acquire) != round {
                        std::hint::spin_loop();
                    }
                    if entry.touch_activity(t1) {
                        reports.fetch_add(1, Ordering::Relaxed);
                    }
                    done.wait();
                }
            });
        }

        let mut marked = Vec::with_capacity(ROUNDS);
        let mut reported = Vec::with_capacity(ROUNDS);
        for round in 1..=ROUNDS {
            marked.push(entry.mark_dead_if_cooldown_elapsed(t0, cooldown));
            start.store(round, Ordering::Release);
            done.wait();
            reported.push(reports.swap(0, Ordering::Relaxed));
        }
        (marked, reported)
    });

    for (round, (marked, reported)) in (1..=ROUNDS).zip(marked.into_iter().zip(reported)) {
        assert!(marked, "round {round}: the mark takes effect");
        assert_eq!(reported, 1, "round {round}: one worker, and only one, reports the clear");
    }
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
