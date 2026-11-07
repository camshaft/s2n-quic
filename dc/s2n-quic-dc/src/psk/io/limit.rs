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
/// which the server sends once it has the client's.
const HANDSHAKE_ROUND_TRIPS: u32 = 2;

/// The round trips that a connection measured: its lowest round-trip time, and the part of it that
/// the packet of that round trip waited in the endpoint's receive queue, as the socket measured
/// it, or else the most that the packet could have waited during the endpoint's last pass, and
/// the handshake's two round trips as the client timed them, with the part of each that the
/// server's reply waited in the endpoint's receive queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RoundTrips {
    pub(super) lowest: Duration,
    /// The part of the lowest round trip that the round trip's packet waited in the endpoint's
    /// receive queue. Where `measured`, it is the whole wait from the socket's receive time of
    /// the packet to the endpoint's read of its batch of datagrams, through each pass of the
    /// endpoint's event loop that ran meanwhile. The endpoint reads its clock once for a batch,
    /// as it handles the batch's first datagram, and the wait of each datagram of the batch ends
    /// at one read of the realtime clock just before that, so the wait doesn't count the time
    /// that the endpoint spent on the datagrams before it.
    ///
    /// Otherwise it is the part of the lowest round trip that the endpoint's pass of its event
    /// loop before the pass that read the round trip's packet ran. The endpoint reads a packet
    /// that arrives while a pass runs in its next pass, so this part is the most that the packet
    /// could have waited during the endpoint's last pass. A packet that waited through more than
    /// one pass, or past the end of the last pass, waited more. A packet that arrived after the
    /// pass began waited less, and this part also counts the time that it was still on the path.
    pub(super) waited: Duration,
    /// Whether `waited` is the wait that the socket measured for the packet, instead of the bound
    /// of the endpoint's last pass.
    pub(super) measured: bool,
    /// The handshake's two round trips as the client timed them, each from a flight it sent to
    /// the server's reply to that flight. The handshake's time holds both, so a peer that delays
    /// its reply adds as much to that time as to the round trip.
    pub(super) flights: [Duration; 2],
    /// The part of each of `flights` that the server's reply to its flight waited in the
    /// endpoint's receive queue, as the socket measured it for the reply's datagram, or `None`
    /// where the datagram didn't hold a measured wait or the round trip isn't timed. A wait that
    /// grows between the two round trips, as when the endpoint's load rises during the handshake,
    /// raises the second round trip by the growth, which isn't a longer path.
    pub(super) flight_waits: [Option<Duration>; 2],
}

impl RoundTrips {
    /// The lowest round-trip time less the part of it that its packet waited, or could have
    /// waited, in the endpoint's receive queue. Where `measured`, it is the time that the path
    /// took for that round trip, as `waited` ends where the round trip ends, at the endpoint's
    /// read of the round trip's batch. Otherwise it is the least time that the path
    /// took, where the packet waited during the endpoint's last pass alone. Where the packet
    /// arrived after the pass began, the path took more, and a handshake's time read against this
    /// counts the rest as the endpoint's time.
    pub(super) fn path(self) -> Duration {
        self.lowest.saturating_sub(self.waited)
    }

    /// The lowest round-trip time and the handshake's two round trips that the spread compares.
    /// Where the socket measured the wait of the reply of the lowest round trip and of the reply
    /// of each of the two, each wait below its round trip, they are each round trip less its
    /// reply's wait: the endpoint's receive queue doesn't raise them, so a wait that grows
    /// between the round trips stays in the handshake's time, and a far path whose round trips
    /// vary still shows that spread. Otherwise, as where a wait that the socket over-counted
    /// reaches its round trip, they are the round trips as the connection measured them.
    fn compared(self) -> (Duration, [Duration; 2]) {
        let [first, second] = self.flights;
        match (self.measured, self.flight_waits) {
            (true, [Some(first_wait), Some(second_wait)])
                if self.waited < self.lowest && first_wait < first && second_wait < second =>
            {
                let first = first.saturating_sub(first_wait);
                let second = second.saturating_sub(second_wait);
                (self.path(), [first, second])
            }
            _ => (self.lowest, self.flights),
        }
    }

    /// The spread of the longer of the handshake's two round trips above the lowest round-trip
    /// time, each as `RoundTrips::compared` reads it.
    pub(super) fn spread(self) -> Duration {
        let (lowest, [first, second]) = self.compared();
        first.max(second).saturating_sub(lowest)
    }

    /// The time of the handshake's two round trips together, as `RoundTrips::compared` reads
    /// them.
    pub(super) fn flights_time(self) -> Duration {
        let (_, [first, second]) = self.compared();
        first.saturating_add(second)
    }
}

/// The round-trip time that a handshake's path accounts for, from `earlier`, the round-trip time
/// that the earlier handshakes with the same peer kept, as `kept_rtt` keeps it, and `rtt`, the
/// round trips that this handshake's connection measured. Where the socket measured the wait of
/// the reply of the lowest round trip, `RoundTrips::path` counts whatever the earlier time: it is
/// the time that this handshake's path took, so it rises above an earlier time below it, as where
/// the route to the peer is longer than the last handshake's, and it falls below an earlier time
/// above it, which may hold the wait of an earlier connection's reply.
///
/// Otherwise, this connection's lowest round trip is the most that the path took, so an earlier
/// time above it counts as the lowest. An earlier time below it may come from a connection whose
/// packets waited less in the endpoint's receive queue, so counting it keeps the rest of this
/// connection's wait in the handshake's time. An earlier time below `RoundTrips::path` counts as
/// it is too: the reply may have waited past the endpoint's last pass, as with a late task wake
/// or a socket that one pass doesn't drain, and this connection's round trips can't tell that
/// wait from a longer route to the peer. Without an earlier time, `RoundTrips::path` counts, and
/// without this connection's round trips, the earlier time.
pub(super) fn path_rtt(earlier: Option<Duration>, rtt: Option<RoundTrips>) -> Option<Duration> {
    match (earlier, rtt) {
        (_, Some(rtt)) if rtt.measured => Some(rtt.path()),
        (Some(earlier), Some(rtt)) => Some(earlier.min(rtt.lowest)),
        (earlier, None) => earlier,
        (None, Some(rtt)) => Some(rtt.path()),
    }
}

/// The time that a handshake's path accounts for: `HANDSHAKE_ROUND_TRIPS` round trips of the
/// time that `path_rtt` reads from `earlier_rtt` and `rtt`, and the spread of `rtt` once, up to
/// the time of its two round trips together, and never below the round trips alone.
pub(super) fn handshake_path(earlier_rtt: Option<Duration>, rtt: Option<RoundTrips>) -> Duration {
    let round_trips = match path_rtt(earlier_rtt, rtt) {
        Some(path_rtt) => path_rtt.saturating_mul(HANDSHAKE_ROUND_TRIPS),
        None => Duration::ZERO,
    };
    match rtt {
        Some(rtt) => round_trips
            .saturating_add(rtt.spread())
            .min(rtt.flights_time())
            .max(round_trips),
        None => round_trips,
    }
}

/// The round-trip time that a handshake keeps with the peer's map entry, which the next handshake
/// with the peer reads as its earlier time. Where the socket measured the wait of the reply of
/// the lowest round trip, it is `RoundTrips::path`, whatever `earlier`: the measured wait doesn't
/// count time that the reply was on the path, so the kept time is the time of this handshake's
/// path, and it rises above an earlier time below it, as after a longer route to the peer.
///
/// Otherwise it is the lower of `earlier` and this connection's lowest round trip.
/// `RoundTrips::waited` stays in it then, since that part is a bound of the reply's wait and may
/// count time that the reply was still on the path: a kept `RoundTrips::path` could lie below the
/// path, and since the lower of two times only goes down, every later handshake with the peer
/// that doesn't measure its reply's wait would read the difference as the endpoint's time.
pub(super) fn kept_rtt(earlier: Option<Duration>, rtt: Option<RoundTrips>) -> Option<Duration> {
    match (earlier, rtt) {
        (_, Some(rtt)) if rtt.measured => Some(rtt.path()),
        (Some(earlier), Some(rtt)) => Some(earlier.min(rtt.lowest)),
        (earlier, None) => earlier,
        (None, Some(rtt)) => Some(rtt.lowest),
    }
}

/// Reads a handshake that reached dc.complete in `elapsed` from its connect: the round-trip time
/// that it keeps with the peer's map entry, as `kept_rtt` reads it, and the start limit's answer,
/// as `Answer::for_handshake` reads it, each from `earlier_rtt`, the round-trip time that the
/// peer's map entry kept, and `rtt`, the round trips that this handshake's connection measured.
/// The answer reads the same round-trip time for the path as the kept time, except without an
/// earlier time and without a measured wait: it then reads `RoundTrips::path`, and the entry
/// keeps the whole lowest round trip. The answer also reads the spread of the handshake's two
/// round trips, which the kept time leaves out.
pub(super) fn read_handshake(
    elapsed: Duration,
    earlier_rtt: Option<Duration>,
    rtt: Option<RoundTrips>,
) -> (Option<Duration>, Answer) {
    let kept = kept_rtt(earlier_rtt, rtt);
    let answer = Answer::for_handshake(elapsed, earlier_rtt, rtt);
    (kept, answer)
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

    /// Reads a handshake that reached dc.complete in `elapsed` from its connect, past the time its
    /// path accounts for, as `handshake_path` reads it: `HANDSHAKE_ROUND_TRIPS` round trips of the
    /// time that `path_rtt` reads from `earlier_rtt`, the lowest round-trip time that the earlier
    /// handshakes with the same peer measured, and `rtt`, the round trips that this handshake's
    /// connection measured, and the spread of the longer of the handshake's two round trips above
    /// the lowest of `rtt`, once. The limit compares the rest with the base. A handshake with a
    /// farther peer waits longer on its path, and a far path's round trips can vary by more than
    /// twice the base, and neither is congestion. The client times each of the two round trips
    /// itself, from a flight it sent to the server's reply, and the spread counts only up to the
    /// time of the two together, which the handshake's time holds: a peer that delays its reply,
    /// or holds the acknowledgement that a recovery sample reads, doesn't take more out of the
    /// handshake's time than it added to it. A connection's own round trips also hold the time
    /// their packets waited in the endpoint's receive queue, which grows under load. Where the
    /// socket measured the wait of the packet of the lowest round trip, `path_rtt` doesn't count
    /// that whole wait, whatever the earlier time, so that a handshake over a longer route than
    /// the peer's last one doesn't count the longer route as the endpoint's time. Otherwise, an
    /// earlier time from a connection whose packets waited less keeps that wait in the rest, and
    /// without an earlier time, `path_rtt` doesn't count the most that the packet of the lowest
    /// round trip could have waited during the endpoint's last pass. Where the socket measured
    /// the wait of each reply, of the lowest round trip and of the two, each below its round
    /// trip, the spread compares each less its reply's wait, so a wait that grows between the
    /// handshake's round trips, as when the endpoint's load rises during the handshake, stays in
    /// the rest. Otherwise the spread takes out the part of the wait that the longer round trip
    /// held above the lowest. Without either time, the rest is the whole time. Where the round
    /// trips take the whole time, the handshake doesn't show a time to compare, and it answers
    /// `Finished`, a clean answer without a time, so that a time of zero doesn't become the base.
    pub(super) fn for_handshake(
        elapsed: Duration,
        earlier_rtt: Option<Duration>,
        rtt: Option<RoundTrips>,
    ) -> Self {
        let path = handshake_path(earlier_rtt, rtt);
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

    /// The round trips of a connection whose lowest round-trip time took `lowest` ms, and whose
    /// packet couldn't have waited during the endpoint's last pass.
    fn rtt(lowest: u64) -> Option<RoundTrips> {
        rtt_waiting(lowest, 0)
    }

    /// The round trips of a connection whose lowest round-trip time took `lowest` ms, and whose
    /// packet could have waited `waited` ms of it during the endpoint's last pass. Each of its
    /// handshake's two round trips took the lowest.
    fn rtt_waiting(lowest: u64, waited: u64) -> Option<RoundTrips> {
        Some(RoundTrips {
            lowest: ms(lowest),
            waited: ms(waited),
            measured: false,
            flights: [ms(lowest), ms(lowest)],
            flight_waits: [None, None],
        })
    }

    /// The round trips of a connection whose lowest round-trip time took `lowest` ms, and whose
    /// packet waited `waited` ms of it in the endpoint's receive queue, as the socket measured it.
    /// Each of its handshake's two round trips took the lowest.
    fn rtt_measured(lowest: u64, waited: u64) -> Option<RoundTrips> {
        Some(RoundTrips {
            lowest: ms(lowest),
            waited: ms(waited),
            measured: true,
            flights: [ms(lowest), ms(lowest)],
            flight_waits: [None, None],
        })
    }

    /// The round trips of a connection whose lowest round-trip time took `lowest` ms, and whose
    /// handshake's two round trips took `lowest` ms and `highest` ms.
    fn rtt_spread(lowest: u64, highest: u64) -> Option<RoundTrips> {
        flights(lowest, lowest, highest)
    }

    /// The round trips of a connection whose lowest round-trip time took `lowest` ms, and whose
    /// handshake's two round trips took `first` ms and `second` ms.
    fn flights(lowest: u64, first: u64, second: u64) -> Option<RoundTrips> {
        Some(RoundTrips {
            lowest: ms(lowest),
            waited: Duration::ZERO,
            measured: false,
            flights: [ms(first), ms(second)],
            flight_waits: [None, None],
        })
    }

    /// The round trips of a connection whose lowest round-trip time took `lowest` ms, of which
    /// its reply waited `waited` ms, as the socket measured it, and whose handshake's two round
    /// trips each took the first ms of its pair in `round_trips`. The second of the pair is the
    /// wait in ms of the round trip's reply, as the socket measured it.
    fn rtt_waits(
        lowest: u64,
        waited: u64,
        round_trips: [(u64, u64); 2],
    ) -> Option<RoundTrips> {
        let [(first, first_wait), (second, second_wait)] = round_trips;
        Some(RoundTrips {
            lowest: ms(lowest),
            waited: ms(waited),
            measured: true,
            flights: [ms(first), ms(second)],
            flight_waits: [Some(ms(first_wait)), Some(ms(second_wait))],
        })
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

    #[test]
    fn a_handshake_answers_its_time_past_two_round_trips_of_its_path() {
        // A handshake's round trips whose second reply waited 4 ms, where the socket measured the
        // wait of each of the two replies and not the wait of the lowest round trip's reply.
        let unmeasured_lowest = rtt_waits(1, 0, [(1, 0), (5, 4)]).map(|rtt| RoundTrips {
            measured: false,
            ..rtt
        });
        for (elapsed, earlier_rtt, rtt, expected) in [
            (ms(103), None, rtt(50), Answer::Latency(ms(3))),
            // An earlier time below this connection's lowest round trip counts as it is, and
            // one above it as the lowest.
            (ms(103), Some(ms(40)), rtt_waiting(50, 10), Answer::Latency(ms(23))),
            (ms(103), Some(ms(45)), rtt_waiting(50, 10), Answer::Latency(ms(13))),
            (ms(103), Some(ms(60)), rtt(50), Answer::Latency(ms(3))),
            (ms(103), Some(ms(50)), None, Answer::Latency(ms(3))),
            // An earlier time below the lowest round trip less the most that its packet could
            // have waited counts as it is too, since the round trips can't tell a longer route
            // from a wait past the endpoint's last pass.
            (ms(103), Some(ms(40)), rtt(50), Answer::Latency(ms(23))),
            (ms(103), Some(ms(30)), rtt_waiting(50, 10), Answer::Latency(ms(43))),
            // Without an earlier time, `RoundTrips::path` counts, and it doesn't go below zero.
            (ms(103), None, rtt_waiting(50, 10), Answer::Latency(ms(23))),
            (ms(103), None, rtt_waiting(5, 9), Answer::Latency(ms(103))),
            // Where the socket measured the packet's wait, the lowest round trip less that wait
            // counts, whatever the earlier time.
            (ms(103), Some(ms(30)), rtt_measured(50, 10), Answer::Latency(ms(23))),
            (ms(103), Some(ms(45)), rtt_measured(50, 10), Answer::Latency(ms(23))),
            // The spread of the handshake's longer round trip above this connection's lowest
            // counts once, beside two of the lower round trip.
            (ms(120), None, rtt_spread(50, 69), Answer::Latency(ms(1))),
            (ms(120), Some(ms(40)), rtt_spread(50, 69), Answer::Latency(ms(21))),
            // The spread counts only up to the handshake's two round trips together: where the
            // peer held its acknowledgements, so that the lowest round trip its connection's
            // recovery measured took 80 ms though the client timed 50 ms and 120 ms, 30 ms is
            // left past the 170 ms of the two.
            (ms(200), None, flights(80, 50, 120), Answer::Latency(ms(30))),
            // Round trips the client timed below the lowest don't take the path below two of it.
            (ms(161), None, flights(80, 50, 60), Answer::Latency(ms(1))),
            // Where the socket measured the wait of each reply, of the lowest round trip and of
            // the two, each less its reply's wait counts: a wait that grows between the two stays
            // in the time, and a far path's spread counts as before.
            (ms(7), Some(ms(1)), rtt_waits(1, 0, [(1, 0), (5, 4)]), Answer::Latency(ms(5))),
            (ms(120), None, rtt_waits(60, 10, [(60, 10), (79, 10)]), Answer::Latency(ms(1))),
            // Where the socket didn't measure the wait of the lowest round trip's reply, the round
            // trips count as the connection measured them, whatever the waits of the two replies.
            (ms(7), Some(ms(1)), unmeasured_lowest, Answer::Latency(ms(1))),
            // Without a round-trip time, the rest is the whole time.
            (ms(103), None, None, Answer::Latency(ms(103))),
            // Where the round trips take the whole time, the handshake doesn't show a time to
            // compare.
            (ms(100), None, rtt(50), Answer::Finished),
            (ms(90), None, rtt(50), Answer::Finished),
            (ms(119), None, rtt_spread(50, 69), Answer::Finished),
        ] {
            assert_eq!(Answer::for_handshake(elapsed, earlier_rtt, rtt), expected);
        }
        let below = flights(80, 50, 60).unwrap();
        assert_eq!(below.spread(), Duration::ZERO);
        assert_eq!(below.flights_time(), ms(110));
        let grown = rtt_waits(1, 0, [(1, 0), (5, 4)]).unwrap();
        assert_eq!(grown.spread(), Duration::ZERO);
        assert_eq!(grown.flights_time(), ms(2));
        // A far path's round trips, each less its reply's wait, keep their spread.
        let far = rtt_waits(60, 10, [(60, 10), (79, 10)]).unwrap();
        assert_eq!(far.spread(), ms(19));
        assert_eq!(far.flights_time(), ms(119));
        // A wait at or above its round trip, as where the socket over-counted it, leaves the
        // round trips as the connection measured them, and so does a wait that reaches the
        // lowest round trip.
        for over in [
            rtt_waits(1, 0, [(1, 1), (5, 4)]),
            rtt_waits(1, 0, [(1, 0), (5, 5)]),
            rtt_waits(1, 1, [(1, 0), (5, 4)]),
        ] {
            let over = over.unwrap();
            assert_eq!(over.spread(), ms(4));
            assert_eq!(over.flights_time(), ms(6));
        }
        // Without the lowest round trip's measured wait, the 4 ms that the second reply waited
        // counts as the spread.
        let unmeasured_lowest = unmeasured_lowest.unwrap();
        assert_eq!(unmeasured_lowest.spread(), ms(4));
        assert_eq!(unmeasured_lowest.flights_time(), ms(6));
    }

    #[test]
    fn handshakes_with_a_near_and_a_far_peer_at_once_dont_lower_the_limit() {
        // A near peer's round trip takes 1 ms, and its handshake 3 ms: the endpoint added 1 ms.
        let near = Answer::for_handshake(ms(3), None, rtt(1));
        // A far peer's round trip takes 50 ms, and its handshake 101 ms: the endpoint added 1 ms.
        let far = Answer::for_handshake(ms(101), None, rtt(50));
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
        let far_without_its_path = Answer::for_handshake(ms(101), None, None);
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(far_without_its_path);
        assert_eq!(limit.current(), 4);
    }

    #[test]
    fn a_far_handshake_whose_endpoint_time_is_past_twice_the_base_is_congestion() {
        let limit = Limit::new();
        round(&limit, Answer::for_handshake(ms(3), None, rtt(1)));
        // The far peer's round trips account for 100 ms of its 103 ms, and the endpoint's 3 ms
        // is past twice the 1 ms base.
        let far = Answer::for_handshake(ms(103), None, rtt(50));
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(far);
        assert_eq!(limit.current(), 1);
    }

    #[test]
    fn a_far_handshake_whose_round_trips_spread_above_its_lowest_doesnt_lower_the_limit() {
        // A near peer's round trip takes 1 ms, and its handshake 3 ms: the endpoint added 1 ms.
        let near = Answer::for_handshake(ms(3), None, rtt(1));
        let limit = Limit::new();
        // The near peer's handshake runs alone, and its 1 ms is the base.
        round(&limit, near);
        assert_eq!(limit.current(), 2);

        // A far peer's lowest round trip takes 50 ms, its handshake's two round trips 50 ms and
        // 69 ms, and its handshake 120 ms: past two of its lowest round trip, 20 ms is left, the
        // 19 ms spread of its longer round trip and the 1 ms the endpoint added. Beside the near
        // peer's handshake, the limit grows.
        let far = Answer::for_handshake(ms(120), None, rtt_spread(50, 69));
        assert_eq!(far, Answer::Latency(ms(1)));
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(far);
        permits.next().unwrap().release(near);
        assert_eq!(limit.current(), 4);

        // Read without the spread of its round trips, the same handshake's 20 ms is past twice
        // the base, which halves the limit.
        let without_its_spread = Answer::for_handshake(ms(120), None, rtt(50));
        assert_eq!(without_its_spread, Answer::Latency(ms(20)));
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(without_its_spread);
        assert_eq!(limit.current(), 2);
    }

    #[test]
    fn a_handshake_whose_round_trips_take_its_whole_time_doesnt_set_the_base() {
        let limit = Limit::new();
        // The handshake runs alone, and its 1 ms past its round trips is the base.
        round(&limit, Answer::for_handshake(ms(3), None, rtt(1)));
        assert_eq!(limit.current(), 2);

        // Beside a handshake whose round trips take its whole time, a handshake with 2 ms past
        // its round trips is within twice the base, and the limit grows.
        let finished = Answer::for_handshake(ms(90), None, rtt(50));
        let within = Answer::for_handshake(ms(4), None, rtt(1));
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
        round(&limit, Answer::for_handshake(ms(3), None, rtt(1)));
        assert_eq!(limit.current(), 2);

        // While the endpoint is busy, its receive queue holds each packet of the next handshake
        // with the peer for 4 ms, so that handshake's connection measures a 5 ms round trip, and
        // its two round trips take 10 ms of its 11 ms. Where its packet waited past the endpoint's
        // last pass, the connection can't tell any of that wait from its path. Against the
        // earlier connection's 1 ms round trip, 9 ms is past twice the base, which halves the
        // limit.
        let late = Answer::for_handshake(ms(11), Some(ms(1)), rtt(5));
        assert_eq!(late, Answer::Latency(ms(9)));
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(late);
        assert_eq!(limit.current(), 1);

        // Read against its own round trips alone, the same handshake shows 1 ms, within twice
        // the base.
        assert_eq!(
            Answer::for_handshake(ms(11), None, rtt(5)),
            Answer::Latency(ms(1))
        );
    }

    #[test]
    fn a_first_handshake_whose_round_trips_hold_the_endpoints_wait_halves_the_limit() {
        let limit = Limit::new();
        // A near peer's handshake runs alone: its connection measures a 1 ms round trip, and the
        // 1 ms past two of them is the base.
        round(&limit, Answer::for_handshake(ms(3), None, rtt(1)));
        assert_eq!(limit.current(), 2);

        // While the endpoint is busy, its receive queue holds each packet of a handshake with a
        // peer that no earlier handshake measured for 4 ms, so that handshake's connection
        // measures a 5 ms round trip, 4 ms of which the endpoint's pass before the one that read
        // its packet ran. Its path takes the 1 ms left of each round trip, and 9 ms of its 11 ms
        // is past twice the base, which halves the limit.
        let late = Answer::for_handshake(ms(11), None, rtt_waiting(5, 4));
        assert_eq!(late, Answer::Latency(ms(9)));
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(late);
        assert_eq!(limit.current(), 1);

        // Read against its whole round trips, the same handshake shows 1 ms, within twice the
        // base.
        assert_eq!(
            Answer::for_handshake(ms(11), None, rtt(5)),
            Answer::Latency(ms(1))
        );
    }

    #[test]
    fn a_near_handshake_whose_wait_grows_between_its_round_trips_still_halves_the_limit() {
        let limit = Limit::new();
        // A near peer's handshake runs alone: its connection measures a 1 ms round trip, and the
        // 1 ms past two of them is the base.
        round(&limit, Answer::for_handshake(ms(3), None, rtt(1)));
        assert_eq!(limit.current(), 2);

        // The endpoint's load rises during the next handshake with the peer. Its lowest round
        // trip takes 1 ms, and the socket measured that the reply didn't wait. The handshake's
        // first round trip takes 1 ms, and its second 5 ms, 4 ms of which the reply waited in
        // the endpoint's receive queue, as the socket measured it. Less each reply's wait, both
        // round trips take 1 ms, so the spread is zero, and 5 ms of the handshake's 7 ms is past
        // two round trips, past twice the base, which halves the limit.
        let grown = rtt_waits(1, 0, [(1, 0), (5, 4)]);
        let late = Answer::for_handshake(ms(7), Some(ms(1)), grown);
        assert_eq!(late, Answer::Latency(ms(5)));
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(late);
        assert_eq!(limit.current(), 1);

        // Read with the round trips as the connection measured them, the 4 ms that the second
        // reply waited counts as the spread, and the same handshake shows 1 ms, within twice the
        // base. Where the socket didn't measure the wait of the second reply, the handshake reads
        // its round trips that way too.
        let as_measured = Answer::for_handshake(ms(7), Some(ms(1)), flights(1, 1, 5));
        assert_eq!(as_measured, Answer::Latency(ms(1)));
        let unmeasured = grown.map(|rtt| RoundTrips {
            flight_waits: [Some(ms(0)), None],
            ..rtt
        });
        assert_eq!(
            Answer::for_handshake(ms(7), Some(ms(1)), unmeasured),
            Answer::Latency(ms(1))
        );
    }

    #[test]
    fn the_path_counts_the_lower_of_an_earlier_time_and_this_connections_lowest_round_trip() {
        for (earlier, rtt, expected) in [
            // An earlier time below this connection's 50 ms lowest round trip counts as it is,
            // whether it is above or below the 40 ms left of that round trip past the 10 ms its
            // packet could have waited, and one above the lowest round trip counts as the lowest.
            (Some(ms(45)), rtt_waiting(50, 10), Some(ms(45))),
            (Some(ms(30)), rtt_waiting(50, 10), Some(ms(30))),
            (Some(ms(60)), rtt_waiting(50, 10), Some(ms(50))),
            // Where the socket measured the 10 ms that the packet waited, the 40 ms left counts
            // whatever the earlier time: it rises above an earlier time below it, and falls below
            // one above it.
            (Some(ms(30)), rtt_measured(50, 10), Some(ms(40))),
            (Some(ms(45)), rtt_measured(50, 10), Some(ms(40))),
            (Some(ms(60)), rtt_measured(50, 10), Some(ms(40))),
            (None, rtt_measured(50, 10), Some(ms(40))),
            // Without an earlier time, the 40 ms counts, and without this connection's round
            // trips, the earlier time.
            (None, rtt_waiting(50, 10), Some(ms(40))),
            (Some(ms(30)), None, Some(ms(30))),
            (None, None, None),
        ] {
            assert_eq!(path_rtt(earlier, rtt), expected);
        }
    }

    #[test]
    fn a_handshake_keeps_the_lower_of_an_earlier_time_and_its_lowest_round_trip() {
        for (earlier, rtt, expected) in [
            // The part of the lowest round trip that its packet could have waited stays in the
            // kept time.
            (None, rtt_waiting(50, 10), Some(ms(50))),
            (Some(ms(30)), rtt_waiting(50, 10), Some(ms(30))),
            (Some(ms(60)), rtt_waiting(50, 10), Some(ms(50))),
            // Where the socket measured the 10 ms that the packet waited, the handshake keeps the
            // 40 ms left, whatever the earlier time.
            (None, rtt_measured(50, 10), Some(ms(40))),
            (Some(ms(30)), rtt_measured(50, 10), Some(ms(40))),
            (Some(ms(60)), rtt_measured(50, 10), Some(ms(40))),
            (Some(ms(30)), None, Some(ms(30))),
            (None, None, None),
        ] {
            assert_eq!(kept_rtt(earlier, rtt), expected);
        }
    }

    #[test]
    fn a_repeat_handshake_after_a_first_whose_reply_could_have_waited_answers_as_before() {
        // A first handshake with a peer measures a 50 ms round trip, and the endpoint's last pass
        // ran for 10 ms of it after its packet was sent, though the reply may have been on the
        // path for all of those 10 ms. Its own answer takes the 40 ms left out of its time twice,
        // and it keeps the whole 50 ms with the peer's entry.
        let first = rtt_waiting(50, 10);
        let first_answer = Answer::for_handshake(ms(101), None, first);
        assert_eq!(first_answer, Answer::Latency(ms(21)));
        let kept = kept_rtt(None, first);
        assert_eq!(kept, Some(ms(50)));

        // The next handshake with the peer runs alone and takes 101 ms over the same 50 ms round
        // trip. Against the kept 50 ms, it answers the 1 ms that the endpoint added; against a
        // kept 40 ms, it would answer 21 ms.
        let next = Answer::for_handshake(ms(101), kept, rtt(50));
        assert_eq!(next, Answer::Latency(ms(1)));
        assert_eq!(kept_rtt(kept, rtt(50)), Some(ms(50)));
    }

    #[test]
    fn a_handshake_keeps_its_whole_lowest_round_trip_and_answers_against_its_path() {
        // A first handshake with a peer measures a 50 ms round trip, 10 ms of which its reply
        // could have waited during the endpoint's last pass, and takes 101 ms. It keeps the whole
        // 50 ms with the peer's entry, and its answer takes the 40 ms left out of its time twice.
        assert_eq!(
            read_handshake(ms(101), None, rtt_waiting(50, 10)),
            (Some(ms(50)), Answer::Latency(ms(21)))
        );

        // With an earlier time, the handshake keeps the lower of it and its lowest round trip,
        // and its answer takes that time out of its time twice.
        assert_eq!(
            read_handshake(ms(101), Some(ms(40)), rtt_waiting(50, 10)),
            (Some(ms(40)), Answer::Latency(ms(21)))
        );
        assert_eq!(
            read_handshake(ms(101), Some(ms(60)), rtt(50)),
            (Some(ms(50)), Answer::Latency(ms(1)))
        );

        // Without this connection's round trips, it keeps and reads the earlier time, and
        // without either time, it doesn't keep a time and its answer is the whole time.
        assert_eq!(
            read_handshake(ms(101), Some(ms(30)), None),
            (Some(ms(30)), Answer::Latency(ms(41)))
        );
        assert_eq!(
            read_handshake(ms(101), None, None),
            (None, Answer::Latency(ms(101)))
        );
    }

    #[test]
    fn a_handshake_over_a_longer_route_than_the_peers_last_doesnt_lower_the_limit() {
        let limit = Limit::new();
        // A handshake with a peer whose route takes 1 ms runs alone and takes 3 ms, and the
        // socket measured that its reply didn't wait. It keeps the 1 ms with the peer's entry,
        // and the 1 ms that the endpoint added is the base.
        let (kept, near) = read_handshake(ms(3), None, rtt_measured(1, 0));
        assert_eq!(kept, Some(ms(1)));
        round(&limit, near);
        assert_eq!(limit.current(), 2);

        // The route to the peer changes, so that the next handshake's round trips take 50 ms, and
        // that handshake takes 101 ms, with a reply that didn't wait. It reads its own 50 ms path
        // instead of the kept 1 ms, answers the 1 ms that the endpoint added, and keeps the 50 ms,
        // so the limit keeps doubling.
        let (kept, longer) = read_handshake(ms(101), kept, rtt_measured(50, 0));
        assert_eq!(longer, Answer::Latency(ms(1)));
        assert_eq!(kept, Some(ms(50)));
        round(&limit, longer);
        assert_eq!(limit.current(), 4);

        // Read against the kept 1 ms, as a handshake without a measured wait reads it, the same
        // handshake shows 99 ms, past twice the base.
        assert_eq!(
            Answer::for_handshake(ms(101), Some(ms(1)), rtt(50)),
            Answer::Latency(ms(99))
        );

        // A repeat handshake over the same route measures a 58 ms round trip, since its reply
        // waited 8 ms through two of the endpoint's passes, and takes 117 ms. Less the measured
        // wait, its path takes 50 ms, and the 17 ms past two round trips of it is past twice the
        // base, which halves the limit.
        let (kept, repeat) = read_handshake(ms(117), kept, rtt_measured(58, 8));
        assert_eq!(repeat, Answer::Latency(ms(17)));
        assert_eq!(kept, Some(ms(50)));
        let mut permits = take_all(&limit).into_iter();
        permits.next().unwrap().release(repeat);
        assert_eq!(limit.current(), 2);
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
