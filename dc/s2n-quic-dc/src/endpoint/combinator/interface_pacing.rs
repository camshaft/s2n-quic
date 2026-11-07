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

use super::{AssignSender, FlowCredits, PathSecretMapEntry};
use crate::{
    endpoint::{
        edt::pick_two_worse_probability,
        id::{IdMap, LocalSenderId},
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

/// How frame dispatch paces the send sockets: the send rate of each interface that holds a send
/// socket, and the interface of each send socket.
#[derive(Clone, Debug, PartialEq)]
pub struct SendPacing {
    /// The send rate of each paced interface, by the interface's position here.
    rates: Vec<Rate>,
    /// The position in `rates` of each send socket's interface, by the send socket's position.
    /// Empty where `rates` holds one rate, which paces every send socket.
    socket_interfaces: Vec<usize>,
}

impl SendPacing {
    /// Paces every send socket together at `rate`.
    pub fn single(rate: Rate) -> Self {
        Self {
            rates: vec![rate],
            socket_interfaces: Vec::new(),
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
        }
    }

    /// Paces the send sockets of each interface of `interfaces` that holds a send socket together,
    /// at that interface's send rate. Where every send socket is on one interface, this is
    /// [`Self::single`] at that interface's send rate, and where there isn't a send socket, at
    /// `overall_send_rate`.
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
        Self::new(rates, socket_interfaces)
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
/// interface holds a batch, the router doesn't read the next one.
///
/// Implements `Receiver<()>` so it can be drained via `ReceiverExt::drain_budgeted`.
pub struct InterfacePacedPickTwo<T, R, S, Clk>
where
    Clk: precision::Clock,
{
    rx: R,
    senders: IdMap<LocalSenderId, S>,
    rng: crate::xorshift::Rng,
    clock: Clk,
    /// The pacer of each interface, by its position in the [`SendPacing`].
    pacers: Vec<Pacer<T, Clk::Timer>>,
    /// The position in `pacers` of each sender's interface.
    sender_interfaces: IdMap<LocalSenderId, usize>,
    /// Round-robin cursor for the first pick candidate. Always in `[0, senders.len())`.
    round_robin_idx: usize,
    /// Whether `rx` closed. The router then doesn't read another batch, and ends once it has
    /// delivered the batches it holds.
    closed: bool,
    pick_counters: IdMap<LocalSenderId, crate::counter::Counter>,
    rejected_counters: IdMap<LocalSenderId, crate::counter::Summary>,
    score_delta: crate::counter::Summary,
    /// Send credit pool. Where a batch can't be delivered, since its worker's receiver is gone,
    /// the credit its frames borrowed returns here, with the credit of each batch held then.
    send_credit_pool: crate::sync::Arc<crate::credit::Pool>,
}

impl<T, R, S, Clk> InterfacePacedPickTwo<T, R, S, Clk>
where
    T: ByteCost + PathSecretMapEntry + AssignSender + FlowCredits,
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
        Self {
            rx,
            senders,
            rng,
            clock,
            pacers,
            sender_interfaces,
            round_robin_idx: 0,
            closed: false,
            pick_counters,
            rejected_counters,
            score_delta,
            send_credit_pool,
        }
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

    /// Chooses a sender for a batch of `entry` among the senders whose interface doesn't hold a
    /// batch, as [`PickTwo`](super::PickTwo) does: the next of them in round-robin order against
    /// a random other one, most often the one with the lower score.
    fn choose_sender(
        &mut self,
        entry: &PathSecretEntry,
        now: precision::Timestamp,
    ) -> LocalSenderId {
        let len = self.senders.len();
        let rr = self.round_robin_idx;
        let first = (0..len)
            .map(|offset| (rr + offset) % len)
            .find(|&sender| self.is_free(sender))
            .expect("the router routes a batch only while a sender is free");
        self.round_robin_idx = (first + 1) % len;

        let others = (0..len)
            .filter(|&sender| sender != first && self.is_free(sender))
            .count();
        if others == 0 {
            return LocalSenderId::from_index(first);
        }
        let nth = if others == 1 {
            0
        } else {
            self.rng.next_usize(others)
        };
        let second = (0..len)
            .filter(|&sender| sender != first && self.is_free(sender))
            .nth(nth)
            .expect("`nth` is less than the count of the other free senders");
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
            worse
        } else {
            self.rejected_counters[worse].record_value(delta);
            better
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

    /// Chooses a sender for `value`, and sends it to the sender where the sender's interface may
    /// send now, or holds it on that interface until it may. Returns `false` where the sender's
    /// receiver is gone.
    fn route(&mut self, cx: &mut task::Context<'_>, mut value: T) -> bool {
        let byte_cost = value.byte_cost();
        let now = self.clock.now();
        let sender = {
            let entry = value.path_secret_entry();
            debug_assert_eq!(self.senders.len(), entry.socket_sender_count());
            self.choose_sender(entry, now)
        };
        self.pick_counters[sender].add(1);
        value.set_sender_id(sender);

        let interface = self.sender_interfaces[sender];
        let routed = if self.pacers[interface].poll_ready(cx) {
            self.deliver(interface, sender, value)
        } else {
            self.pacers[interface].held = Some((sender, value));
            true
        };
        if routed {
            // Tell upstream that the batch was consumed, as `PickTwo` does once it sends one.
            self.rx.on_consumed(byte_cost);
        }
        routed
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
                // The worker's receiver is gone (worker teardown): this batch and the batches
                // held for the other interfaces will never be assembled, so the send credit their
                // frames borrowed must return to the pool, as `PickTwo` returns it.
                let mut leaked = value.total_flow_credits();
                drop(value);
                for pacer in &mut self.pacers {
                    if let Some((_, held)) = pacer.held.take() {
                        leaked += held.total_flow_credits();
                    }
                }
                if leaked > 0 {
                    self.send_credit_pool.release(leaked);
                }
                false
            }
        }
    }
}

impl<T, R, S, Clk> Receiver<()> for InterfacePacedPickTwo<T, R, S, Clk>
where
    T: ByteCost + PathSecretMapEntry + AssignSender + FlowCredits,
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

        if !self.closed && self.has_free_sender() {
            match self.rx.poll_recv(cx, budget) {
                Poll::Ready(Some(value)) => {
                    if !self.route(cx, value) {
                        return Poll::Ready(None);
                    }
                    return Poll::Ready(Some(()));
                }
                Poll::Ready(None) => self.closed = true,
                Poll::Pending => {}
            }
        }

        if self.closed && self.pacers.iter().all(|pacer| pacer.held.is_none()) {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    fn on_consumed(&mut self, bytes: u64) {
        self.rx.on_consumed(bytes);
    }
}
