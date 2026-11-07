// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Pacing of each local interface's send sockets at that interface's send rate.
//!
//! [`SendPacing`] holds the send rate of each interface that holds a send socket, and the
//! interface of each send socket. Where every send socket is on one interface, frame dispatch
//! paces them together with one [`Paced`](crate::socket::channel::Paced) stage before
//! [`PickTwo`](super::PickTwo) chooses a worker. Where they are on several, frame dispatch uses
//! [`InterfacePacedPickTwo`], which chooses the worker of each batch first and then paces the
//! batch with the token bucket of that worker's interface, so that a batch waiting for one
//! interface's tokens doesn't hold a batch for another interface behind it.
//!
//! Where the endpoint chooses an interface for its highest-priority traffic, a batch whose frames
//! are all of the highest priority level goes only to a worker of that interface.
//! Frame dispatch then batches the frames of that level apart from the others, and passes
//! [`InterfacePacedPickTwo`] an input of its own for them.

use super::{AssignSender, FlowCredits, HighestPriority, PathSecretMapEntry};
use crate::{
    credentials,
    endpoint::{
        edt::pick_two_worse_probability,
        id::{Id, IdMap, LocalSenderId},
    },
    path::secret::map::Entry as PathSecretEntry,
    socket::{
        channel::{Budget, ByteCost, Receiver, UnboundedSender},
        interface,
        rate::{Rate, TokenBucket},
    },
    time::precision,
};
use core::{
    task::{self, Poll},
    time::Duration,
};
use std::collections::{HashMap, VecDeque};

/// The batches of one peer that [`InterfacePacedPickTwo`] read from `rx` and that wait in it.
struct WaitingPeer<T> {
    /// The peer's batches, in the order the router read them.
    batches: VecDeque<T>,
    /// Whether the peer waits on each interface, by the interface's position in the pacers: an
    /// entry of the peer in that interface's waiters stands while it is set.
    waits_on: Vec<bool>,
}

/// How frame dispatch paces the send sockets: the send rate of each interface that holds a send
/// socket, and the interface of each send socket.
#[derive(Clone, Debug, PartialEq)]
pub struct SendPacing {
    /// The send rate of each paced interface, by the interface's position here.
    rates: Vec<Rate>,
    /// The position in `rates` of each send socket's interface, by the send socket's position.
    /// Empty where `rates` holds one rate, which paces every send socket.
    socket_interfaces: Vec<usize>,
    /// The position in `rates` of the interface that carries the highest-priority traffic,
    /// where the endpoint chooses one.
    highest_priority: Option<usize>,
}

impl SendPacing {
    /// Paces every send socket together at `rate`.
    pub fn single(rate: Rate) -> Self {
        Self {
            rates: vec![rate],
            socket_interfaces: Vec::new(),
            highest_priority: None,
        }
    }

    /// Paces send socket `i` at `rates[socket_interfaces[i]]`, together with the other send
    /// sockets of that interface.
    ///
    /// # Panics
    ///
    /// Where `rates` is empty, or where `socket_interfaces` holds a position past the end of
    /// `rates`.
    pub fn new(rates: Vec<Rate>, socket_interfaces: Vec<usize>) -> Self {
        assert!(!rates.is_empty(), "send pacing needs at least one rate");
        assert!(
            socket_interfaces
                .iter()
                .all(|&position| position < rates.len()),
            "a send socket's interface is past the end of the send rates"
        );
        Self {
            rates,
            socket_interfaces,
            highest_priority: None,
        }
    }

    /// Sends the batches whose frames are all of the highest priority level only on the sockets
    /// of the interface at `position`.
    ///
    /// # Panics
    ///
    /// Where `position` is past the end of the send rates.
    pub fn with_highest_priority(mut self, position: usize) -> Self {
        assert!(
            position < self.rates.len(),
            "the interface for highest-priority traffic is past the end of the send rates"
        );
        self.highest_priority = Some(position);
        self
    }

    /// Paces the send sockets of each interface of `interfaces` that holds a send socket together,
    /// at that interface's send rate. Where every send socket is on one interface, this is
    /// [`Self::single`] at that interface's send rate, and where there isn't a send socket, at
    /// `overall_send_rate`.
    /// Where the table has an interface for the highest-priority traffic and more than one
    /// interface holds a send socket, the batches whose frames are all of the highest priority
    /// level go only on the sockets of that interface.
    pub fn from_interfaces(interfaces: &interface::Table, overall_send_rate: Rate) -> Self {
        let mut ids = interfaces.send_sockets().to_vec();
        ids.sort_unstable();
        ids.dedup();
        let rate_of = |id: interface::Id| {
            interfaces
                .get(id)
                .map_or(overall_send_rate, |found| found.send_rate)
        };
        if ids.len() <= 1 {
            return Self::single(ids.first().map_or(overall_send_rate, |&id| rate_of(id)));
        }
        let rates = ids.iter().map(|&id| rate_of(id)).collect();
        let socket_interfaces = interfaces
            .send_sockets()
            .iter()
            .map(|id| ids.binary_search(id).unwrap_or(0))
            .collect();
        let pacing = Self::new(rates, socket_interfaces);
        match interfaces
            .highest_priority()
            .and_then(|id| ids.binary_search(&id).ok())
        {
            Some(position) => pacing.with_highest_priority(position),
            None => pacing,
        }
    }

    /// Returns how many interfaces the send sockets are paced on, each with a token bucket of
    /// its own.
    pub fn interface_count(&self) -> usize {
        self.rates.len()
    }

    /// Returns the send rate of all send sockets together: the sum of the paced interfaces' send
    /// rates.
    pub fn total_rate(&self) -> Rate {
        match self.rates.as_slice() {
            [rate] => *rate,
            rates => Rate::new(rates.iter().map(Rate::gigabits_per_second).sum()),
        }
    }

    /// Returns the send rate of the paced interface at `position`.
    fn rate(&self, position: usize) -> Rate {
        self.rates[position]
    }

    /// Returns the position of the interface of the send socket at `socket`.
    fn interface_of(&self, socket: usize) -> usize {
        self.socket_interfaces.get(socket).copied().unwrap_or(0)
    }

    /// Returns the position of the interface that carries the highest-priority traffic, where
    /// the endpoint chooses one.
    pub fn highest_priority(&self) -> Option<usize> {
        self.highest_priority
    }
}

/// The token bucket that paces the send sockets of one interface, and the batch it holds for one
/// of them until the bucket has paid for the batches sent before it.
struct Pacer<T, Tm> {
    rate: Rate,
    bucket: TokenBucket,
    timer: Tm,
    held: Option<(LocalSenderId, T)>,
}

impl<T, Tm: precision::Timer> Pacer<T, Tm> {
    fn new(timer: Tm, rate: Rate) -> Self {
        let bucket = TokenBucket::new(timer.now(), &rate);
        Self {
            rate,
            bucket,
            timer,
            held: None,
        }
    }

    /// Returns whether the interface may send now: its timer isn't armed, or it fired. Where the
    /// interface may not send yet, the timer wakes `cx` when it fires.
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> bool {
        if !self.timer.is_armed() {
            return true;
        }
        if self.timer.poll_ready(cx).is_ready() {
            self.timer.cancel();
            return true;
        }
        false
    }

    /// Charges the bucket for `bytes` sent at `now`. Where that puts the bucket in debt, arms the
    /// timer for the time the bucket has paid the debt.
    fn charge(&mut self, now: precision::Timestamp, bytes: u64) {
        let sleep_nanos = self.bucket.consume(now, self.rate.nanos_for_bytes(bytes));
        if sleep_nanos > 0 {
            self.timer.update(now + Duration::from_nanos(sleep_nanos));
        }
    }
}

/// Receiver combinator that chooses a send worker for each batch as [`PickTwo`](super::PickTwo)
/// does, and then paces the batch with the token bucket of that worker's interface.
///
/// Each worker's score is its load score plus the debt of its interface's token bucket, so new
/// batches go to the interface that can send them sooner. A batch whose interface may send now
/// goes to its worker at once. Any other batch waits on its interface until the bucket has paid
/// for the batches sent before it; meanwhile the router doesn't choose a worker of that
/// interface, and goes on routing batches to the workers of the other interfaces. While every
/// interface holds a batch, the router doesn't read the next one, unless it has an interface for
/// the highest-priority traffic.
///
/// Where the [`SendPacing`] has an interface for the highest-priority traffic, a batch whose
/// frames are all of the highest priority level goes only to a worker of that interface, and goes
/// at once, charged to that interface's bucket, even while the interface holds a batch. While
/// every interface holds a batch, the router still reads the next batch: it sends a batch of that
/// level at once, and keeps any other batch until an interface may take it. It doesn't read
/// another batch while it keeps one, so that it routes the other batches in the order it reads
/// them.
///
/// Where frame dispatch passes the router an input of its own for the highest priority level,
/// with [`Self::with_highest_priority_input`], the router reads that input before the other one,
/// and sends each of its batches at once to a worker of the interface for the highest-priority
/// traffic, even while every interface holds a batch. It then reads the other input only while an
/// interface doesn't hold a batch, so that it doesn't keep a batch read ahead.
///
/// While the interface pair of a worker is marked lost and another worker of the batch's peer
/// delivers, the router doesn't choose the lost worker, and a highest-priority batch whose
/// interface's workers are all lost goes, paced, to a worker that delivers. A batch of either
/// input that no worker may take now waits in the router, behind the batches of its peer that
/// wait there from the same input, while the router routes the batches of other peers.
///
/// Implements `Receiver<()>` so it can be drained via `ReceiverExt::drain_budgeted`.
pub struct InterfacePacedPickTwo<T, R, S, Clk>
where
    Clk: precision::Clock,
{
    rx: R,
    /// The input of the highest priority level, where frame dispatch passes the router one. The
    /// router drops it once it closes, and doesn't end while it has it.
    highest_rx: Option<R>,
    senders: IdMap<LocalSenderId, S>,
    rng: crate::xorshift::Rng,
    clock: Clk,
    /// The pacer of each interface, by its position in the [`SendPacing`].
    pacers: Vec<Pacer<T, Clk::Timer>>,
    /// The position in `pacers` of each sender's interface.
    sender_interfaces: IdMap<LocalSenderId, usize>,
    /// The position in `pacers` of the interface that carries the highest-priority traffic,
    /// where the [`SendPacing`] has one that holds a sender.
    highest_priority: Option<usize>,
    /// Round-robin cursor for the first pick candidate. Always in `[0, senders.len())`.
    round_robin_idx: usize,
    /// Whether `rx` closed. The router then doesn't read another batch, and ends once it has
    /// delivered the batches it holds.
    closed: bool,
    /// A batch of `rx` that the router read while every interface held a batch, which it keeps
    /// until an interface may take it. The router doesn't read `rx` while it keeps one.
    pending: Option<T>,
    /// The batches of `rx` that no sender may take yet, since each sender that delivers to their
    /// peer is on an interface that holds a batch, by the path secret of their peer, each peer's
    /// in the order the router read them. Meanwhile the router reads and routes the batches of
    /// other peers. Their count isn't bounded, but in `frame_dispatch` their frames are: the
    /// send credit pool bounds the credit they hold, and a
    /// [`CreditlessWindow`](super::CreditlessWindow) bounds the bytes of those without send
    /// credit, which stay in the window until the router delivers their batch through a
    /// [`WindowSender`](super::WindowSender).
    waiting: HashMap<credentials::Id, WaitingPeer<T>>,
    /// The peers of `waiting` that wait on each interface, by the interface's position in
    /// `pacers`, in the order they began to wait on it: a peer waits on each interface whose
    /// senders could take its first waiting batch, and the router offers that peer's batches
    /// again only once one of those interfaces frees. An entry of a peer whose batches left stays
    /// until its interface frees, and the router then skips it, or, where the peer waits again,
    /// offers its batches.
    waiters: Vec<VecDeque<credentials::Id>>,
    /// How many batches `route_waiting` offered to the senders, read by tests alone.
    #[cfg(test)]
    waiting_offers: usize,
    /// The batches of `highest_rx` that no sender that delivers to their peer may take yet, by
    /// the path secret of their peer, each peer's in the order the router read them, as `waiting`
    /// holds those of `rx`. Where the router has an interface for the highest-priority traffic,
    /// such a batch waits while every sender of that interface is marked lost for its peer and
    /// each other sender that delivers is on an interface that holds a batch. Meanwhile the router
    /// reads and routes the batches of other peers. Their count isn't bounded, and their frames
    /// are bounded in `frame_dispatch` as those of `waiting` are.
    highest_waiting: HashMap<credentials::Id, VecDeque<T>>,
    /// The peers of `highest_waiting`, which the router offers to the senders in turn.
    highest_waiting_order: VecDeque<credentials::Id>,
    /// The lost mark of each sender of the batch being routed, read once for each batch so that
    /// each pass over the senders sees the same marks. Where a sender has an acknowledgement
    /// after the lost pairs stopped delivering, every sender without one counts as lost.
    lost_marks: Vec<bool>,
    pick_counters: IdMap<LocalSenderId, crate::counter::Counter>,
    rejected_counters: IdMap<LocalSenderId, crate::counter::Summary>,
    score_delta: crate::counter::Summary,
    /// Send credit pool. Where a batch can't be delivered, since its worker's receiver is gone,
    /// the credit its frames borrowed returns here, with the credit of each batch held then.
    send_credit_pool: crate::sync::Arc<crate::credit::Pool>,
}

impl<T, R, S, Clk> InterfacePacedPickTwo<T, R, S, Clk>
where
    T: ByteCost + PathSecretMapEntry + AssignSender + FlowCredits + HighestPriority,
    R: Receiver<T>,
    S: UnboundedSender<T>,
    Clk: precision::Clock,
{
    pub fn new(
        rx: R,
        senders: IdMap<LocalSenderId, S>,
        rng: crate::xorshift::Rng,
        clock: Clk,
        pacing: &SendPacing,
        counter_registry: &crate::counter::Registry,
        send_credit_pool: crate::sync::Arc<crate::credit::Pool>,
    ) -> Self {
        debug_assert!(!senders.is_empty());
        let pacers = (0..pacing.interface_count())
            .map(|position| Pacer::new(clock.timer(), pacing.rate(position)))
            .collect();
        let sender_interfaces = senders
            .iter()
            .map(|(id, _)| (id, pacing.interface_of(id.as_usize())))
            .collect();
        let pick_counters = senders
            .iter()
            .map(|(id, _)| {
                let counter =
                    counter_registry.register_nominal("pick_two.chosen", format_args!("send.{id}"));
                (id, counter)
            })
            .collect();
        let rejected_counters = senders
            .iter()
            .map(|(id, _)| {
                let summary = counter_registry.register_nominal_summary(
                    "pick_two.rejected",
                    format_args!("send.{id}"),
                    crate::counter::Unit::Microsecond,
                );
                (id, summary)
            })
            .collect();
        let score_delta = counter_registry
            .register_summary("pick_two.score_delta", crate::counter::Unit::Microsecond);
        // The interface for the highest-priority traffic counts only where a sender is on it.
        let highest_priority = pacing.highest_priority().filter(|&position| {
            senders
                .iter()
                .any(|(id, _)| pacing.interface_of(id.as_usize()) == position)
        });
        Self {
            rx,
            highest_rx: None,
            senders,
            rng,
            clock,
            pacers,
            sender_interfaces,
            highest_priority,
            round_robin_idx: 0,
            closed: false,
            pending: None,
            waiting: HashMap::new(),
            waiters: (0..pacing.interface_count())
                .map(|_| VecDeque::new())
                .collect(),
            #[cfg(test)]
            waiting_offers: 0,
            highest_waiting: HashMap::new(),
            highest_waiting_order: VecDeque::new(),
            lost_marks: Vec::new(),
            pick_counters,
            rejected_counters,
            score_delta,
            send_credit_pool,
        }
    }

    /// Adds `highest_rx` to the router, as an input of its own for the batches of the highest
    /// priority level. The router reads it before `rx`, and sends each of its batches at once to a
    /// worker of the interface for the highest-priority traffic, as a batch whose frames are all
    /// of that level, even while every interface holds a batch. It then reads `rx` only while an
    /// interface doesn't hold a batch. Where the [`SendPacing`] doesn't have that interface, or
    /// that interface doesn't hold a sender, the router routes the batches of `highest_rx` as any
    /// other, while an interface doesn't hold a batch.
    pub fn with_highest_priority_input(mut self, highest_rx: R) -> Self {
        self.highest_rx = Some(highest_rx);
        self
    }

    /// Returns whether the interface of the sender at `sender` doesn't hold a batch.
    fn is_free(&self, sender: usize) -> bool {
        let interface = self.sender_interfaces[LocalSenderId::from_index(sender)];
        self.pacers[interface].held.is_none()
    }

    /// Returns whether the interface of a sender doesn't hold a batch, so that the router may
    /// route the next one.
    fn has_free_sender(&self) -> bool {
        (0..self.senders.len()).any(|sender| self.is_free(sender))
    }

    /// Returns whether the sender at `sender` is on the interface for the highest-priority
    /// traffic.
    fn is_on_highest_interface(&self, sender: usize) -> bool {
        let interface = self.sender_interfaces[LocalSenderId::from_index(sender)];
        Some(interface) == self.highest_priority
    }

    /// Reads the lost mark of each sender of `entry` once, for the passes over the senders that
    /// choose a sender for one batch: the send workers set and clear the marks meanwhile, and
    /// each pass must see the same marks.
    ///
    /// Where a sender has an acknowledgement after the lost pairs stopped delivering, a sender
    /// without one counts as lost too, since it may be on the same down interface and not be
    /// marked yet.
    fn read_lost_marks(&mut self, entry: &PathSecretEntry) {
        let prefer_delivered = entry.has_sender_delivering_since_lost();
        self.lost_marks.clear();
        self.lost_marks
            .extend((0..self.senders.len()).map(|sender| {
                let sender = LocalSenderId::from_index(sender);
                if prefer_delivered {
                    !entry.delivers_since_lost(sender)
                } else {
                    entry.is_sender_lost(sender)
                }
            }));
    }

    /// Returns whether a sender on the interface for the highest-priority traffic delivers to
    /// the batch's peer by the marks [`Self::read_lost_marks`] read: its interface pair isn't
    /// marked lost.
    fn highest_interface_delivers(&self) -> bool {
        (0..self.senders.len())
            .any(|sender| self.is_on_highest_interface(sender) && !self.lost_marks[sender])
    }

    /// Returns the rule by which the router chooses a sender for a batch, where `highest` says
    /// whether the batch goes only to the interface for the highest-priority traffic, by the
    /// marks [`Self::read_lost_marks`] read. While the interface pair of a sender is lost and
    /// another sender of the peer delivers, the router leaves the lost senders out; where every
    /// sender of the interface for the highest-priority traffic is lost then, such a batch goes,
    /// paced, to another interface.
    fn candidates(&self, highest: bool) -> Candidates {
        let avoid_lost = self.lost_marks.contains(&true) && self.lost_marks.contains(&false);
        let on_highest_interface = highest && (!avoid_lost || self.highest_interface_delivers());
        Candidates {
            on_highest_interface,
            avoid_lost,
        }
    }

    /// Returns whether the sender at `sender` may take a batch by `rule` and the marks
    /// [`Self::read_lost_marks`] read: where the rule leaves lost senders out, only a sender
    /// whose interface pair isn't marked lost; for a batch that goes to the interface for the
    /// highest-priority traffic, a sender on that interface; and for any other batch, a sender
    /// whose interface doesn't hold a batch.
    fn is_candidate(&self, sender: usize, rule: Candidates) -> bool {
        if rule.avoid_lost && self.lost_marks[sender] {
            return false;
        }
        if rule.on_highest_interface {
            self.is_on_highest_interface(sender)
        } else {
            self.is_free(sender)
        }
    }

    /// Chooses a sender for a batch of `entry` as [`Self::route`] does, and calls `between`
    /// once the router has read the senders' lost marks and before it chooses, so that a test
    /// can change the marks between the two.
    #[cfg(test)]
    pub(super) fn choose_sender_after_read(
        &mut self,
        entry: &PathSecretEntry,
        highest: bool,
        between: impl FnOnce(),
    ) -> Option<LocalSenderId> {
        let now = self.clock.now();
        self.read_lost_marks(entry);
        let rule = self.candidates(highest);
        between();
        self.choose_sender(entry, now, rule)
    }

    /// Chooses a sender for a batch of `entry` among the candidates of [`Self::is_candidate`] by
    /// `rule`, as [`PickTwo`](super::PickTwo) does: the next of them in round-robin order against
    /// a random other one, most often the one with the lower score. Its passes read the lost
    /// marks of [`Self::read_lost_marks`], so they agree on the candidates. Returns `None` where
    /// no sender may take the batch now.
    fn choose_sender(
        &mut self,
        entry: &PathSecretEntry,
        now: precision::Timestamp,
        rule: Candidates,
    ) -> Option<LocalSenderId> {
        let len = self.senders.len();
        let rr = self.round_robin_idx;
        let first = (0..len)
            .map(|offset| (rr + offset) % len)
            .find(|&sender| self.is_candidate(sender, rule))?;
        self.round_robin_idx = (first + 1) % len;

        let others = (0..len)
            .filter(|&sender| sender != first && self.is_candidate(sender, rule))
            .count();
        if others == 0 {
            return Some(LocalSenderId::from_index(first));
        }
        let nth = if others == 1 {
            0
        } else {
            self.rng.next_usize(others)
        };
        let second = (0..len)
            .filter(|&sender| sender != first && self.is_candidate(sender, rule))
            .nth(nth)
            .expect("`nth` is less than the count of the other candidates");
        let first = LocalSenderId::from_index(first);
        let second = LocalSenderId::from_index(second);

        let score1 = self.score(entry, first, now);
        let score2 = self.score(entry, second, now);
        let delta = score1.abs_diff(score2);
        self.score_delta.record_value(delta);

        // As in `PickTwo`, route to the worse candidate with a delta-dependent probability, so
        // that the senders of a slower interface keep receiving a share of the batches.
        let (better, worse) = if score1 <= score2 {
            (first, second)
        } else {
            (second, first)
        };
        if self.rng.next_f64() < pick_two_worse_probability(delta) {
            self.rejected_counters[better].record_value(delta);
            Some(worse)
        } else {
            self.rejected_counters[worse].record_value(delta);
            Some(better)
        }
    }

    /// Returns the score of `sender`: its load score in `entry`, plus the debt of its interface's
    /// token bucket at `now`, both in nanoseconds.
    fn score(
        &self,
        entry: &PathSecretEntry,
        sender: LocalSenderId,
        now: precision::Timestamp,
    ) -> u64 {
        let interface = self.sender_interfaces[sender];
        let debt = self.pacers[interface].bucket.debt_nanos(now);
        entry.sender_load_score(sender).saturating_add(debt)
    }

    /// Returns whether `value` goes only to the interface for the highest-priority traffic: a
    /// batch whose frames are all of the highest priority level, where the [`SendPacing`] has
    /// that interface.
    fn is_highest(&self, value: &T) -> bool {
        self.highest_priority.is_some() && value.holds_highest_priority()
    }

    /// Routes `value` as [`Self::dispatch`] does, and else keeps it for its `input` until a
    /// sender may take it. A batch whose peer has batches of the same input that wait in the
    /// router waits behind them, so that each peer's batches of an input go in the order the
    /// router read them. `highest` is true where `value` goes only to the interface for the
    /// highest-priority traffic. Returns `false` where the sender's receiver is gone.
    fn route(&mut self, cx: &mut task::Context<'_>, value: T, highest: bool, input: Input) -> bool {
        let peer = *value.path_secret_entry().id();
        match input {
            Input::Highest => {
                if let Some(waiting) = self.highest_waiting.get_mut(&peer) {
                    waiting.push_back(value);
                    return true;
                }
            }
            Input::Other => {
                if let Some(waiting) = self.waiting.get_mut(&peer) {
                    waiting.batches.push_back(value);
                    return true;
                }
            }
        }
        let value = match self.dispatch(cx, value, highest) {
            Ok(routed) => return routed,
            Err(value) => value,
        };
        // Each sender that delivers to the batch's peer is on an interface that holds a batch: the
        // batch waits in the router until one of them is free.
        match input {
            Input::Highest => {
                self.highest_waiting.insert(peer, VecDeque::from([value]));
                self.highest_waiting_order.push_back(peer);
            }
            Input::Other => {
                let waiting = WaitingPeer {
                    batches: VecDeque::from([value]),
                    waits_on: vec![false; self.pacers.len()],
                };
                self.waiting.insert(peer, waiting);
                self.wait_on_interfaces(peer);
            }
        }
        true
    }

    /// Chooses a sender for `value`, and sends it to the sender where the sender's interface may
    /// send now or where `value` goes to the interface for the highest-priority traffic, and else
    /// holds it on that interface until it may. `highest` is true where `value` goes only to the
    /// interface for the highest-priority traffic. Returns `Err` with `value` where no sender may
    /// take it now, and `Ok(false)` where the sender's receiver is gone.
    fn dispatch(
        &mut self,
        cx: &mut task::Context<'_>,
        mut value: T,
        highest: bool,
    ) -> Result<bool, T> {
        let now = self.clock.now();
        let (sender, rule) = {
            let entry = value.path_secret_entry();
            debug_assert_eq!(self.senders.len(), entry.socket_sender_count());
            self.read_lost_marks(entry);
            let rule = self.candidates(highest);
            (self.choose_sender(entry, now, rule), rule)
        };
        let Some(sender) = sender else {
            return Err(value);
        };
        self.pick_counters[sender].add(1);
        value.set_sender_id(sender);

        let interface = self.sender_interfaces[sender];
        // A highest-priority batch doesn't wait for its interface's tokens: it goes at once, and
        // the bucket is charged for it, so the batches after it wait for its bytes.
        if rule.on_highest_interface || self.pacers[interface].poll_ready(cx) {
            Ok(self.deliver(interface, sender, value))
        } else {
            self.pacers[interface].held = Some((sender, value));
            Ok(true)
        }
    }

    /// Offers the batches of `rx` that wait in the router to the senders. For each interface that
    /// doesn't hold a batch, it offers the batches of the peers that wait on it, in the order
    /// they began to wait, and each peer's batches in the order the router read them, so that a
    /// batch that no sender may take yet keeps the later batches of its peer behind it. A peer
    /// whose batches stay waits again on the interfaces that hold a batch, so the router doesn't
    /// offer its batches again before one of those interfaces frees, whether or not they go only
    /// to the interface for the highest-priority traffic. Returns how many batches it sent or
    /// held on an interface, or `None` where a sender's receiver is gone.
    fn route_waiting(&mut self, cx: &mut task::Context<'_>) -> Option<usize> {
        let mut routed = 0;
        for interface in 0..self.pacers.len() {
            // A peer whose batches stay doesn't come back to this interface's waiters while the
            // interface is free: it waits only on interfaces that hold a batch.
            while self.pacers[interface].held.is_none() {
                let Some(peer) = self.waiters[interface].pop_front() else {
                    break;
                };
                match self.waiting.get_mut(&peer) {
                    Some(waiting) => waiting.waits_on[interface] = false,
                    // The peer's batches left the router after it began to wait here.
                    None => continue,
                }
                loop {
                    let waiting = self
                        .waiting
                        .get_mut(&peer)
                        .expect("the peer has waiting batches");
                    let Some(value) = waiting.batches.pop_front() else {
                        self.waiting.remove(&peer);
                        break;
                    };
                    #[cfg(test)]
                    {
                        self.waiting_offers += 1;
                    }
                    let highest = self.is_highest(&value);
                    match self.dispatch(cx, value, highest) {
                        Ok(true) => routed += 1,
                        Ok(false) => return None,
                        Err(value) => {
                            self.waiting
                                .get_mut(&peer)
                                .expect("the peer has waiting batches")
                                .batches
                                .push_front(value);
                            self.wait_on_interfaces(peer);
                            break;
                        }
                    }
                }
            }
        }
        Some(routed)
    }

    /// Offers the batches of `highest_rx` that wait in the router to the senders: the waiting
    /// peers in turn, and each peer's batches in the order the router read them, so that a batch
    /// that no sender may take yet keeps the later batches of its peer behind it. A batch goes
    /// while an interface doesn't hold a batch, or at once where the router has an interface for
    /// the highest-priority traffic. Returns how many batches it sent or held on an interface, or
    /// `None` where a sender's receiver is gone.
    fn route_highest_waiting(&mut self, cx: &mut task::Context<'_>) -> Option<usize> {
        let mut routed = 0;
        for _ in 0..self.highest_waiting_order.len() {
            let peer = self
                .highest_waiting_order
                .pop_front()
                .expect("the order holds each waiting peer");
            loop {
                let highest = match self.highest_waiting[&peer].front() {
                    Some(_) => self.highest_priority.is_some(),
                    None => break,
                };
                if !highest && !self.has_free_sender() {
                    break;
                }
                let value = self
                    .highest_waiting
                    .get_mut(&peer)
                    .expect("each peer of the order has waiting batches")
                    .pop_front()
                    .expect("the peer has a waiting batch");
                match self.dispatch(cx, value, highest) {
                    Ok(true) => routed += 1,
                    Ok(false) => return None,
                    Err(value) => {
                        self.highest_waiting
                            .get_mut(&peer)
                            .expect("each peer of the order has waiting batches")
                            .push_front(value);
                        break;
                    }
                }
            }
            if self.highest_waiting[&peer].is_empty() {
                self.highest_waiting.remove(&peer);
            } else {
                self.highest_waiting_order.push_back(peer);
            }
        }
        Some(routed)
    }

    /// Lets `peer` wait on each interface whose senders could take its first waiting batch by
    /// the marks [`Self::read_lost_marks`] read for that batch: the interfaces of the senders
    /// whose pairs aren't marked lost where another sender of the peer delivers, and else the
    /// interfaces of every sender. No sender could take the batch, so each of those interfaces
    /// holds a batch.
    fn wait_on_interfaces(&mut self, peer: credentials::Id) {
        let avoid_lost = self.candidates(false).avoid_lost;
        let waiting = self
            .waiting
            .get_mut(&peer)
            .expect("the peer has waiting batches");
        for sender in 0..self.senders.len() {
            if avoid_lost && self.lost_marks[sender] {
                continue;
            }
            let interface = self.sender_interfaces[LocalSenderId::from_index(sender)];
            if !waiting.waits_on[interface] {
                waiting.waits_on[interface] = true;
                self.waiters[interface].push_back(peer);
            }
        }
    }

    /// Returns how many entries the waiters of every interface hold.
    #[cfg(test)]
    pub(super) fn waiter_entries(&self) -> usize {
        self.waiters.iter().map(VecDeque::len).sum()
    }

    /// Returns how many batches `route_waiting` offered to the senders.
    #[cfg(test)]
    pub(super) fn waiting_offers(&self) -> usize {
        self.waiting_offers
    }

    /// Sends `value` to `sender`, and charges the token bucket of its `interface` for it. Where
    /// the sender's receiver is gone, returns the send credit of `value` and of each batch the
    /// router holds to the pool, drops them, and returns `false`.
    fn deliver(&mut self, interface: usize, sender: LocalSenderId, value: T) -> bool {
        let byte_cost = value.byte_cost();
        match self.senders[sender].send(value) {
            Ok(()) => {
                let now = self.clock.now();
                self.pacers[interface].charge(now, byte_cost);
                true
            }
            Err(value) => {
                // The worker's receiver is gone (worker teardown): this batch, the batches held
                // for the other interfaces and the batches the router keeps will never be
                // assembled, so the send credit their frames borrowed must return to the pool, as
                // `PickTwo` returns it.
                let mut leaked = value.total_flow_credits();
                drop(value);
                for pacer in &mut self.pacers {
                    if let Some((_, held)) = pacer.held.take() {
                        leaked += held.total_flow_credits();
                    }
                }
                if let Some(pending) = self.pending.take() {
                    leaked += pending.total_flow_credits();
                }
                for waiting_batch in self
                    .waiting
                    .drain()
                    .flat_map(|(_, waiting)| waiting.batches)
                {
                    leaked += waiting_batch.total_flow_credits();
                }
                for waiters in &mut self.waiters {
                    waiters.clear();
                }
                for waiting_batch in self.highest_waiting.drain().flat_map(|(_, queue)| queue) {
                    leaked += waiting_batch.total_flow_credits();
                }
                self.highest_waiting_order.clear();
                if leaked > 0 {
                    self.send_credit_pool.release(leaked);
                }
                false
            }
        }
    }
}

/// How [`InterfacePacedPickTwo`] chooses the senders that may take a batch.
#[derive(Clone, Copy, Debug)]
struct Candidates {
    /// Whether the batch goes, unpaced, to the interface for the highest-priority traffic,
    /// whether or not that interface holds a batch.
    on_highest_interface: bool,
    /// Whether the senders whose interface pairs are marked lost are left out, since another
    /// sender of the batch's peer delivers.
    avoid_lost: bool,
}

/// The input of [`InterfacePacedPickTwo`] that a batch came from, which says where the router
/// keeps the batch while no sender may take it.
#[derive(Clone, Copy, Debug)]
enum Input {
    /// The input of the highest priority level.
    Highest,
    /// The other input.
    Other,
}

#[cfg(test)]
impl<T, R, S, Clk> InterfacePacedPickTwo<T, R, S, Clk>
where
    Clk: precision::Clock,
{
    /// Returns the debt of the token bucket of the interface at `position` now, in nanoseconds.
    pub(super) fn debt_nanos(&self, position: usize) -> u64 {
        self.pacers[position].bucket.debt_nanos(self.clock.now())
    }
}

impl<T, R, S, Clk> Receiver<()> for InterfacePacedPickTwo<T, R, S, Clk>
where
    T: ByteCost + PathSecretMapEntry + AssignSender + FlowCredits + HighestPriority,
    R: Receiver<T>,
    S: UnboundedSender<T>,
    Clk: precision::Clock,
{
    fn poll_recv(&mut self, cx: &mut task::Context<'_>, budget: &mut Budget) -> Poll<Option<()>> {
        // A held batch whose interface may send now goes first. Each pacer this polls that holds
        // a batch and may not send yet registers `cx` with its timer.
        if let Some(interface) = self
            .pacers
            .iter_mut()
            .position(|pacer| pacer.held.is_some() && pacer.poll_ready(cx))
        {
            let (sender, value) = self.pacers[interface]
                .held
                .take()
                .expect("the pacer holds a batch");
            if !self.deliver(interface, sender, value) {
                return Poll::Ready(None);
            }
            return Poll::Ready(Some(()));
        }

        // The input of the highest priority level goes before the other one. Each of its batches
        // goes at once to a worker of the chosen interface where the router has one, even while
        // every interface holds a batch, and else as any other batch, while an interface doesn't
        // hold a batch.
        let chosen = self.highest_priority.is_some();
        if chosen || self.has_free_sender() {
            // The batches of that input that wait in the router go before the router reads it
            // again. A batch whose peer has batches that wait there waits behind them, and the
            // router reads and routes the batches of other peers meanwhile.
            match self.route_highest_waiting(cx) {
                None => return Poll::Ready(None),
                Some(0) => {}
                Some(_) => return Poll::Ready(Some(())),
            }
            if let Some(highest_rx) = &mut self.highest_rx {
                match highest_rx.poll_recv(cx, budget) {
                    Poll::Ready(Some(value)) => {
                        let byte_cost = value.byte_cost();
                        if !self.route(cx, value, chosen, Input::Highest) {
                            return Poll::Ready(None);
                        }
                        // Tell upstream that the batch was consumed, as for the other input. A
                        // batch that waits in the router counts here, as a batch held on an
                        // interface does.
                        if let Some(highest_rx) = &mut self.highest_rx {
                            highest_rx.on_consumed(byte_cost);
                        }
                        return Poll::Ready(Some(()));
                    }
                    // The input closed: the router goes on with the other input alone.
                    Poll::Ready(None) => self.highest_rx = None,
                    Poll::Pending => {}
                }
            }
        }

        // The batches of the other input that wait in the router go before the router routes
        // the batch it read ahead or reads the next one.
        match self.route_waiting(cx) {
            None => return Poll::Ready(None),
            Some(0) => {}
            Some(_) => return Poll::Ready(Some(())),
        }

        // A batch read ahead from the other input is routed before the router reads another, once
        // an interface doesn't hold a batch, or at once where it goes only to the interface for
        // the highest-priority traffic. Where its peer has batches that wait in the router, it
        // waits behind them.
        let kept_may_go = match &self.pending {
            Some(value) => self.has_free_sender() || self.is_highest(value),
            None => false,
        };
        if kept_may_go {
            let value = self.pending.take().expect("the router keeps a batch");
            let highest = self.is_highest(&value);
            if !self.route(cx, value, highest, Input::Other) {
                return Poll::Ready(None);
            }
            return Poll::Ready(Some(()));
        }

        // While every interface holds a batch, the router still reads the next batch where it has
        // an interface for the highest-priority traffic, so that a batch of that level doesn't
        // wait behind them, but not where that level has an input of its own, which the router
        // reads above. It doesn't read while it keeps a batch read ahead.
        if !self.closed
            && self.pending.is_none()
            && (self.has_free_sender()
                || (self.highest_priority.is_some() && self.highest_rx.is_none()))
        {
            match self.rx.poll_recv(cx, budget) {
                Poll::Ready(Some(value)) => {
                    let byte_cost = value.byte_cost();
                    let highest = self.is_highest(&value);
                    if self.has_free_sender() || highest {
                        if !self.route(cx, value, highest, Input::Other) {
                            return Poll::Ready(None);
                        }
                    } else {
                        // Every interface holds a batch: keep this one until an interface may
                        // take it.
                        self.pending = Some(value);
                    }
                    // Tell upstream that the batch was consumed, as `PickTwo` does once it sends
                    // one. A kept batch counts here, as a batch held on an interface does.
                    self.rx.on_consumed(byte_cost);
                    return Poll::Ready(Some(()));
                }
                Poll::Ready(None) => self.closed = true,
                Poll::Pending => {}
            }
        }

        if self.closed
            && self.highest_rx.is_none()
            && self.highest_waiting.is_empty()
            && self.pending.is_none()
            && self.waiting.is_empty()
            && self.pacers.iter().all(|pacer| pacer.held.is_none())
        {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    fn on_consumed(&mut self, bytes: u64) {
        self.rx.on_consumed(bytes);
    }
}
