// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Additional tests for the hierarchical timing wheel

use super::*;
use crate::{
    intrusive::{EntryAdapter, Queue},
    socket::channel::Receiver as _,
    time::{
        precision::Clock as _,
        testing::{Clock, Timer as ClockTimer},
    },
};
use core::pin::pin;
use s2n_quic_core::task::waker;
use std::{collections::BTreeMap, time::Duration};

type TestWheel<'a> = Wheel<EntryAdapter<TestEntry>, ClockTimer, &'a TestChannel, 1>;

// ── Test utilities ─────────────────────────────────────────────────────

/// Poll a future once and return the result if Ready, or None if Pending
fn poll_once<F: core::future::Future>(future: F) -> Option<F::Output> {
    let mut future = pin!(future);
    let waker = waker::noop();
    let mut cx = core::task::Context::from_waker(&waker);

    match future.as_mut().poll(&mut cx) {
        core::task::Poll::Ready(output) => Some(output),
        core::task::Poll::Pending => None,
    }
}

// Mock entry type for testing
struct TestEntry {
    meta: u16,
    transmission_time: Option<precision::Timestamp>,
}

impl SingleTimer for TestEntry {
    fn target_time(&self) -> Option<precision::Timestamp> {
        self.transmission_time
    }

    fn set_target_time(&mut self, time: precision::Timestamp) {
        self.transmission_time = Some(time);
    }
}

// Simple channel for testing
struct TestChannel {
    queue: std::sync::Mutex<std::collections::VecDeque<Queue<TestEntry>>>,
    closed: std::sync::atomic::AtomicBool,
}

impl TestChannel {
    fn new() -> Self {
        Self {
            queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn send_batch(&self, batch: Queue<TestEntry>) {
        self.queue.lock().unwrap().push_back(batch);
    }
}

impl channel::Receiver<Queue<TestEntry>> for &TestChannel {
    fn poll_recv(
        &mut self,
        _cx: &mut task::Context<'_>,
        _budget: &mut channel::Budget,
    ) -> task::Poll<Option<Queue<TestEntry>>> {
        if let Some(batch) = self.queue.lock().unwrap().pop_front() {
            return task::Poll::Ready(Some(batch));
        }

        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return task::Poll::Ready(None);
        }

        task::Poll::Pending
    }

    fn on_consumed(&mut self, _bytes: u64) {}
}

// ── Tests ──────────────────────────────────────────────────────────────

#[test]
fn test_immediate_transmission() {
    let clock = Clock::new(Duration::from_micros(1000));
    let channel = TestChannel::new();
    let mut wheel: TestWheel = Wheel::new(&channel, clock.timer());

    // Entry with None timestamp should bypass wheel and be immediately available
    let mut batch = Queue::new();
    batch.push_back(
        TestEntry {
            meta: 42,
            transmission_time: None,
        }
        .into(),
    );
    channel.send_batch(batch);

    let mut queue = poll_once(core::future::poll_fn(|cx| {
        wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
    }))
    .unwrap()
    .unwrap();

    assert_eq!(queue.pop_front().unwrap().meta, 42);
    assert!(queue.is_empty());
}

#[test]
fn test_len_tracking() {
    let clock = Clock::new(Duration::from_micros(1000));
    let channel = TestChannel::new();
    let mut wheel: TestWheel = Wheel::new(&channel, clock.timer());

    assert_eq!(wheel.len, 0);

    // Send 5 entries with future timestamps (should stay in wheel)
    let future_time = clock.get_time() + Duration::from_micros(10);
    let mut batch = Queue::new();
    for i in 0..5 {
        batch.push_back(
            TestEntry {
                meta: 100 + i,
                transmission_time: Some(future_time),
            }
            .into(),
        );
    }
    channel.send_batch(batch);

    // Poll to insert them into wheel - should insert and check if any are ready
    let result = poll_once(core::future::poll_fn(|cx| {
        wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
    }));
    assert!(
        result.is_none(),
        "Entries should not be ready yet (they're in the future)"
    );
    assert_eq!(wheel.len, 5, "Len should be 5 after inserting");

    // Advance timer to future time and drain them
    clock.set(future_time);
    let mut queue = poll_once(core::future::poll_fn(|cx| {
        wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
    }))
    .unwrap()
    .unwrap();

    while queue.pop_front().is_some() {}
    assert_eq!(wheel.len, 0, "Len should be 0 after draining");
}

#[test]
fn test_cascade() {
    // With GRANULARITY_US=1 and 256 slots/level, we need >256 ticks to trigger cascade
    let clock = Clock::new(Duration::from_micros(1000));
    let channel = TestChannel::new();
    let mut wheel: TestWheel = Wheel::new(&channel, clock.timer());

    // Insert entry 300 µs in future (beyond level-0's 256-slot range)
    let future_time = clock.get_time() + Duration::from_micros(300);
    let mut batch = Queue::new();
    batch.push_back(
        TestEntry {
            meta: 999,
            transmission_time: Some(future_time),
        }
        .into(),
    );
    channel.send_batch(batch);

    // Poll to insert
    let _ = poll_once(core::future::poll_fn(|cx| {
        wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
    }));

    // Advance timer to 256 ticks - should still be pending (in level 1)
    clock.advance(Duration::from_micros(256));
    let result = poll_once(core::future::poll_fn(|cx| {
        wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
    }));
    assert!(result.is_none(), "Entry should not be ready yet");

    // Advance to target time - should get the entry after cascade
    clock.set(future_time);
    let mut queue = poll_once(core::future::poll_fn(|cx| {
        wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
    }))
    .unwrap()
    .unwrap();

    assert_eq!(queue.pop_front().unwrap().meta, 999);
}

#[test]
fn test_ordering() {
    let clock = Clock::new(Duration::from_micros(1000));
    let channel = TestChannel::new();
    let mut wheel: TestWheel = Wheel::new(&channel, clock.timer());

    // Insert entries in reverse order - wheel should reorder by time
    // Start at 1 to avoid entries at current time (which are immediately ready)
    let mut batch = Queue::new();
    for i in (1..11u16).rev() {
        batch.push_back(
            TestEntry {
                meta: i,
                transmission_time: Some(clock.get_time() + Duration::from_micros(i as u64)),
            }
            .into(),
        );
    }
    channel.send_batch(batch);

    // Poll to insert
    let _ = poll_once(core::future::poll_fn(|cx| {
        wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
    }));

    // Advance timer to drain all
    clock.advance(Duration::from_micros(100));

    let mut queue = poll_once(core::future::poll_fn(|cx| {
        wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
    }))
    .unwrap()
    .unwrap();

    // Should come out in order 1, 2, 3, ..., 10
    let mut collected = Vec::new();
    while let Some(entry) = queue.pop_front() {
        collected.push(entry.meta);
    }

    assert_eq!(collected.len(), 10);
    for (idx, expected) in (1..11).enumerate() {
        assert_eq!(
            collected[idx], expected as u16,
            "Entry at position {idx} should be {expected}"
        );
    }
}

#[test]
fn test_empty_wheel_fast_path() {
    let clock = Clock::new(Duration::from_micros(1000));
    let channel = TestChannel::new();
    let mut wheel: TestWheel = Wheel::new(&channel, clock.timer());

    // With empty wheel, advancing time should be fast (no slot scanning)
    let start_tick = wheel.current_tick;

    // Advance timer by 1000 ticks
    clock.advance(Duration::from_micros(1000));

    // Poll should handle the time advance efficiently
    let result = poll_once(core::future::poll_fn(|cx| {
        wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
    }));
    assert!(result.is_none());

    // Current tick should have advanced
    assert_eq!(
        wheel.current_tick,
        start_tick + 1000,
        "Wheel should advance time even when empty"
    );
}

#[test]
fn test_some_to_none_while_queued_still_returns_entry() {
    let clock = Clock::new(Duration::from_micros(1000));
    let channel = TestChannel::new();
    let mut wheel: TestWheel = Wheel::new(&channel, clock.timer());

    let future_time = clock.get_time() + Duration::from_micros(50);
    let mut batch = Queue::new();
    batch.push_back(
        TestEntry {
            meta: 77,
            transmission_time: Some(future_time),
        }
        .into(),
    );
    channel.send_batch(batch);

    let result = poll_once(core::future::poll_fn(|cx| {
        wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
    }));
    assert!(result.is_none(), "entry should be queued in wheel");
    assert_eq!(wheel.len, 1);

    let mut mutated = 0usize;
    for level in wheel.levels.iter_mut() {
        for slot in level.slots.iter_mut() {
            for entry in slot.iter_mut() {
                if entry.meta == 77 {
                    entry.transmission_time = None;
                    mutated += 1;
                }
            }
        }
    }
    assert_eq!(mutated, 1, "expected to mutate exactly one queued entry");

    clock.advance(Duration::from_micros(300));
    let mut queue = poll_once(core::future::poll_fn(|cx| {
        wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
    }))
    .unwrap()
    .unwrap();

    let entry = queue.pop_front().expect("entry should be returned");
    assert_eq!(entry.meta, 77);
    assert!(queue.is_empty());
}

/// Regression for the claimed "tick_to forward-only first_occupied_after strands
/// level-0 slots behind current_slot" bug.
///
/// We insert an entry whose target tick is in the NEXT rotation (so its level-0
/// slot index is numerically LESS than current_slot), advance current_tick to a
/// point past that slot index but still within the same rotation, then keep
/// advancing across the cascade boundary to the target. The claim says the entry
/// strands forever; the invariant says it must drain exactly at its target tick.
#[test]
fn next_rotation_low_slot_drains_after_wrap() {
    // Start at a tick whose slot index is high within the rotation.
    // base_us chosen so start_tick & 255 == 200.
    let base_us = 256 * 4000 + 200; // rotation-aligned + slot 200
    let clock = Clock::new(Duration::from_micros(base_us));
    let channel = TestChannel::new();
    let mut wheel: TestWheel = Wheel::new(&channel, clock.timer());

    let start_tick = wheel.current_tick;
    assert_eq!(start_tick & 0xff, 200, "test setup: expected slot 200");

    // Target 100µs in the future: absolute tick = start+100, slot index
    // = (start+100) & 255 = (200+100) & 255 = 44 — BEHIND current_slot 200.
    let target_offset = 100u64;
    let future_time = clock.get_time() + Duration::from_micros(target_offset);
    let target_tick = start_tick + target_offset;
    assert_eq!(
        (target_tick & 0xff) as usize,
        44,
        "test setup: target lands in slot 44 (behind current_slot 200)"
    );

    let mut batch = Queue::new();
    batch.push_back(
        TestEntry {
            meta: 1234,
            transmission_time: Some(future_time),
        }
        .into(),
    );
    channel.send_batch(batch);

    // Insert into wheel.
    let result = poll_once(core::future::poll_fn(|cx| {
        wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
    }));
    assert!(result.is_none(), "entry should be queued, not yet due");
    assert_eq!(wheel.len, 1);

    // Advance to a tick PAST slot 44's index but BEFORE the cascade boundary and
    // before the target. e.g. advance 10µs -> current_tick slot 210. This is the
    // moment the claim says first_occupied_after(210) returns None and strands
    // slot 44.
    clock.advance(Duration::from_micros(10));
    let result = poll_once(core::future::poll_fn(|cx| {
        wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
    }));
    assert!(
        result.is_none(),
        "entry still in the future (target is +100), nothing due yet"
    );
    assert_eq!(wheel.len, 1, "entry must still be queued, NOT stranded/lost");

    // Now advance to the target time. The wheel must cross the cascade boundary
    // (256-aligned) and drain slot 44 in the next rotation.
    clock.set(future_time);
    let queue = poll_once(core::future::poll_fn(|cx| {
        wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
    }));

    let mut queue = queue
        .expect("poll should be Ready")
        .expect("entry must be delivered, not stranded");
    let entry = queue.pop_front().expect("the +100µs entry must drain");
    assert_eq!(entry.meta, 1234);
    assert_eq!(wheel.len, 0, "wheel drained");
}

/// Scheduler-driven oracle fuzz test.
///
/// The existing `fuzz_oracle_comparison` re-polls the wheel at EVERY tick, so it
/// exercises `tick_to` exhaustively but NEVER depends on `next_expiry()` to decide
/// when to wake. Production does the opposite: the consumer sleeps until the time
/// `next_expiry()` reports, then polls once. A "context linked in the wheel that
/// never fires" is a wake-scheduling failure on exactly this path — invisible to
/// the every-tick oracle. This test drives the wheel the way production does.
#[test]
fn fuzz_next_expiry_driven() {
    use bolero::check;

    const MAX_OFFSET: u64 = 5_000_000;

    check!()
        .with_type::<(u32, Vec<u32>)>()
        .with_test_time(core::time::Duration::from_secs(10))
        .for_each(|(start_offset, offsets)| {
            if offsets.is_empty() {
                return;
            }
            let base_us = 1_000 + (*start_offset as u64) * 256;
            let start = precision::Timestamp {
                nanos: Duration::from_micros(base_us).as_nanos() as u64,
            };
            let clock = Clock::new(Duration::from_micros(base_us));
            let channel = TestChannel::new();
            let mut wheel: TestWheel = Wheel::new(&channel, clock.timer());

            let start_tick = timestamp_to_tick(start, 1);

            let mut oracle: BTreeMap<u64, Vec<u16>> = BTreeMap::new();
            let mut batch = Queue::new();
            for (i, &raw_offset) in offsets.iter().enumerate() {
                let offset_us = ((raw_offset as u64) % MAX_OFFSET) + 1;
                let target_tick = start_tick + offset_us;
                let time = start + Duration::from_micros(offset_us);
                batch.push_back(
                    TestEntry {
                        meta: i as u16,
                        transmission_time: Some(time),
                    }
                    .into(),
                );
                oracle.entry(target_tick).or_default().push(i as u16);
            }
            let expected_count = offsets.len();
            channel.send_batch(batch);

            // Insert.
            let _ = poll_once(core::future::poll_fn(|cx| {
                wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
            }));
            assert_eq!(wheel.len, expected_count);

            // Drive the wheel ONLY by what next_expiry() reports. If next_expiry
            // ever returns None while entries remain, or reports a time past which
            // an entry was already due (so it gets skipped), we either spin forever
            // (caught by the guard) or lose entries (caught by the final count).
            let mut total_collected = 0usize;
            let mut guard = 0usize;
            let guard_limit = expected_count * 4 + 16;

            while !oracle.is_empty() {
                guard += 1;
                assert!(
                    guard <= guard_limit,
                    "next_expiry-driven loop exceeded {guard_limit} iterations: \
                     likely a stranded entry (next_expiry not advancing). \
                     remaining oracle entries: {}",
                    oracle.values().map(|v| v.len()).sum::<usize>()
                );

                let expiry = wheel
                    .next_expiry()
                    .expect("entries remain but next_expiry() returned None (stranded!)");

                // next_expiry() must NEVER over-report: the wake time it returns
                // must not be strictly later than the earliest still-pending entry.
                // If it did, production would sleep past a due entry, leaving a
                // linked context stranded (the field-observed symptom). The clamp
                // is `earliest_tick.max(base + 1)`, so the floor is current_tick+1.
                let earliest_due_tick = *oracle.keys().next().unwrap();
                let expiry_tick_check = timestamp_to_tick(expiry, 1);
                assert!(
                    expiry_tick_check <= earliest_due_tick.max(wheel.current_tick + 1),
                    "next_expiry() over-reported: returned tick {expiry_tick_check} but \
                     earliest pending entry is due at tick {earliest_due_tick} \
                     (current_tick={}). Sleeping this long strands the entry.",
                    wheel.current_tick
                );

                // Sleep exactly until the reported expiry, like production does.
                clock.set(expiry);
                let expiry_tick = timestamp_to_tick(expiry, 1);

                let poll_result = poll_once(core::future::poll_fn(|cx| {
                    wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
                }));

                if let Some(Some(mut queue)) = poll_result {
                    while let Some(entry) = queue.pop_front() {
                        // Every drained entry must have been genuinely due.
                        let due_tick = oracle
                            .iter()
                            .find(|(_, metas)| metas.contains(&entry.meta))
                            .map(|(t, _)| *t)
                            .expect("drained an entry not in oracle (double-drain?)");
                        assert!(
                            due_tick <= expiry_tick,
                            "entry {} drained at tick {expiry_tick} but was due at {due_tick} \
                             (next_expiry woke us too early/late)",
                            entry.meta
                        );
                        // remove from oracle
                        if let Some(metas) = oracle.get_mut(&due_tick) {
                            metas.retain(|m| *m != entry.meta);
                            if metas.is_empty() {
                                oracle.remove(&due_tick);
                            }
                        }
                        total_collected += 1;
                    }
                }
            }

            assert_eq!(
                total_collected, expected_count,
                "all entries must drain via next_expiry-driven scheduling"
            );
            assert_eq!(wheel.len, 0);
        });
}

/// Drives the wheel the way production does: instead of polling every tick, it
/// only advances the clock to the wake time the wheel itself requests via its
/// internal `next_expiry`/timer. If `next_expiry` ever schedules a wake later
/// than an entry's true due time, that entry comes out late — exactly the
/// "stuck past its due date" symptom. We assert no entry is ever drained late.
#[test]
fn next_expiry_never_strands_entry() {
    use bolero::check;

    const MAX_OFFSET: u64 = 5_000_000;

    check!()
        .with_type::<(u32, Vec<u32>)>()
        .with_test_time(core::time::Duration::from_secs(10))
        .for_each(|(start_offset, offsets)| {
            if offsets.is_empty() {
                return;
            }

            let base_us = 1_000 + (*start_offset as u64) * 7;
            let start = precision::Timestamp {
                nanos: Duration::from_micros(base_us).as_nanos() as u64,
            };
            let clock = Clock::new(Duration::from_micros(base_us));
            let channel = TestChannel::new();
            let mut wheel: TestWheel = Wheel::new(&channel, clock.timer());

            let start_tick = timestamp_to_tick(start, 1);

            // record the due tick for each entry
            let mut due_tick: BTreeMap<u16, u64> = BTreeMap::new();
            let mut batch = Queue::new();
            for (i, &raw_offset) in offsets.iter().enumerate() {
                let offset_us = ((raw_offset as u64) % MAX_OFFSET) + 1;
                let time = start + Duration::from_micros(offset_us);
                let meta = i as u16;
                batch.push_back(
                    TestEntry {
                        meta,
                        transmission_time: Some(time),
                    }
                    .into(),
                );
                due_tick.insert(meta, (start_tick + offset_us).max(start_tick));
            }
            let expected_count = offsets.len();
            channel.send_batch(batch);

            // initial poll: insert everything, arm the timer
            let _ = poll_once(core::future::poll_fn(|cx| {
                wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
            }));

            // Track which entries are still queued and their due ticks.
            let mut remaining: BTreeMap<u16, u64> = due_tick.clone();

            let mut guard = 0u64;
            // Drive ONLY via the wheel's requested wake time, like the real task.
            while !remaining.is_empty() {
                guard += 1;
                assert!(
                    guard < (expected_count as u64 + 16) * 8,
                    "wheel failed to make progress: {} entries left",
                    remaining.len()
                );

                // The earliest tick at which SOME queued entry is actually due.
                let earliest_due = *remaining.values().min().unwrap();

                // What time does the wheel want to be woken at?
                let Some(expiry) = wheel.next_expiry() else {
                    panic!(
                        "next_expiry returned None with {} entries still queued",
                        wheel.len
                    );
                };
                let expiry_tick = timestamp_to_tick(expiry, 1);

                // CORE INVARIANT: the wheel must not schedule its wake LATER than the
                // earliest due entry. If it does, the task sleeps past the due time and
                // the entry is stranded until some unrelated event re-polls the wheel.
                assert!(
                    expiry_tick <= earliest_due.max(timestamp_to_tick(clock.get_time(), 1)),
                    "next_expiry scheduled wake at tick {expiry_tick}, but an entry is \
                     due at tick {earliest_due} — entry stranded past its due date"
                );

                // Simulate the task sleeping until exactly that wake time.
                let now = expiry.max(clock.get_time());
                clock.set(now);
                let now_tick = timestamp_to_tick(now, 1);

                let poll_result = poll_once(core::future::poll_fn(|cx| {
                    wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))
                }));

                if let Some(Some(mut queue)) = poll_result {
                    while let Some(entry) = queue.pop_front() {
                        let d = remaining.remove(&entry.meta).expect("entry drained twice");
                        assert!(
                            now_tick >= d,
                            "entry {} drained early at tick {now_tick} but due at {d}",
                            entry.meta
                        );
                    }
                }
            }

            assert_eq!(wheel.len, 0, "all entries should be drained");
        });
}

/// Oracle-based fuzz test: compare the wheel against a BTreeMap oracle.
///
/// For random insertion patterns, we verify that:
/// 1. Entries are retrieved in correct order by time
/// 2. All entries are retrieved (no lost entries)
/// 3. Len tracking is accurate
#[test]
fn fuzz_oracle_comparison() {
    use bolero::check;

    const MAX_OFFSET: u64 = 20_000_000;

    check!()
        .with_type::<(u32, Vec<u32>)>()
        .with_test_time(core::time::Duration::from_secs(10))
        .for_each(|(start_offset, offsets)| {
            let base_us = 1_000 + (*start_offset as u64) * 256;
            let start = precision::Timestamp {
                nanos: Duration::from_micros(base_us).as_nanos() as u64,
            };
            let clock = Clock::new(Duration::from_micros(base_us));
            let channel = TestChannel::new();
            let mut wheel: TestWheel =
                Wheel::new(&channel, clock.timer());

            let start_tick =
                timestamp_to_tick(start, 1);

            // Oracle: BTreeMap<tick, Vec<meta>> preserves insertion order per tick
            let mut oracle: BTreeMap<u64, Vec<u16>> = BTreeMap::new();

            // Insert entries with varying offsets (always at least 1µs in the future)
            let mut batch = Queue::new();
            for (i, &raw_offset) in offsets.iter().enumerate() {
                let offset_us = ((raw_offset as u64) % MAX_OFFSET) + 1;
                let target_tick = start_tick + offset_us;
                let time = start + Duration::from_micros(offset_us);
                let effective_tick = target_tick.max(start_tick);

                let meta = i as u16;
                batch.push_back(
                    TestEntry {
                        meta,
                        transmission_time: Some(time),
                    }
                    .into(),
                );
                oracle.entry(effective_tick).or_default().push(meta);
            }

            let expected_count = offsets.len();
            channel.send_batch(batch);

            // Poll to insert entries
            let _ = poll_once(core::future::poll_fn(|cx| wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))));

            // Verify len tracking
            assert_eq!(
                wheel.len, expected_count,
                "Wheel len should match number of inserted entries"
            );

            // Step through ticks and verify entries match oracle
            let mut current_tick = start_tick;
            let end_tick = start_tick + MAX_OFFSET;
            let mut total_collected = 0;

            while current_tick <= end_tick && !oracle.is_empty() {
                let current_time =
                    tick_to_timestamp(current_tick, 1);

                // Update timer to current time
                clock.set(current_time);

                // Poll to get entries
                let poll_result =
                    poll_once(core::future::poll_fn(|cx| wheel.poll_recv(cx, &mut channel::Budget::new(usize::MAX))));

                let mut wheel_entries = Vec::new();
                if let Some(Some(mut queue)) = poll_result {
                    while let Some(entry) = queue.pop_front() {
                        wheel_entries.push(entry.meta);
                        total_collected += 1;
                    }
                }

                // Check against oracle for this tick
                if let Some(oracle_entries) = oracle.remove(&current_tick) {
                    assert_eq!(
                        wheel_entries.len(),
                        oracle_entries.len(),
                        "At tick {current_tick}: wheel returned {} entries, oracle expected {}",
                        wheel_entries.len(),
                        oracle_entries.len()
                    );

                    // Entries should match in order (FIFO within same tick)
                    for (w, o) in wheel_entries.iter().zip(oracle_entries.iter()) {
                        assert_eq!(
                            w, o,
                            "At tick {current_tick}: entry mismatch. Wheel: {w}, Oracle: {o}"
                        );
                    }
                } else {
                    assert!(
                        wheel_entries.is_empty(),
                        "At tick {current_tick}: wheel returned {} entries but oracle expected none",
                        wheel_entries.len()
                    );
                }

                current_tick += 1;
            }

            // Verify all entries were collected
            assert_eq!(
                total_collected, expected_count,
                "Should collect all inserted entries. Got {total_collected}, expected {expected_count}"
            );

            // Verify len is back to 0
            assert_eq!(
                wheel.len, 0,
                "Wheel len should be 0 after draining all entries"
            );
        });
}
