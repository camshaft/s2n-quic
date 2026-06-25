// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Gating for the storage IO scheduler's per-op flight recorder (see [`crate::fs::trace`]).
//!
//! This is the storage analog of [`crate::endpoint::dbg`] — a single `const` gate so every trace
//! call site folds to nothing in a plain release build. It is deliberately a **separate** gate from
//! `endpoint::dbg`/`queue-dbg`: the storage scheduler is a distinct subsystem with a different
//! work-unit and a different operator audience, so its tracing is switchable independently of the
//! QUIC `QueueDbg` machinery.

/// Whether the storage IO trace is compiled in: enabled under test, the `testing` feature, or the
/// dedicated `io-dbg` feature. A `const` so [`on_enabled`] folds to nothing in production.
///
/// The cfg set here is exactly the one that gates the [`IoOp::op_seq`](crate::fs::op::IoOp) field, so
/// wherever an emit body actually runs (`ENABLED == true`) the `op_seq` field exists to be read.
pub(crate) const ENABLED: bool = cfg!(any(test, feature = "testing", feature = "io-dbg"));

/// Run `f` only when the IO trace is [`ENABLED`].
///
/// In production `ENABLED` is a `false` const, so the optimizer strips the closure entirely: the
/// [`IoOpEvent`](crate::fs::trace::IoOpEvent) is never constructed and the [`backbeat::global`]
/// recorder (and its dumper thread) is never built.
#[inline(always)]
pub(crate) fn on_enabled<F: FnOnce()>(f: F) {
    if ENABLED {
        f();
    }
}
