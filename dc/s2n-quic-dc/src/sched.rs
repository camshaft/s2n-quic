// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Neutral facade over the generic scheduling substrate.
//!
//! The credit pool, the channel combinator pipeline, and the token-bucket pacer were all written
//! against generic traits — they schedule abstract cost units and know nothing about QUIC. They
//! happen to live under `socket::channel` / `socket::rate` / `credit` because the network endpoint
//! was their first consumer, but the surface is reusable by any work scheduler.
//!
//! This module re-exports that generic surface under one name so a scheduler (e.g. [`crate::fs`])
//! can depend on `crate::sched::*` rather than reaching into `socket::*`. It is a pure facade —
//! no new behavior, no moved files — so the boundary is documented at zero cost and a later
//! physical move becomes a pure path change.
//!
//! The QUIC-specific *leaves* of the channel module (the socket send/recv adapters
//! [`crate::socket::channel::SocketSender`] / [`crate::socket::channel::SocketReceiver`], and the
//! [`crate::socket::channel::Sendable`] trait bound to the UDP socket traits) are deliberately
//! NOT re-exported here — a storage scheduler's egress leaf is its backend, not a UDP socket.

// ── Channel pipeline (generic combinators + traits) ─────────────────────────
pub use crate::socket::channel::{
    Budget, ByteCost, EntryBoxSender, FilterMap, Flatten, FlattenList, FlattenQueue,
    GaugedReceiver, GaugedSender, Inspect, InspectErr, Map, Paced, Priority, PrioritySelect,
    Receiver, ReceiverExt, Sender, UnboundedSender, UnwrapOk,
};

// ── Token-bucket pacer ──────────────────────────────────────────────────────
pub use crate::socket::rate::{Rate, TokenBucket};

// ── Credit pool (priority-tiered, demand-elastic fair share) ────────────────
pub use crate::credit::{
    AbandonResult, Config as CreditConfig, Distributor, GrantResult, Pool, Priority as TierPriority,
    Refill, Slot, WakerSink,
};
