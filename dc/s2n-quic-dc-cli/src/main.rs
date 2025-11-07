// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

mod client;
mod config;
mod endpoint;
mod metrics;
mod protocol;
mod server;
mod shared_state;
mod tui;
mod tui_logger;

use clap::{Parser, Subcommand};
use shared_state::SharedState;
use std::net::SocketAddr;

#[derive(Parser)]
#[command(name = "s2n-quic-dc-cli")]
#[command(about = "CLI tool for testing s2n-quic-dc with artificial workloads")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Path to configuration file
    #[arg(short, long, global = true)]
    config: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the server
    Server {
        /// Enable TUI mode
        #[arg(long)]
        tui: bool,
    },
    /// Run the client
    Client {
        /// Server address to connect to
        #[arg(short, long)]
        server: Option<String>,

        /// Workload name(s) to run
        #[arg(short, long)]
        workload: Vec<String>,

        /// Enable TUI mode
        #[arg(long)]
        tui: bool,
    },
}

fn setup_panic_handler(state: SharedState) {
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let panic_msg = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            format!("PANIC: {}", s)
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            format!("PANIC: {}", s)
        } else {
            "PANIC: unknown payload".to_string()
        };

        let location = if let Some(location) = panic_info.location() {
            format!(
                " at {}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        } else {
            " at unknown location".to_string()
        };

        let full_msg = format!("{}{}", panic_msg, location);

        // Log to state
        state.add_panic(full_msg.clone());
        state.add_log(full_msg.clone());

        // Also call the default panic handler for backtrace
        // default_panic(panic_info);
    }));
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Load configuration
    let config = if let Some(config_path) = cli.config {
        config::Config::from_file(config_path)?
    } else {
        config::Config::default()
    };

    match cli.command {
        Commands::Server { tui: use_tui } => {
            if use_tui {
                // Run server with TUI
                let state = SharedState::new();

                // Set up global panic handler to capture all panics
                setup_panic_handler(state.clone());

                // Initialize TUI logging - logs go to SharedState instead of stdout
                tui_logger::init_tui_tracing(state.clone());

                state.set_status("Initializing server".to_string());
                state.add_log("Server starting with TUI mode".to_string());

                let server = server::Server::new(config).await?.with_state(state.clone());

                // Spawn TUI task
                let tui_state = state.clone();
                let tui_handle = tokio::spawn(async move {
                    let mut tui = tui::Tui::new(tui_state);
                    tui.run().await
                });

                // Spawn server task
                let mut server_handle = tokio::spawn(async move { server.run().await });

                // Wait for either TUI to exit (user presses 'q') or server to fail
                tokio::select! {
                    tui_result = tui_handle => {
                        // User exited TUI, abort server
                        server_handle.abort();
                        tui_result??;
                    }
                    server_result = &mut server_handle => {
                        // Server task exited (error), abort TUI
                        state.set_status("Server stopped".to_string());
                        state.add_log("Server task ended".to_string());
                        server_result??;
                    }
                }

                Ok(())
            } else {
                // Run server without TUI (original behavior)
                tui_logger::init_normal_tracing();
                let server = server::Server::new(config).await?;
                server.run().await
            }
        }
        Commands::Client {
            server,
            workload,
            tui: use_tui,
        } => {
            // Determine server address - use CLI arg or default to server's listen address
            let server_addr: SocketAddr = if let Some(addr) = server {
                addr.parse()?
            } else {
                config.server.listen_address
            };

            if use_tui {
                // Run client with TUI
                let state = SharedState::new();

                // Set up global panic handler to capture all panics
                setup_panic_handler(state.clone());

                // Initialize TUI logging - logs go to SharedState instead of stdout
                tui_logger::init_tui_tracing(state.clone());

                state.set_status("Initializing client".to_string());
                state.add_log("Client starting with TUI mode".to_string());

                let client = client::Client::new(config.clone(), server_addr)
                    .await?
                    .with_state(state.clone());

                // Spawn TUI task
                let tui_state = state.clone();
                let tui_handle = tokio::spawn(async move {
                    let mut tui = tui::Tui::new(tui_state);
                    tui.run().await
                });

                // Run workloads
                let workload_result = async {
                    if !workload.is_empty() {
                        for workload_name in workload {
                            client.run_workload(&workload_name).await?;
                        }
                    } else {
                        state.add_log(
                            "No workload specified, running all configured workloads".to_string(),
                        );
                        let workload_names = client.workload_names();
                        for workload_name in workload_names {
                            client.run_workload(&workload_name).await?;
                        }
                    }
                    anyhow::Ok(())
                }
                .await;

                // Wait for TUI to exit (user presses 'q') or workload to fail
                tokio::select! {
                    tui_result = tui_handle => {
                        tui_result??;
                    }
                    workload_result = async { workload_result } => {
                        workload_result?;
                        // Workloads completed, wait for user to exit TUI
                        state.set_status("All workloads completed - press 'q' to exit".to_string());
                        state.add_log("All workloads completed".to_string());
                        // Wait a bit for user to see final metrics
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }

                Ok(())
            } else {
                // Run client without TUI (original behavior)
                tui_logger::init_normal_tracing();
                let client = client::Client::new(config, server_addr).await?;

                if !workload.is_empty() {
                    for workload_name in workload {
                        client.run_workload(&workload_name).await?;
                    }
                } else {
                    tracing::info!("No workload specified, running all configured workloads");
                    let workload_names = client.workload_names();
                    for workload_name in workload_names {
                        client.run_workload(&workload_name).await?;
                    }
                }

                Ok(())
            }
        }
    }
}
