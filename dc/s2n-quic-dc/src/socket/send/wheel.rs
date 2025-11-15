// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    intrusive_queue::Queue,
    socket::send::transmission::{Entry, Transmission},
};
use core::fmt;
use parking_lot::{Mutex, MutexGuard};
use s2n_quic_core::time::Timestamp;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    task::{Poll, Waker},
    time::Duration,
};

// TODO tune this
pub const DEFAULT_GRANULARITY_US: u64 = 64;

pub struct WakerState {
    waker: Waker,
    should_wake: AtomicBool,
}

impl WakerState {
    pub async fn new() -> Arc<Self> {
        let waker = core::future::poll_fn(|cx| Poll::Ready(cx.waker().clone())).await;
        Arc::new(Self {
            waker,
            should_wake: AtomicBool::new(false),
        })
    }

    fn wake(&self) {
        self.should_wake.store(true, Ordering::Relaxed);
        self.waker.wake_by_ref();
    }

    pub async fn wait(&self) {
        core::future::poll_fn(|_cx| {
            if self.should_wake.swap(false, Ordering::Relaxed) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }
}

pub struct Wheel<Info, Meta, Completion, const GRANULARITY_US: u64 = DEFAULT_GRANULARITY_US>(
    Arc<State<Info, Meta, Completion>>,
);

impl<Info, Meta, Completion, const GRANULARITY_US: u64> Clone
    for Wheel<Info, Meta, Completion, GRANULARITY_US>
{
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<Info, Meta, Completion, const GRANULARITY_US: u64> fmt::Debug
    for Wheel<Info, Meta, Completion, GRANULARITY_US>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wheel")
            .field("len", &self.len())
            .field("start", &self.start())
            .field("granularity", &Duration::from_micros(GRANULARITY_US))
            .field("horizon", &self.horizon())
            .finish()
    }
}

impl<Info, Meta, Completion, const GRANULARITY_US: u64>
    Wheel<Info, Meta, Completion, GRANULARITY_US>
{
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
        let start = Self::timestamp_to_full_index(clock.get_time());

        let sync_state = SyncState {
            start_idx: start,
            waker: None,
            entries,
        };

        Self(Arc::new(State {
            sync_state: Mutex::new(sync_state),
            len: AtomicUsize::new(0),
            mask,
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

    pub fn insert(
        &self,
        mut entry: Entry<Info, Meta, Completion>,
        time: Timestamp,
    ) -> Result<(), (Entry<Info, Meta, Completion>, Timestamp)> {
        let (index, full_idx, waker, mut lock) = match self.index(time) {
            Ok(v) => v,
            Err(time) => {
                return Err((entry, time));
            }
        };
        let transmission_time = Self::full_index_to_timestamp(full_idx);
        entry.transmission_time = Some(transmission_time);
        unsafe {
            debug_assert!(lock.entries.len() > index);
            lock.entries.get_unchecked_mut(index).push_back(entry);
        }
        drop(lock);

        self.0.len.fetch_add(1, Ordering::Relaxed);

        if let Some(waker) = waker {
            waker.wake();
        }

        Ok(())
    }

    pub fn set_waker(&mut self, waker: Arc<WakerState>) {
        self.0.sync_state.lock().waker = Some(waker);
    }

    /// Returns the current transmissions along with the next timestamp to expire
    pub fn tick(
        &self,
        waker: Option<Arc<WakerState>>,
    ) -> (Timestamp, Queue<Transmission<Info, Meta, Completion>>) {
        let mut lock = self.0.sync_state.lock();
        let start = lock.start_idx;
        let next = start + 1;
        lock.start_idx = next;
        lock.waker = waker;
        let index = (start & self.0.mask) as usize;
        let queue = unsafe {
            debug_assert!(lock.entries.len() > index);
            core::mem::take(lock.entries.get_unchecked_mut(index))
        };
        drop(lock);

        let next_tick = Self::full_index_to_timestamp(next);

        (next_tick, queue)
    }

    /// Called by the sender after every transmission, which updates the wheel length
    pub(crate) fn on_send(&self) {
        self.0.len.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn start(&self) -> Timestamp {
        let lock = self.0.sync_state.lock();
        let start = lock.start_idx;
        drop(lock);

        Self::full_index_to_timestamp(start)
    }

    fn index(
        &self,
        timestamp: Timestamp,
    ) -> Result<
        (
            usize,
            u64,
            Option<Arc<WakerState>>,
            MutexGuard<'_, SyncState<Info, Meta, Completion>>,
        ),
        Timestamp,
    > {
        let full_idx = Self::timestamp_to_full_index(timestamp);

        let mut lock = self.0.sync_state.lock();
        let mut min = lock.start_idx;
        let max = min + self.0.mask;

        if full_idx > max {
            // return when the application should resubmit
            let target = full_idx - self.0.mask + 1;
            let target = Self::full_index_to_timestamp(target);
            return Err(target);
        }

        let waker = if let Some(waker) = lock.waker.take() {
            let full_idx = full_idx.max(min);
            min = full_idx;
            lock.start_idx = full_idx;
            Some(waker)
        } else {
            None
        };

        if cfg!(debug_assertions) {
            let min = full_idx.min(min);
            let expected_min = Self::full_index_to_timestamp(min);
            let expected_max = Self::full_index_to_timestamp(max);
            assert!(
                (expected_min..=expected_max).contains(&timestamp),
                "{} not in {}..={}",
                timestamp,
                expected_min,
                expected_max
            );
        }

        // bound the timestamp to the current window
        let full_idx = full_idx.max(min);

        // wrap the index around the slots
        let idx = full_idx & self.0.mask;
        let info = (idx as usize, full_idx, waker, lock);
        Ok(info)
    }

    fn full_index_to_timestamp(full_idx: u64) -> Timestamp {
        unsafe { Timestamp::from_duration(Duration::from_micros(full_idx * GRANULARITY_US)) }
    }

    fn timestamp_to_full_index(timestamp: Timestamp) -> u64 {
        unsafe { timestamp.as_duration().as_micros() as u64 / GRANULARITY_US }
    }
}

struct State<Info, Meta, Completion> {
    sync_state: Mutex<SyncState<Info, Meta, Completion>>,
    mask: u64,
    len: AtomicUsize,
}

struct SyncState<Info, Meta, Completion> {
    start_idx: u64,
    waker: Option<Arc<WakerState>>,
    entries: Box<[Queue<Transmission<Info, Meta, Completion>>]>,
}

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

    fn new(slots: usize) -> (Wheel<(), u16, (), 8>, Pool, Clock) {
        let horizon = Duration::from_micros(8 * slots as u64);
        let clock = Clock::new(horizon);
        let pool = Pool::new(u16::MAX, 16);
        (Wheel::new(horizon, &clock), pool, clock)
    }

    // Helper to create a test transmission
    fn create_transmission(pool: &Pool, payload_len: u16) -> Transmission<(), u16, ()> {
        let socket_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, payload_len));

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
            meta: payload_len,
            completion: None,
        }
    }

    #[test]
    #[cfg(todo)]
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
        wheel.insert(entry, clock.get_time()).unwrap();

        // Tick should return this entry
        let (next_time, mut queue) = wheel.tick(None);
        clock.inc_by(Duration::from_micros(8));
        assert_eq!(next_time, clock.get_time());

        let entry = queue.pop_front().unwrap();
        assert_eq!(entry.meta, 100);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_wheel_multiple_entries_same_slot() {
        let (wheel, pool, clock) = new(64);
        let now = clock.get_time();

        // Insert multiple entries at the same timestamp
        wheel
            .insert(Entry::new(create_transmission(&pool, 10)), now)
            .unwrap();
        wheel
            .insert(Entry::new(create_transmission(&pool, 20)), now)
            .unwrap();
        wheel
            .insert(Entry::new(create_transmission(&pool, 30)), now)
            .unwrap();

        // Tick should return all entries in FIFO order
        let (_, mut queue) = wheel.tick(None);

        assert_eq!(queue.pop_front().unwrap().meta, 10);
        assert_eq!(queue.pop_front().unwrap().meta, 20);
        assert_eq!(queue.pop_front().unwrap().meta, 30);
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

        wheel
            .insert(Entry::new(create_transmission(&pool, 100)), t0)
            .unwrap();
        wheel
            .insert(Entry::new(create_transmission(&pool, 200)), t1)
            .unwrap();
        wheel
            .insert(Entry::new(create_transmission(&pool, 300)), t2)
            .unwrap();

        // First tick gets entry 1
        let (_, mut queue) = wheel.tick(None);
        assert_eq!(queue.pop_front().unwrap().meta, 100);
        assert!(queue.is_empty());

        // Second tick gets entry 2
        let (_, mut queue) = wheel.tick(None);
        assert_eq!(queue.pop_front().unwrap().meta, 200);
        assert!(queue.is_empty());

        // Third tick gets entry 3
        let (_, mut queue) = wheel.tick(None);
        assert_eq!(queue.pop_front().unwrap().meta, 300);
        assert!(queue.is_empty());
    }

    #[test]
    #[cfg(todo)]
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
                let _ = wheel.tick(None);
            }
        }
    }

    #[test]
    #[cfg(todo)]
    fn test_wheel_bounds() {
        let (wheel, pool, mut clock) = new(64);

        // advance the wheel
        for _ in 0..64 {
            let _ = wheel.tick(None);
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
}
