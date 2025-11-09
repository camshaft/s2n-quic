// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    socket::pool::descriptor,
    sync::intrusive_queue::{self as queue, Queue},
};
use core::{fmt, task::Waker};
use parking_lot::{Mutex, RwLock};
use s2n_quic_core::time::Timestamp;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Weak,
    },
    time::Duration,
};

pub const DEFAULT_GRANULARITY_US: u64 = 8;

pub type Entry<Info> = queue::Entry<Transmission<Info>>;

pub struct Transmission<Info> {
    pub descriptor: descriptor::Segments,
    pub transmission: Option<Timestamp>,
    pub info: Info,
    pub completion: Weak<Queue<Self>>,
}

pub struct Wheel<Info, const GRANULARITY_US: u64 = DEFAULT_GRANULARITY_US>(Arc<State<Info>>);

impl<Info, const GRANULARITY_US: u64> Clone for Wheel<Info, GRANULARITY_US> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<Info, const GRANULARITY_US: u64> fmt::Debug for Wheel<Info, GRANULARITY_US> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wheel")
            .field("len", &self.len())
            .field("start", &self.start())
            .field("granularity", &Duration::from_micros(GRANULARITY_US))
            .field("horizon", &self.horizon())
            .finish()
    }
}

impl<Info, const GRANULARITY_US: u64> Wheel<Info, GRANULARITY_US> {
    /// Create a new Wheel with the specified number of slots (must be a power of 2)
    pub fn new<Clk: s2n_quic_core::time::Clock>(horizon: Duration, clock: &Clk) -> Self {
        let horizon = (horizon.as_micros() as u64).max(1);
        let slots = (horizon / GRANULARITY_US) as usize;
        let slots = slots.next_power_of_two();

        let entries = (0..slots)
            .map(|_| Queue::new())
            .collect::<Vec<_>>()
            .into_boxed_slice();

        let mask = (slots - 1) as u64;
        let start = unsafe { clock.get_time().as_duration().as_micros() as u64 / GRANULARITY_US };

        Self(Arc::new(State {
            start_idx: RwLock::new(start),
            len: AtomicUsize::new(0),
            mask,
            entries,
            waker: Mutex::new(None),
        }))
    }

    /// Returns the total number of entries waiting for transmission
    ///
    /// Can be used to determine queue load and potentially rebalance to other queues
    pub fn len(&self) -> usize {
        self.0.len.load(Ordering::Relaxed)
    }

    pub fn horizon(&self) -> Duration {
        Duration::from_micros(self.0.mask * GRANULARITY_US)
    }

    /// Insert an entry into the wheel at the specified timestamp
    ///
    /// If the wheel transitions from empty to non-empty (i.e., this is the first
    /// entry after being empty), any stored waker will be notified. This allows
    /// a spin-down sender task to wake up and resume processing.
    ///
    /// Returns the actual timestamp the entry was scheduled for (may be clamped
    /// to the wheel's valid range).
    pub fn insert(&self, entry: Entry<Info>, timestamp: Timestamp) -> Timestamp {
        let (index, abs_idx) = self.index(timestamp);
        let prev_len = self.0.len.fetch_add(1, Ordering::Relaxed);
        self.0.entries[index].push_back(entry);

        // If we transitioned from empty to non-empty, wake the stored waker
        if prev_len == 0 {
            if let Some(waker) = self.0.waker.lock().take() {
                waker.wake();
            }
        }

        unsafe { Timestamp::from_duration(Duration::from_micros(abs_idx * GRANULARITY_US)) }
    }

    /// Returns the current transmissions along with the next timestamp to expire
    pub fn tick(&self) -> (Timestamp, crate::intrusive_queue::Queue<Transmission<Info>>) {
        let mut lock = self.0.start_idx.write();
        let start = *lock;
        let next = start + 1;
        *lock = next;
        let index = (start & self.0.mask) as usize;
        let queue = self.0.entries[index].take();
        drop(lock);

        let now = unsafe { Timestamp::from_duration(Duration::from_micros(next * GRANULARITY_US)) };

        (now, queue)
    }

    /// Called by the sender after every transmission, which updates the wheel length
    pub(crate) fn on_send(&self) {
        self.0.len.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn start(&self) -> Timestamp {
        let micros = self.start_idx() * GRANULARITY_US;
        unsafe { Timestamp::from_duration(Duration::from_micros(micros)) }
    }

    /// Check if the wheel is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Store a waker to be notified when the wheel becomes non-empty
    pub fn store_waker(&self, waker: Waker) {
        *self.0.waker.lock() = Some(waker);
    }

    /// Advance the wheel's start time to catch up to the current time
    /// This should be called after waking up from a spin-down to avoid being too far behind
    pub fn advance_to<Clk: s2n_quic_core::time::Clock>(&self, clock: &Clk) {
        let current_time = unsafe { clock.get_time().as_duration().as_micros() as u64 };
        let new_start_idx = current_time / GRANULARITY_US;
        let mut lock = self.0.start_idx.write();
        *lock = new_start_idx;
    }

    fn start_idx(&self) -> u64 {
        *self.0.start_idx.read()
    }

    fn index(&self, timestamp: Timestamp) -> (usize, u64) {
        let min = self.start_idx();
        let max = min + self.0.mask;

        let idx = unsafe { timestamp.as_duration().as_micros() as u64 / GRANULARITY_US };
        // bound the timestamp to the current window
        let idx = idx.clamp(min, max);
        // wrap the index around the slots
        let idx = idx & self.0.mask;
        (idx as usize, min + idx)
    }
}

struct State<Info> {
    start_idx: RwLock<u64>,
    mask: u64,
    len: AtomicUsize,
    entries: Box<[Queue<Transmission<Info>>]>,
    /// Waker to be notified when the wheel transitions from empty to non-empty
    waker: Mutex<Option<Waker>>,
}

impl<Info> Wheel<Info> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::pool::Pool;
    use s2n_quic_core::time::{self, Clock as _};
    use std::{
        convert::Infallible,
        net::{Ipv4Addr, SocketAddr},
        time::Duration,
    };

    struct Clock(Timestamp);

    impl Default for Clock {
        fn default() -> Self {
            // `Timestamp` can't represent 0
            Self::new(Duration::from_micros(1))
        }
    }

    impl time::Clock for Clock {
        fn get_time(&self) -> Timestamp {
            self.0
        }
    }

    impl Clock {
        fn new(start: Duration) -> Self {
            Self(unsafe { Timestamp::from_duration(start) })
        }

        fn inc_by(&mut self, duration: Duration) {
            self.0 += duration;
        }
    }

    fn new(slots: usize) -> (Wheel<u16, 8>, Pool, Clock) {
        let horizon = Duration::from_micros(8 * slots as u64);
        let clock = Clock::new(horizon);
        let pool = Pool::new(u16::MAX, 16);
        (Wheel::new(horizon, &clock), pool, clock)
    }

    // Helper to create a test transmission
    fn create_transmission(pool: &Pool, payload_len: u16) -> Transmission<u16> {
        Transmission {
            descriptor: pool
                .alloc_or_grow()
                .fill_with(|addr, _cmsg, mut payload| {
                    let socket_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, payload_len));
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
            info: payload_len,
            completion: Weak::new(),
        }
    }

    #[test]
    fn test_wheel_creation() {
        let (wheel, _pool, clock) = new(64);

        assert_eq!(wheel.0.entries.len(), 64);
        assert_eq!(wheel.0.mask, 63);

        let start = wheel.start();
        assert_eq!(start, clock.get_time());
    }

    #[test]
    fn test_wheel_insert_and_tick() {
        let (wheel, pool, mut clock) = new(64);

        // Insert an entry at the current time
        let entry = Entry::new(create_transmission(&pool, 100));
        wheel.insert(entry, clock.get_time());

        // Tick should return this entry
        let (next_time, mut queue) = wheel.tick();
        clock.inc_by(Duration::from_micros(8));
        assert_eq!(next_time, clock.get_time());

        let entry = queue.pop_front().unwrap();
        assert_eq!(entry.info, 100);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_wheel_multiple_entries_same_slot() {
        let (wheel, pool, clock) = new(64);
        let now = clock.get_time();

        // Insert multiple entries at the same timestamp
        wheel.insert(Entry::new(create_transmission(&pool, 10)), now);
        wheel.insert(Entry::new(create_transmission(&pool, 20)), now);
        wheel.insert(Entry::new(create_transmission(&pool, 30)), now);

        // Tick should return all entries in FIFO order
        let (_, mut queue) = wheel.tick();

        assert_eq!(queue.pop_front().unwrap().info, 10);
        assert_eq!(queue.pop_front().unwrap().info, 20);
        assert_eq!(queue.pop_front().unwrap().info, 30);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_wheel_multiple_slots() {
        let (wheel, pool, mut clock) = new(64);

        // Insert entries at different times
        let t0 = clock.get_time();
        clock.inc_by(Duration::from_micros(8));
        let t1 = clock.get_time();
        clock.inc_by(Duration::from_micros(8));
        let t2 = clock.get_time();

        wheel.insert(Entry::new(create_transmission(&pool, 100)), t0);
        wheel.insert(Entry::new(create_transmission(&pool, 200)), t1);
        wheel.insert(Entry::new(create_transmission(&pool, 300)), t2);

        // First tick gets entry 1
        let (_, mut queue) = wheel.tick();
        assert_eq!(queue.pop_front().unwrap().info, 100);
        assert!(queue.is_empty());

        // Second tick gets entry 2
        let (_, mut queue) = wheel.tick();
        assert_eq!(queue.pop_front().unwrap().info, 200);
        assert!(queue.is_empty());

        // Third tick gets entry 3
        let (_, mut queue) = wheel.tick();
        assert_eq!(queue.pop_front().unwrap().info, 300);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_wheel_index_calculation() {
        let (wheel, _pool, mut clock) = new(64);

        for (iteration, expected) in (0usize..64).cycle().take(512).enumerate() {
            let t = clock.get_time();
            let idx = wheel.index(t).0;

            assert_eq!(
                idx,
                expected,
                "iteration={iteration} ts={t:?} start={:?} horizon={:?} diff={:?}",
                wheel.start(),
                wheel.horizon(),
                t.saturating_duration_since(wheel.start()),
            );
            clock.inc_by(Duration::from_micros(8));

            // slowly drag behind by missing a tick every 64 iterations
            if expected > 0 {
                let _ = wheel.tick();
            }
        }
    }

    #[test]
    fn test_wheel_bounds() {
        let (wheel, pool, mut clock) = new(64);

        // advance the wheel
        for _ in 0..64 {
            let _ = wheel.tick();
        }

        let actual_time = wheel.insert(
            Entry::new(create_transmission(&pool, 100)),
            clock.get_time(),
        );
        assert_eq!(actual_time, wheel.start());

        // advance the clock outside of the horizon
        clock.inc_by(Duration::from_micros(8 * 1024));

        let actual_time = wheel.insert(
            Entry::new(create_transmission(&pool, 200)),
            clock.get_time(),
        );
        assert_eq!(actual_time, wheel.start() + wheel.horizon());
    }

    #[test]
    fn test_wheel_is_empty() {
        let (wheel, pool, clock) = new(64);

        // Initially empty
        assert!(wheel.is_empty());
        assert_eq!(wheel.len(), 0);

        // After inserting an entry
        let t0 = clock.get_time();
        wheel.insert(Entry::new(create_transmission(&pool, 100)), t0);
        assert!(!wheel.is_empty());
        assert_eq!(wheel.len(), 1);

        // After ticking and consuming
        let (_, mut queue) = wheel.tick();
        let entry = queue.pop_front().unwrap();
        wheel.on_send();
        assert!(wheel.is_empty());
        assert_eq!(wheel.len(), 0);
        assert_eq!(entry.info, 100);
    }

    #[test]
    fn test_wheel_waker_on_insert() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::task::{Context, Wake};

        struct TestWaker {
            woken: Arc<AtomicBool>,
        }

        impl Wake for TestWaker {
            fn wake(self: Arc<Self>) {
                self.woken.store(true, Ordering::SeqCst);
            }
        }

        let (wheel, pool, clock) = new(64);

        // Store a waker
        let woken = Arc::new(AtomicBool::new(false));
        let test_waker = Arc::new(TestWaker {
            woken: woken.clone(),
        });
        let waker = test_waker.clone().into();
        wheel.store_waker(waker);

        // Initially not woken
        assert!(!woken.load(Ordering::SeqCst));

        // Insert an entry - should wake
        let t0 = clock.get_time();
        wheel.insert(Entry::new(create_transmission(&pool, 100)), t0);

        // Verify waker was called
        assert!(woken.load(Ordering::SeqCst));
    }

    #[test]
    fn test_wheel_advance_to() {
        let (wheel, _pool, mut clock) = new(64);

        let start = wheel.start();

        // Advance the clock
        clock.inc_by(Duration::from_micros(8 * 100));

        // Advance the wheel to catch up
        wheel.advance_to(&clock);

        let new_start = wheel.start();

        // Start should have advanced
        assert!(new_start > start);
        assert!(new_start.as_duration().as_micros() >= clock.get_time().as_duration().as_micros());
    }
}
