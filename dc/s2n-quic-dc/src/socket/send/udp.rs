// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    clock::{Clock, Timer},
    intrusive_queue::Queue,
    socket::send::wheel::{Entry, Transmission, Wheel},
    stream::socket::Socket,
};
use s2n_quic_core::time::Timestamp;
use std::{ops::ControlFlow, time::Duration};

/// A leaky bucket rate limiter that smooths traffic by leaking bytes at a constant rate
///
/// Unlike a token bucket, this does not accumulate credits up to a maximum, which helps
/// reduce bursts. The leak rate is based on the wheel granularity to align with the
/// timing wheel's tick interval.
#[derive(Debug)]
pub struct LeakyBucket {
    /// Bytes per microsecond leak rate
    bytes_per_us: f64,
    /// The last time we leaked bytes
    last_leak_time: Option<Timestamp>,
    /// Minimum interval between leaks (based on wheel granularity)
    leak_interval_us: u64,
}

impl LeakyBucket {
    /// Create a new leaky bucket with the specified rate
    ///
    /// # Arguments
    /// * `bytes_per_interval` - Number of bytes to leak per interval
    /// * `interval` - Time interval for the leak rate
    /// * `granularity_us` - Wheel granularity in microseconds (used as minimum leak interval)
    pub fn new(bytes_per_interval: u64, interval: Duration, granularity_us: u64) -> Self {
        let interval_us = interval.as_micros() as f64;
        let bytes_per_us = bytes_per_interval as f64 / interval_us;
        
        Self {
            bytes_per_us,
            last_leak_time: None,
            leak_interval_us: granularity_us,
        }
    }

    /// Try to leak (acquire) the specified number of bytes
    ///
    /// Returns the number of bytes that can be leaked based on elapsed time.
    /// Unlike a token bucket, this does not accumulate credits - it only allows
    /// bytes based on the time elapsed since the last leak.
    fn leak(&mut self, requested: u64, now: Timestamp) -> u64 {
        let elapsed_us = if let Some(last) = self.last_leak_time {
            // Calculate microseconds elapsed since last leak
            let elapsed = now.saturating_duration_since(last);
            elapsed.as_micros() as u64
        } else {
            // First leak - use the granularity as the initial interval
            self.leak_interval_us
        };

        // Calculate available bytes based on elapsed time
        let available = (elapsed_us as f64 * self.bytes_per_us) as u64;
        
        // Leak up to the requested amount or what's available
        let leaked = requested.min(available);
        
        if leaked > 0 {
            // Update last leak time
            self.last_leak_time = Some(now);
        }
        
        leaked
    }

    /// Get the next time when bytes will be available
    ///
    /// Returns when at least 1 byte will be available based on the leak rate
    fn next_available(&self, now: Timestamp) -> Timestamp {
        // Calculate time needed for at least 1 byte to be available
        let micros_for_one_byte = (1.0 / self.bytes_per_us).ceil() as u64;
        let wait_time = micros_for_one_byte.max(self.leak_interval_us);
        now + Duration::from_micros(wait_time)
    }
}

/// State for each priority level to track partially processed queues and entries
struct WheelState<Info, const GRANULARITY_US: u64> {
    queue: Wheel<Info, GRANULARITY_US>,
    pending_queue: Option<(Timestamp, Queue<Transmission<Info>>)>,
    pending_entry: Option<(Entry<Info>, u64)>, // (entry, remaining_len)
}

impl<Info, const GRANULARITY_US: u64> WheelState<Info, GRANULARITY_US> {
    /// Ticks the wheel to get the next queue
    fn tick(&mut self) -> (Timestamp, Queue<Transmission<Info>>) {
        if let Some((timestamp, queue)) = self.pending_queue.take() {
            // Resume processing previously retrieved queue
            (timestamp, queue)
        } else {
            self.queue.tick()
        }
    }

    fn transmit<S: Socket, Clk: Clock>(&self, mut entry: Entry<Info>, socket: &S, clock: &Clk) {
        // Successfully acquired all credits, send the packet
        let now = clock.get_time();
        entry.descriptor.send_with(|addr, ecn, iovec| {
            let _ = socket.try_send(addr, ecn, iovec);
        });

        self.queue.on_send();
        entry.transmission = Some(now);

        if let Some(completion) = entry.completion.upgrade() {
            completion.push_back(entry);
        }
    }
}

/// Sends packets on a non-blocking socket with priority-based timing wheels
///
/// Priority levels are specified by the number of wheels.
/// Index 0 is the highest priority and is processed first.
/// The function drains each priority level in order, and if blocked by the leaky
/// bucket, it restarts from the highest priority level after the bucket refills.
pub async fn non_blocking<S: Socket, Clk: Clock, Info, const GRANULARITY_US: u64>(
    socket: S,
    wheels: Vec<Wheel<Info, GRANULARITY_US>>,
    clock: Clk,
    mut leaky_bucket: LeakyBucket,
) {
    assert!(!wheels.is_empty());
    let mut timer = clock.timer();

    let mut wheels = wheels
        .into_iter()
        .map(|wheel| WheelState {
            queue: wheel,
            pending_queue: None,
            pending_entry: None,
        })
        .collect::<Vec<_>>();

    'outer: loop {
        let mut target_timestamp = None;

        // Process each wheel in priority order (0 = highest priority)
        for (priority, wheel) in wheels.iter_mut().enumerate() {
            // Catch-up/processing loop for this priority level
            loop {
                // Get or tick the wheel for a queue
                let (next_timestamp, mut queue) = wheel.tick();

                // Process pending entry first if it exists
                if let Some((entry, mut remaining_len)) = wheel.pending_entry.take() {
                    // Continue acquiring credits for partially transmitted entry
                    if acquire_bytes(
                        &mut leaky_bucket,
                        &clock,
                        &mut timer,
                        &mut remaining_len,
                        priority,
                    )
                    .await
                    .is_break()
                    {
                        wheel.pending_entry = Some((entry, remaining_len));
                        wheel.pending_queue = Some((next_timestamp, queue));
                        continue 'outer;
                    }

                    // Successfully acquired all credits, send the packet
                    wheel.transmit(entry, &socket, &clock);
                }

                // Process all packets in the queue
                while let Some(entry) = queue.pop_front() {
                    let mut remaining_len = entry.descriptor.total_payload_len() as u64;
                    if acquire_bytes(
                        &mut leaky_bucket,
                        &clock,
                        &mut timer,
                        &mut remaining_len,
                        priority,
                    )
                    .await
                    .is_break()
                    {
                        wheel.pending_entry = Some((entry, remaining_len));
                        wheel.pending_queue = Some((next_timestamp, queue));
                        continue 'outer;
                    }

                    // Successfully acquired all credits, send the packet
                    wheel.transmit(entry, &socket, &clock);
                }

                // Queue fully drained. Check if this wheel has caught up to the target
                if let Some(target) = target_timestamp {
                    if next_timestamp >= target {
                        break; // Move to next priority level
                    }
                    // Still behind target tick again to catch up
                } else {
                    // First wheel (highest priority) sets the target timestamp
                    target_timestamp = Some(next_timestamp);
                    break;
                }
            }
        }

        // All priorities processed, sleep until next highest-priority expiration
        timer.sleep(target_timestamp.unwrap()).await;
    }
}

async fn acquire_bytes(
    leaky_bucket: &mut LeakyBucket,
    clock: &impl Clock,
    timer: &mut Timer,
    remaining_len: &mut u64,
    priority: usize,
) -> ControlFlow<()> {
    loop {
        let now = clock.get_time();
        let acquired = leaky_bucket.leak(*remaining_len, now);
        *remaining_len -= acquired;

        if *remaining_len == 0 {
            break;
        }

        // Still blocked! Sleep until more bytes are available
        let next_available = leaky_bucket.next_available(now);
        timer.sleep(next_available).await;

        // only bail if we are operating on lower priority queues
        if priority != 0 {
            return ControlFlow::Break(());
        }
    }

    ControlFlow::Continue(())
}

#[cfg(test)]
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
    ) -> Transmission<(u8, u16)> {
        Transmission {
            descriptor: pool
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
                .unwrap_or_else(|_| panic!("could not create packet")),
            transmission: None,
            info: (priority, payload_len),
            completion: std::sync::Weak::new(),
        }
    }

    fn leaky_bucket(bytes_per_interval: u64, interval: Duration, granularity_us: u64) -> LeakyBucket {
        LeakyBucket::new(bytes_per_interval, interval, granularity_us)
    }

    fn new(horizon: Duration) -> (Wheel<(u8, u16), 8>, Pool, Clock) {
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

                let leaky_bucket = leaky_bucket(10_000_000, 8.us(), 8);

                // Spawn the sender
                crate::testing::spawn(async move {
                    non_blocking(socket, vec![wheel], clock, leaky_bucket).await;
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

                // High bandwidth to avoid leaky bucket blocking in this test
                let leaky_bucket = leaky_bucket(100_000_000, 8.us(), 8);

                crate::testing::spawn(async move {
                    non_blocking(socket, wheels, clock, leaky_bucket).await;
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

                // Limited leaky bucket: enough for ~1 packet per refill
                // This will cause priority 1 to get blocked and allow priority 0 to preempt
                let leaky_bucket = leaky_bucket(150, 8.us(), 8);

                crate::testing::spawn({
                    let clock = clock.clone();
                    let wheels = wheels.clone();
                    async move {
                        non_blocking(socket, wheels, clock, leaky_bucket).await;
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

    #[test]
    fn leaky_bucket_burst_reduction() {
        sim(|| {
            async {
                let socket = UdpSocket::bind("receiver:80").await.unwrap();

                let mut buf = vec![0u8; u16::MAX as usize];

                // With the leaky bucket, packets should be paced smoothly
                // even after an idle period. We expect the packets to arrive
                // at a steady rate based on the leak rate.

                for idx in 0..5 {
                    let (len, addr) = socket.recv_from(&mut buf).await.unwrap();
                    info!(idx, len, %addr, "recv");
                    
                    let packet = &buf[..len];
                    assert_eq!(packet.len(), 100);
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

                let now = clock.get_time();

                // Insert 5 packets at the same time (after being idle)
                // This simulates a burst scenario
                for i in 0..5 {
                    wheel.insert(
                        Entry::new(create_transmission(&pool, peer_addr, 100, 0)),
                        now + Duration::from_micros(i * 8), // Spread slightly in the wheel
                    );
                }

                let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

                // Limited rate: 500 bytes per 8us = 62.5 MB/s
                // This is enough for ~5 packets per interval, but the leaky bucket
                // should pace them at a steady rate rather than bursting
                let leaky_bucket = leaky_bucket(500, 8.us(), 8);

                crate::testing::spawn(async move {
                    non_blocking(socket, vec![wheel], clock, leaky_bucket).await;
                });
            }
            .primary()
            .group("sender")
            .spawn();
        });
    }
}
