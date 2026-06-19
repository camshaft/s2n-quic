// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Workload configuration for the fs scheduler stress tester (TOML or CLI-overridable defaults).

use serde::{Deserialize, Serialize};

/// Which backend to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Bounded blocking pread/pwrite thread pool.
    #[default]
    Syscall,
    /// io_uring (Linux only).
    Uring,
}

/// Read/write mix and access pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OpMix {
    /// All reads.
    Read,
    /// All writes.
    #[default]
    Write,
    /// Alternating read/write per op (50/50).
    Mixed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Backend to drive.
    pub backend: Backend,
    /// Number of execution lanes. One dedicated blocking thread per lane on either backend (a
    /// `pread`/`pwrite` worker for syscall, an io_uring ring thread for uring) — there is no separate
    /// worker-pool knob; the thread count *is* the lane count.
    pub lanes: usize,
    /// io_uring ring depth (uring only).
    pub ring_depth: u32,
    /// Use O_DIRECT (unbuffered, zero-copy aligned) IO.
    pub direct: bool,

    /// Device queue depth — max in-flight ops (the credit-pool capacity, in op cost units).
    pub queue_depth: u64,
    /// Number of concurrent submitter tasks (streams).
    pub streams: usize,
    /// Bytes per op (must be a multiple of 4096 in direct mode).
    pub op_size: usize,
    /// Read/write mix.
    pub op_mix: OpMix,
    /// Total file size in bytes (ops wrap within this range).
    pub file_size: u64,
    /// How long to run, in seconds.
    pub duration_secs: u64,
    /// Path to the test file (will be created/truncated). Defaults to a temp path.
    pub file: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: Backend::Syscall,
            lanes: 2,
            ring_depth: 256,
            direct: false,
            queue_depth: 128,
            streams: 16,
            op_size: 64 * 1024,
            op_mix: OpMix::Write,
            file_size: 1 << 30, // 1 GiB
            duration_secs: 5,
            file: None,
        }
    }
}

impl Config {
    /// Load from a TOML file, falling back to defaults for any unset field.
    pub fn from_toml(path: &str) -> std::io::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        toml::from_str(&s).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}
