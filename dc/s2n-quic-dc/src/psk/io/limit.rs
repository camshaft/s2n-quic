// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A limit on the work that runs at once, which the work's answers set instead of a fixed
//! number.
//!
//! The limit starts at one. Until an answer first signals congestion, it grows by one for each
//! clean answer, so it doubles with each round of answers, and after that by one for each round
//! of clean answers. An answer that signals congestion halves it, down to one: work whose time
//! past what its path accounts for was more than twice the base, or work with a peer that timed
//! out or that the peer refused, where work with another peer failed too since the last clean
//! answer. One peer's failures alone say that the peer is slow or refuses the work, so they don't
//! change the limit. Only work that ran while the limit was in full use grows it, and an answer
//! of work that started before a decrease doesn't change it. Work past the limit waits for a
//! permit, in the order it asked for one.

use std::{
    io,
    net::SocketAddr,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::sync::Semaphore;

/// What the end of a piece of work says about the limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Answer {
    /// The work finished, and spent this time past what its path accounts for. A time more than
    /// twice the base signals congestion: the work spent more than half of it waiting behind
    /// other work.
    Latency(Duration),
    /// The work finished before its deadline, without a time to compare.
    Finished,
    /// The work with this peer timed out, or the peer refused it. It signals congestion only
    /// where work with another peer failed too since the last clean answer.
    Failed(SocketAddr),
    /// The work's end doesn't say anything about the limit, as with a TLS failure or a cancel.
    Neutral,
}

/// The round trips a client's handshake waits on its peer from its connect to dc.complete: one
/// for the server's first flight, and one for the server's `DC_STATELESS_RESET_TOKENS` frame,
/// which the server sends once it has the client's. A handshake's `Waits` count the round trips
/// it waited past these.
const HANDSHAKE_ROUND_TRIPS: u32 = 2;

/// What a client's handshake waited on its peer past `HANDSHAKE_ROUND_TRIPS`, read from its
/// connection's events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Waits {
    /// The round trips the handshake added: one for a Retry the client took, after which it sent
    /// its first flight again, and one for a HelloRetryRequest, after which it sent a new
    /// ClientHello.
    pub(super) round_trips: u32,
    /// Whether the handshake lost a packet: one that the client sent, other than an MTU probe,
    /// which the connection declared lost, or one of the server's packets before its stateless
    /// reset tokens, which the client read from what it received of them. The wait for the loss
    /// to show and for the packet's data to arrive again isn't a whole number of round trips.
    pub(super) lost: bool,
}

impl Answer {
    /// Reads what a failed handshake with `peer` says about the limit: a handshake that timed
    /// out, or that the peer refused, is a failure of that peer, and any other failure doesn't
    /// say anything.
    pub(super) fn for_error(error: &io::Error, peer: SocketAddr) -> Self {
        match error.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::ConnectionRefused => Self::Failed(peer),
            _ => Self::Neutral,
        }
    }

    /// Reads a handshake that reached dc.complete in `elapsed` from its connect. The path
    /// accounts for `HANDSHAKE_ROUND_TRIPS` round trips, and the round trips past them that
    /// `waits` counts for a Retry or a HelloRetryRequest, of the lower of two times, each where a
    /// connection measured it: `earlier_rtt`, the lowest round-trip time that the earlier
    /// handshakes with the same peer measured, and `rtt`, the lowest that this handshake's
    /// connection measured. The limit compares the rest with the base. A handshake with a
    /// farther peer waits longer on its path, and that wait isn't congestion: a Retry or a
    /// HelloRetryRequest makes it wait one more round trip of that path. A connection's own
    /// round trips also hold the time its packets waited in the endpoint's receive queue, so
    /// under load `rtt` grows by that wait, and the lower `earlier_rtt` of a connection that
    /// waited less keeps the wait in the rest. Without either time, the rest is the whole time.
    /// Where the round trips take the whole time, the handshake doesn't show a time to compare,
    /// and it answers `Finished`, a clean answer without a time, so that a time of zero doesn't
    /// become the base. A handshake that lost a packet, the client's or the server's, answers
    /// `Neutral`: it waited for the loss to show and for the packet's data to arrive again, which
    /// isn't a whole number of round trips, so its time doesn't say how long it waited behind
    /// other work.
    pub(super) fn for_handshake(
        elapsed: Duration,
        earlier_rtt: Option<Duration>,
        rtt: Option<Duration>,
        waits: Waits,
    ) -> Self {
        if waits.lost {
            return Self::Neutral;
        }
        let round_trips = HANDSHAKE_ROUND_TRIPS.saturating_add(waits.round_trips);
        let path = match earlier_rtt.into_iter().chain(rtt).min() {
            Some(lowest) => lowest.saturating_mul(round_trips),
            None => Duration::ZERO,
        };
        match elapsed.checked_sub(path) {
            Some(latency) if !latency.is_zero() => Self::Latency(latency),
            _ => Self::Finished,
        }
    }
}

/// A limit on the work that runs at once, which the work's answers set.
pub(super) struct Limit(Arc<Shared>);

struct Shared {
    /// Each permit the semaphore holds lets one more piece of work start.
    permits: Semaphore,
    state: Mutex<State>,
}

struct State {
    /// How much work may run at once.
    limit: usize,
    /// How much work holds a permit.
    in_use: usize,
    /// Permits that a decrease took away while work held them, each retired as it comes back.
    debt: usize,
    /// Whether the limit grows by one for each clean answer, which the first congestion ends.
    slow_start: bool,
    /// Clean answers since the limit last grew, once slow start has ended.
    clean: usize,
    /// Raised by each decrease. Work that started before it ran under the larger limit, so its
    /// answer doesn't change the limit.
    generation: u64,
    /// Raised each time the work that holds a permit reaches the limit, so that an answer grows
    /// the limit only where the limit was in full use while its work ran.
    saturated: u64,
    /// The base time: the time of the last work that ran alone, or a lower time since.
    base: Option<Duration>,
    /// The peer whose work failed since the last clean answer, if any: a failure of work with
    /// another peer signals congestion.
    failed: Option<SocketAddr>,
}

impl Limit {
    pub(super) fn new() -> Self {
        Self(Arc::new(Shared {
            permits: Semaphore::new(1),
            state: Mutex::new(State {
                limit: 1,
                in_use: 0,
                debt: 0,
                slow_start: true,
                clean: 0,
                generation: 0,
                saturated: 0,
                base: None,
                failed: None,
            }),
        }))
    }

    /// Waits for a permit: work past the limit waits here, in the order it asked for one.
    pub(super) async fn acquire(&self) -> Permit {
        let permit = self.0.permits.acquire().await;
        permit.expect("never closed").forget();
        self.taken()
    }

    #[cfg(test)]
    fn try_acquire(&self) -> Option<Permit> {
        self.0.permits.try_acquire().ok()?.forget();
        Some(self.taken())
    }

    /// How much work may run at once now.
    #[cfg(test)]
    pub(super) fn current(&self) -> usize {
        self.0.lock().limit
    }

    fn taken(&self) -> Permit {
        let mut state = self.0.lock();
        state.in_use += 1;
        let saturated_at_start = state.in_use >= state.limit;
        if saturated_at_start {
            state.saturated += 1;
        }
        Permit {
            shared: self.0.clone(),
            released: false,
            generation: state.generation,
            saturated: state.saturated,
            saturated_at_start,
            alone: state.in_use == 1,
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Lets one piece of work run. [`Permit::release`] returns it with the work's answer, and a
/// permit dropped without one returns as [`Answer::Neutral`].
pub(super) struct Permit {
    shared: Arc<Shared>,
    released: bool,
    /// The limit's generation as the permit was taken.
    generation: u64,
    /// The limit's count of saturations as the permit was taken.
    saturated: u64,
    /// Whether taking this permit brought the work that holds a permit to the limit.
    saturated_at_start: bool,
    /// Whether no other work held a permit as this permit was taken.
    alone: bool,
}

impl Permit {
    /// Returns the permit with the work's answer.
    pub(super) fn release(mut self, answer: Answer) {
        self.finish(answer);
    }

    fn finish(&mut self, answer: Answer) {
        if std::mem::replace(&mut self.released, true) {
            return;
        }
        let shared = &*self.shared;
        let mut state = shared.lock();
        if self.generation == state.generation {
            // The limit was in full use while the work ran where taking this permit brought it
            // there, or where other work brought it there since.
            let full = self.saturated_at_start || state.saturated != self.saturated;
            // The work ran alone where no other work held a permit at its start or at its end.
            let alone = self.alone && state.in_use == 1;
            state.on_answer(&shared.permits, answer, full, alone);
        }
        state.in_use -= 1;
        state.add_permit(&shared.permits);
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.finish(Answer::Neutral);
    }
}

impl State {
    fn on_answer(&mut self, permits: &Semaphore, answer: Answer, full: bool, alone: bool) {
        let congested = match answer {
            Answer::Latency(latency) => self.is_late(latency, alone),
            Answer::Finished => false,
            // One peer's failures alone don't change the limit, and a failure of a second peer
            // signals congestion.
            Answer::Failed(peer) => match self.failed.replace(peer) {
                Some(other) if other != peer => true,
                _ => return,
            },
            Answer::Neutral => return,
        };
        if congested {
            self.decrease(permits);
        } else {
            // A clean answer starts the count of failed peers again.
            self.failed = None;
            if full {
                self.increase(permits);
            }
        }
    }

    /// Compares the work's time with the base. Work that ran alone didn't wait behind other work,
    /// so its time is the base from then on, and a lower time lowers the base.
    fn is_late(&mut self, latency: Duration, alone: bool) -> bool {
        match self.base {
            Some(base) if !alone && latency >= base => latency > base.saturating_mul(2),
            _ => {
                self.base = Some(latency);
                false
            }
        }
    }

    fn increase(&mut self, permits: &Semaphore) {
        if !self.slow_start {
            self.clean += 1;
            if self.clean < self.limit {
                return;
            }
            self.clean = 0;
        }
        self.limit += 1;
        self.add_permit(permits);
    }

    fn decrease(&mut self, permits: &Semaphore) {
        self.generation += 1;
        self.slow_start = false;
        self.clean = 0;
        self.failed = None;
        let limit = (self.limit / 2).max(1);
        self.debt += self.limit - limit;
        self.limit = limit;
        // Retires the permits that no work holds at once, and the others as they come back.
        while self.debt > 0 {
            let Ok(permit) = permits.try_acquire() else {
                break;
            };
            permit.forget();
            self.debt -= 1;
        }
    }

    /// Adds a permit to the semaphore, or retires it where a decrease left permits to retire.
    fn add_permit(&mut self, permits: &Semaphore) {
        if self.debt > 0 {
            self.debt -= 1;
        } else {
            permits.add_permits(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    fn peer(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// Takes each permit the limit has, and checks that it has as many as its limit.
    fn take_all(limit: &Limit) -> Vec<Permit> {
        let mut permits = vec![];
        while let Some(permit) = limit.try_acquire() {
            permits.push(permit);
        }
        assert_eq!(permits.len(), limit.current(), "the rest waits");
        permits
    }

    /// Takes each permit the limit has, and returns each one with the same answer.
    fn round(limit: &Limit, answer: Answer) {
        for permit in take_all(limit) {
            permit.release(answer);
        }
    }

    #[test]
    fn starts_at_one_and_doubles_while_the_answers_are_clean() {
        let limit = Limit::new();
        assert_eq!(limit.current(), 1);
        round(&limit, Answer::Latency(ms(1)));
        assert_eq!(limit.current(), 2);
        round(&limit, Answer::Latency(ms(2)));
        assert_eq!(limit.current(), 4);
        round(&limit, Answer::Finished);
        assert_eq!(limit.current(), 8);
    }

    #[test]
    fn congestion_halves_the_limit_and_then_each_clean_round_adds_one() {
        let limit = Limit::new();
        for _ in 0..3 {
            round(&limit, Answer::Finished);
        }
        assert_eq!(limit.current(), 8);

        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(Answer::Failed(peer(1)));
        assert_eq!(limit.current(), 8);
        // A second peer's failure since the last clean answer halves the limit.
        permits.next().unwrap().release(Answer::Failed(peer(2)));
        assert_eq!(limit.current(), 4);
        // The 6 permits still out are more than the new limit of 4, so no permit is free.
        assert!(limit.try_acquire().is_none());
        // Their work started under the larger limit, so their answers don't change it: neither
        // the failures of two more peers nor the clean answers after them.
        permits.next().unwrap().release(Answer::Failed(peer(3)));
        permits.next().unwrap().release(Answer::Failed(peer(4)));
        for permit in permits {
            permit.release(Answer::Finished);
        }
        assert_eq!(limit.current(), 4);

        round(&limit, Answer::Finished);
        assert_eq!(limit.current(), 5);
        round(&limit, Answer::Finished);
        assert_eq!(limit.current(), 6);
    }

    #[test]
    fn a_decrease_starts_the_count_of_clean_answers_again() {
        let limit = Limit::new();
        for _ in 0..3 {
            round(&limit, Answer::Finished);
        }
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(Answer::Failed(peer(1)));
        permits.next().unwrap().release(Answer::Failed(peer(2)));
        drop(permits);
        assert_eq!(limit.current(), 4);

        // Two of the four clean answers that a round at 4 needs, then failures of two peers.
        let mut permits = take_all(&limit).into_iter();
        for permit in permits.by_ref().take(2) {
            permit.release(Answer::Finished);
        }
        permits.next().unwrap().release(Answer::Failed(peer(1)));
        permits.next().unwrap().release(Answer::Failed(peer(2)));
        assert_eq!(limit.current(), 2);

        // The count starts again, so the limit grows only once a whole round at 2 is clean.
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(Answer::Finished);
        assert_eq!(limit.current(), 2);
        permits.next().unwrap().release(Answer::Finished);
        assert_eq!(limit.current(), 3);
    }

    #[test]
    fn a_second_decrease_while_permits_of_the_first_are_out_keeps_the_count() {
        let limit = Limit::new();
        for _ in 0..4 {
            round(&limit, Answer::Finished);
        }
        assert_eq!(limit.current(), 16);

        let mut old = take_all(&limit);
        old.pop().unwrap().release(Answer::Failed(peer(1)));
        old.pop().unwrap().release(Answer::Failed(peer(2)));
        assert_eq!(limit.current(), 8);
        // Of the 14 permits still out, the first 6 that come back are retired, and the next 2
        // are free.
        for permit in old.drain(..8) {
            permit.release(Answer::Neutral);
        }
        let first = limit.try_acquire().unwrap();
        let second = limit.try_acquire().unwrap();
        assert!(limit.try_acquire().is_none());
        // The decrease started the count of failed peers again, so this failure alone leaves
        // the limit.
        first.release(Answer::Failed(peer(1)));
        assert_eq!(limit.current(), 8);
        second.release(Answer::Failed(peer(2)));
        assert_eq!(limit.current(), 4);

        // The last 6 old permits come back, and the limit has exactly 4 free.
        for permit in old {
            permit.release(Answer::Neutral);
        }
        take_all(&limit);
    }

    #[test]
    fn one_peers_failures_dont_change_the_limit() {
        let limit = Limit::new();
        round(&limit, Answer::Finished);
        round(&limit, Answer::Finished);
        round(&limit, Answer::Failed(peer(1)));
        round(&limit, Answer::Failed(peer(1)));
        assert_eq!(limit.current(), 4);
    }

    #[test]
    fn a_clean_answer_between_two_peers_failures_keeps_the_limit() {
        let limit = Limit::new();
        round(&limit, Answer::Finished);
        round(&limit, Answer::Finished);

        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(Answer::Failed(peer(1)));
        // The clean answer starts the count of failed peers again, and grows the limit.
        permits.next().unwrap().release(Answer::Finished);
        assert_eq!(limit.current(), 5);
        permits.next().unwrap().release(Answer::Failed(peer(2)));
        assert_eq!(limit.current(), 5);
        // A failure of a second peer since the last clean answer halves the limit.
        permits.next().unwrap().release(Answer::Failed(peer(1)));
        assert_eq!(limit.current(), 2);
    }

    #[test]
    fn a_time_past_twice_the_base_is_congestion() {
        let limit = Limit::new();
        round(&limit, Answer::Latency(ms(10)));
        assert_eq!(limit.current(), 2);

        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(Answer::Latency(ms(20)));
        assert_eq!(limit.current(), 3, "twice the base is clean");
        permits.next().unwrap().release(Answer::Latency(ms(21)));
        assert_eq!(limit.current(), 1, "past twice the base is congestion");
        // The decrease retired the free permits past the new limit at once.
        take_all(&limit);
    }

    #[test]
    fn work_that_ran_alone_sets_the_base_and_a_lower_time_lowers_it() {
        let limit = Limit::new();
        round(&limit, Answer::Latency(ms(10)));
        // This work ran alone too, so its time is the base though it is higher. It didn't fill
        // the limit of 2, so the limit doesn't grow.
        let permit = limit.try_acquire().unwrap();
        permit.release(Answer::Latency(ms(40)));
        assert_eq!(limit.current(), 2);

        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(Answer::Latency(ms(80)));
        assert_eq!(limit.current(), 3, "80 ms is twice the 40 ms base");
        permits.next().unwrap().release(Answer::Latency(ms(20)));
        assert_eq!(limit.current(), 4);

        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(Answer::Latency(ms(41)));
        assert_eq!(limit.current(), 2, "41 ms is past twice the 20 ms base");
    }

    /// Reads a handshake that waited on its peer for its two round trips alone.
    fn handshake(
        elapsed: Duration,
        earlier_rtt: Option<Duration>,
        rtt: Option<Duration>,
    ) -> Answer {
        Answer::for_handshake(elapsed, earlier_rtt, rtt, Waits::default())
    }

    #[test]
    fn a_handshake_answers_its_time_past_two_round_trips_of_its_path() {
        for (elapsed, earlier_rtt, rtt, expected) in [
            (ms(103), None, Some(ms(50)), Answer::Latency(ms(3))),
            // The lower of an earlier connection's round trip and this connection's counts.
            (ms(103), Some(ms(40)), Some(ms(50)), Answer::Latency(ms(23))),
            (ms(103), Some(ms(60)), Some(ms(50)), Answer::Latency(ms(3))),
            (ms(103), Some(ms(50)), None, Answer::Latency(ms(3))),
            // Without a round-trip time, the rest is the whole time.
            (ms(103), None, None, Answer::Latency(ms(103))),
            // Where the round trips take the whole time, the handshake doesn't show a time to
            // compare.
            (ms(100), None, Some(ms(50)), Answer::Finished),
            (ms(90), None, Some(ms(50)), Answer::Finished),
        ] {
            assert_eq!(handshake(elapsed, earlier_rtt, rtt), expected);
        }
    }

    #[test]
    fn a_retry_or_a_hello_retry_request_adds_a_round_trip_and_a_loss_answers_neutral() {
        for (elapsed, round_trips, lost, expected) in [
            // Each Retry or HelloRetryRequest adds a 50 ms round trip of the path.
            (ms(153), 1, false, Answer::Latency(ms(3))),
            (ms(203), 2, false, Answer::Latency(ms(3))),
            (ms(150), 1, false, Answer::Finished),
            // A lost packet's recovery wait isn't a whole number of round trips.
            (ms(303), 0, true, Answer::Neutral),
            (ms(353), 1, true, Answer::Neutral),
        ] {
            let waits = Waits { round_trips, lost };
            assert_eq!(
                Answer::for_handshake(elapsed, None, Some(ms(50)), waits),
                expected
            );
        }
    }

    #[test]
    fn a_far_handshake_with_a_retry_or_a_lost_packet_doesnt_halve_the_limit() {
        // A near peer's round trip takes 1 ms, and its handshake 3 ms: the endpoint added 1 ms.
        let near = handshake(ms(3), None, Some(ms(1)));
        // A far peer's round trip takes 50 ms. Its server sends a Retry, which adds a third round
        // trip, and its handshake takes 151 ms: the endpoint added 1 ms.
        let retry = Waits {
            round_trips: 1,
            lost: false,
        };
        let far_retry = Answer::for_handshake(ms(151), None, Some(ms(50)), retry);
        assert_eq!(far_retry, Answer::Latency(ms(1)));
        let limit = Limit::new();
        // The near peer's handshake runs alone, and its 1 ms is the base.
        round(&limit, near);
        assert_eq!(limit.current(), 2);

        // Each round runs a handshake with the far peer that took a Retry while handshakes with
        // the near peer run, and the limit keeps doubling.
        for expected in [4, 8] {
            let mut permits = take_all(&limit).into_iter();
            permits.next().unwrap().release(far_retry);
            for permit in permits {
                permit.release(near);
            }
            assert_eq!(limit.current(), expected);
        }

        // A handshake with the far peer loses its first packet and takes 251 ms. The wait for the
        // packet's recovery isn't a whole number of round trips, so its time doesn't say how
        // long it waited behind other work, and it leaves the limit as it is.
        let lost = Waits {
            round_trips: 0,
            lost: true,
        };
        let far_lost = Answer::for_handshake(ms(251), None, Some(ms(50)), lost);
        assert_eq!(far_lost, Answer::Neutral);
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(far_lost);
        assert_eq!(limit.current(), 8);

        // Read with two round trips alone, the handshake that took a Retry shows 51 ms, past
        // twice the base, which halves the limit.
        let retry_as_two_round_trips = handshake(ms(151), None, Some(ms(50)));
        permits.next().unwrap().release(retry_as_two_round_trips);
        assert_eq!(limit.current(), 4);

        // Read with two round trips alone, the handshake that lost a packet shows 151 ms, which
        // halves the limit too.
        drop(permits);
        let lost_as_two_round_trips = handshake(ms(251), None, Some(ms(50)));
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(lost_as_two_round_trips);
        assert_eq!(limit.current(), 2);
    }

    #[test]
    fn handshakes_with_a_near_and_a_far_peer_at_once_dont_lower_the_limit() {
        // A near peer's round trip takes 1 ms, and its handshake 3 ms: the endpoint added 1 ms.
        let near = handshake(ms(3), None, Some(ms(1)));
        // A far peer's round trip takes 50 ms, and its handshake 101 ms: the endpoint added 1 ms.
        let far = handshake(ms(101), None, Some(ms(50)));
        let limit = Limit::new();
        // The near peer's handshake runs alone, and its 1 ms is the base.
        round(&limit, near);
        assert_eq!(limit.current(), 2);

        // Each round runs a handshake with the far peer while handshakes with the near peer run,
        // and the limit keeps doubling.
        for expected in [4, 8] {
            let mut permits = take_all(&limit).into_iter();
            permits.next().unwrap().release(far);
            for permit in permits {
                permit.release(near);
            }
            assert_eq!(limit.current(), expected);
        }

        // Read without its path's round trips, the far peer's 101 ms is past twice the base,
        // which halves the limit.
        let far_without_its_path = handshake(ms(101), None, None);
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(far_without_its_path);
        assert_eq!(limit.current(), 4);
    }

    #[test]
    fn a_far_handshake_whose_endpoint_time_is_past_twice_the_base_is_congestion() {
        let limit = Limit::new();
        round(&limit, handshake(ms(3), None, Some(ms(1))));
        // The far peer's round trips account for 100 ms of its 103 ms, and the endpoint's 3 ms
        // is past twice the 1 ms base.
        let far = handshake(ms(103), None, Some(ms(50)));
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(far);
        assert_eq!(limit.current(), 1);
    }

    #[test]
    fn a_handshake_whose_round_trips_take_its_whole_time_doesnt_set_the_base() {
        let limit = Limit::new();
        // The handshake runs alone, and its 1 ms past its round trips is the base.
        round(&limit, handshake(ms(3), None, Some(ms(1))));
        assert_eq!(limit.current(), 2);

        // Beside a handshake whose round trips take its whole time, a handshake with 2 ms past
        // its round trips is within twice the base, and the limit grows.
        let finished = handshake(ms(90), None, Some(ms(50)));
        let within = handshake(ms(4), None, Some(ms(1)));
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(finished);
        permits.next().unwrap().release(within);
        assert_eq!(limit.current(), 4);
    }

    #[test]
    fn a_near_handshake_whose_round_trips_hold_the_endpoints_wait_still_halves_the_limit() {
        let limit = Limit::new();
        // A near peer's handshake runs alone: its connection measures a 1 ms round trip, and the
        // 1 ms past two of them is the base.
        round(&limit, handshake(ms(3), None, Some(ms(1))));
        assert_eq!(limit.current(), 2);

        // While the endpoint is busy, its receive queue holds each packet of the next handshake
        // with the peer for 4 ms, so that handshake's connection measures a 5 ms round trip, and
        // its two round trips take 10 ms of its 11 ms. Against the earlier connection's 1 ms
        // round trip, 9 ms is past twice the base, which halves the limit.
        let late = handshake(ms(11), Some(ms(1)), Some(ms(5)));
        assert_eq!(late, Answer::Latency(ms(9)));
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(late);
        assert_eq!(limit.current(), 1);

        // Read against its own round trips alone, the same handshake shows 1 ms, within twice
        // the base.
        assert_eq!(
            handshake(ms(11), None, Some(ms(5))),
            Answer::Latency(ms(1))
        );
    }

    #[test]
    fn work_that_doesnt_fill_the_limit_doesnt_grow_it() {
        let limit = Limit::new();
        round(&limit, Answer::Finished);
        for _ in 0..10 {
            limit.try_acquire().unwrap().release(Answer::Finished);
        }
        assert_eq!(limit.current(), 2);
    }

    #[test]
    fn a_neutral_answer_or_a_dropped_permit_leaves_the_limit() {
        let limit = Limit::new();
        limit.try_acquire().unwrap().release(Answer::Neutral);
        drop(limit.try_acquire().unwrap());
        assert_eq!(limit.current(), 1);
        // The permit came back each time.
        take_all(&limit);
    }

    #[test]
    fn the_limit_doesnt_go_below_one() {
        let limit = Limit::new();
        for port in [1, 2, 1, 2] {
            let permit = limit.try_acquire().unwrap();
            permit.release(Answer::Failed(peer(port)));
        }
        assert_eq!(limit.current(), 1);
    }

    #[test]
    fn a_handshake_that_timed_out_or_was_refused_is_a_failure_of_its_peer() {
        for (kind, answer) in [
            (io::ErrorKind::TimedOut, Answer::Failed(peer(1))),
            (io::ErrorKind::ConnectionRefused, Answer::Failed(peer(1))),
            (io::ErrorKind::ConnectionReset, Answer::Neutral),
            (io::ErrorKind::Other, Answer::Neutral),
        ] {
            let error = io::Error::from(kind);
            assert_eq!(Answer::for_error(&error, peer(1)), answer);
        }
    }

    #[test]
    fn work_past_the_limit_waits_until_the_limit_lets_it_start() {
        let limit = Limit::new();
        let mut cx = Context::from_waker(Waker::noop());
        let mut taking_first = pin!(limit.acquire());
        let Poll::Ready(first) = taking_first.as_mut().poll(&mut cx) else {
            panic!("the first piece of work starts at once");
        };
        let mut taking_second = pin!(limit.acquire());
        let waits = taking_second.as_mut().poll(&mut cx).is_pending();
        assert!(waits, "the second piece of work waits");

        // The first answer is clean and the limit was in full use, so the limit grows to 2.
        first.release(Answer::Finished);
        let starts = taking_second.as_mut().poll(&mut cx).is_ready();
        assert!(starts, "the second piece of work starts");
        assert_eq!(limit.current(), 2);
    }
}
