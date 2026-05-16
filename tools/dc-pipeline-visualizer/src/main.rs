mod graph;
mod metrics;
mod protocol;
mod server;

use anyhow::Result;
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf, time::Duration};

#[derive(Debug, Parser)]
#[command(name = "dc-pipeline-visualizer")]
#[command(about = "Visualize s2n-quic-dc metrics pipelines in real time")]
struct Cli {
    #[arg(long)]
    metrics_file: PathBuf,

    #[arg(long)]
    graph_config: PathBuf,

    #[arg(long, default_value = "127.0.0.1:3000")]
    bind: SocketAddr,

    #[arg(long, default_value_t = 90)]
    history_seconds: u64,

    #[arg(long, default_value = "ui/dist")]
    ui_dist: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let graph = graph::GraphConfig::load(&cli.graph_config)?;
    let store = metrics::Store::new(metrics::StoreConfig {
        history_window: Duration::from_secs(cli.history_seconds),
    });

    server::run(cli.bind, graph, store, cli.metrics_file, cli.ui_dist).await
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .with_env_var("S2N_LOG")
        .from_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
