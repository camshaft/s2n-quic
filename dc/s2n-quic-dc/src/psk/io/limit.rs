// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A limit on the work that runs at once, which the work's answers set instead of a fixed
//! number.
//!
//! The limit starts at one. It grows by one for each clean answer until an answer first signals
//! congestion, which doubles it with each round of answers, and by one for each round of clean
//! answers after that. An answer that signals congestion halves it, down to one: the work timed
//! out, the peer refused it, or it took more than twice the base time. Only work that ran while
//! the limit was in full use grows it, and an answer of work that started before a decrease
//! doesn't change it. Work past the limit waits for a permit, in the order it asked for one.

use std::{
    io,
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
    /// The work signals congestion: it timed out, or the peer refused it.
    Congested,
    /// The work's end doesn't say anything about the limit, as with a TLS failure or a cancel.
    Neutral,
}

impl Answer {
    /// Reads what a failed handshake says about the limit: a handshake that timed out, or that
    /// the peer refused, signals congestion, and any other failure doesn't say anything.
    pub(super) fn for_error(error: &io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::ConnectionRefused => Self::Congested,
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
            Answer::Congested => true,
            Answer::Neutral => return,
        };
        if congested {
            self.decrease(permits);
        } else if full {
            self.increase(permits);
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
        permits.next().unwrap().release(Answer::Congested);
        assert_eq!(limit.current(), 4);
        // The 7 permits still out are past the new limit, so no permit is free.
        assert!(limit.try_acquire().is_none());
        // Their work started under the larger limit, so their answers don't change it again.
        for permit in permits {
            permit.release(Answer::Congested);
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
        round(&limit, Answer::Congested);
        assert_eq!(limit.current(), 4);

        // Three of the four clean answers that a round at 4 needs, then congestion.
        let mut permits = take_all(&limit).into_iter();
        for permit in permits.by_ref().take(3) {
            permit.release(Answer::Finished);
        }
        permits.next().unwrap().release(Answer::Congested);
        assert_eq!(limit.current(), 2);

        // The count starts again, so the limit grows only once a whole round at 2 is clean.
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(Answer::Finished);
        assert_eq!(limit.current(), 2);
        permits.next().unwrap().release(Answer::Finished);
        assert_eq!(limit.current(), 3);
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
        round(&limit, Answer::Congested);
        round(&limit, Answer::Congested);
        assert_eq!(limit.current(), 1);
    }

    #[test]
    fn a_handshake_that_timed_out_or_was_refused_signals_congestion() {
        for (kind, answer) in [
            (io::ErrorKind::TimedOut, Answer::Congested),
            (io::ErrorKind::ConnectionRefused, Answer::Congested),
            (io::ErrorKind::ConnectionReset, Answer::Neutral),
            (io::ErrorKind::Other, Answer::Neutral),
        ] {
            assert_eq!(Answer::for_error(&io::Error::from(kind)), answer);
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
