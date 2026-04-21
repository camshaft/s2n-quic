// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    clock::precision::{self, Timer as _, Timestamp},
    intrusive_queue::Queue,
    socket::send::{
        channel::{self, Flatten, Priority, Reporter},
        completion::{Completer as _, Completion},
        transmission::{self, Entry, Transmission},
        wheel::{self, Wheel},
    },
    stream::socket::Socket,
};
use std::{
    future::{poll_fn, Future},
    pin::pin,
    task::Poll,
    time::Duration,
};
use tokio::select;

// ── Idle Strategy ──────────────────────────────────────────────────────────

/// Controls how the wheel ticker waits for new work between iterations.
///
/// Two strategies:
/// - [`BusyPollIdle`]: Yields once per iteration (for busy-poll runtimes)
/// - [`WakerIdle`]: Registers a waker and sleeps until next expiry or new insertions
pub trait Idle: Copy + Send + 'static {
    /// Called after each tick_to iteration when no entries were due (or after sending).
    /// Should yield at least once to allow other tasks to run.
    fn idle<Info, Meta, C, const GRANULARITY_US: u64>(
        &self,
        ticker: &wheel::Ticker<Info, Meta, C, GRANULARITY_US>,
        timer: &mut impl precision::Timer,
    ) -> impl Future<Output = ()>;
}

/// Busy-poll idle: just yield once so other tasks can be polled.
#[derive(Clone, Copy, Debug)]
pub struct BusyPollIdle;

impl Idle for BusyPollIdle {
    async fn idle<Info, Meta, C, const GRANULARITY_US: u64>(
        &self,
        _ticker: &wheel::Ticker<Info, Meta, C, GRANULARITY_US>,
        _timer: &mut impl precision::Timer,
    ) {
        // Yield exactly once to let other tasks run
        let mut yielded = false;
        poll_fn(|cx| {
            // busy poll runtime doesn't use wakers at all
            let _ = cx;

            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                Poll::Pending
            }
        })
        .await;
    }
}

/// Waker-based idle: register waker on the wheel and sleep until next expiry.
#[derive(Clone, Copy, Debug)]
pub struct WakerIdle;

impl Idle for WakerIdle {
    async fn idle<Info, Meta, C, const GRANULARITY_US: u64>(
        &self,
        ticker: &wheel::Ticker<Info, Meta, C, GRANULARITY_US>,
        timer: &mut impl precision::Timer,
    ) {
        // Sleep until the next expiry or until woken by new insertions
        let deadline = ticker.next_expiry();

        let timer_fut = async {
            if let Some(deadline) = deadline {
                timer.sleep_until(deadline).await;
            } else {
                // if we don't have a deadline it means the wheel is empty and we just need to wait for it
                core::future::pending::<()>().await;
            }
        };

        let queue_updates = poll_fn(|cx| {
            // the waker was already registered when the task started
            let _ = cx;

            if ticker.has_pending() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });

        select! {
            biased;

            _ = queue_updates => {}
            _ = timer_fut => {}
        }
    }
}

// ── Wheel Ticker ───────────────────────────────────────────────────────────

/// Ticks a single timing wheel and pushes ready queues into a channel sender.
///
/// Creates a Ticker from the wheel and loops:
/// 1. Drains the shared queue, inserts into wheel slots, advances to now
/// 2. Sends due entries through the channel
/// 3. Sleeps until next expiry or woken by new insertions
///
/// Returns when the channel is closed (receiver dropped).
async fn wheel_ticker<Info, Meta, C, S, I, const GRANULARITY_US: u64>(
    wheel: Wheel<Info, Meta, C, GRANULARITY_US>,
    clock: &impl precision::Clock,
    mut tx: S,
    idle: I,
) where
    S: channel::Sender<Queue<Transmission<Info, Meta, C>>>,
    I: Idle,
{
    let mut ticker = wheel.ticker(clock);
    poll_fn(|cx| {
        ticker.set_waker(cx.waker().clone());
        Poll::Ready(())
    })
    .await;

    let mut timer = clock.timer();
    let mut pending_queue = Queue::new();

    loop {
        let now: Timestamp = timer.now();

        // Advance the wheel to now and collect due entries
        let mut queue = ticker.tick_to(now.into());

        // Append to pending queue from previous iteration
        pending_queue.append(&mut queue);

        // Only send non-empty queues to avoid unnecessary wakeups
        if !pending_queue.is_empty() {
            let to_send = core::mem::take(&mut pending_queue);
            let mut send_fut = pin!(tx.send(to_send));

            // Poll the send future, ticking the wheel while blocked
            let result = poll_fn(|cx| {
                match send_fut.as_mut().poll(cx) {
                    Poll::Ready(result) => Poll::Ready(result),
                    Poll::Pending => {
                        // While send is blocked, tick the wheel and accumulate
                        let now: Timestamp = timer.now();
                        let mut new_queue = ticker.tick_to(now.into());
                        pending_queue.append(&mut new_queue);
                        Poll::Pending
                    }
                }
            })
            .await;

            if result.is_err() {
                return;
            }
        } else {
            // Yield / sleep using the idle strategy
            idle.idle(&ticker, &mut timer).await;
        }
    }
}

// ── Socket Sender ──────────────────────────────────────────────────────────

/// Receives entries from a channel and sends them on a socket.
///
/// After sending each packet, computes how long the bytes should take at the
/// configured rate and sleeps until that time. This naturally paces
/// transmissions without the complexity of a token bucket with debt tracking.
///
/// Returns when the receiver signals that all senders are gone.
async fn socket_sender<S, Clk, Info, Meta, C, R>(socket: S, clock: &Clk, rate: Rate, mut rx: R)
where
    S: Socket,
    Clk: precision::Clock,
    Meta: transmission::Meta<Info = Info>,
    C: Completion<Info, Meta>,
    R: channel::Receiver<Entry<Info, Meta, C>>,
{
    let mut timer = clock.timer();
    let mut bucket = TokenBucket::new(timer.now(), &rate);
    let mut packets_per_poll = 0u32;

    loop {
        packets_per_poll += 1;

        let mut recv_fut = pin!(rx.recv());
        let Some(mut entry) = poll_fn(|cx| {
            if packets_per_poll > 10 {
                cx.waker().wake_by_ref();
                packets_per_poll = 0;
                return Poll::Pending;
            }

            let result = recv_fut.as_mut().poll(cx);
            if result.is_pending() {
                // If recv returned Pending, we yielded. Reset counters.
                packets_per_poll = 0;
            }
            result
        })
        .await
        else {
            break;
        };

        // Try to upgrade the completion channel before sending.
        let total_len = entry.total_len as u64;
        let Some(completion) = entry.completion.upgrade() else {
            // The sender went away; skip this entry
            continue;
        };

        // Record the actual transmission time before calling UDP send
        entry.transmission_time = Some(timer.now());

        // Send the packet
        entry.send_with(|addr, ecn, iovec| {
            let _ = socket.try_send(addr, ecn, iovec);
        });

        completion.complete(entry);

        // Consume tokens for this packet. The bucket refills based on elapsed
        // time since the last consume, allowing microbursts up to the burst
        // capacity. If we've exceeded the rate, sleep until tokens recover.
        let now = timer.now();
        let cost_nanos = rate.nanos_for_bytes(total_len);
        let sleep_nanos = bucket.consume(now, cost_nanos);

        if sleep_nanos > 0 {
            let target = now + Duration::from_nanos(sleep_nanos);
            timer.sleep_until(target).await;

            // Reset counters after yielding
            packets_per_poll = 0;
        }
    }

    tracing::info!(local_addr = %socket.local_addr().unwrap(), "shutting down UDP sender");
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Sends packets on a non-blocking socket with priority-based timing wheels.
///
/// Each wheel represents a priority level (index 0 = highest priority).
/// A ticker task is spawned per wheel that drains due entries into a channel.
/// A single sender task reads from a priority-merging receiver and transmits
/// on the socket, applying rate-based pacing.
pub fn non_blocking<S, Clk, Info, Meta, C, const GRANULARITY_US: u64>(
    socket: S,
    wheels: Vec<Wheel<Info, Meta, C, GRANULARITY_US>>,
    clock: Clk,
    rate: Rate,
    idle: impl Idle,
) -> impl Future<Output = ()> + Send
where
    S: Socket,
    Clk: precision::Clock + Clone,
    Meta: transmission::Meta<Info = Info>,
    C: Completion<Info, Meta>,
{
    // SAFETY: The future uses Rc-based cell channels internally, which are !Send.
    // However, the entire future is polled as a single unit — the Rc's are created
    // inside the async block and never escape it. No Rc crosses a thread boundary.
    AssertSend(non_blocking_inner(socket, wheels, clock, rate, idle))
}

async fn non_blocking_inner<S, Clk, Info, Meta, C, const GRANULARITY_US: u64>(
    socket: S,
    wheels: Vec<Wheel<Info, Meta, C, GRANULARITY_US>>,
    clock: Clk,
    rate: Rate,
    idle: impl Idle,
) where
    S: Socket,
    Clk: precision::Clock + Clone,
    Meta: transmission::Meta<Info = Info>,
    C: Completion<Info, Meta>,
{
    assert!(!wheels.is_empty());

    // Create one slot channel per priority level and spawn a ticker per wheel.
    let mut senders = Vec::with_capacity(wheels.len());
    let mut receivers = Vec::with_capacity(wheels.len());

    for wheel in &wheels {
        let (tx, rx) = channel::cell::new();
        senders.push(tx);

        // Flatten the queue of transmissions into individual entries
        let rx = Flatten::new(rx);

        let wheel = wheel.clone();
        // decrement the wheel's len counter when entries are received
        let rx = channel::Inspect::new(rx, move |_entry: &Entry<Info, Meta, C>| {
            wheel.on_send();
        });
        receivers.push(rx);
    }

    // Then merge all via Priority, then wrap in Reporter.
    let receivers = Priority::new(receivers);
    let receivers = Reporter::new(receivers, clock.clone());

    // Create ticker futures
    let mut tickers: Vec<_> = wheels
        .into_iter()
        .zip(senders)
        .map(|(wheel, tx)| Some(Box::pin(wheel_ticker(wheel, &clock, tx, idle))))
        .collect();

    let mut sender = Some(Box::pin(socket_sender(socket, &clock, rate, receivers)));

    core::future::poll_fn(|cx| {
        let mut any_pending = false;
        for slot in &mut tickers {
            if let Some(ticker) = slot {
                if ticker.as_mut().poll(cx).is_ready() {
                    *slot = None;
                } else {
                    any_pending = true;
                }
            }
        }

        if let Some(s) = &mut sender {
            if s.as_mut().poll(cx).is_ready() {
                // If the socket is gone then there's no point in continuing
                return core::task::Poll::Ready(());
            } else {
                any_pending = true;
            }
        }

        if any_pending {
            core::task::Poll::Pending
        } else {
            core::task::Poll::Ready(())
        }
    })
    .await
}

// ── Rate ───────────────────────────────────────────────────────────────────

/// A token-bucket rate limiter for pacing packet transmissions.
///
/// Tokens are added at a constant rate (based on the configured throughput).
/// Each send consumes tokens proportional to the packet size. When the bucket
/// is empty, the sender sleeps until enough tokens accumulate. The bucket
/// has a bounded capacity to allow microbursts — if the sender was idle, it
/// can burst up to `burst_nanos` worth of data at line rate before pacing
/// kicks in.
pub struct Rate {
    /// Nanoseconds per byte at the configured rate.
    nanos_per_byte: f64,
    /// Maximum burst allowance in nanoseconds of credit.
    /// This allows microbursts after idle periods.
    burst_nanos: u64,
}

impl Rate {
    pub fn new(gigabits_per_second: f64) -> Self {
        // nanos/byte = 8 / Gbps
        let nanos_per_byte = 8.0 / gigabits_per_second;

        // Allow up to 64KB worth of burst (1x GSO batch)
        // Reduced from 256KB to avoid overwhelming NIC TX ring
        let burst_nanos = (u16::MAX as f64 * nanos_per_byte) as u64;

        Self {
            nanos_per_byte,
            burst_nanos,
        }
    }

    /// Returns the number of nanoseconds to sleep after sending `bytes`.
    fn nanos_for_bytes(&self, bytes: u64) -> u64 {
        (bytes as f64 * self.nanos_per_byte) as u64
    }
}

/// Token bucket state for pacing.
struct TokenBucket {
    /// Timestamp when the bucket was last refilled.
    last_refill: Timestamp,
    /// Available tokens in nanoseconds. Can go negative (debt).
    tokens_nanos: i64,
    /// Maximum tokens (burst capacity) in nanoseconds.
    capacity_nanos: i64,
}

impl TokenBucket {
    fn new(now: Timestamp, rate: &Rate) -> Self {
        Self {
            last_refill: now,
            tokens_nanos: rate.burst_nanos as i64,
            capacity_nanos: rate.burst_nanos as i64,
        }
    }

    /// Refill tokens based on elapsed time, then consume `cost_nanos`.
    /// Returns the number of nanos to sleep (0 if tokens are available).
    fn consume(&mut self, now: Timestamp, cost_nanos: u64) -> u64 {
        // Refill: add tokens for elapsed time since last refill
        let elapsed = now.nanos_since(self.last_refill);
        self.last_refill = now;
        self.tokens_nanos = (self.tokens_nanos + elapsed as i64).min(self.capacity_nanos);

        // Consume tokens
        self.tokens_nanos -= cost_nanos as i64;

        // If we went negative, we need to sleep until tokens recover
        if self.tokens_nanos < 0 {
            (-self.tokens_nanos) as u64
        } else {
            0
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Wrapper that asserts a future is `Send` even if the compiler can't prove it.
///
/// SAFETY: The caller must ensure the future is only polled from a single thread
/// and that no !Send data escapes the future.
struct AssertSend<F>(F);

// SAFETY: The non_blocking_inner future contains Rc-based cell channels which are
// !Send. However, all Rc's are created within the async block, polled inline via
// futures_join (never spawned separately), and dropped when the future completes.
// No Rc ever crosses a thread boundary.
unsafe impl<F: Future> Send for AssertSend<F> {}

impl<F: Future> Future for AssertSend<F> {
    type Output = F::Output;

    fn poll(
        self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        // SAFETY: We're just delegating to the inner future. Pin projection is safe
        // because AssertSend is a transparent wrapper.
        unsafe { self.map_unchecked_mut(|s| &mut s.0) }.poll(cx)
    }
}
