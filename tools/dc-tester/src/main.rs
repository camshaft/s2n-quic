// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

mod busy_poll;
mod client;
mod config;
mod endpoint;
mod psk;
mod server;
mod stats;

use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf, time::Duration};

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Parser)]
#[command(name = "dc-tester")]
#[command(about = "dcQUIC load testing tool")]
struct Cli {
    /// Directory to write diagnostic event traces for errored streams
    #[arg(long, default_value = "/tmp/dc-traces")]
    trace_dir: PathBuf,

    /// Print the endpoint runtime pipeline graph in Graphviz DOT format.
    #[arg(long)]
    print_pipeline_dot: bool,

    /// Arm the frame/packet flight recorder and dump it to disk every N seconds (requires the
    /// `frame-trace` build feature). Each dump is a self-describing backbeat `.bb` file under
    /// `BACKBEAT_PATH` (default `${TMPDIR}/backbeat.<pid>.bb`); read with the `backbeat` CLI.
    #[arg(long, value_name = "SECS")]
    frame_trace_interval: Option<u64>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the RPC server
    Server {
        /// Path to configuration file
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Override the server address
        #[arg(short, long)]
        address: Option<SocketAddr>,
    },
    /// Start the RPC client
    Client {
        /// Path to configuration file
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Server address(es) to connect to (e.g., [::1]:4433). Can be specified multiple times for round-robin.
        #[arg(short, long)]
        server_addr: Vec<SocketAddr>,

        /// Workload names to run (defaults to first in config if omitted)
        #[arg(short, long)]
        workloads: Vec<String>,
    },
}

fn main() -> std::io::Result<()> {
    init_tracing();

    if let Ok(malloc_conf) = std::env::var("_RJEM_MALLOC_CONF") {
        eprintln!("_RJEM_MALLOC_CONF is set: {}", malloc_conf);
    } else {
        eprintln!("_RJEM_MALLOC_CONF is NOT set");
    }

    match tikv_jemalloc_ctl::profiling::prof::read() {
        Ok(enabled) => eprintln!("jemalloc profiling enabled: {}", enabled),
        Err(e) => eprintln!("jemalloc profiling check failed: {}", e),
    }
    match tikv_jemalloc_ctl::profiling::prof_final::read() {
        Ok(final_dump) => eprintln!("jemalloc prof_final: {}", final_dump),
        Err(e) => eprintln!("jemalloc prof_final check failed: {}", e),
    }
    match tikv_jemalloc_ctl::profiling::lg_prof_interval::read() {
        Ok(interval) => {
            let mb = interval
                .checked_sub(20)
                .and_then(|v| 1u64.checked_shl(v as u32))
                .unwrap_or(0);
            eprintln!("jemalloc lg_prof_interval: {} ({}MB)", interval, mb);
        }
        Err(e) => eprintln!("jemalloc lg_prof_interval check failed: {}", e),
    }
    match tikv_jemalloc_ctl::background_thread::write(true) {
        Ok(_) => eprintln!("jemalloc background_thread: enabled"),
        Err(e) => eprintln!("jemalloc background_thread: failed to enable: {}", e),
    }
    match unsafe { tikv_jemalloc_ctl::raw::write(b"arenas.dirty_decay_ms\0", 1000_isize) } {
        Ok(_) => eprintln!("jemalloc dirty_decay_ms: 1000"),
        Err(e) => eprintln!("jemalloc dirty_decay_ms: failed to set: {}", e),
    }
    match unsafe { tikv_jemalloc_ctl::raw::write(b"arenas.muzzy_decay_ms\0", 5000_isize) } {
        Ok(_) => eprintln!("jemalloc muzzy_decay_ms: 5000"),
        Err(e) => eprintln!("jemalloc muzzy_decay_ms: failed to set: {}", e),
    }

    let cli = Cli::parse();

    let config = match &cli.command {
        Commands::Server { config, .. } | Commands::Client { config, .. } => {
            if let Some(path) = config {
                config::Config::load(path)?
            } else {
                config::Config::default()
            }
        }
    };

    let busy_poll_workers = config.endpoint.total_workers();
    let available_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let tokio_threads = available_cpus.saturating_sub(busy_poll_workers).max(1);

    eprintln!("CPUs: {available_cpus}, busy_poll: {busy_poll_workers}, tokio: {tokio_threads}");

    std::env::set_var("TOKIO_WORKER_THREADS", tokio_threads.to_string());
    let dial9_config = dial9_tokio_telemetry::Dial9Config::from_env();
    let runtime = dial9_tokio_telemetry::TracedRuntime::new(dial9_config);

    runtime.block_on(async move {
        if let Some(secs) = cli.frame_trace_interval {
            spawn_frame_trace_dumper(secs);
        }

        let spawner = busy_poll::create_pool(busy_poll_workers);
        let data_bind: SocketAddr = "[::]:0".parse().unwrap();
        let endpoint = endpoint::create(
            &config.endpoint,
            data_bind,
            &spawner,
            cli.print_pipeline_dot,
        )?;

        let mut reporter_config = s2n_quic_dc::counter::ReporterConfig::new(Duration::from_secs(1));
        reporter_config.sparse_mode = s2n_quic_dc::counter::SparseMode::Once;
        reporter_config.os_stats = true;
        endpoint
            .counters
            .clone()
            .spawn_reporter_with_config(reporter_config);

        match cli.command {
            Commands::Server { address, .. } => {
                let server_addr = address.unwrap_or(config.server.address);
                server::run(endpoint, server_addr).await
            }
            Commands::Client {
                server_addr,
                workloads,
                ..
            } => {
                // wait for the server to boot
                tokio::time::sleep(core::time::Duration::from_secs(1)).await;

                let server_addrs = if server_addr.is_empty() {
                    vec![config.server.address]
                } else {
                    server_addr
                };

                let mut client_config = config.client;
                if !workloads.is_empty() {
                    client_config
                        .workloads
                        .retain(|w| workloads.contains(&w.name));
                } else if client_config.workloads.len() > 1 {
                    client_config.workloads.truncate(1);
                }

                client::run(endpoint, client_config, server_addrs).await
            }
        }
    })
}

fn init_tracing() {
    use tracing_subscriber::{
        fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
    };

    let filter = EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .with_env_var("S2N_LOG")
        .from_env()
        .unwrap();

    let fmt_layer = fmt::layer().with_target(false).with_filter(filter);

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer::new())
        .init();
}

/// Arms the frame/packet flight recorder and spawns a task that dumps it to disk every `secs`
/// seconds. A healthy load test never hits the `QueueDbg` path that triggers a dump on its own, so
/// this drives `backbeat::global::trigger()` on an interval to capture rolling snapshots of the ring
/// across the run. Each `trigger()` returns immediately; the background dumper writes a fresh
/// timestamp-named `.bb`.
#[cfg(feature = "frame-trace")]
fn spawn_frame_trace_dumper(secs: u64) {
    // `s2n_quic_dc` records behind the `queue-dbg` gate (pulled in by our `frame-trace` feature);
    // backbeat capture starts disabled, so arm it here for the lifetime of the process.
    backbeat::global::enable();
    let period = Duration::from_secs(secs.max(1));
    eprintln!("frame-trace: enabled, dumping every {}s", period.as_secs());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        // Skip the immediate first tick — let the ring fill before the first dump.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            backbeat::global::trigger();
        }
    });
}

/// Stub when the `frame-trace` feature is not compiled in: the recorder's trace points are gated out
/// of `s2n-quic-dc`, so `--frame-trace-interval` cannot do anything. Fail loudly rather than silently.
#[cfg(not(feature = "frame-trace"))]
fn spawn_frame_trace_dumper(_secs: u64) {
    panic!(
        "--frame-trace-interval requires the `frame-trace` build feature \
         (cargo build -p dc-tester --features frame-trace)"
    );
}
