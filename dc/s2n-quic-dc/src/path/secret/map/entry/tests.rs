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
            // default one-element peer_data_addrs list (one std SocketAddr, 32 bytes), and the
            // empty peer_data_interfaces list (an Arc<[Option<u16>]>, 16 bytes).
            // The senders' delivery times add a Box<[AtomicU64]> (16 bytes), and the time the
            // lost pairs stopped delivering an AtomicU64 (8 bytes).
            // The end of the preference for the senders that deliver since a loss adds an
            // AtomicU64 (8 bytes), and the end that an acknowledgement reported another (8 bytes).
            393
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

/// A dead mark asks for a fresh entry once the dead-peer cooldown has passed and the peer has
/// been silent for a whole idle timeout. A mark within the cooldown, a shorter silence, a young
/// entry that hasn't heard from the peer yet, or a packet from the peer doesn't.
#[test]
fn dead_and_silent_after_cooldown_and_one_idle_timeout() {
    let entry = test_entry_with_senders(0);
    let idle = entry.idle_timeout();
    assert!(
        idle >= Duration::from_secs(1),
        "the cases below need an idle timeout of at least 1s"
    );
    let cooldown = idle / 4;
    let old = idle * 10;
    let ms = Duration::from_millis(1);

    let t0: crate::time::precision::Timestamp =
        (unsafe { Timestamp::from_duration(Duration::from_secs(1000)) }).into();
    entry.touch_activity(t0);
    assert!(
        !entry.is_dead_and_silent(t0 + idle * 2, old, cooldown),
        "never marked dead"
    );

    let marked = t0 + idle / 10;
    assert!(entry.mark_dead_if_cooldown_elapsed(marked, cooldown));
    assert!(
        !entry.is_dead_and_silent(marked + cooldown - ms, old, cooldown),
        "within the dead-peer cooldown"
    );
    assert!(
        !entry.is_dead_and_silent(t0 + idle - ms, old, cooldown),
        "past the cooldown, silent for less than an idle timeout"
    );
    assert!(
        entry.is_dead_and_silent(t0 + idle, old, cooldown),
        "past the cooldown, and silent for an idle timeout"
    );

    entry.touch_activity(t0 + idle + Duration::from_secs(1));
    assert!(
        !entry.is_dead_and_silent(t0 + idle * 3, old, cooldown),
        "a packet from the peer clears the dead mark"
    );

    // An entry that hasn't received a packet yet counts its silence from its creation.
    let fresh = test_entry_with_senders(0);
    assert!(fresh.mark_dead_if_cooldown_elapsed(marked, cooldown));
    let after_cooldown = marked + cooldown;
    assert!(
        !fresh.is_dead_and_silent(after_cooldown, idle - ms, cooldown),
        "an untouched entry younger than an idle timeout"
    );
    assert!(
        fresh.is_dead_and_silent(after_cooldown, idle, cooldown),
        "an untouched entry as old as an idle timeout"
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

#[test]
fn a_sender_marked_lost_counts_as_lost_until_cleared() {
    let entry = test_entry_with_senders(2);
    let sender0 = crate::endpoint::id::LocalSenderId::from_index(0);
    let sender1 = crate::endpoint::id::LocalSenderId::from_index(1);
    assert!(!entry.has_lost_sender());
    assert!(entry.has_delivering_sender());

    assert!(
        entry.set_sender_lost(sender0, true),
        "the first mark changes the slot"
    );
    assert!(
        !entry.set_sender_lost(sender0, true),
        "a second mark doesn't change it"
    );
    assert!(entry.is_sender_lost(sender0));
    assert!(!entry.is_sender_lost(sender1));
    assert!(entry.has_lost_sender());
    assert!(entry.has_delivering_sender());

    assert!(entry.set_sender_lost(sender1, true));
    assert!(
        !entry.has_delivering_sender(),
        "no sender delivers once every one is lost"
    );

    assert!(entry.set_sender_lost(sender0, false));
    assert!(!entry.is_sender_lost(sender0));
    assert!(entry.has_delivering_sender());
}

#[test]
fn a_load_score_update_keeps_the_lost_mark() {
    let entry = test_entry_with_senders(1);
    let sender = crate::endpoint::id::LocalSenderId::from_index(0);
    let now = unsafe { Timestamp::from_duration(Duration::from_micros(10)) };
    let bandwidth =
        s2n_quic_core::recovery::bandwidth::Bandwidth::new(1_000, Duration::from_millis(1));

    entry.update_sender_load_score(sender, now, 4_000, bandwidth);
    let score = entry.sender_load_score(sender);
    assert!(entry.set_sender_lost(sender, true));
    assert_eq!(
        entry.sender_load_score(sender),
        score,
        "the lost mark doesn't change the load score"
    );

    entry.update_sender_load_score(sender, now, 8_000, bandwidth);
    assert!(
        entry.is_sender_lost(sender),
        "a score update keeps the lost mark"
    );
    assert!(entry.sender_load_score(sender) > score);
}

#[test]
fn a_message_for_a_lost_sender_goes_to_the_next_sender_that_delivers() {
    let entry = test_entry_with_senders(4);
    let sender = crate::endpoint::id::LocalSenderId::from_index;
    assert_eq!(entry.delivering_sender(sender(1)), sender(1));

    assert!(entry.set_sender_lost(sender(1), true));
    assert!(entry.set_sender_lost(sender(2), true));
    assert_eq!(
        entry.delivering_sender(sender(1)),
        sender(3),
        "the first sender after a lost one whose pair delivers"
    );

    assert!(entry.set_sender_lost(sender(3), true));
    assert_eq!(
        entry.delivering_sender(sender(2)),
        sender(0),
        "the search goes on from the first slot"
    );

    assert!(entry.set_sender_lost(sender(0), true));
    assert_eq!(
        entry.delivering_sender(sender(2)),
        sender(2),
        "where every pair is lost, the preferred sender keeps the message"
    );
}

#[test]
fn a_message_goes_to_a_sender_that_delivers_since_a_loss_where_one_does() {
    use crate::time::precision::Timestamp as PreciseTimestamp;
    let entry = test_entry_with_senders(4);
    let sender = crate::endpoint::id::LocalSenderId::from_index;
    let at = |nanos| PreciseTimestamp { nanos };

    // The pair of sender 0 is marked lost, and sender 1, acknowledged before the lost pair
    // stopped delivering, may be on the same down interface. While no sender has an
    // acknowledgement since, a sender delivers while it isn't marked lost.
    entry.record_sender_delivered(sender(1), at(100));
    entry.set_senders_lost_since(at(200));
    entry.set_senders_preferred_until(at(1_000));
    assert!(entry.set_sender_lost(sender(0), true));
    assert!(!entry.has_sender_delivering_since_lost());
    assert_eq!(entry.delivering_sender(sender(0)), sender(1));
    assert_eq!(entry.delivering_sender(sender(1)), sender(1));

    // Once sender 3 has an acknowledgement since the loss, a message for a sender without one
    // goes to it, though that sender isn't marked lost.
    entry.record_sender_delivered(sender(3), at(300));
    assert!(entry.has_sender_delivering_since_lost());
    assert_eq!(
        entry.delivering_sender(sender(1)),
        sender(3),
        "a sender without an acknowledgement since the loss may be on the down interface"
    );
    assert_eq!(entry.delivering_sender(sender(0)), sender(3));
    assert_eq!(entry.delivering_sender(sender(3)), sender(3));

    // The search goes on by slot, among the senders with an acknowledgement since the loss.
    entry.record_sender_delivered(sender(2), at(400));
    assert_eq!(entry.delivering_sender(sender(1)), sender(2));
    assert_eq!(entry.delivering_sender(sender(2)), sender(2));

    // Once no pair is marked lost, each sender keeps its message.
    assert!(entry.set_sender_lost(sender(0), false));
    assert!(!entry.has_sender_delivering_since_lost());
    assert_eq!(entry.delivering_sender(sender(0)), sender(0));
    assert_eq!(entry.delivering_sender(sender(1)), sender(1));
}

#[test]
fn a_sender_delivers_since_a_loss_only_after_the_lost_pairs_stopped() {
    use crate::time::precision::Timestamp as PreciseTimestamp;
    let entry = test_entry_with_senders(3);
    let sender0 = crate::endpoint::id::LocalSenderId::from_index(0);
    let sender1 = crate::endpoint::id::LocalSenderId::from_index(1);
    let sender2 = crate::endpoint::id::LocalSenderId::from_index(2);
    let at = |nanos| PreciseTimestamp { nanos };

    entry.record_sender_delivered(sender1, at(150));
    entry.record_sender_delivered(sender2, at(300));
    assert!(
        !entry.has_sender_delivering_since_lost(),
        "no sender delivers since a loss while no pair is lost"
    );

    entry.set_senders_lost_since(at(200));
    assert!(entry.set_sender_lost(sender0, true));
    entry.record_sender_delivered(sender0, at(400));
    assert!(
        !entry.delivers_since_lost(sender0),
        "a lost sender doesn't deliver, whatever its delivery time"
    );
    assert!(
        !entry.delivers_since_lost(sender1),
        "a sender acknowledged before the lost pairs stopped may be on the same down interface"
    );
    assert!(entry.delivers_since_lost(sender2));
    assert!(entry.has_sender_delivering_since_lost());

    entry.set_senders_lost_since(at(100));
    assert!(
        !entry.delivers_since_lost(sender1),
        "an earlier stop time doesn't move the recorded one back"
    );
    entry.set_senders_lost_since(at(300));
    assert!(
        !entry.delivers_since_lost(sender2),
        "a delivery at the stop time doesn't count"
    );
    assert!(!entry.has_sender_delivering_since_lost());

    entry.record_sender_delivered(sender1, at(500));
    entry.record_sender_delivered(sender1, at(50));
    assert!(
        entry.delivers_since_lost(sender1),
        "an earlier delivery doesn't move the recorded one back"
    );
    assert!(entry.has_sender_delivering_since_lost());
}

/// The routers prefer the senders that deliver since a loss until a sender has an
/// acknowledgement after the end of the preference, so that a sender idle at the loss gets
/// items again.
#[test]
fn the_preference_for_senders_that_deliver_since_a_loss_has_an_end() {
    use crate::time::precision::Timestamp as PreciseTimestamp;
    let entry = test_entry_with_senders(3);
    let sender0 = crate::endpoint::id::LocalSenderId::from_index(0);
    let sender1 = crate::endpoint::id::LocalSenderId::from_index(1);
    let sender2 = crate::endpoint::id::LocalSenderId::from_index(2);
    let at = |nanos| PreciseTimestamp { nanos };

    entry.set_senders_lost_since(at(200));
    entry.set_senders_preferred_until(at(1_000));
    assert!(entry.set_sender_lost(sender0, true));
    assert!(
        !entry.prefers_senders_delivering_since_lost(),
        "no sender delivers since the loss yet"
    );

    entry.record_sender_delivered(sender1, at(1_000));
    assert!(
        entry.prefers_senders_delivering_since_lost(),
        "an acknowledgement at the preference's end is within it"
    );

    entry.record_sender_delivered(sender2, at(1_001));
    assert!(entry.has_sender_delivering_since_lost());
    assert!(
        !entry.prefers_senders_delivering_since_lost(),
        "an acknowledgement after the preference's end ends it"
    );

    entry.set_senders_preferred_until(at(2_000));
    assert!(
        entry.prefers_senders_delivering_since_lost(),
        "a later end holds the preference again"
    );
    entry.set_senders_preferred_until(at(500));
    assert!(
        entry.prefers_senders_delivering_since_lost(),
        "an earlier end doesn't move the recorded one back"
    );
}

/// Once a sender has an acknowledgement after the end of the routers' preference for the
/// senders that deliver since a loss, a message keeps its sender while that sender's pair
/// isn't marked lost, as the routers choose then.
#[test]
fn a_message_keeps_its_sender_once_the_preference_for_senders_that_deliver_ends() {
    use crate::time::precision::Timestamp as PreciseTimestamp;
    let entry = test_entry_with_senders(4);
    let sender = crate::endpoint::id::LocalSenderId::from_index;
    let at = |nanos| PreciseTimestamp { nanos };

    // The pair of sender 0 is marked lost, and within the preference a message for sender 1,
    // without an acknowledgement since the loss, goes to sender 3, which has one.
    entry.set_senders_lost_since(at(200));
    entry.set_senders_preferred_until(at(1_000));
    assert!(entry.set_sender_lost(sender(0), true));
    entry.record_sender_delivered(sender(3), at(300));
    assert_eq!(entry.delivering_sender(sender(1)), sender(3));

    // Once sender 3 has an acknowledgement after the preference's end, a pair delivers while
    // it isn't marked lost again.
    entry.record_sender_delivered(sender(3), at(1_100));
    assert!(entry.has_sender_delivering_since_lost());
    assert!(!entry.prefers_senders_delivering_since_lost());
    assert_eq!(
        entry.delivering_sender(sender(1)),
        sender(1),
        "a sender that isn't marked lost keeps its message once the preference ends"
    );
    assert_eq!(entry.delivering_sender(sender(0)), sender(1));
}

/// An acknowledgement reports the end of the routers' preference for the senders that deliver
/// since a loss where it is the first after that end, while the routers prefer them.
#[test]
fn the_acknowledgement_that_ends_the_preference_for_senders_that_deliver_reports_it() {
    use crate::time::precision::Timestamp as PreciseTimestamp;
    let sender = crate::endpoint::id::LocalSenderId::from_index;
    let at = |nanos| PreciseTimestamp { nanos };

    let unmarked = test_entry_with_senders(3);
    unmarked.set_senders_lost_since(at(200));
    unmarked.set_senders_preferred_until(at(1_000));
    assert!(
        !unmarked.record_sender_delivered(sender(1), at(1_100)),
        "an entry without a lost pair doesn't prefer any sender, so nothing ends"
    );

    let entry = test_entry_with_senders(3);
    entry.set_senders_lost_since(at(200));
    entry.set_senders_preferred_until(at(1_000));
    assert!(entry.set_sender_lost(sender(0), true));
    assert!(
        !entry.record_sender_delivered(sender(1), at(300)),
        "an acknowledgement within the preference doesn't end it"
    );
    assert!(
        !entry.record_sender_delivered(sender(1), at(1_000)),
        "an acknowledgement at the preference's end is within it"
    );
    assert!(entry.prefers_senders_delivering_since_lost());
    assert!(
        entry.record_sender_delivered(sender(2), at(1_100)),
        "the first acknowledgement after the preference's end ends it"
    );
    assert!(!entry.prefers_senders_delivering_since_lost());
    assert!(
        !entry.record_sender_delivered(sender(2), at(1_200)),
        "a later acknowledgement of the same sender doesn't end it again"
    );
    assert!(
        !entry.record_sender_delivered(sender(1), at(1_300)),
        "nor does a later acknowledgement of another sender"
    );

    entry.set_senders_preferred_until(at(2_000));
    assert!(entry.prefers_senders_delivering_since_lost());
    assert!(
        entry.record_sender_delivered(sender(1), at(2_100)),
        "an acknowledgement after a later end ends that preference too"
    );
}

/// The entry reports each end of the routers' preference once, though the acknowledgements of
/// several senders after the end may race to report it.
#[test]
fn an_end_of_the_preference_is_reported_once() {
    use crate::time::precision::Timestamp as PreciseTimestamp;
    let entry = test_entry_with_senders(3);
    let sender = crate::endpoint::id::LocalSenderId::from_index;
    let at = |nanos| PreciseTimestamp { nanos };

    entry.set_senders_lost_since(at(200));
    entry.set_senders_preferred_until(at(1_000));
    assert!(entry.set_sender_lost(sender(0), true));
    entry.record_sender_delivered(sender(1), at(300));
    assert!(entry.prefers_senders_delivering_since_lost());

    // Another sender's acknowledgement reports the end first, as on another thread.
    assert!(entry.record_preference_end(1_000));
    assert!(
        !entry.record_sender_delivered(sender(2), at(1_100)),
        "an acknowledgement after an end that was reported doesn't report it again"
    );
    assert!(!entry.record_preference_end(1_000));
    assert!(
        entry.record_preference_end(2_000),
        "a later end is reported once too"
    );
}
