// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Support for the `QueueDbg` end-to-end stuck-stream diagnostic.
//!
//! A `dump_id` is a unique `u64` minted when an application fires `emit_debug` on a handle it
//! believes is stuck. It is stamped onto every log line that the diagnostic produces — the
//! emitter's own state, each send-pipeline hop the frame passes through, the receiving dispatch
//! context, and the woken peer Reader/Writer — so that grepping a single `dump_id` reconstructs
//! the whole end-to-end trace in order.

/// Whether the `QueueDbg` diagnostic is compiled in: enabled under test, the `testing` feature, or
/// the dedicated `queue-dbg` feature. A `const` so [`on_enabled`] folds to nothing in production.
pub(crate) const ENABLED: bool = cfg!(any(test, feature = "testing", feature = "queue-dbg"));

/// Run `f` only when the diagnostic is [`ENABLED`].
///
/// This is the single gate for every diagnostic side effect — minting ids, dumping state, pushing
/// wake-up entries. In production `ENABLED` is a `false` const, so the optimizer strips the closure
/// entirely: nothing is evaluated, logged, or sent. Centralizing the gate here keeps the call sites
/// free of repeated `#[cfg(...)]` attributes while the supporting code still always compiles (so
/// the `QueueDbg` frame stays wire-decodable in every build).
#[inline(always)]
pub(crate) fn on_enabled<F: FnOnce()>(f: F) {
    if ENABLED {
        f();
    }
}

/// Mint a fresh `dump_id`.
///
/// In production this is a random `u64` (via `aws_lc_rs`), matching how the crate sources other
/// unique identifiers (see `path::secret::stateless_reset`). A random id needs no shared state and
/// cannot collide across processes.
///
/// Under test/simulation it is a per-thread monotonic counter so dump ids are deterministic and
/// reproducible — bach is single-threaded and cooperatively scheduled, so a plain `thread_local`
/// counter is race-free and yields stable, snapshot-friendly ids.
pub(crate) fn testing_dump_id() -> u64 {
    use core::cell::Cell;
    thread_local! {
        static COUNTER: Cell<u64> = const { Cell::new(1) };
    }
    COUNTER.with(|c| {
        let id = c.get();
        c.set(id.wrapping_add(1));
        id
    })
}

fn should_use_testing_dump() -> bool {
    #[cfg(any(test, feature = "testing"))]
    return ::bach::is_active();

    #[cfg(not(any(test, feature = "testing")))]
    return false;
}

/// Mint a fresh random `dump_id`. See the test variant above for the deterministic counterpart.
///
/// Masked to 62 bits so the id always fits in a `VarInt` (the wire type on `Header::QueueDbg`)
/// without clamping — clamping would collapse all out-of-range values onto `VarInt::MAX` and
/// destroy uniqueness.
pub(crate) fn next_dump_id() -> u64 {
    if should_use_testing_dump() {
        return testing_dump_id();
    }

    let mut bytes = [0u8; 8];
    aws_lc_rs::rand::fill(&mut bytes).unwrap();
    u64::from_be_bytes(bytes) & ((1u64 << 62) - 1)
}
