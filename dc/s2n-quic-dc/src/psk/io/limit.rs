// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A limit on the work that runs at once, which the work's answers set instead of a fixed
//! number.
//!
//! The limit starts at one. Until an answer first signals congestion, it grows by one for each
//! clean answer, so it doubles with each round of answers, and after that by one for each round
//! of clean answers. An answer that signals congestion halves it, down to one: work that took
//! more than twice the base time, or work with a peer that timed out or that the peer refused,
//! where the work took the last free permit and such work with another peer failed too since the
//! last clean answer or the last decrease. One peer's failures alone say that the peer is slow or
//! refuses the work, so they don't change the limit, and neither does a failure of work that took
//! its permit while others were free, since that work didn't contend for the limit. Only work
//! that ran while the limit was in full use grows it, and an answer of work that started before a
//! decrease doesn't change it. Work past the limit waits for a permit, in the order it asked for
//! one.

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
    /// The work finished in this time. A time more than twice the base signals congestion: the
    /// work spent more than half of it waiting behind other work.
    Latency(Duration),
    /// The work finished before its deadline, without a time to compare.
    Finished,
    /// The work with this peer timed out, or the peer refused it. It signals congestion only
    /// where the work took the last free permit, and such work with another peer failed too since
    /// the last clean answer or the last decrease.
    Failed(SocketAddr),
    /// The work's end doesn't say anything about the limit, as with a TLS failure or a cancel.
    Neutral,
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
    /// The peer whose work failed after taking the last free permit, since the last clean answer
    /// or the last decrease, if any: such a failure of work with another peer signals congestion.
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

    /// Takes a permit where one is free, without waiting.
    #[cfg(test)]
    pub(super) fn try_acquire(&self) -> Option<Permit> {
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
            // The work contended for the limit where it took the last free permit: it took its
            // permit while the limit was in full use.
            let contended = self.saturated_at_start;
            state.on_answer(&shared.permits, answer, full, contended, alone);
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
    fn on_answer(
        &mut self,
        permits: &Semaphore,
        answer: Answer,
        full: bool,
        contended: bool,
        alone: bool,
    ) {
        let congested = match answer {
            Answer::Latency(latency) => self.is_late(latency, alone),
            Answer::Finished => false,
            // A failure of work that took its permit while others were free says only that its
            // peer is slow or refuses the work, since the work didn't contend for the limit.
            Answer::Failed(_) if !contended => return,
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

    /// Takes the last free permit, so that its work takes its permit while the limit is in full
    /// use. It keeps each other free permit it takes in `held`, and where no permit is free, it
    /// releases one of `held` first.
    fn take_last(limit: &Limit, held: &mut Vec<Permit>) -> Permit {
        loop {
            match limit.try_acquire() {
                Some(permit) if permit.saturated_at_start => return permit,
                Some(permit) => held.push(permit),
                None => {
                    let permit = held.pop().expect("work holds a permit");
                    permit.release(Answer::Neutral);
                }
            }
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

        // Four pieces of work each take the last free permit: the limit is in full use as each
        // takes its permit.
        let mut held = vec![];
        let first = take_last(&limit, &mut held);
        let second = take_last(&limit, &mut held);
        let third = take_last(&limit, &mut held);
        let fourth = take_last(&limit, &mut held);
        first.release(Answer::Failed(peer(1)));
        assert_eq!(limit.current(), 8);
        // A second peer's failure since the last clean answer halves the limit.
        second.release(Answer::Failed(peer(2)));
        assert_eq!(limit.current(), 4);
        // The 6 permits still out are more than the new limit of 4, so no permit is free.
        assert!(limit.try_acquire().is_none());
        // Their work started under the larger limit, so their answers don't change it: neither
        // the failures of two more peers nor the clean answers after them.
        third.release(Answer::Failed(peer(3)));
        fourth.release(Answer::Failed(peer(4)));
        for permit in held {
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
        let mut held = vec![];
        take_last(&limit, &mut held).release(Answer::Failed(peer(1)));
        take_last(&limit, &mut held).release(Answer::Failed(peer(2)));
        drop(held);
        assert_eq!(limit.current(), 4);

        // Two of the four clean answers that a round at 4 needs, then failures of two peers.
        let mut permits = take_all(&limit).into_iter();
        for permit in permits.by_ref().take(2) {
            permit.release(Answer::Finished);
        }
        let mut held: Vec<_> = permits.collect();
        take_last(&limit, &mut held).release(Answer::Failed(peer(1)));
        take_last(&limit, &mut held).release(Answer::Failed(peer(2)));
        assert_eq!(limit.current(), 2);
        drop(held);

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

        let mut old = vec![];
        take_last(&limit, &mut old).release(Answer::Failed(peer(1)));
        take_last(&limit, &mut old).release(Answer::Failed(peer(2)));
        assert_eq!(limit.current(), 8);
        // Of the 15 permits still out, the first 7 that come back are retired, and the next 2
        // are free.
        for permit in old.drain(..9) {
            permit.release(Answer::Neutral);
        }
        let first = limit.try_acquire().unwrap();
        let second = limit.try_acquire().unwrap();
        assert!(limit.try_acquire().is_none());
        // The decrease started the count of failed peers again, so this failure alone leaves
        // the limit. The second permit is the last free one, and so is the one taken after it.
        second.release(Answer::Failed(peer(1)));
        assert_eq!(limit.current(), 8);
        take_last(&limit, &mut old).release(Answer::Failed(peer(2)));
        assert_eq!(limit.current(), 4);
        drop(first);

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
    fn failures_of_two_peers_whose_work_didnt_contend_for_the_limit_leave_it() {
        let limit = Limit::new();
        round(&limit, Answer::Finished);
        round(&limit, Answer::Finished);
        // Each piece of work takes its permit while three others are free, so it doesn't contend
        // for the limit, and the failures of two peers leave it.
        for port in [1, 2, 1, 2] {
            let permit = limit.try_acquire().unwrap();
            permit.release(Answer::Failed(peer(port)));
        }
        assert_eq!(limit.current(), 4);

        // This work takes its permit while three others are free too, though other work takes the
        // last free permit while it runs, so its failure doesn't count toward a decrease either:
        // the failure of another peer's work that took the last free permit leaves the limit.
        let mut held = vec![limit.try_acquire().unwrap()];
        let last = take_last(&limit, &mut held);
        held.remove(0).release(Answer::Failed(peer(2)));
        last.release(Answer::Failed(peer(1)));
        assert_eq!(limit.current(), 4);
    }

    #[test]
    fn failures_of_two_peers_whose_work_contended_for_the_limit_halve_it() {
        let limit = Limit::new();
        round(&limit, Answer::Finished);
        round(&limit, Answer::Finished);
        let mut held = vec![];
        take_last(&limit, &mut held).release(Answer::Failed(peer(1)));
        assert_eq!(limit.current(), 4);
        take_last(&limit, &mut held).release(Answer::Failed(peer(2)));
        assert_eq!(limit.current(), 2);
    }

    #[test]
    fn a_clean_answer_between_two_peers_failures_keeps_the_limit() {
        let limit = Limit::new();
        round(&limit, Answer::Finished);
        round(&limit, Answer::Finished);

        let mut held = vec![];
        take_last(&limit, &mut held).release(Answer::Failed(peer(1)));
        // The clean answer starts the count of failed peers again, and grows the limit.
        held.pop().unwrap().release(Answer::Finished);
        assert_eq!(limit.current(), 5);
        take_last(&limit, &mut held).release(Answer::Failed(peer(2)));
        assert_eq!(limit.current(), 5);
        // A failure of a second peer since the last clean answer halves the limit.
        take_last(&limit, &mut held).release(Answer::Failed(peer(1)));
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
