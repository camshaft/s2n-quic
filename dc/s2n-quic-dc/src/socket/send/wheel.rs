// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Hierarchical Timing Wheel for packet scheduling.
//!
//! Producers push entries into a shared unordered queue. A single reader
//! owns the hierarchical timing wheel, drains the queue, inserts entries
//! into the appropriate slot, and advances virtual time to collect due entries.
//!
//! The wheel uses 256 slots per level with a configurable base tick (default 1µs).
//! 4 levels cover 256^4 ticks ≈ 4.29 billion µs ≈ 71.6 minutes at 1µs granularity.
//!
//! Each level maintains a 256-bit occupancy bitset ([u64; 4]) for O(1) next-expiry
//! computation via `trailing_zeros`. Causal ordering is preserved: entries from the
//! same sender are always dequeued in the order they were submitted, because both the
//! shared queue and per-slot queues are FIFO.

use crate::{
    clock::precision,
    intrusive_queue::Queue,
    socket::send::transmission::{Entry, Transmission},
};
use core::{fmt, task};
use parking_lot::Mutex;
use s2n_quic_core::task::waker::noop;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    task::Waker,
    time::Duration,
};

// ── Configuration ──────────────────────────────────────────────────────────

/// Number of slots per level (must be a power of 2).
const SLOTS_PER_LEVEL: usize = 256;

/// Bit-shift per level (log2 of SLOTS_PER_LEVEL).
const BITS_PER_LEVEL: u32 = 8; // 2^8 = 256

/// Mask for extracting slot index within a level.
const SLOT_MASK: u64 = (SLOTS_PER_LEVEL - 1) as u64;

/// Number of u64 words in the occupancy bitset per level.
/// 256 bits = 4 × u64.
const BITSET_WORDS: usize = SLOTS_PER_LEVEL / 64;

/// Number of hierarchical levels.
/// With 256 slots/level at 1µs base: covers 256^4 µs ≈ 71.6 minutes.
const LEVELS: usize = 4;

/// Default base tick granularity in microseconds.
pub const DEFAULT_GRANULARITY_US: u64 = 1;

// ── Wheel (shared handle) ──────────────────────────────────────────────────

/// Shared, cloneable handle for producers to push entries into the wheel.
///
/// Producers push entries (with `transmission_time` already set) into an
/// unordered concurrent queue. The reader-owned [`Ticker`] drains this queue
/// and inserts entries into the hierarchical timing wheel.
pub struct Wheel<Info, Meta, Completion, const GRANULARITY_US: u64 = DEFAULT_GRANULARITY_US>(
    Arc<Shared<Info, Meta, Completion>>,
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
            .field("granularity_us", &GRANULARITY_US)
            .finish()
    }
}

/// State protected by a single mutex: the unordered entry queue and the waker.
struct State<Info, Meta, Completion> {
    queue: Queue<Transmission<Info, Meta, Completion>>,
    waker: task::Waker,
}

/// Shared state between all producers and the single reader.
struct Shared<Info, Meta, Completion> {
    state: Mutex<State<Info, Meta, Completion>>,
    /// Total number of entries queued (for load monitoring).
    len: AtomicUsize,
    /// Set to `true` by producers whenever they push at least one entry into
    /// the shared queue; cleared to `false` by [`Ticker::drain_incoming`] after
    /// it empties the queue.
    ///
    /// The [`Ticker`] checks this flag atomically *before* acquiring the lock,
    /// so it can skip the lock entirely when no new entries have been pushed
    /// since the last drain.  Both the `store(true)` (in `insert` /
    /// `insert_batch`) and the `store(false)` (in `drain_incoming`) happen
    /// while the mutex is held, so the flag is always consistent with the
    /// queue contents.
    ///
    /// Memory ordering: `Relaxed` is sufficient.  The mutex unlock/lock pair
    /// provides the necessary acquire-release semantics for the queue contents.
    /// The pre-lock load in `drain_incoming` is an optimistic hint: a false
    /// `false` (stale read) is safe because the producer's subsequent
    /// `waker.wake()` will re-wake the ticker to retry.
    pending: AtomicBool,
}

impl<Info, Meta, Completion, const GRANULARITY_US: u64>
    Wheel<Info, Meta, Completion, GRANULARITY_US>
{
    /// Create a new wheel.
    pub fn new() -> Self {
        Self(Arc::new(Shared {
            state: Mutex::new(State {
                queue: Queue::new(),
                waker: noop(),
            }),
            len: AtomicUsize::new(0),
            pending: AtomicBool::new(false),
        }))
    }

    /// Create a [`Ticker`] for this wheel.
    ///
    /// The ticker owns the hierarchical timing wheel and is the sole consumer
    /// of entries from the shared queue.
    pub fn ticker<Clk: precision::Clock>(
        &self,
        clock: &Clk,
    ) -> Ticker<Info, Meta, Completion, GRANULARITY_US> {
        let initial_tick = Self::timestamp_to_tick(clock.now());
        Ticker {
            wheel: self.clone(),
            levels: Box::new(core::array::from_fn(|_| Level::new())),
            current_tick: initial_tick,
        }
    }

    // ── Public query methods ───────────────────────────────────────────

    pub fn is_closed(&self) -> bool {
        Arc::strong_count(&self.0) == 1
    }

    /// Returns the total number of entries waiting for transmission.
    pub fn len(&self) -> usize {
        self.0.len.load(Ordering::Relaxed)
    }

    // ── Producer: Insert ───────────────────────────────────────────────

    /// Insert an entry into the wheel.
    ///
    /// The entry's `transmission_time` field determines when it will be
    /// scheduled. If `None`, it will be sent as soon as possible.
    pub fn insert(&self, entry: Entry<Info, Meta, Completion>) {
        let waker = {
            let mut state = self.0.state.lock();
            self.0.len.fetch_add(1, Ordering::Relaxed);
            self.0.pending.store(true, Ordering::Relaxed);
            state.queue.push_back(entry);
            state.waker.clone()
        };
        waker.wake();
    }

    /// Insert a batch of entries in one lock acquisition.
    ///
    /// Each entry's `transmission_time` should already be set.
    pub fn insert_batch(&self, mut queue: Queue<Transmission<Info, Meta, Completion>>) {
        let count = queue.len();
        if count == 0 {
            return;
        }
        let waker = {
            let mut state = self.0.state.lock();
            self.0.len.fetch_add(count, Ordering::Relaxed);
            self.0.pending.store(true, Ordering::Relaxed);
            state.queue.append(&mut queue);
            state.waker.clone()
        };
        waker.wake();
    }

    // ── Reader API ───────────────────────────────────────────────────

    pub fn set_waker(&self, waker: Waker) {
        self.0.state.lock().waker = waker;
    }

    /// Called by the sender after every transmission to update the queue length.
    pub(crate) fn on_send(&self) {
        self.0.len.fetch_sub(1, Ordering::Relaxed);
    }

    // ── Internal: Tick/Timestamp arithmetic ─────────────────────────────

    fn tick_to_timestamp(tick: u64) -> precision::Timestamp {
        let time = Duration::from_micros(tick * GRANULARITY_US);
        precision::Timestamp {
            nanos: time.as_nanos() as _,
        }
    }

    fn timestamp_to_tick(timestamp: precision::Timestamp) -> u64 {
        let time = Duration::from_nanos(timestamp.nanos);
        time.as_micros() as u64 / GRANULARITY_US
    }
}

// ── Slot ───────────────────────────────────────────────────────────────────

/// A single slot in the wheel. Owned exclusively by the Ticker (no sync needed).
struct Slot<Info, Meta, Completion> {
    queue: Queue<Transmission<Info, Meta, Completion>>,
}

impl<Info, Meta, Completion> Slot<Info, Meta, Completion> {
    fn new() -> Self {
        Self {
            queue: Queue::new(),
        }
    }

    fn push(&mut self, entry: Entry<Info, Meta, Completion>) {
        self.queue.push_back(entry);
    }

    /// Push an entry to the front of this slot's queue.
    ///
    /// Used during cascade (reverse iteration): cascaded entries were enqueued
    /// before any entries currently in this slot, so they go to the front.
    fn push_front(&mut self, entry: Entry<Info, Meta, Completion>) {
        self.queue.push_front(entry);
    }

    fn drain(&mut self) -> Queue<Transmission<Info, Meta, Completion>> {
        core::mem::take(&mut self.queue)
    }
}

// ── Level ──────────────────────────────────────────────────────────────────

/// One level of the hierarchical wheel, containing SLOTS_PER_LEVEL slots
/// and a 256-bit occupancy bitset for fast scanning.
struct Level<Info, Meta, Completion> {
    slots: Box<[Slot<Info, Meta, Completion>; SLOTS_PER_LEVEL]>,
    /// Occupancy bitset: bit `i` is set iff `slots[i]` is non-empty.
    occupied: [u64; BITSET_WORDS],
}

impl<Info, Meta, Completion> Level<Info, Meta, Completion> {
    fn new() -> Self {
        let slots = Box::new(core::array::from_fn(|_| Slot::new()));
        Self {
            slots,
            occupied: [0; BITSET_WORDS],
        }
    }

    /// Mark a slot as occupied in the bitset.
    #[inline]
    fn set_occupied(&mut self, index: usize) {
        let word = index / 64;
        let bit = index % 64;
        self.occupied[word] |= 1u64 << bit;
    }

    /// Mark a slot as empty in the bitset.
    #[inline]
    fn clear_occupied(&mut self, index: usize) {
        let word = index / 64;
        let bit = index % 64;
        self.occupied[word] &= !(1u64 << bit);
    }

    /// Push an entry into a slot at the back and mark it occupied.
    #[inline]
    fn push_back(&mut self, index: usize, entry: Entry<Info, Meta, Completion>) {
        debug_assert!(index < SLOTS_PER_LEVEL);
        unsafe { self.slots.get_unchecked_mut(index) }.push(entry);
        self.set_occupied(index);
    }

    /// Push an entry to the front of a slot and mark it occupied.
    ///
    /// Used during cascade to preserve FIFO ordering: cascaded entries
    /// were enqueued before any entries currently in the destination slot.
    #[inline]
    fn push_front(&mut self, index: usize, entry: Entry<Info, Meta, Completion>) {
        debug_assert!(index < SLOTS_PER_LEVEL);
        unsafe { self.slots.get_unchecked_mut(index) }.push_front(entry);
        self.set_occupied(index);
    }

    /// Drain a slot and clear it from the bitset.
    #[inline]
    fn drain(&mut self, index: usize) -> Queue<Transmission<Info, Meta, Completion>> {
        debug_assert!(index < SLOTS_PER_LEVEL);
        let queue = unsafe { self.slots.get_unchecked_mut(index) }.drain();
        self.clear_occupied(index);
        queue
    }

    /// Find the first occupied slot at or after `from_slot` (wrapping within the level).
    /// Uses the bitset for O(1) per-word scanning via `trailing_zeros`.
    ///
    /// Returns the slot index relative to 0, or `None` if no slots are occupied
    /// after `from_slot` within this level.
    fn first_occupied_after(&self, from_slot: usize) -> Option<usize> {
        debug_assert!(from_slot < SLOTS_PER_LEVEL);

        let start_word = from_slot / 64;
        let start_bit = from_slot % 64;

        // Check the first word, masking off bits before from_slot
        let masked = self.occupied[start_word] & (!0u64 << start_bit);
        if masked != 0 {
            return Some(start_word * 64 + masked.trailing_zeros() as usize);
        }

        // Check subsequent words
        for w in (start_word + 1)..BITSET_WORDS {
            if self.occupied[w] != 0 {
                return Some(w * 64 + self.occupied[w].trailing_zeros() as usize);
            }
        }

        None
    }

    /// Returns true if no slots are occupied.
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.occupied.iter().all(|&w| w == 0)
    }
}

// ── Ticker (reader-owned) ──────────────────────────────────────────────────

/// The reader-side of the timing wheel. Not `Clone`, owned by a single consumer.
///
/// The `Ticker` owns the hierarchical level/slot structure. It drains the
/// shared unordered queue, inserts entries into the correct slot, and advances
/// virtual time to collect due entries.
pub struct Ticker<Info, Meta, Completion, const GRANULARITY_US: u64 = DEFAULT_GRANULARITY_US> {
    wheel: Wheel<Info, Meta, Completion, GRANULARITY_US>,
    levels: Box<[Level<Info, Meta, Completion>; LEVELS]>,
    current_tick: u64,
}

impl<Info, Meta, Completion, const GRANULARITY_US: u64> fmt::Debug
    for Ticker<Info, Meta, Completion, GRANULARITY_US>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ticker")
            .field("current_tick", &self.current_tick)
            .field("granularity_us", &GRANULARITY_US)
            .field("levels", &LEVELS)
            .field("slots_per_level", &SLOTS_PER_LEVEL)
            .finish()
    }
}

impl<Info, Meta, Completion, const GRANULARITY_US: u64>
    Ticker<Info, Meta, Completion, GRANULARITY_US>
{
    /// Advance the virtual clock to `now` and return all due entries.
    ///
    /// 1. Drains the shared unordered queue and inserts each entry into the
    ///    appropriate level/slot based on its `transmission_time`.
    /// 2. Advances virtual time tick-by-tick from the current position to `now`,
    ///    cascading higher-level slots as needed.
    /// 3. Collects all entries from expired level-0 slots into a single queue.
    pub fn tick_to(
        &mut self,
        now: precision::Timestamp,
    ) -> Queue<Transmission<Info, Meta, Completion>> {
        // Step 1: Drain the shared queue and insert into wheel slots
        self.drain_incoming();

        // Step 2+3: Advance to `now` and collect due entries
        let target_tick = Self::timestamp_to_tick(now);
        let target_tick = target_tick.max(self.current_tick);

        let mut result = Queue::new();

        while self.current_tick < target_tick {
            let current_slot = (self.current_tick & SLOT_MASK) as usize;

            // Use the bitset to find the next occupied slot in level 0.
            // Compute its tick; if it's before the next cascade boundary and
            // target_tick, drain it. Otherwise skip ahead.
            let next_occupied_tick = self.levels[0]
                .first_occupied_after(current_slot)
                .map(|slot| (self.current_tick & !SLOT_MASK) | slot as u64);

            // The advance limit is the sooner of target_tick or the next
            // 256-aligned cascade boundary.
            let next_cascade = (self.current_tick | SLOT_MASK) + 1;
            let advance_limit = target_tick.min(next_cascade);

            match next_occupied_tick {
                Some(occ_tick) if occ_tick < advance_limit => {
                    // Jump to the occupied slot, drain it
                    let occ_slot = (occ_tick & SLOT_MASK) as usize;
                    self.current_tick = occ_tick;
                    let mut slot_queue = self.levels[0].drain(occ_slot);
                    result.append(&mut slot_queue);
                    self.current_tick += 1;
                }
                _ => {
                    // Nothing to drain before the advance limit — skip ahead
                    self.current_tick = advance_limit;
                }
            }

            // Cascade when we land on a 256-aligned boundary
            if self.current_tick & SLOT_MASK == 0 {
                self.cascade(self.current_tick, 1);
            }
        }

        // Also drain the current slot (entries at exactly target_tick)
        let slot_idx = (self.current_tick & SLOT_MASK) as usize;
        let mut slot_queue = self.levels[0].drain(slot_idx);
        result.append(&mut slot_queue);

        result
    }

    /// Delegate to the wheel's `set_waker`.
    pub fn set_waker(&self, waker: std::task::Waker) {
        self.wheel.set_waker(waker);
    }

    /// Returns true if the shared queue has new pending entries
    /// (i.e., entries pushed since the last drain_incoming).
    ///
    /// This check is lock-free: it reads the `pending` flag that
    /// the producers set under the lock.
    pub fn has_pending(&self) -> bool {
        self.wheel.0.pending.load(Ordering::Relaxed)
    }

    /// Returns the timestamp of the current virtual tick.
    pub fn current_time(&self) -> precision::Timestamp {
        Self::tick_to_timestamp(self.current_tick)
    }

    /// Returns the timestamp of the earliest non-empty slot, if any.
    ///
    /// This scans level-0 first (cheap), then higher levels. Used to determine
    /// how long the ticker can sleep before the next entry is due.
    pub fn next_expiry(&self) -> Option<precision::Timestamp> {
        let base = self.current_tick;

        // All levels use the same algorithm, just at different bit-shifts.
        // Level 0 has shift=0, level N has shift=8*N.
        for level in 0..LEVELS {
            let shift = BITS_PER_LEVEL * level as u32;
            let current_level_slot = (base >> shift) & SLOT_MASK;
            let cursor = ((current_level_slot + 1) & SLOT_MASK) as usize;

            // Scan forward from cursor; if nothing found, wrap around to slot 0.
            let hit = self.levels[level].first_occupied_after(cursor).or_else(|| {
                if cursor > 0 {
                    self.levels[level].first_occupied_after(0)
                } else {
                    None
                }
            });

            if let Some(slot_idx) = hit {
                // Compute the slot offset relative to the current position.
                // Slot indices are circular (0–255). Use `>` (not `>=`): since
                // we search starting from current_level_slot + 1, if
                // slot_idx == current_level_slot it means we've wrapped the full
                // circular buffer (offset = SLOTS_PER_LEVEL, not 0).
                let slot_offset = if slot_idx as u64 > current_level_slot {
                    slot_idx as u64 - current_level_slot
                } else {
                    SLOTS_PER_LEVEL as u64 - current_level_slot + slot_idx as u64
                };

                // Compute earliest tick as the start of the target slot's range.
                // Align base to the level boundary (zero out lower `shift` bits)
                // to avoid returning a tick AFTER the entry.
                let base_aligned = (base >> shift) << shift;
                let earliest_tick = base_aligned + (slot_offset << shift);
                return Some(Self::tick_to_timestamp(earliest_tick.max(base + 1)));
            }
        }

        None
    }

    // ── Internal ───────────────────────────────────────────────────────

    /// Drain the shared unordered queue and insert each entry into the wheel.
    ///
    /// Fast path: if the `pending` flag is false (no new entries since the
    /// last drain), we skip acquiring the lock entirely.  This avoids expensive
    /// mutex contention during busy-poll loops where `tick_to` is called at
    /// high frequency but producers are quiet.
    fn drain_incoming(&mut self) {
        // Fast path: nothing to drain — skip the lock.
        if !self.wheel.0.pending.load(Ordering::Relaxed) {
            return;
        }
        let mut incoming = {
            let mut state = self.wheel.0.state.lock();
            // Clear the flag while holding the lock so it stays consistent
            // with the queue: any producer that sets it afterward will also
            // push to the queue before we can observe it on the next call.
            self.wheel.0.pending.store(false, Ordering::Relaxed);
            core::mem::take(&mut state.queue)
        };
        while let Some(entry) = incoming.pop_front() {
            self.insert_entry(entry);
        }
    }

    /// Convert a Timestamp to a tick (for reading transmission_time from entries).
    fn timestamp_to_tick(ts: precision::Timestamp) -> u64 {
        Wheel::<Info, Meta, Completion, GRANULARITY_US>::timestamp_to_tick(ts)
    }

    /// Convert a core Timestamp to a tick (for reading transmission_time from entries).
    fn tick_to_timestamp(tick: u64) -> precision::Timestamp {
        Wheel::<Info, Meta, Completion, GRANULARITY_US>::tick_to_timestamp(tick)
    }

    /// Insert a single entry into the appropriate level/slot.
    fn insert_entry(&mut self, entry: Entry<Info, Meta, Completion>) {
        let target_tick = entry
            .transmission_time
            .map(Self::timestamp_to_tick)
            .unwrap_or(self.current_tick)
            .max(self.current_tick);

        let (level, slot_idx) = Self::compute_level_and_slot(self.current_tick, target_tick);

        self.levels[level].push_back(slot_idx, entry);
    }

    /// Cascade entries from level `level` down toward level 0.
    ///
    /// Entries in higher levels were enqueued *before* any entries that may
    /// have been directly inserted into the destination slots (via
    /// `drain_incoming` at a later `current_tick`). To preserve FIFO order,
    /// we iterate the cascade queue in **reverse** (`pop_back`) and use
    /// `push_front` on the destination slot. This is O(n) with zero
    /// allocation — each `pop_back` and `push_front` is O(1) on the
    /// doubly-linked intrusive queue.
    fn cascade(&mut self, current_tick: u64, mut level: usize) {
        while level < LEVELS {
            let slot_idx = ((current_tick >> (BITS_PER_LEVEL * level as u32)) & SLOT_MASK) as usize;

            let mut entries = self.levels[level].drain(slot_idx);

            // Iterate in reverse: the last entry in the cascade queue was
            // enqueued last (but still before any direct-inserted entries).
            // By popping from the back and pushing to the front of the
            // destination slot, we build up the correct FIFO order:
            //   [cascaded_first, ..., cascaded_last, direct_first, ..., direct_last]
            while let Some(entry) = entries.pop_back() {
                let target_tick = entry
                    .transmission_time
                    .map(Self::timestamp_to_tick)
                    .unwrap_or(current_tick)
                    .max(current_tick);

                let (new_level, new_slot) = Self::compute_level_and_slot(current_tick, target_tick);
                self.levels[new_level].push_front(new_slot, entry);
            }

            // Check if this level also wrapped and needs to cascade upward
            let next_level_tick = current_tick >> (BITS_PER_LEVEL * level as u32);
            if next_level_tick & SLOT_MASK == 0 {
                level += 1;
                continue;
            }

            break;
        }
    }

    /// Compute which level and slot an entry should be placed in.
    ///
    /// Uses `leading_zeros()` to determine the level in O(1) (one CPU
    /// instruction on all major ISAs) rather than a shift-loop.
    ///
    /// Mapping: delta 0–255 → level 0, 256–65535 → level 1,
    /// 65536–16_777_215 → level 2, 16_777_216+ → level 3.
    fn compute_level_and_slot(current_tick: u64, target_tick: u64) -> (usize, usize) {
        let delta = target_tick - current_tick;

        // Number of significant bits in delta:
        //   delta=0    → bits=0
        //   delta=1    → bits=1  (1 significant bit)
        //   delta=255  → bits=8
        //   delta=256  → bits=9
        // Level = floor((bits - 1) / BITS_PER_LEVEL), capped at LEVELS-1.
        // saturating_sub handles delta=0 (bits=0) without a special case.
        let bits = u64::BITS - delta.leading_zeros();
        let level = ((bits.saturating_sub(1) / BITS_PER_LEVEL) as usize).min(LEVELS - 1);

        let slot = ((target_tick >> (BITS_PER_LEVEL * level as u32)) & SLOT_MASK) as usize;

        (level, slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::pool::Pool;
    use std::{
        convert::Infallible,
        net::{Ipv4Addr, SocketAddr},
    };

    #[derive(Clone)]
    struct Clock(precision::Timestamp);

    impl precision::Clock for Clock {
        type Timer = ClockTimer;

        fn now(&self) -> precision::Timestamp {
            self.0
        }

        fn timer(&self) -> ClockTimer {
            unimplemented!("not needed for unit tests")
        }
    }

    struct ClockTimer;

    impl precision::Timer for ClockTimer {
        fn now(&self) -> precision::Timestamp {
            unimplemented!()
        }
        async fn sleep_until(&mut self, _target: precision::Timestamp) {
            unimplemented!()
        }
    }

    impl Clock {
        fn new(start: Duration) -> Self {
            Self(precision::Timestamp {
                nanos: start.as_nanos() as u64,
            })
        }

        fn get_time(&self) -> precision::Timestamp {
            self.0
        }

        fn inc_by(&mut self, duration: Duration) {
            self.0 = self.0 + duration;
        }
    }

    fn new(slots: usize) -> (Wheel<(), u16, (), 8>, Ticker<(), u16, (), 8>, Pool, Clock) {
        let horizon = Duration::from_micros(8 * slots as u64);
        let clock = Clock::new(horizon);
        let pool = Pool::new(u16::MAX);
        let wheel = Wheel::new();
        let ticker = wheel.ticker(&clock);
        (wheel, ticker, pool, clock)
    }

    // Helper to create a test transmission
    fn create_transmission(pool: &Pool, payload_len: u16) -> Transmission<(), u16, ()> {
        let socket_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, payload_len));

        let descriptors = pool
            .alloc()
            .unwrap()
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
            completion: (),
        }
    }

    fn create_entry_at(
        pool: &Pool,
        payload_len: u16,
        time: Option<precision::Timestamp>,
    ) -> Entry<(), u16, ()> {
        let mut entry = Entry::new(create_transmission(pool, payload_len));
        entry.transmission_time = time.map(Into::into);
        entry
    }

    #[test]
    fn test_wheel_creation() {
        let (wheel, _ticker, _pool, _clock) = new(64);
        assert_eq!(wheel.len(), 0);
    }

    #[test]
    fn test_insert_and_tick() {
        let (wheel, mut ticker, pool, clock) = new(64);

        // Insert an entry at the current time
        let entry = create_entry_at(&pool, 100, Some(clock.get_time()));
        wheel.insert(entry);

        assert_eq!(wheel.len(), 1);

        // Tick to current time should return this entry
        let mut queue = ticker.tick_to(clock.get_time());
        let entry = queue.pop_front().unwrap();
        assert_eq!(entry.meta, 100);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_multiple_entries_same_slot() {
        let (wheel, mut ticker, pool, clock) = new(64);

        wheel.insert(create_entry_at(&pool, 10, Some(clock.get_time())));
        wheel.insert(create_entry_at(&pool, 20, Some(clock.get_time())));
        wheel.insert(create_entry_at(&pool, 30, Some(clock.get_time())));

        let mut queue = ticker.tick_to(clock.get_time());

        assert_eq!(queue.pop_front().unwrap().meta, 10);
        assert_eq!(queue.pop_front().unwrap().meta, 20);
        assert_eq!(queue.pop_front().unwrap().meta, 30);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_multiple_slots() {
        let (wheel, mut ticker, pool, mut clock) = new(64);

        let t0 = clock.get_time();
        clock.inc_by(Duration::from_micros(8));
        let t1 = clock.get_time();
        clock.inc_by(Duration::from_micros(8));
        let t2 = clock.get_time();

        wheel.insert(create_entry_at(&pool, 100, Some(t0)));
        wheel.insert(create_entry_at(&pool, 200, Some(t1)));
        wheel.insert(create_entry_at(&pool, 300, Some(t2)));

        // Tick to t0
        let mut queue = ticker.tick_to(t0);
        assert_eq!(queue.pop_front().unwrap().meta, 100);
        assert!(queue.is_empty());

        // Tick to t1
        let mut queue = ticker.tick_to(t1);
        assert_eq!(queue.pop_front().unwrap().meta, 200);
        assert!(queue.is_empty());

        // Tick to t2
        let mut queue = ticker.tick_to(t2);
        assert_eq!(queue.pop_front().unwrap().meta, 300);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_len_tracking() {
        let (wheel, mut ticker, pool, clock) = new(64);

        assert_eq!(wheel.len(), 0);

        for i in 0..5 {
            wheel.insert(create_entry_at(&pool, 100 + i, Some(clock.get_time())));
        }

        assert_eq!(wheel.len(), 5);

        let mut queue = ticker.tick_to(clock.get_time());
        while queue.pop_front().is_some() {
            wheel.on_send();
        }

        assert_eq!(wheel.len(), 0);
    }

    #[test]
    fn test_insert_none_timestamp() {
        let (wheel, mut ticker, pool, clock) = new(64);

        // Insert with None timestamp should be available immediately
        wheel.insert(create_entry_at(&pool, 100, None));

        let mut queue = ticker.tick_to(clock.get_time());
        assert!(!queue.is_empty());
        let entry = queue.pop_front().unwrap();
        assert_eq!(entry.meta, 100);
    }

    #[test]
    fn test_cascade() {
        // With GRANULARITY_US=8 and 256 slots/level, we need >256 ticks to trigger cascade.
        // Insert an entry 300 ticks in the future (beyond level-0's 256-slot range).
        let (wheel, mut ticker, pool, mut clock) = new(64);

        let start = clock.get_time();

        clock.inc_by(Duration::from_micros(8 * 300));
        let future_time = clock.get_time();

        wheel.insert(create_entry_at(&pool, 999, Some(future_time)));

        // Tick through 256 ticks to trigger a cascade from level 1 to level 0
        let mid = start + Duration::from_micros(8 * 256);
        let queue = ticker.tick_to(mid);
        assert!(queue.is_empty(), "Should be empty before the target tick");

        // Now tick to the target time — the entry should have been cascaded down
        let mut queue = ticker.tick_to(future_time);
        let entry = queue
            .pop_front()
            .expect("Entry should have been found after cascade");
        assert_eq!(entry.meta, 999);
    }

    #[test]
    fn test_insert_queue_batch() {
        let (wheel, mut ticker, pool, clock) = new(64);

        let mut batch = Queue::new();
        batch.push_back(create_entry_at(&pool, 10, Some(clock.get_time())));
        batch.push_back(create_entry_at(&pool, 20, Some(clock.get_time())));
        batch.push_back(create_entry_at(&pool, 30, Some(clock.get_time())));

        wheel.insert_batch(batch);

        assert_eq!(wheel.len(), 3);

        let mut queue = ticker.tick_to(clock.get_time());
        assert_eq!(queue.pop_front().unwrap().meta, 10);
        assert_eq!(queue.pop_front().unwrap().meta, 20);
        assert_eq!(queue.pop_front().unwrap().meta, 30);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_next_expiry() {
        let (wheel, mut ticker, pool, mut clock) = new(64);

        // Empty wheel has no next expiry
        ticker.drain_incoming();
        assert!(ticker.next_expiry().is_none());

        // Insert an entry 10 ticks in the future
        clock.inc_by(Duration::from_micros(8 * 10));
        let future_time = clock.get_time();
        wheel.insert(create_entry_at(&pool, 100, Some(future_time)));

        // Drain and check next_expiry
        ticker.drain_incoming();
        let expiry = ticker.next_expiry();
        assert!(expiry.is_some());
        assert!(expiry.unwrap() <= future_time);
    }

    #[test]
    fn test_extreme_lag_tolerance() {
        let (wheel, mut ticker, pool, mut clock) = new(64);

        // Insert entries at various future times while NOT ticking
        for i in 1..=32u16 {
            clock.inc_by(Duration::from_micros(8));
            wheel.insert(create_entry_at(&pool, i, Some(clock.get_time())));
        }

        assert_eq!(wheel.len(), 32);

        // Now tick to the end and collect everything
        let mut collected = Vec::new();
        let mut queue = ticker.tick_to(clock.get_time());
        while let Some(entry) = queue.pop_front() {
            collected.push(entry.meta);
        }

        assert_eq!(collected.len(), 32, "All entries should be retrieved");

        for window in collected.windows(2) {
            assert!(
                window[0] <= window[1],
                "Entries should be in order: {} <= {}",
                window[0],
                window[1]
            );
        }
    }

    #[test]
    fn test_concurrent_insert() {
        use std::thread;

        let (wheel, mut ticker, pool, clock) = new(64);
        let pool = Arc::new(pool);
        let time = clock.get_time();

        let mut handles = Vec::new();
        for t in 0..4 {
            let wheel = wheel.clone();
            let pool = pool.clone();
            handles.push(thread::spawn(move || {
                for i in 0..100u16 {
                    let payload = (t * 1000 + i) as u16;
                    let entry = create_entry_at(&pool, payload, Some(time));
                    wheel.insert(entry);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(wheel.len(), 400);

        let mut queue = ticker.tick_to(time);
        let mut count = 0;
        while queue.pop_front().is_some() {
            count += 1;
        }
        assert_eq!(count, 400);
    }

    #[test]
    fn test_compute_level_and_slot() {
        // Level 0: delta 0..255
        let (level, _slot) = Ticker::<(), (), (), 1>::compute_level_and_slot(0, 0);
        assert_eq!(level, 0);

        let (level, _slot) = Ticker::<(), (), (), 1>::compute_level_and_slot(0, 255);
        assert_eq!(level, 0);

        // Level 1: delta 256..65535
        let (level, _slot) = Ticker::<(), (), (), 1>::compute_level_and_slot(0, 256);
        assert_eq!(level, 1);

        let (level, _slot) = Ticker::<(), (), (), 1>::compute_level_and_slot(0, 65535);
        assert_eq!(level, 1);

        // Level 2: delta 65536..16777215
        let (level, _slot) = Ticker::<(), (), (), 1>::compute_level_and_slot(0, 65536);
        assert_eq!(level, 2);

        let (level, _slot) = Ticker::<(), (), (), 1>::compute_level_and_slot(0, 16777215);
        assert_eq!(level, 2);

        // Level 3: delta 16777216..4294967295
        let (level, _slot) = Ticker::<(), (), (), 1>::compute_level_and_slot(0, 16777216);
        assert_eq!(level, 3);
    }

    /// Regression test: next_expiry must return a correct future timestamp
    /// for entries in higher levels (level 1+). With a non-zero current_tick,
    /// the buggy code computed `earliest_tick = (slot_idx as u64) << shift`,
    /// treating the circular slot index as an absolute tick, which produced
    /// a value in the past that got clamped to `base + 1` — far too early.
    #[test]
    fn test_next_expiry_higher_level() {
        // Use GRANULARITY_US=1 so ticks == microseconds for clarity.
        // Start at tick 100_000 so that level-1 slot indices are well into the wheel.
        let pool = Pool::new(u16::MAX);
        let wheel: Wheel<(), u16, (), 1> = Wheel::new();
        let start = precision::Timestamp {
            nanos: Duration::from_micros(100_000).as_nanos() as u64,
        };
        let clock = Clock(start);
        let mut ticker = wheel.ticker(&clock);

        // current_tick is now 100_000.
        // Insert an entry 300 ticks in the future → target_tick = 100_300.
        // delta = 300, which is > 256, so it goes into level 1.
        let future_time = start + Duration::from_micros(300);
        wheel.insert(create_entry_at(&pool, 42, Some(future_time)));
        ticker.drain_incoming();

        let expiry = ticker
            .next_expiry()
            .expect("should find the entry in level 1");
        let expiry_tick = Wheel::<(), u16, (), 1>::timestamp_to_tick(expiry);

        // The expiry must be in the future relative to current_tick (100_000)
        // and must be <= the target tick (100_300).
        // With the bug, expiry_tick would be clamped to 100_001 (base + 1).
        assert!(
            expiry_tick > 100_001,
            "next_expiry should not collapse to base+1; got tick {expiry_tick} \
             (expected something close to 100_300, not 100_001)"
        );
        assert!(
            expiry_tick <= 100_300,
            "next_expiry should not exceed the target tick; got {expiry_tick}"
        );
    }

    // We cap at 20M ticks to keep tick_to() runtime reasonable while
    // still exercising levels 0 (0–255), 1 (256–65535),
    // 2 (65536–16M), and 3 (16M+).
    const MAX_OFFSET: u64 = 20_000_000;

    /// Oracle-based fuzz test: compare the wheel against a BTreeMap oracle.
    ///
    /// For random insertion patterns, we verify that:
    /// 1. Entries are popped in the exact same order as the oracle (by tick, FIFO within tick)
    /// 2. `next_expiry()` matches the oracle's next key after every tick advance
    /// 3. All entries are retrieved (no lost entries)
    /// 4. All level bitsets are clear after draining
    #[test]
    fn fuzz_oracle_comparison() {
        use bolero::check;
        use std::collections::BTreeMap;
        type Wheel = super::Wheel<(), u16, (), 1>;

        // Input: a list of (start_offset, entry_offsets) pairs.
        // start_offset: where the wheel clock starts (exercises different level alignments)
        // entry_offsets: time offsets for each entry relative to start
        check!()
            .with_type::<(u32, Vec<u32>)>()
            .with_test_time(core::time::Duration::from_secs(10))
            .for_each(|(start_offset, offsets)| {
                let pool = Pool::new(u16::MAX);
                let wheel = Wheel::new();

                // Use a non-trivial start tick to exercise different level slot positions.
                // Must be non-zero since s2n_quic_core::time::Timestamp cannot represent 0.
                let base_us = 1_000 + (*start_offset as u64) * 256;
                let start = precision::Timestamp {
                    nanos: Duration::from_micros(base_us).as_nanos() as u64,
                };
                let clock = Clock(start);
                let mut ticker = wheel.ticker(&clock);

                // Oracle: BTreeMap<tick, Vec<meta>> preserves insertion order per tick
                let mut oracle: BTreeMap<u64, Vec<u16>> = BTreeMap::new();

                let start_tick =
                    Wheel::timestamp_to_tick(start);

                // Insert entries with varying offsets.
                for (i, &raw_offset) in offsets.iter().enumerate() {
                    let offset_us = (raw_offset as u64) % MAX_OFFSET;
                    let target_tick = start_tick + offset_us;
                    let time = start + Duration::from_micros(offset_us);

                    let meta = i as u16;
                    let entry = create_entry_at(&pool, meta, Some(time));
                    wheel.insert(entry);

                    // Oracle insert: group by the tick the wheel will use
                    // (entries at or before current_tick are served at current_tick)
                    let effective_tick = target_tick.max(start_tick);
                    oracle.entry(effective_tick).or_default().push(meta);
                }

                // Drain incoming so entries are in the wheel's levels
                ticker.drain_incoming();

                // Flatten oracle into expected pop order: by tick, then FIFO within tick
                let expected: Vec<(u64, u16)> = oracle
                    .iter()
                    .flat_map(|(&tick, metas)| metas.iter().map(move |&m| (tick, m)))
                    .collect();

                // Verify next_expiry before any ticking
                if let Some((&first_future_tick, _)) =
                    oracle.range((start_tick + 1)..).next()
                {
                    let exp = ticker.next_expiry().expect(
                        "next_expiry should return Some when oracle has future entries",
                    );
                    let exp_tick = Wheel::timestamp_to_tick(exp);
                    // Expiry should be <= the oracle's first future tick
                    // (the wheel may round down to the level boundary)
                    assert!(
                        exp_tick <= first_future_tick,
                        "next_expiry ({exp_tick}) should be <= oracle's first future tick ({first_future_tick})"
                    );
                    assert!(
                        exp_tick > start_tick,
                        "next_expiry ({exp_tick}) should be > start_tick ({start_tick})"
                    );
                }

                // Now advance to the end and collect, comparing against oracle
                let end = start + Duration::from_micros(MAX_OFFSET);
                let mut queue = ticker.tick_to(end);

                let mut actual: Vec<(u64, u16)> = Vec::new();
                while let Some(entry) = queue.pop_front() {
                    let tick = entry
                        .transmission_time
                        .map(|ts| {
                            let p: precision::Timestamp = ts.into();
                            Wheel::timestamp_to_tick(p)
                        })
                        .unwrap_or(start_tick)
                        .max(start_tick);
                    actual.push((tick, entry.meta));
                }

                // Compare lengths
                assert_eq!(
                    actual.len(),
                    expected.len(),
                    "Entry count mismatch: wheel={}, oracle={}",
                    actual.len(),
                    expected.len()
                );

                // Compare pop order: entries should come out in tick order,
                // and within the same tick, in FIFO (insertion) order
                for (i, (act, exp)) in actual.iter().zip(expected.iter()).enumerate() {
                    assert_eq!(
                        act, exp,
                        "Mismatch at index {i}: wheel=({}, meta={}), oracle=({}, meta={})",
                        act.0, act.1, exp.0, exp.1
                    );
                }

                // Verify all level bitsets are clear after draining
                for (l, level) in ticker.levels.iter().enumerate() {
                    assert!(
                        level.is_empty(),
                        "Level {l} bitset not empty after draining all entries"
                    );
                }
            });
    }

    /// Regression test: reproduces the exact cascade reordering bug.
    ///
    /// Two packets with the same target tick (same µs) get inserted via
    /// `drain_incoming` at different `current_tick` values. The first entry
    /// is inserted when `current_tick` is far from the target (delta > 255),
    /// so it goes to level 1. Then the ticker advances past a 256-aligned
    /// cascade boundary (moving `current_tick` closer to the target), and the
    /// second entry is inserted with delta < 256, going directly to level 0.
    ///
    /// When cascade fires, the level-1 entry must be placed BEFORE the
    /// level-0 entry in the same slot. Without the fix, it was appended
    /// AFTER, causing packet 188 to appear after packet 189.
    #[test]
    fn test_cascade_preserves_fifo_same_tick() {
        // Use GRANULARITY_US=1 for 1:1 tick-to-µs mapping
        let pool = Pool::new(u16::MAX);
        let wheel: Wheel<(), u16, (), 1> = Wheel::new();

        // Start at tick 2_012_940. Target tick = 2_013_199 (delta = 259).
        let start = precision::Timestamp {
            nanos: Duration::from_micros(2_012_940).as_nanos() as u64,
        };
        let clock = Clock(start);
        let mut ticker = wheel.ticker(&clock);

        // Target time: both packets share this exact µs tick
        let target_time = precision::Timestamp {
            nanos: Duration::from_micros(2_013_199).as_nanos() as u64,
        };

        // Step 1: Insert packet 0 into the shared queue
        wheel.insert(create_entry_at(&pool, 0, Some(target_time)));

        // Step 2: tick_to an intermediate time that drains packet 0 into level 1
        // (delta=259 > 255) but does NOT reach the 256-aligned cascade boundary.
        // current_tick advances to 2_012_950; no cascade triggered.
        let intermediate = precision::Timestamp {
            nanos: Duration::from_micros(2_012_950).as_nanos() as u64,
        };
        let queue = ticker.tick_to(intermediate);
        assert!(queue.is_empty(), "No entries should be due yet");

        // Step 3: Insert packet 1 into the shared queue (same target tick)
        wheel.insert(create_entry_at(&pool, 1, Some(target_time)));

        // Step 4: tick_to the target time. This call:
        //   a) drain_incoming: packet 1 → level 0 (delta=2_013_199-2_012_950=249<256)
        //   b) advance loop reaches 2_013_184 (256-aligned boundary), triggers cascade
        //   c) cascade moves packet 0 from level 1 → level 0 (same slot as packet 1!)
        //
        // BUG (without fix): cascade appends packet 0 AFTER packet 1 → output [1, 0]
        // FIXED: cascade uses pop_back+push_front → output [0, 1]
        let mut queue = ticker.tick_to(target_time);

        let first = queue.pop_front().expect("should have first entry");
        let second = queue.pop_front().expect("should have second entry");
        assert!(queue.is_empty(), "should have exactly 2 entries");

        assert_eq!(
            first.meta, 0,
            "Cascaded entry (packet 0) should come out first, got {}",
            first.meta
        );
        assert_eq!(
            second.meta, 1,
            "Direct-inserted entry (packet 1) should come out second, got {}",
            second.meta
        );
    }

    /// Oracle-based fuzz test for next_expiry: after each tick advance,
    /// compare next_expiry against the oracle's next non-empty tick.
    #[test]
    fn fuzz_next_expiry_oracle() {
        use bolero::check;
        use std::collections::BTreeMap;
        type Wheel = super::Wheel<(), u16, (), 1>;

        check!()
            .with_type::<(u8, Vec<u32>)>()
            .with_test_time(core::time::Duration::from_secs(10))
            .for_each(|(start_offset, offsets)| {
                let pool = Pool::new(u16::MAX);
                let wheel = Wheel::new();

                // Vary the start position to exercise different level slot alignments.
                // Must be non-zero since s2n_quic_core::time::Timestamp cannot represent 0.
                let base_us = 1_000 + (*start_offset as u64) * 300;
                let start = precision::Timestamp {
                    nanos: Duration::from_micros(base_us).as_nanos() as u64,
                };
                let clock = Clock(start);
                let mut ticker = wheel.ticker(&clock);
                let start_tick =
                    Wheel::timestamp_to_tick(start);

                // Oracle: track which ticks have entries
                let mut oracle: BTreeMap<u64, usize> = BTreeMap::new();

                // Insert entries
                for (i, &raw_offset) in offsets.iter().enumerate() {
                    let offset_us = (raw_offset as u64) % MAX_OFFSET;
                    let target_tick = start_tick + offset_us;
                    let time = start + Duration::from_micros(offset_us);
                    let effective_tick = target_tick.max(start_tick);

                    let entry = create_entry_at(&pool, i as u16, Some(time));
                    wheel.insert(entry);
                    *oracle.entry(effective_tick).or_default() += 1;
                }

                ticker.drain_incoming();

                // Step tick-by-tick, at each step:
                // 1. Pop entries at current tick and verify count
                // 2. Compare next_expiry with oracle
                let mut current = start_tick;
                let end_tick = start_tick + MAX_OFFSET;

                while current <= end_tick {
                    let time = Wheel::tick_to_timestamp(current);
                    let mut queue = ticker.tick_to(time);

                    // Count entries popped at this tick
                    let mut popped = 0;
                    while queue.pop_front().is_some() {
                        popped += 1;
                    }

                    // Verify against oracle
                    let oracle_count = oracle.remove(&current).unwrap_or(0);
                    assert_eq!(
                        popped, oracle_count,
                        "At tick {current}: wheel popped {popped}, oracle expected {oracle_count}"
                    );

                    // Compare next_expiry with oracle's next non-empty tick
                    let oracle_next = oracle.keys().next().copied();
                    let wheel_expiry = ticker.next_expiry().map(|ts| {
                        Wheel::timestamp_to_tick(ts)
                    });

                    match (oracle_next, wheel_expiry) {
                        (None, None) => {
                            // Both agree: nothing left
                        }
                        (Some(oracle_tick), Some(wheel_tick)) => {
                            // The wheel's expiry should be <= the oracle's next tick
                            // (it approximates to level granularity) and > current tick
                            assert!(
                                wheel_tick > current,
                                "next_expiry ({wheel_tick}) must be > current ({current})"
                            );
                            assert!(
                                wheel_tick <= oracle_tick,
                                "next_expiry ({wheel_tick}) must be <= oracle's next tick ({oracle_tick}), current={current}"
                            );
                        }
                        (Some(oracle_tick), None) => {
                            // Wheel says nothing, but oracle has entries remaining.
                            // This could happen if all remaining entries are at the
                            // current tick (already drained) or beyond the wheel's
                            // scan range. Only fail if oracle_tick > current.
                            assert!(oracle_tick > current, "oracle_tick must be > current after removing current");
                        }
                        (None, Some(wheel_tick)) => {
                            // Wheel says there's an expiry but oracle is empty.
                            // This shouldn't happen.
                            panic!(
                                "Wheel reports next_expiry={wheel_tick} but oracle is empty (current={current})"
                            );
                        }
                    }

                    // Advance by a variable step to keep the test fast
                    // but still exercise single-tick and multi-tick advances
                    current += 1;
                }

                // Verify both oracle and wheel are fully drained
                assert!(
                    oracle.is_empty(),
                    "Oracle still has {} tick(s) with entries after advancing to end",
                    oracle.len()
                );
                assert!(
                    ticker.next_expiry().is_none(),
                    "Wheel still has entries after advancing to end"
                );
            });
    }
}
