// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    clock::precision::{Timer, Timestamp},
    intrusive_queue::Queue,
    socket::send::{
        completion::{Completer as _, Completion},
        transmission::{Entry, Transmission},
        wheel::{WakerState, Wheel},
    },
    stream::socket::Socket,
};
use std::{future::poll_fn, ops::ControlFlow, sync::Arc, time::Duration};

/// State for each priority level to track partially processed queues and entries
struct WheelState<Info, Meta, Completion, const GRANULARITY_US: u64> {
    queue: Wheel<Info, Meta, Completion, GRANULARITY_US>,
    next_tick: Timestamp,
    pending_queue: Option<Queue<Transmission<Info, Meta, Completion>>>,
    pending_entry: Option<Entry<Info, Meta, Completion>>,
}

impl<Info, Meta, C, const GRANULARITY_US: u64> WheelState<Info, Meta, C, GRANULARITY_US>
where
    C: Completion<Info, Meta>,
{
    /// Ticks the wheel to get the next queue
    fn tick(&mut self, waker: &Option<&Arc<WakerState>>) -> Queue<Transmission<Info, Meta, C>> {
        if let Some(queue) = self.pending_queue.take() {
            // Resume processing previously retrieved queue
            if let Some(waker) = *waker {
                self.queue.set_waker(waker.clone());
            }
            queue
        } else {
            let (ts, queue) = self.queue.tick(waker.cloned());
            if cfg!(debug_assertions) {
                let min_ts = ts - Duration::from_micros(GRANULARITY_US);
                let range = min_ts..ts;
                for entry in queue.iter() {
                    let target = entry.transmission_time.unwrap();
                    assert!(range.contains(&target), "{range:?} {target}");
                }
            }
            self.next_tick = ts.into();
            queue
        }
    }

    fn transmit<S: Socket, Clk: s2n_quic_core::time::Clock>(
        &self,
        mut entry: Entry<Info, Meta, C>,
        socket: &S,
        clock: &Clk,
    ) {
        // Successfully acquired all credits, send the packet
        let now = clock.get_time();
        entry.send_with(|addr, ecn, iovec| {
            let _ = socket.try_send(addr, ecn, iovec);
        });

        self.queue.on_send();
        entry.transmission_time = Some(now);

        if let Some(completion) = entry.completion.as_ref().and_then(|c| c.upgrade()) {
            completion.complete(entry);
        }
    }
}

pub trait WakeMode {
    const BUSY_POLL: bool;
}

pub struct BusyPoll;

impl WakeMode for BusyPoll {
    const BUSY_POLL: bool = true;
}

pub struct WithWaker;

impl WakeMode for WithWaker {
    const BUSY_POLL: bool = false;
}

/// Sends packets on a non-blocking socket with priority-based timing wheels
///
/// Priority levels are specified by the number of wheels.
/// Index 0 is the highest priority and is processed first.
/// The function drains each priority level in order, and if blocked by the token
/// bucket, it restarts from the highest priority level after the bucket refills.
pub async fn non_blocking<S, T, Info, Meta, C, W, const GRANULARITY_US: u64>(
    socket: S,
    wheels: Vec<Wheel<Info, Meta, C, GRANULARITY_US>>,
    mut timer: T,
    bucket: LeakyBucket,
    wake_mode: W,
) where
    S: Socket,
    T: Timer,
    C: Completion<Info, Meta>,
    W: WakeMode,
{
    assert!(!wheels.is_empty());
    let _ = wake_mode;

    let start = timer.now();

    let mut wheels = wheels
        .into_iter()
        .map(|wheel| WheelState {
            queue: wheel,
            next_tick: start,
            pending_queue: None,
            pending_entry: None,
        })
        .collect::<Vec<_>>();

    let waker = WakerState::new().await;

    let granularity = Duration::from_micros(GRANULARITY_US);
    let mut bucket = bucket.init(granularity, timer.now());
    let horizon = wheels[0].queue.horizon();
    let mut sleep_time = start + horizon;
    let mut reporter = Reporter::new(start);

    loop {
        // let mut target_timestamp = None;
        let start = timer.now();
        reporter.flush(start);
        let mut has_pending_transmissions = false;
        let mut should_park = false;
        let waker_ref = if !W::BUSY_POLL && start > sleep_time {
            should_park = true;
            Some(&waker)
        } else {
            None
        };

        let mut next_sleep = None;

        // Process each wheel in priority order (0 = highest priority)
        'batch: for (priority, wheel) in wheels.iter_mut().enumerate() {
            let _ = priority;

            // Catch-up/processing loop for this priority level
            loop {
                let start = timer.now();

                if next_sleep.is_none() {
                    next_sleep = Some(wheel.next_tick);
                }

                // If we're busy polling then check if we should tick or not
                if wheel.pending_queue.is_none() && wheel.next_tick > start {
                    break;
                }

                let mut queue = wheel.tick(&waker_ref);

                // Process pending entry first if it exists
                if let Some(entry) = wheel.pending_entry.take() {
                    let now = timer.now();
                    has_pending_transmissions = true;

                    // Continue acquiring credits for partially transmitted entry
                    if let ControlFlow::Break(target) = bucket.take(entry.total_len as _, now) {
                        if !W::BUSY_POLL {
                            next_sleep = Some(target);
                        }
                        wheel.pending_entry = Some(entry);
                        wheel.pending_queue = Some(queue);
                        break 'batch;
                    }

                    // Successfully acquired all credits, send the packet
                    reporter.on_send(entry.total_len as _);
                    wheel.transmit(entry, &socket, &now);
                }

                // Process all packets in the queue
                while let Some(entry) = queue.pop_front() {
                    let now = timer.now();
                    has_pending_transmissions = true;

                    // Continue acquiring credits for partially transmitted entry
                    if let ControlFlow::Break(target) = bucket.take(entry.total_len as _, now) {
                        if !W::BUSY_POLL {
                            next_sleep = Some(target);
                        }
                        wheel.pending_entry = Some(entry);
                        wheel.pending_queue = Some(queue);
                        break 'batch;
                    }

                    // Successfully acquired all credits, send the packet
                    reporter.on_send(entry.total_len as _);
                    wheel.transmit(entry, &socket, &now);
                }

                if let Some(next_sleep) = next_sleep {
                    // if the wheel is caught up with the target then continue to the next priority
                    if next_sleep <= wheel.next_tick {
                        break;
                    }
                } else {
                    next_sleep = Some(wheel.next_tick);
                    break;
                }
            }
        }

        if W::BUSY_POLL {
            yield_now().await;
        } else {
            if has_pending_transmissions {
                sleep_time = start + horizon;
            } else if should_park {
                waker.wait().await;
                sleep_time = start + horizon;
                continue;
            }

            timer.sleep_until(next_sleep.unwrap()).await;
        }
    }
}

async fn yield_now() {
    let mut yielded = false;
    use std::task::Poll;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}

struct Reporter {
    last_emit: Timestamp,
    next_emit: Timestamp,
    sent: u64,
}

impl Reporter {
    pub fn new(start: Timestamp) -> Self {
        Self {
            last_emit: start,
            next_emit: start + Duration::from_secs(1),
            sent: 0,
        }
    }

    fn on_send(&mut self, amount: u64) {
        self.sent += amount;
    }

    fn flush(&mut self, now: Timestamp) {
        if now < self.next_emit {
            return;
        }
        if self.sent > 0 {
            let elapsed_nanos = now.nanos_since(self.last_emit) as f64;
            let elapsed = elapsed_nanos / 1_000_000_000.0;
            let mut rate = self.sent as f64 * 8.0 / elapsed;
            let prefixes = [("G", 1e9), ("M", 1e6), ("K", 1e3)];
            let mut prefix = "";
            for (pref, divisor) in prefixes {
                if rate > divisor {
                    rate /= divisor;
                    prefix = pref;
                    break;
                }
            }
            tracing::info!("{now}: {rate:.2} {prefix}bps");
        }
        self.last_emit = now;
        self.next_emit = now + Duration::from_secs(1);
        self.sent = 0;
    }
}

pub struct LeakyBucket {
    bytes_per_nanos: f64,
}

impl LeakyBucket {
    pub fn new(gigabits_per_second: f64) -> Self {
        let bytes_per_nanos = gigabits_per_second / 8.0;
        Self { bytes_per_nanos }
    }

    fn init(&self, granularity: Duration, now: Timestamp) -> LeakyBucketInstance {
        let bytes_per_nanos = self.bytes_per_nanos;
        let max_size = bytes_per_nanos * granularity.as_nanos() as f64;
        let bytes = max_size;
        LeakyBucketInstance {
            bytes_per_nanos,
            last_refill: now,
            max_size,
            bytes,
            debt: 0.0,
        }
    }
}

struct LeakyBucketInstance {
    bytes_per_nanos: f64,
    last_refill: Timestamp,
    max_size: f64,
    bytes: f64,
    debt: f64,
}

impl LeakyBucketInstance {
    fn refill(&mut self, now: Timestamp) {
        let diff_nanos = now.nanos_since(self.last_refill) as f64;
        let mut refill_amount = self.bytes_per_nanos * diff_nanos;
        if self.debt > 0.0 {
            let debt_paid = refill_amount.min(self.debt);
            refill_amount -= debt_paid;
            self.debt -= debt_paid;
        }
        let bytes = self.bytes + refill_amount;
        self.bytes = bytes.min(self.max_size);
        self.last_refill = now;
    }

    fn take(&mut self, bytes: u64, now: Timestamp) -> ControlFlow<Timestamp> {
        self.refill(now);

        let bytes = bytes as f64;

        if self.bytes < bytes && self.debt > 0.0 {
            let remaining = bytes - self.bytes + self.debt;
            let remaining_nanos = remaining / self.bytes_per_nanos;
            let mut target = now;
            target.nanos += remaining_nanos.ceil() as u64;
            return ControlFlow::Break(target);
        }

        if self.bytes >= bytes {
            self.bytes = self.bytes - bytes;
        } else {
            self.bytes = 0.0;
            self.debt = bytes - self.bytes;
        }

        ControlFlow::Continue(())
    }
}

#[cfg(todo)]
mod tests {
    use super::*;
    use crate::{
        clock::bach::Clock,
        socket::pool::Pool,
        testing::{ext::*, sim},
    };
    use bach::net::UdpSocket;
    use s2n_quic_core::time::{Clock as _, Duration};
    use std::{convert::Infallible, net::SocketAddr};
    use tracing::info;

    // Helper to create a test transmission
    fn create_transmission(
        pool: &Pool,
        socket_addr: SocketAddr,
        payload_len: u16,
        priority: u8,
    ) -> Transmission<(), (u8, u16), ()> {
        let descriptors = pool
            .alloc_or_grow()
            .fill_with(|addr, _cmsg, mut payload| {
                addr.set(socket_addr.into());
                let len = payload_len as usize;
                for chunk in payload[..len].chunks_mut(2) {
                    if chunk.len() == 2 {
                        chunk.copy_from_slice(&payload_len.to_be_bytes());
                    } else {
                        chunk[0] = 255;
                    }
                }
                <Result<_, Infallible>>::Ok(len)
            })
            .unwrap_or_else(|_| panic!("could not create packet"))
            .into_iter()
            .map(|desc| (desc, ()))
            .collect();
        Transmission {
            descriptors,
            total_len: payload_len,
            transmission_time: None,
            meta: (priority, payload_len),
            completion: None,
        }
    }

    fn new(horizon: Duration) -> (Wheel<(), (u8, u16), (), 8>, Pool, Clock) {
        let clock = Clock::default();
        let pool = Pool::new(u16::MAX, 16);
        let wheel = Wheel::new(horizon, &clock);
        (wheel, pool, clock)
    }

    #[test]
    fn single_priority_basic() {
        sim(|| {
            async {
                let socket = UdpSocket::bind("receiver:80").await.unwrap();

                let mut buf = vec![0u8; u16::MAX as usize];
                for idx in 0..2 {
                    let (len, addr) = socket.recv_from(&mut buf).await.unwrap();
                    info!(len, %addr, "recv");
                    let packet = &buf[..len];
                    assert_eq!(packet.len(), (idx + 1) * 100);
                }
            }
            .primary()
            .group("receiver")
            .spawn();

            async {
                let (wheel, pool, clock) = new(Duration::from_millis(100));

                let peer_addr = bach::net::lookup_host("receiver:80")
                    .await
                    .unwrap()
                    .next()
                    .unwrap();

                // Insert some packets
                let now = clock.get_time();
                wheel.insert(
                    Entry::new(create_transmission(&pool, peer_addr, 100, 0)),
                    now,
                );
                wheel.insert(
                    Entry::new(create_transmission(&pool, peer_addr, 200, 0)),
                    now + Duration::from_millis(10),
                );

                let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

                let bucket = LeakyBucket::new(1.0);

                // Spawn the sender
                crate::testing::spawn(async move {
                    non_blocking(socket, vec![wheel], clock, bucket).await;
                });
            }
            .primary()
            .group("sender")
            .spawn();
        });
    }

    #[test]
    fn multi_priority_ordering() {
        sim(|| {
            async {
                let socket = UdpSocket::bind("receiver:80").await.unwrap();

                let mut buf = vec![0u8; u16::MAX as usize];

                // We should receive packets in priority order
                // Priority 0 packets first, then priority 1
                let expected = [100, 110, 200, 210];

                for expected in expected {
                    let (len, addr) = socket.recv_from(&mut buf).await.unwrap();
                    info!(len, %addr, "recv");
                    let packet = &buf[..len];
                    assert_eq!(packet.len(), expected);
                }
            }
            .primary()
            .group("receiver")
            .spawn();

            async {
                let (wheel0, pool, clock) = new(Duration::from_millis(100));
                let wheel1 = Wheel::new(wheel0.horizon(), &clock);
                let wheels = vec![wheel0, wheel1];

                let peer_addr = bach::net::lookup_host("receiver:80")
                    .await
                    .unwrap()
                    .next()
                    .unwrap();

                let now = clock.get_time();

                // Priority 0 packets (should be sent first despite being inserted after)
                let packets = [(200, 1), (210, 1), (100, 0), (110, 0)];

                for (payload_len, priority) in packets {
                    wheels[priority].insert(
                        Entry::new(create_transmission(
                            &pool,
                            peer_addr,
                            payload_len,
                            priority as _,
                        )),
                        now,
                    );
                }

                let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

                // High bandwidth to avoid token bucket blocking in this test
                let bucket = LeakyBucket::new(1.0);

                crate::testing::spawn(async move {
                    non_blocking(socket, wheels, clock, bucket).await;
                });
            }
            .primary()
            .group("sender")
            .spawn();
        });
    }

    #[test]
    fn token_bucket_preemption() {
        sim(|| {
            async {
                let socket = UdpSocket::bind("receiver:80").await.unwrap();

                let mut buf = vec![0u8; u16::MAX as usize];

                // With limited token bucket, we should see:
                // 1. Priority 0 packet (100 bytes)
                // 2. Priority 1 starts but gets blocked
                // 3. After refill, priority 0 packet (110 bytes) - preempts priority 1
                // 4. Then priority 1 packets (200, 210 bytes)
                let expected = [100, 110, 200, 210];

                for expected in expected {
                    let (len, addr) = socket.recv_from(&mut buf).await.unwrap();
                    info!(len, %addr, "recv");
                    let packet = &buf[..len];
                    assert_eq!(packet.len(), expected);
                }
            }
            .primary()
            .group("receiver")
            .spawn();

            async {
                let (wheel0, pool, clock) = new(Duration::from_millis(100));
                let wheel1 = Wheel::new(wheel0.horizon(), &clock);
                let wheels = vec![wheel0, wheel1];

                let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

                // Limited token bucket: enough for ~1 packet per refill
                // This will cause priority 1 to get blocked and allow priority 0 to preempt
                let bucket = LeakyBucket::new(1.0);

                crate::testing::spawn({
                    let clock = clock.clone();
                    let wheels = wheels.clone();
                    async move {
                        non_blocking(socket, wheels, clock, bucket).await;
                    }
                });

                let peer_addr = bach::net::lookup_host("receiver:80")
                    .await
                    .unwrap()
                    .next()
                    .unwrap();

                let now = clock.get_time();

                // Insert all packets at the same time
                let packets = [
                    (100, 0, 0), // Priority 0 - sent first
                    (200, 1, 4), // Priority 1 - starts but gets blocked
                    (210, 1, 0), // Priority 1 - waiting
                    (110, 0, 0), // Priority 0 - preempts after refill
                ];

                for (payload_len, priority, sleep) in packets {
                    info!(payload_len, priority, sleep, "insert");
                    wheels[priority].insert(
                        Entry::new(create_transmission(
                            &pool,
                            peer_addr,
                            payload_len,
                            priority as _,
                        )),
                        now,
                    );
                    if sleep > 0 {
                        info!(sleep = ?sleep.us(), "sleep");
                        sleep.us().sleep().await;
                    }
                }
            }
            .primary()
            .group("sender")
            .spawn();
        });
    }
}
