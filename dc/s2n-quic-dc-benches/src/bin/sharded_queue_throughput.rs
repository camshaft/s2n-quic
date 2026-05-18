// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Throughput test for intrusive queue channels.
//!
//! Spins up a configurable number of sender threads that each continuously
//! submit single-entry lists (worst case for contention) and a single receiver
//! thread that drains the queue. Throughput is reported every second.
//!
//! Run with `--release` for representative throughput numbers:
//!
//!     cargo run -p s2n-quic-dc-benches --bin sharded_queue_throughput --release -- \
//!         --channel sharded --senders 8 --shards 16 --duration 30
//!
//! Usage: sharded_queue_throughput [OPTIONS]
//!   --channel <sharded|sync> – channel implementation to test (default: sharded)
//!   --senders <N>            – number of sender threads (default: 4)
//!   --shards <N>             – sharded channel shard count, power-of-two required
//!                              (default: next power-of-two >= senders)
//!   --duration <SECONDS>     – how long to run (default: 10)
//!
//! For backwards compatibility, positional arguments are also accepted:
//!   sharded_queue_throughput [channel] [senders] [shards] [duration_secs]

use s2n_quic_dc::{
    intrusive_queue::{Entry, EntryAdapter, Queue},
    socket::channel::{
        intrusive_queue::{sharded, sync},
        Receiver as ChannelReceiver,
    },
};
use std::{
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Waker},
    thread,
    time::{Duration, Instant},
};

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChannelKind {
    Sharded,
    Sync,
}

impl ChannelKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sharded => "sharded",
            Self::Sync => "sync",
        }
    }
}

impl FromStr for ChannelKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "sharded" => Ok(Self::Sharded),
            "sync" | "non-sharded" | "non_sharded" => Ok(Self::Sync),
            other => Err(format!(
                "unknown channel {other:?}; expected 'sharded' or 'sync'"
            )),
        }
    }
}

#[derive(Debug)]
struct Config {
    channel: ChannelKind,
    senders: usize,
    shards: Option<usize>,
    duration: Duration,
}

impl Config {
    fn parse() -> Self {
        let mut channel = ChannelKind::Sharded;
        let mut senders = 4usize;
        let mut shards = None;
        let mut duration = Duration::from_secs(10);
        let mut positionals = Vec::new();

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--channel" => {
                    let Some(value) = args.next() else {
                        usage_and_exit("--channel requires a value");
                    };
                    channel = value
                        .parse::<ChannelKind>()
                        .unwrap_or_else(|err| usage_and_exit(&err));
                }
                "--senders" => {
                    let Some(value) = args.next() else {
                        usage_and_exit("--senders requires a value");
                    };
                    senders = parse_arg("--senders", &value);
                }
                "--shards" => {
                    let Some(value) = args.next() else {
                        usage_and_exit("--shards requires a value");
                    };
                    shards = Some(parse_arg("--shards", &value));
                }
                "--duration" => {
                    let Some(value) = args.next() else {
                        usage_and_exit("--duration requires a value");
                    };
                    duration = Duration::from_secs(parse_arg("--duration", &value));
                }
                "--help" | "-h" => usage_and_exit(""),
                value if value.starts_with('-') => {
                    usage_and_exit(&format!("unknown option {value:?}"));
                }
                value => positionals.push(value.to_owned()),
            }
        }

        if let Some(first) = positionals.first() {
            if let Ok(parsed_channel) = first.parse() {
                channel = parsed_channel;
                positionals.remove(0);
            }
        }

        if let Some(value) = positionals.first() {
            senders = parse_arg("senders", value);
        }
        if let Some(value) = positionals.get(1) {
            shards = Some(parse_arg("shards", value));
        }
        if let Some(value) = positionals.get(2) {
            duration = Duration::from_secs(parse_arg("duration_secs", value));
        }
        if positionals.len() > 3 {
            usage_and_exit("too many positional arguments");
        }

        Self {
            channel,
            senders,
            shards,
            duration,
        }
    }

    fn shard_count(&self) -> usize {
        let default = self.senders.next_power_of_two().max(1);
        let shards = self.shards.unwrap_or(default);
        if !shards.is_power_of_two() || shards == 0 {
            usage_and_exit("--shards must be a non-zero power of two");
        }
        shards
    }
}

fn parse_arg<T>(name: &str, value: &str) -> T
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .unwrap_or_else(|err| usage_and_exit(&format!("invalid {name}: {err}")))
}

fn usage_and_exit(message: &str) -> ! {
    if !message.is_empty() {
        eprintln!("error: {message}\n");
    }
    eprintln!(
        "usage: sharded_queue_throughput [--channel sharded|sync] [--senders N] [--shards N] [--duration SECONDS]\n\
         run with: cargo run -p s2n-quic-dc-benches --bin sharded_queue_throughput --release -- [OPTIONS]"
    );
    std::process::exit(if message.is_empty() { 0 } else { 2 });
}

// ── No-op waker ──────────────────────────────────────────────────────────────

fn noop_waker() -> Waker {
    Waker::noop().clone()
}

// ── Channel abstraction ───────────────────────────────────────────────────────

trait BenchSender {
    fn send_batch(&mut self, batch: Queue<u64>) -> Result<(), Queue<u64>>;
}

impl BenchSender for sharded::Sender<EntryAdapter<u64>> {
    #[inline(always)]
    fn send_batch(&mut self, batch: Queue<u64>) -> Result<(), Queue<u64>> {
        sharded::Sender::send_batch(self, batch)
    }
}

impl BenchSender for sync::Sender<u64> {
    #[inline(always)]
    fn send_batch(&mut self, batch: Queue<u64>) -> Result<(), Queue<u64>> {
        sync::Sender::send_batch(self, batch)
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let config = Config::parse();
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };

    if cfg!(debug_assertions) {
        eprintln!("warning: running a debug build; use --release for throughput numbers");
    }

    match config.channel {
        ChannelKind::Sharded => {
            let shards = config.shard_count();
            let (tx, rx) = sharded::new::<u64>(shards);
            let waker = noop_waker();

            // Sharded receivers require registration before senders are cloned
            // or exposed to other threads. The benchmark busy polls the receiver,
            // so use a no-op waker to mirror production behavior.
            rx.register(&waker);

            run(config, Some(shards), profile, tx, rx, waker);
        }
        ChannelKind::Sync => {
            let (tx, rx) = sync::new::<u64>();
            let waker = noop_waker();
            run(config, None, profile, tx, rx, waker);
        }
    }
}

fn run<S, R>(config: Config, shards: Option<usize>, profile: &str, tx: S, mut rx: R, waker: Waker)
where
    S: BenchSender + Clone + Send + 'static,
    R: ChannelReceiver<Queue<u64>>,
{
    let duration_secs = config.duration.as_secs();
    let received = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    match shards {
        Some(shards) => eprintln!(
            "queue_throughput: channel={} profile={profile} senders={} shards={shards} duration={}s",
            config.channel.as_str(),
            config.senders,
            duration_secs,
        ),
        None => eprintln!(
            "queue_throughput: channel={} profile={profile} senders={} duration={}s",
            config.channel.as_str(),
            config.senders,
            duration_secs,
        ),
    }

    // Spawn sender threads. Each clone loops, submitting a single-entry list
    // until `stop` is set.
    let sender_handles: Vec<_> = (0..config.senders)
        .map(|_| {
            let mut sender = tx.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let mut list = Queue::new();
                    list.push_back(Entry::new(0u64));
                    if sender.send_batch(list).is_err() {
                        break;
                    }
                }
            })
        })
        .collect();

    // Drop the original sender so only the per-thread clones keep the channel
    // alive.
    drop(tx);

    let stats_handle = {
        let received = received.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            let mut prev = 0u64;
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_secs(1));
                let curr = received.load(Ordering::Relaxed);
                eprintln!("{} msgs/s", curr - prev);
                prev = curr;
            }
        })
    };

    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + config.duration;

    while Instant::now() < deadline {
        match rx.poll_recv(&mut cx) {
            std::task::Poll::Ready(Some(list)) => {
                received.fetch_add(list.len() as u64, Ordering::Relaxed);
            }
            std::task::Poll::Ready(None) => break,
            std::task::Poll::Pending => std::hint::spin_loop(),
        }
    }

    stop.store(true, Ordering::Relaxed);
    for h in sender_handles {
        let _ = h.join();
    }
    let _ = stats_handle.join();

    let total = received.load(Ordering::Relaxed);
    eprintln!("total received: {total} msgs in {duration_secs}s");
    eprintln!("average: {} msgs/s", total / duration_secs.max(1));
}
