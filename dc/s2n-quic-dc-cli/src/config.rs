// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::SocketAddr, path::Path};

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub client: ClientConfig,
    #[serde(default)]
    pub tui: TuiConfig,
    #[serde(default)]
    pub workload: HashMap<String, WorkloadConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    #[serde(default = "default_server_listen_address")]
    pub listen_address: SocketAddr,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_address: default_server_listen_address(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClientConfig {}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {}
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TuiConfig {
    #[serde(default = "default_tui_refresh_rate_ms")]
    pub refresh_rate_ms: u64,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            refresh_rate_ms: default_tui_refresh_rate_ms(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkloadConfig {
    pub num_streams: usize,
    #[serde(default = "default_request_size")]
    pub request_size: usize,
    pub response_size: usize,
    #[serde(default)]
    pub delay_ms: u64,
}

fn default_server_listen_address() -> SocketAddr {
    "[::1]:4433".parse().unwrap()
}

fn default_tui_refresh_rate_ms() -> u64 {
    100
}

fn default_request_size() -> usize {
    1024
}

impl Config {
    pub fn from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }
}
