# Endpoint-Wide Flow Control in s2n-quic-dc

## Background and Problem Statement

`s2n-quic-dc` streams are lightweight datagram-based flows that share a single UDP socket (or a small
pool of them) on each endpoint. Unlike a traditional TCP connection, there is no OS-level send buffer
acting as a natural throttle; writers push frames directly into an in-process channel and the transport
pipeline drains them. This creates two related problems that interact badly with each other.

### Sender-side: no endpoint-wide budget

Each `Writer` today has a per-stream inflight limit (`max_inflight_bytes`) derived from the
path-secret `local_send_max_data` parameter. That parameter is a path-level constant set at
handshake time — it bounds how many bytes a single stream may have unacknowledged, but nothing
bounds the aggregate across all streams on the endpoint at once.

Under load, hundreds of application threads may each hold a `Writer` and call `write_all_from`
concurrently. Each one independently decides it has budget and submits frames to the shared
channel. The channel is bounded only by memory, so the endpoint accumulates an unbounded backlog of
encrypted-but-not-yet-sent frames. This wastes memory, inflates latency (frames queued early sit
behind a growing pile), and defeats the CCA's pacing signal: by the time BBR wants to send a packet
its queue is already overflowing.

PR [#103](https://github.com/camshaft/s2n-quic/pull/103) began addressing this for the `stream3`
(now `stream`) module by adding an endpoint-local credit controller that tracks queued bytes and
inflight bytes globally and parks writers until credits are available. That design is a useful
starting point but several aspects need revisiting, particularly around lock-free credit acquisition
and the unified sender/receiver model described below.

### Receiver-side: no aggregate receive budget

Each `Reader` advertises a per-stream `MAX_DATA` window to its remote `Writer`. When the application
is reading from many streams simultaneously (e.g. a scatter-gather query fan-out), the total amount
of data that all remote peers are permitted to have in flight at once is the sum of all individual
windows. With 20 peers each holding a 6 MiB window, the client may receive up to 120 MiB
simultaneously — this is the **incast** problem from the previous discussion. The client's receive
buffer and application processing pipeline cannot absorb that burst, leading to packet drops,
retransmissions, and CPU waste even though each individual stream is "within limits."

Additionally the `window_size` used by `maybe_send_max_data` is a fixed constant today (see
`stream/reader.rs` TODO). The reader never auto-tunes the window based on how fast the application
is draining data.

### The ordering deadlock

A particularly nasty failure mode arises when a client fans out N requests in parallel but then must
consume the responses in a fixed order (e.g. indexed joins, streaming aggregates). The client
actively reads from stream 0 and ignores streams 1..N-1 until stream 0 is done. Each server is
sending on its own stream and is willing to fill the per-stream window. Servers 1..N-1 fill their
allowed windows, get ACKed by the transport, and ask for more. The client keeps advertising
`MAX_DATA` for those streams because its transport layer is happy to buffer — it just never delivers
to the application. Meanwhile the server for stream 0 may get blocked by its _own_ per-stream
window if the client's advertised limit is also finite.

The result is a classical credit-based deadlock:
- Active stream (0) is blocked waiting for window.
- Client is not granting more window because it is "busy" (the receive path is not draining it).
- Client's receive path is not draining stream 0 because… it cannot make progress.

This was observed directly with the earlier s2n-quic connection-level flow control, where inactive
streams consumed the full connection window and stalled the active one.

### Shared mechanism opportunity

The sender-side and receiver-side problems are structurally identical: both are about controlling
how much work is queued or in-flight across many streams, and both need the same properties:

- Sub-microsecond credit acquisition on the hot path.
- No contention between unrelated streams.
- Wakeup delivery that scales to hundreds of concurrent waiters.
- Fairness: a slow or idle stream should not starve a fast one.
- Deadlock safety: active streams must always be able to make forward progress.

A single primitive that solves both would reduce implementation surface area and make it easier to
reason about the global invariants.

---

## Requirements and Goals

The following are listed in approximate priority order.

### R1 — Correctness: bounded memory at all times

The aggregate amount of data queued for transmission (across all writers on an endpoint) must be
bounded by a configurable limit. Likewise, the aggregate amount of data buffered for application
consumption (across all readers) must be bounded. Exceeding either bound must cause writers/readers
to stall, not to accumulate unbounded backlog.

### R2 — No deadlock

Active streams — those that are currently being consumed or sent — must always be able to acquire
enough credit to make forward progress, even if the endpoint's global budget is temporarily
exhausted by other streams. Concretely: a reader that is actively being polled must not be denied
credit because an idle reader is holding a large window.

### R3 — Lock-free credit acquisition on the fast path

Hundreds of threads compete for shared budget. The common case — budget is available — must
complete with a single atomic read-modify-write (e.g. `fetch_sub` on an `AtomicU64`). No mutex,
no park, no syscall. Slow paths (budget exhausted, wakeup delivery) may use heavier primitives.

### R4 — Symmetric API for send and receive

The mechanism should be the same data structure for both sides. A sender acquiring "queued-byte
credits" and a receiver acquiring "receive-window credits" are the same operation on a shared pool.
This halves the implementation surface and makes testing easier.

### R5 — Fair, priority-aware credit distribution

When the budget is exhausted and multiple streams are waiting, credits should be distributed
fairly (FIFO within a priority tier, round-robin across tiers). Higher-priority streams (e.g.
latency-sensitive RPCs) may preempt lower-priority ones without starving them entirely.

### R6 — Auto-tuning

Both the sender inflight budget and the receive window per stream should auto-tune based on
observed drain rate. A stream draining slowly should receive smaller credits; one draining fast
should receive larger credits. The endpoint-level cap provides the hard bound; per-stream
allocation is elastic within it.

### R7 — Minimal memory overhead per stream

There may be millions of short-lived streams. The per-stream data structure must be small (ideally
a handful of atomics and a pointer into a shared wait list) and must be safe to drop at any point
without leaking credits.

---

## Solution Options

### Option A — Mutex-guarded global counter with a priority wait queue (PR #103 approach)

**Description.** A single `Arc<Mutex<State>>` holds the global byte budget. `State` contains the
available count and a priority-ordered intrusive list of waking entries. Acquisition: lock, check,
subtract, unlock. If budget is zero, insert a waker entry into the wait list and return
`Poll::Pending`. Release: lock, add back, pop wakers until refilled, unlock.

**Pros.**
- Simple to reason about; easy to prove correct.
- Priority queue naturally expressed as an intrusive list.
- Already partially implemented in PR #103.

**Cons.**
- Every write on every stream acquires the mutex. With hundreds of threads this becomes a serial
  bottleneck even when budget is plentiful. A frame that fits in a single MTU (~1400 bytes) still
  pays the full mutex round-trip.
- Wakeup storms: when a large release refills the pool, all waiting streams wake simultaneously,
  each re-acquires the mutex, and only one succeeds — classic thundering herd.
- Does not satisfy R3 on the fast path.

**Fits requirements:** R1 ✓, R2 (partially) ✓, R3 ✗, R4 ✓, R5 ✓, R6 partial, R7 ✓.

---

### Option B — Atomic token bucket with parking lot fallback

**Description.** Use a single `AtomicU64` as the global byte budget. Acquisition:
`fetch_sub(requested, Acquire)` and check the result; if the result remains ≥ 0 (using signed
semantics or a compare-exchange loop), the credit is acquired without any lock. When the result
goes negative the thread must park: re-add the requested amount, register in a wait queue
(protected by a separate thin mutex used only when parking), and return `Poll::Pending`. Release:
`fetch_add(returned, Release)`, then wake at most *k* waiters (where *k* is derived from the
amount returned, not all waiters at once).

More concretely using saturating subtraction:

```
loop {
    let current = BUDGET.load(Acquire);
    if current < requested { park(); return Pending; }
    if BUDGET.compare_exchange(current, current - requested).is_ok() { break; }
}
```

This is a classic "optimistic" CAS loop — contention only matters when many threads hit the budget
limit simultaneously, which is precisely when they should be parking anyway.

**Pros.**
- Fast path is a single CAS on a cache line — no lock, no syscall.
- Slow path (parking) only activates under actual pressure.
- Thundering herd is mitigated by issuing at most *k* wakeups proportional to refill size.
- Symmetric: same primitive for both send credits and receive credits.

**Cons.**
- CAS loop can retry under high contention, though this only matters when budget runs out — a
  regime where writers will park anyway.
- Priority ordering is harder: the wait queue still needs a mutex; fairness among priority tiers
  requires careful ordering at wakeup time.
- Credit granularity: if a single stream requests a large credit (e.g. a full send quantum, perhaps
  64 KB) it can starve many small-credit streams. Needs a burst cap per acquisition.
- Dropped tasks can leave credits stranded if not carefully accounted for.

**Fits requirements:** R1 ✓, R2 ✓ (active streams always find credits available unless truly
exhausted), R3 ✓, R4 ✓, R5 partial (needs wakeup priority), R6 ✓, R7 ✓.

---

### Option C — Per-peer sharded credit pools

**Description.** Rather than a single global pool, maintain one pool per peer (or per path-secret
entry, which maps 1:1 to peers today). Each stream credits against the pool for its peer rather
than a process-wide pool. The pools are independent and can be managed with simple per-pool atomics.
A separate global "memory pressure" signal (an `AtomicBool` or a coarse counter) allows the
endpoint to shed load globally without per-peer contention.

**Pros.**
- No cross-peer contention at all; each pool is modified only by streams to/from one peer.
- Easy to reason about fairness within a peer.
- Fits naturally with the existing path-secret map structure.

**Cons.**
- Does not solve the incast problem directly: if a client has 20 peers, the total in-flight data is
  still 20 × per-peer-limit. A global cap is needed anyway.
- A global memory-pressure signal still needs to be fast and coordination-free, which is just the
  original problem restated.
- Adds complexity (two levels of budgeting).

**Fits requirements:** R1 partial (no hard global cap), R2 ✓, R3 ✓, R4 ✓, R5 partial, R6 ✓,
R7 ✓.

---

### Option D — Tokio-style semaphore with batch acquisition

**Description.** Use `tokio::sync::Semaphore` (or a similar permit-based async semaphore) as the
global budget. Writers acquire permits in batches (one permit = one MTU worth of bytes), and hold
them until the ACK/completion fires. Release is via permit drop. Tokio's semaphore uses an atomic
waiter count and a lock only for the wait list; acquisition is O(1) amortized.

**Pros.**
- High-quality battle-tested implementation with known correctness properties.
- Async-native: integrates with Tokio's scheduler directly.
- Batch acquisition allows sending a send quantum's worth of data per wakeup.

**Cons.**
- Tokio semaphore is `!Send` across runtimes and harder to use from non-Tokio contexts (Beachhead's
  runtime uses its own executor, bach for testing, etc.).
- Permit = MTU granularity means that at 20 million streams the permit pool must hold up to
  20M permits — may exceed `u32::MAX` permits that Tokio supports today.
- Priority ordering requires wrapping or forking the semaphore.
- Not symmetric with the receiver side without a custom implementation.

**Fits requirements:** R1 ✓, R2 ✓, R3 ✓, R4 partial, R5 ✗ (no built-in priority), R6 partial,
R7 ✓.

---

### Option E — Active/inactive stream tiering with a reservation gate

**Description.** This option specifically targets R2 (deadlock safety). Rather than trying to make
all streams compete fairly for a single pool, partition streams into two tiers:

1. **Active** — a stream that has been polled (by either a writer or a reader) within the last RTT,
   or that is explicitly marked active by the application. Active streams draw from a reserved
   portion of the global budget (e.g. 50%) and are never denied.
2. **Inactive** — streams that have not been polled recently. They share the remaining budget on a
   best-effort basis.

Tier promotion is automatic (every `poll_write_from` or `poll_read_into` marks the stream active)
and demotion happens on a timer (one smoothed RTT of inactivity). An application that reads
streams in a fixed order automatically promotes stream 0 and demotes 1..N-1, so stream 0 always
gets the reserved slice.

**Pros.**
- Directly prevents the ordering deadlock described in the problem statement without requiring
  application changes.
- Reserved tier can be a simple separate atomic counter, keeping fast-path cost low.
- Composable with Options B or C for the actual credit mechanics.

**Cons.**
- Two tiers may not be enough for all use cases; applications may need explicit priority hints.
- Timer-based demotion adds complexity and needs careful tuning to avoid oscillation.
- The "reserved 50%" is a policy constant that may not suit all workloads.

**Fits requirements:** R1 ✓, R2 ✓ (by design), R3 ✓ (two atomics), R4 ✓, R5 partial, R6 partial,
R7 ✓.

---

### Option F — Per-stream leaky bucket with a global pressure signal

**Description.** Each stream has its own leaky bucket (a token accumulator that refills at a
rate proportional to the peer's BBR-estimated bandwidth and the stream's priority weight). A global
pressure gauge — an `AtomicU64` measuring total endpoint backlog — modulates the refill rate: when
pressure is high, all buckets refill slower. This avoids any shared lock entirely; the only shared
state is the pressure gauge.

**Pros.**
- No cross-stream synchronization other than a single atomic gauge read.
- Rate-based control naturally spreads transmission over time (pacing).
- Per-stream buckets encode priority as different refill multipliers.

**Cons.**
- Complexity: each stream needs its own timer and bucket state; this is significantly more complex
  than a counter pool.
- Convergence is not guaranteed: under certain load patterns the pressure gauge overshoots and the
  system oscillates.
- Does not provide a hard memory bound — a burst of new streams can each pre-fill their bucket
  before the gauge reflects the pressure.

**Fits requirements:** R1 ✗ (no hard bound), R2 ✓, R3 ✓, R4 partial, R5 ✓, R6 ✓, R7 partial.

---

## Recommendation

Use **Option B** (atomic CAS token pool) as the core mechanism, layered with **Option E** (active/inactive tiering) to solve the deadlock requirement.

### Rationale

Option B satisfies the performance requirement (R3) decisively: the common case — budget available — is a single `compare_exchange` on one cache line with no lock, no allocation, and no function call overhead beyond the atomic operation itself. The slow path (parking) is exercised only when the pool is genuinely exhausted and waiting is correct behavior. This is better than what PR #103 does, which pays mutex overhead on every acquisition regardless of budget availability.

The symmetric property (R4) comes for free: a "receive credit" and a "send credit" are both just a
u64 being subtracted from the same type of pool. The pool does not know or care whether the caller
is a reader or a writer. This reduces the implementation surface and means the correctness argument
needs to be made only once.

Option E's tiering solves the deadlock scenario (R2) without requiring the application to change.
It works because the key insight is that a stream being actively consumed is by definition polled
frequently — it self-promotes by exercising the credit path. Inactive streams accumulate large
windows but those windows are served from a logically separate (and smaller) budget slice. A
practical starting point is to reserve 25% of the global pool for active streams and let inactive
streams share the remaining 75%. The ratio is a tunable parameter, not a hardcoded constant.

Option C's per-peer sharding is an optional optimization layer that can be layered on top of B
without changing the API: the global atomic can be replaced with a per-peer atomic once profiling
demonstrates cross-peer contention, which is unlikely in practice because streams to different
peers have different pacing rates and rarely compete at the same instant.

### Unified credit pool design sketch

```rust
/// A single global credit pool shared by all streams on one endpoint.
///
/// `available` is stored as an i64 to allow transient over-subscription
/// (a stream that acquired credit and then grew its request). The value
/// is always expected to be ≥ 0 in steady state; negative values indicate
/// that more budget was issued than exists and must be repaid before new
/// acquisitions are granted.
struct CreditPool {
    /// Remaining credits; modified by fetch_sub (acquire) and fetch_add (release).
    available: AtomicI64,
    /// Active-stream reserved floor; streams in the "active" tier draw from
    /// `available` but are guaranteed at least this many bytes without parking.
    active_reserve: AtomicI64,
    /// Wait queue protected by a thin mutex — only reached when budget is zero.
    waiters: Mutex<WaiterQueue>,
}
```

Acquisition fast path (writer wants to send `n` bytes):

1. If the stream is marked **active** and `active_reserve.fetch_sub(n, Acquire) >= 0`, credit
   granted from the active reserve. No contention with inactive streams.
2. Otherwise, attempt `available.fetch_sub(n, Acquire)`. If result ≥ 0, credit granted.
3. If result < 0, restore: `available.fetch_add(n, Release)`. Acquire the waiters mutex, push
   a waker entry with the stream's priority, and return `Poll::Pending`.

Release (ACK arrives, frame cancelled, or reader consumes data):

1. `available.fetch_add(returned, Release)`.
2. Lock the waiters mutex; drain waiters in priority order, issuing credits from the newly
   returned amount (at most a configurable burst cap per wakeup cycle).
3. Drop the lock; call `waker.wake()` on each issued waiter — outside the lock, to avoid
   waking threads that then immediately re-contend.

The active reserve refills on a timer proportional to the smoothed RTT so that active streams
that consumed their reservation can re-acquire without hitting the shared pool.

### Credit lifecycle for stream writers

The lifecycle mirrors what PR #103 designed but using the atomic fast path above:

1. **Pre-submission:** writer acquires queued-byte credits before building frames.
2. **Frame submitted:** credits move from the pool's "queued" notional bucket to "inflight" (no
   real state change needed; both deduct from `available`).
3. **ACK received:** completion fires; inflight credits are returned to `available`.
4. **Cancellation / TTL exhaustion:** same as ACK — credits returned.
5. **Retransmission:** no re-acquisition needed (credits already deducted); the inflight counter
   stays constant.

### Credit lifecycle for stream readers

The lifecycle is the mirror image:

1. **Initial window advertised:** reader acquires receive-buffer credits equal to the initial
   `MAX_DATA` window it will advertise. This is the "reservation" for data that may arrive.
2. **Data consumed by application:** credits are released back to the pool, and the reader
   decides whether to advertise a fresh `MAX_DATA` (per the existing `maybe_send_max_data` logic)
   only if new credits are available from the pool.
3. **Stream closed / STOP_SENDING:** all unreleased credits are returned immediately.

This means `maybe_send_max_data` becomes:

```
if pool.try_acquire(window_increment).is_ok() {
    send MAX_DATA(consumed + window_increment);
}
```

If the pool is under pressure, the reader simply does not expand the window, naturally throttling
the remote sender without any protocol-level change.

### Deadlock prevention via active-stream tiering

The active reserve guarantees that a stream being polled always has a path to credit. Specifically:

- On every call to `poll_write_from` or `poll_read_into`, the stream sets a timestamp (a
  `AtomicU64` epoch counter, not wall clock) marking itself active. This is a single relaxed store.
- The credit pool's `try_acquire_active` path checks this timestamp; if the stream's last-active
  epoch is within one RTT of the current epoch, it draws from `active_reserve`.
- Inactive streams (not polled recently) lose their active status and compete for the shared pool.

For the ordering-deadlock scenario: the client is reading from stream 0. Stream 0 is active;
streams 1..19 are inactive. Streams 1..19 may hold large receive windows but those windows are
served from the (intentionally larger) shared pool. Stream 0's active reserve is always topped up
by completed reads on streams 1..19. The client never runs out of active-tier credits for the
stream it is actively consuming.

### What this does NOT address

- **Incast from multiple senders:** if 20 servers each hold the maximum allowed window and all
  send simultaneously, the client's NIC buffer and kernel socket buffer can still be overwhelmed
  before the transport layer has a chance to apply flow control. This is a layer below the credit
  pool and requires either smaller initial windows, ECN, or DSCP-based network-level throttling.
  The credit pool limits how much data the application _allows_, but not how fast the network
  delivers it in the short term.
- **Per-stream fairness within the shared pool:** the current design does FIFO-within-priority-tier
  scheduling among waiters, not weighted fair queuing. A burst of many equal-priority streams will
  be served one at a time in arrival order. This is acceptable for most workloads but may be
  revisited if starvation is observed in practice.
