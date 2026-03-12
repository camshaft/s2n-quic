// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Benchmarks for the send timing wheel.
//!
//! Scenarios measured:
//!
//! * `tick_to/empty`        – wheel has entries in slots but the shared input
//!                            queue is empty; exercises the `drain_incoming`
//!                            fast-path that avoids the mutex.
//! * `tick_to/single`       – one due entry per call (level-0, same-tick).
//! * `tick_to/batch_N`      – N due entries already in level-0 slots.
//! * `tick_to/cascade`      – entries live in level-1 and must cascade to
//!                            level-0 before being returned.
//! * `insert/single`        – one producer insert (lock + push + wake).
//! * `insert/batch_N`       – batch insert of N entries (single lock).
//! * `has_pending/lock_free` – the new lock-free `has_pending()` check.

use criterion::{BenchmarkId, Criterion, Throughput};
use s2n_quic_dc::{
    clock::precision,
    intrusive_queue::Queue,
    socket::{
        pool::Pool,
        send::{
            transmission::{Entry, Transmission},
            wheel::{self, Wheel},
        },
    },
};
use std::{
    convert::Infallible,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

// ── Clock stub ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct FixedClock(precision::Timestamp);

impl precision::Clock for FixedClock {
    type Timer = FixedTimer;
    fn now(&self) -> precision::Timestamp {
        self.0
    }
    fn timer(&self) -> FixedTimer {
        FixedTimer(self.0)
    }
}

struct FixedTimer(precision::Timestamp);

impl precision::Timer for FixedTimer {
    fn now(&self) -> precision::Timestamp {
        self.0
    }
    async fn sleep_until(&mut self, _target: precision::Timestamp) {}
}

// ── Entry factory ─────────────────────────────────────────────────────────────

fn make_entry(
    pool: &Pool,
    at: Option<precision::Timestamp>,
) -> Entry<(), u16, ()> {
    let socket_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 1234u16));
    let descriptors = pool
        .alloc()
        .unwrap()
        .fill_with(|addr, _cmsg, payload| {
            addr.set(socket_addr.into());
            let len = 64.min(payload.len());
            <Result<_, Infallible>>::Ok(len)
        })
        .unwrap()
        .into_iter()
        .map(|desc| (desc, ()))
        .collect();
    let mut entry = Entry::new(Transmission {
        descriptors,
        total_len: 64,
        meta: 0u16,
        transmission_time: None,
        completion: (),
    });
    entry.transmission_time = at.map(Into::into);
    entry
}

fn make_wheel(
    _pool: &Pool,
    start_us: u64,
) -> (Wheel<(), u16, (), 1>, wheel::Ticker<(), u16, (), 1>, precision::Timestamp) {
    let now = precision::Timestamp::from_nanos(Duration::from_micros(start_us).as_nanos() as u64);
    let clock = FixedClock(now);
    let wheel: Wheel<(), u16, (), 1> = Wheel::new();
    let ticker = wheel.ticker(&clock);
    (wheel, ticker, now)
}

// ── Benchmarks ────────────────────────────────────────────────────────────────

pub fn benches(c: &mut Criterion) {
    let pool = Pool::new(u16::MAX);

    // ── tick_to: empty (no new entries in shared queue) ───────────────────
    {
        let mut group = c.benchmark_group("send_wheel/tick_to");
        group.throughput(Throughput::Elements(1));

        // The wheel has no entries at all; drain_incoming should take the fast
        // path (pending==0) and avoid the mutex entirely.
        group.bench_function("empty", |b| {
            let (_wheel, mut ticker, now) = make_wheel(&pool, 1_000_000);
            b.iter(|| {
                let q = ticker.tick_to(now);
                let _ = std::hint::black_box(q);
            });
        });

        // One entry already drained into a level-0 slot (due now).
        group.bench_function("single_due", |b| {
            let (wheel, mut ticker, now) = make_wheel(&pool, 1_000_000);
            b.iter(|| {
                // Re-insert so there is always exactly one due entry.
                wheel.insert(make_entry(&pool, Some(now)));
                let q = ticker.tick_to(now);
                let _ = std::hint::black_box(q);
            });
        });

        // N entries already in the wheel's level-0 slots, all due now.
        for n in [8u64, 64, 256] {
            group.throughput(Throughput::Elements(n));
            group.bench_with_input(BenchmarkId::new("batch_due", n), &n, |b, &n| {
                let (wheel, mut ticker, now) = make_wheel(&pool, 1_000_000);
                b.iter(|| {
                    for _ in 0..n {
                        wheel.insert(make_entry(&pool, Some(now)));
                    }
                    let mut q = ticker.tick_to(now);
                    while q.pop_front().is_some() {}
                });
            });
        }

        // Entry lives in level-1 and must cascade into level-0.
        group.throughput(Throughput::Elements(1));
        group.bench_function("cascade", |b| {
            let (wheel, mut ticker, now) = make_wheel(&pool, 1_000_000);
            b.iter(|| {
                // 300 µs in the future → delta=300 > 255 → goes to level 1.
                let future = now + Duration::from_micros(300);
                wheel.insert(make_entry(&pool, Some(future)));
                // Advance past the cascade boundary (256 µs).
                let mid = now + Duration::from_micros(256);
                let _ = ticker.tick_to(mid);
                // Advance to the target: entry should cascade down and be returned.
                let q = ticker.tick_to(future);
                let _ = std::hint::black_box(q);
            });
        });
    }

    // ── insert ────────────────────────────────────────────────────────────
    {
        let mut group = c.benchmark_group("send_wheel/insert");
        group.throughput(Throughput::Elements(1));

        group.bench_function("single", |b| {
            let (wheel, mut ticker, now) = make_wheel(&pool, 1_000_000);
            b.iter(|| {
                wheel.insert(make_entry(&pool, Some(now)));
                // Drain so the wheel stays clean between iterations.
                let _ = ticker.tick_to(now);
            });
        });

        for n in [8u64, 64, 256] {
            group.throughput(Throughput::Elements(n));
            group.bench_with_input(BenchmarkId::new("batch", n), &n, |b, &n| {
                let (wheel, mut ticker, now) = make_wheel(&pool, 1_000_000);
                b.iter(|| {
                    let mut batch: Queue<Transmission<(), u16, ()>> = Queue::new();
                    for _ in 0..n {
                        batch.push_back(make_entry(&pool, Some(now)));
                    }
                    wheel.insert_batch(batch);
                    let mut q = ticker.tick_to(now);
                    while q.pop_front().is_some() {}
                });
            });
        }
    }

    // ── has_pending: lock-free check ──────────────────────────────────────
    {
        let mut group = c.benchmark_group("send_wheel/has_pending");
        group.throughput(Throughput::Elements(1));

        // Nothing pending — exercises the fast (false) path.
        group.bench_function("false", |b| {
            let (_wheel, ticker, _now) = make_wheel(&pool, 1_000_000);
            b.iter(|| std::hint::black_box(ticker.has_pending()));
        });
    }
}
