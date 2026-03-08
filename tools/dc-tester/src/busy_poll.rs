// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Busy poll worker pool for UDP socket I/O.
//!
//! Creates dedicated threads that busy-poll for incoming/outgoing packets,
//! reducing latency compared to epoll-based approaches.

use s2n_quic_dc::busy_poll;
use std::sync::OnceLock;

const DEFAULT_WORKERS: usize = if cfg!(debug_assertions) { 1 } else { 2 };

struct BusyPoll(OnceLock<busy_poll::Pool>);

impl BusyPoll {
    const fn new() -> Self {
        Self(OnceLock::new())
    }

    fn init() -> busy_poll::Pool {
        let workers = DEFAULT_WORKERS;

        let mut handles = Vec::with_capacity(workers);

        for idx in 0..workers {
            let (handle, runner) = busy_poll::Handle::new();

            std::thread::Builder::new()
                .name(format!("dc-tester:busy_poll:{idx}"))
                .spawn(move || runner.run())
                .unwrap();

            handles.push(handle);
        }

        handles.into()
    }
}

impl core::ops::Deref for BusyPoll {
    type Target = busy_poll::Pool;

    fn deref(&self) -> &Self::Target {
        self.0.get_or_init(Self::init)
    }
}

static SEND_BUSY_POLL: BusyPoll = BusyPoll::new();
static RECV_BUSY_POLL: BusyPoll = BusyPoll::new();

/// Returns a clone of the send busy poll pool
pub fn send_pool() -> busy_poll::Pool {
    SEND_BUSY_POLL.clone()
}

/// Returns a clone of the recv busy poll pool
pub fn recv_pool() -> busy_poll::Pool {
    RECV_BUSY_POLL.clone()
}
