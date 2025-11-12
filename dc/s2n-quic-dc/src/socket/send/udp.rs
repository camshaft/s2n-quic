// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    clock::Clock,
    intrusive_queue::Queue,
    socket::send::{
        completion::{Completer as _, Completion},
        transmission::{Entry, Transmission},
        wheel::Wheel,
    },
    stream::socket::Socket,
};
use s2n_quic_core::time::Timestamp;
use std::{ops::ControlFlow, time::Duration};

/// State for each priority level to track partially processed queues and entries
struct WheelState<Info, Meta, Completion, const GRANULARITY_US: u64> {
    queue: Wheel<Info, Meta, Completion, GRANULARITY_US>,
    pending_queue: Option<(Timestamp, Queue<Transmission<Info, Meta, Completion>>)>,
    pending_entry: Option<Entry<Info, Meta, Completion>>,
}

impl<Info, Meta, C, const GRANULARITY_US: u64> WheelState<Info, Meta, C, GRANULARITY_US>
where
    C: Completion<Info, Meta>,
{
    /// Ticks the wheel to get the next queue
    fn tick(&mut self) -> (Timestamp, Queue<Transmission<Info, Meta, C>>) {
        if let Some((timestamp, queue)) = self.pending_queue.take() {
            // Resume processing previously retrieved queue
            (timestamp, queue)
        } else {
            self.queue.tick()
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

/// Sends packets on a non-blocking socket with priority-based timing wheels
///
/// Priority levels are specified by the number of wheels.
/// Index 0 is the highest priority and is processed first.
/// The function drains each priority level in order, and if blocked by the token
/// bucket, it restarts from the highest priority level after the bucket refills.
pub async fn non_blocking<S, Clk, Info, Meta, C, const GRANULARITY_US: u64>(
    socket: S,
    wheels: Vec<Wheel<Info, Meta, C, GRANULARITY_US>>,
    clock: Clk,
    mut bucket: LeakyBucket,
) where
    S: Socket,
    Clk: Clock,
    C: Completion<Info, Meta>,
{
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

    loop {
        let mut target_timestamp = None;

        // Process each wheel in priority order (0 = highest priority)
        for (_priority, wheel) in wheels.iter_mut().enumerate() {
            // Catch-up/processing loop for this priority level
            loop {
                // Get or tick the wheel for a queue
                let (next_timestamp, mut queue) = wheel.tick();

                // Process pending entry first if it exists
                if let Some(entry) = wheel.pending_entry.take() {
                    let now = clock.get_time();

                    // Continue acquiring credits for partially transmitted entry
                    if let ControlFlow::Break(target) = bucket.take(entry.total_len as _, now) {
                        target_timestamp = Some(target);
                        wheel.pending_entry = Some(entry);
                        wheel.pending_queue = Some((next_timestamp, queue));
                        break;
                    }

                    // Successfully acquired all credits, send the packet
                    wheel.transmit(entry, &socket, &now);
                }

                // Process all packets in the queue
                while let Some(entry) = queue.pop_front() {
                    let now = clock.get_time();

                    // Continue acquiring credits for partially transmitted entry
                    if let ControlFlow::Break(target) = bucket.take(entry.total_len as _, now) {
                        target_timestamp = Some(target);
                        wheel.pending_entry = Some(entry);
                        wheel.pending_queue = Some((next_timestamp, queue));
                        break;
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

pub struct LeakyBucket {
    nanos_per_byte: f64,
    last_transmission_nanos: u64,
}

impl LeakyBucket {
    pub fn new(gigabits_per_second: f64) -> Self {
        let nanos_per_byte = 1e9 / (gigabits_per_second * 1e3);
        Self {
            nanos_per_byte,
            last_transmission_nanos: 0,
        }
    }

    fn take(&mut self, bytes: u64, now: Timestamp) -> ControlFlow<Timestamp> {
        let delay_nanos = bytes as f64 * self.nanos_per_byte;
        let target_nanos = self.last_transmission_nanos + (delay_nanos as u64).max(1);

        let current_nanos = unsafe { now.as_duration().as_nanos() as u64 };

        if current_nanos < target_nanos {
            let nanos = target_nanos - current_nanos;
            let mut target = now + Duration::from_nanos(nanos as u64);

            // Make sure we wait at least a microsecond
            if target == now {
                target += Duration::from_micros(1);
            }

            return ControlFlow::Break(target);
        }

        ControlFlow::Continue(())
    }
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
