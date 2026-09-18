# dcQUIC performance wins

This document tracks the dcQUIC (`s2n-quic-dc`) transport-performance patches developed against this
fork. Each entry is a single, standalone change with its own PR, so they can be reviewed and merged
independently. For every patch we record the **hypothesis**, the **code-grounded mechanism**, the
**measured delta** (with the baseline commit and the load configuration it was measured under), and a
link to the atomic **PR**.

## How to read this

Each patch carries a measurement status:

- **MEASURED** — an A/B was run on the integrated-service benchmark rig and produced a real number
  (baseline commit + load config recorded). This is a confirmed win.
- **BUILT / correctness-verified** — the change is implemented, builds, and passes the unit/loom/sim
  suite (and, where noted, an *in-process* structural check such as a lock-count or cache-hit assertion),
  but the end-to-end throughput/latency A/B on the integrated rig has not been run yet. The mechanism is
  sound; the perf number is pending a rig run and is **not** claimed until measured.
- **OBSERVABILITY** — a measurement-only change (counters/gates) that adds no behavior change; it exists
  to unblock the A/B for a companion patch.
- **REFUTED** — the change was built and measured, and the measurement did **not** support the
  hypothesis. Recorded honestly; retained default-off (harmless) or as a documented negative so the same
  idea is not re-attempted blind.

Most levers were validated for *correctness* here; the integrated-service throughput/latency A/Bs run on
a dedicated benchmark rig, so several deltas are marked pending until that rig run lands.

---

## 1. Measured wins (rig-confirmed)

### 1.1 Prioritize ACK + timer-wheel drains ahead of packet dispatch (tail-latency)

- **Status:** MEASURED — confirmed win.
- **Hypothesis:** the dominant p99 tail at high concurrency is send-worker timer-wheel *lateness* — the
  single `recv_dispatch` worker services packet dispatch ahead of ACK-completion and PTO/TX timer
  drains, so ACKs and loss timers fire late under load.
- **Mechanism:** an env-gated priority mode (`DCQUIC_ACK_PRIORITY`) that drains the `ack_completion`
  channel and the TX/PTO timer wheels *before* `packet_dispatch` on the recv-dispatch worker
  (`runtime.rs` Spawner trait + `endpoint.rs` latency-priority mode + `tasks.rs` wheel spawns).
- **Measured delta:** baseline `bbfad0c0`, 64 KiB objects at concurrency 64, 3 reps, integrated-service
  rig: **p99 1698 µs → 1190 µs (−30%)**, throughput **+2.8%** (no regression), p50 flat. Still ~+28%
  above the cross-implementation p99 target (927 µs); a per-poll dispatch-budget follow-on is in flight.
- **PR:** _to be opened_ — currently tracked as a standalone patch; a dedicated atomic PR against this
  fork is being prepared (the change lives in the `s2n-quic-dc` runtime/endpoint/tasks path).

---

## 2. Built + correctness-verified (throughput/latency A/B pending integrated rig)

These are implemented, build clean, and pass the suite (plus the in-process structural checks noted).
The end-to-end perf number is pending a rig A/B and is not claimed as a win until measured.

### 2.1 Lock-free "has-items" fast path on channel `poll_recv`

- **Hypothesis:** at 64 KiB / c32 the integrated flamegraph shows channel `poll_recv` at ~23% self —
  the busy-poll loop takes and releases the channel `Mutex` on *every* poll, and most polls are on an
  empty channel, so the empty-poll lock churn is the throughput cap.
- **Mechanism:** a `has_items: AtomicBool` on the intrusive sync-channel `Shared`. The consumer reads it
  `Acquire` *before* taking the mutex and returns `Pending` without locking on the common empty
  busy-poll case; the producer stores `true` `Release` under the lock *before* taking the waker, so no
  wakeup is lost. Extended to the `Entry<T>` channel.
- **Delta:** builds + clippy + 17 channel/loom tests green (incl. the concurrent send/recv loom test that
  guards against lost wakeups). Measured NULL on the bare channel micro-test (network-bound there);
  the decisive integrated-rig re-profile — `poll_recv` self% drop + 64 KiB throughput/TPS rise — is
  pending.
- **PR:** #560 (base `perf-staging`; being retargeted to `main`).

### 2.2 Lock-free per-endpoint waker set

- **Hypothesis:** the wake/drain-path `Mutex<BitSet>` on the per-endpoint readiness set contributes to
  the c16+ TTFB p99 tail.
- **Mechanism:** replace the readiness `Mutex<BitSet>` with a lock-free atomic set (`AtomicU64`
  segments; wake = `fetch_or`, drain = per-word `swap(0)`); a lock remains only on cold registration.
- **Delta:** correctness + concurrency green (512-thread wake test, no lost wakeups). c16+ TTFB p99-tail
  A/B pending rig. Draft.
- **PR:** #558 (base `perf-staging`; being retargeted to `main`).

### 2.3 Coalesce per-packet RX-dispatch wakes into one splice + wake per worker per batch

- **Hypothesis:** dispatching each decoded datagram individually takes the destination worker's channel
  lock and wakes it per packet; under a recv-completion batch this is O(packets) locks/wakes.
- **Mechanism:** stage decoded datagrams per destination worker during a recv-completion batch, then
  splice each worker channel with one locked append + one wake at end of batch.
- **Delta:** in-process CONFIRMED — wakes/locks drop O(packets) → O(active-workers) per burst (no-op at
  batch ≈ 1); suite green (906 tests). Throughput + p50/p99 at concurrency pending rig.
- **PR:** #543 (`camshaft/s2n-quic`; retarget to `main` if not already).

### 2.4 Drain the recv recycle channel once per replenish pass

- **Hypothesis:** the cross-core recycle channel is locked per freed buffer descriptor during replenish,
  i.e. O(bids) channel locks per pass.
- **Mechanism:** split `SyncReuseRing::alloc_or_reuse` into `drain()` (one channel lock) + `take_one()`
  (lock-free local pop), draining the recycle channel once per replenish pass rather than per freed bid.
- **Delta:** in-process CONFIRMED — recycle-channel locks O(bids) → O(1) per pass, proven by
  descriptor-address identity; suite green. Throughput / cache-traffic at high PPS pending rig.
- **PR:** #545.

### 2.5 One-entry last-hit front cache on the RX peer-Context lookup

- **Hypothesis:** the RX per-packet peer-`Context` lookup (FxHash + hashmap probe) is redundant when
  consecutive packets share a peer.
- **Mechanism:** a 1-entry last-hit cache of `(Key, Rc<Context>)`; on a matching `Key` + `key_id` it
  serves the cached `Context`, skipping the hash + hashmap probe. Invalidated on remove / key-advance;
  `front_hit`/`miss` counters added.
- **Delta:** mechanism confirmed in-process (`front_hit` fires in 18/38 sim traces); suite green.
  Per-packet CPU / throughput at high PPS pending rig.
- **PR:** #548.

### 2.6 Software-prefetch the next recv-completion payload during current-packet decrypt

- **Hypothesis:** the recv decrypt path stalls on load misses fetching each packet's payload; prefetching
  the next completion's payload during the current decrypt hides the miss.
- **Mechanism:** env-gated (`DCQUIC_RX_PREFETCH`) peekable CQE drain that issues `_mm_prefetch(T0)` on the
  next completion's payload head while the current packet decrypts (adds `Unfilled::payload_ptr()`).
- **Delta:** builds clean. Expect fewer decrypt-path load misses on x86_64 io_uring recv; A/B pending
  (NULL risk under DDIO; a no-op on aarch64). Default-off.
- **PR:** #564.

### 2.7 Build the recv io_uring with `SINGLE_ISSUER | DEFER_TASKRUN`

- **Hypothesis:** deferring completion task-run to the recv thread and asserting a single issuer cuts
  recv wakeup overhead on supported kernels.
- **Mechanism:** build the recv ring with `SINGLE_ISSUER | DEFER_TASKRUN | COOP_TASKRUN`, kernel-gated;
  create the ring `R_DISABLED`, register the buffer ring, then `ENABLE` it from the recv thread so the
  issuer binds correctly (fixes an `-EEXIST` issuer-binding bug found during development).
- **Delta:** correctness CONFIRMED in-process (delivery + teardown green). High-PPS
  throughput/p99/wakeup-rate deltas pending rig.
- **PR:** #544.

### 2.8 Per-sweep clock cache on the busy-poll loop

- **Hypothesis:** the busy-poll loop reads the OS clock per timer poll; caching it once per sweep cuts
  `clock_gettime` off the hot path (the integrated flamegraph shows `clock_gettime` ~10% self at 64 KiB).
- **Mechanism:** cache the OS clock once per busy-poll sweep in a per-thread `Cell` (`clock::refresh` at
  sweep top; `Clock::now` reads the cache). A companion change extends the coarse timestamp to the pacing
  EDT, sojourn stamps, and timer-expiry reads.
- **Delta:** implemented (`DCQUIC_CLOCK_CACHE`); an earlier micro-test A/B was NULL (network-bound);
  busy-poll iters/sec + 64 KiB throughput A/B on the integrated rig pending.
- **PR:** #535 (base `perf-staging`; retarget to `main`).

### 2.9 Opt-in io_uring NAPI busy-poll on the recv ring (latency knob)

- **Hypothesis:** NIC-IRQ → softirq → wake adds recv latency; polling the socket NAPI queue in
  `submit_and_wait` removes it.
- **Mechanism:** opt-in `S2N_DC_RECV_NAPI_BUSY_POLL_US` → `io_uring_register_napi`, with probe + fallback;
  default OFF.
- **Delta:** correctness green (default OFF). p50/p99 latency A/B (c1/c16 × 8k/64k/1MB) pending rig.
  Draft.
- **PR:** #553 (base `perf-staging`; retarget to `main`).

### 2.10 Advertised recv-window override (integrated path)

- **Hypothesis:** the integrated server/client advertise the 64 KiB default per-stream recv window and
  never received an earlier window bump, capping TTFB at c16.
- **Mechanism:** env `DCQUIC_RECV_WINDOW` overrides the advertised per-stream recv window on the
  integrated `Server`+`Client` (`psk/io.rs` `with_bidirectional_remote_data_window`); default unchanged.
- **Delta:** Step-1 confirmed in source (integrated advertises the 64 KiB default). Step-2 window-sweep
  A/B (64 KiB / c16 TTFB expected ≈ −1 RTT) pending rig.
- **PR:** #559 (base `perf-staging`; retarget to `main`).

### 2.11 CCA-bypass experiment knob (cap localization)

- **Hypothesis:** to *locate* the 64 KiB throughput cap, bypass the congestion controller so the transport
  is never cwnd/pacing-limited and observe the residual ceiling.
- **Mechanism:** env `S2N_DC_CCA_BYPASS` makes the congestion `Controller` bypass BBR (pacing off, fixed
  1 GiB cwnd); flow-control / send-budget still bound in-flight. This is a diagnostic knob, not a
  shipping default.
- **Delta:** correctness green (default OFF). 64 KiB (+1 MiB) throughput-toward-line-rate A/B pending
  rig. Draft.
- **PR:** #554 (base `perf-staging`; retarget to `main`).

---

## 3. Observability landed to unblock measurement

Measurement-only, no behavior change; each unblocks the A/B for a companion lever above.

- **RX decrypt fast/slow split** — `rx.decrypt.fast` / `rx.decrypt.slow` counters at the two decrypt
  branches (scatter-decrypt vs per-packet `BytesMut` alloc). Gates the buffer-pooling follow-on on the
  production fast/slow ratio. **PR #546.**
- **RX ring exhaustion** — `rx.ring.rearm` / `rx.ring.no_buffer` counters in the io_uring recv loop, to
  measure buffer-ring exhaustion before changing ring depth. **PR #547.**
- **TX-assemble metrics gate** — env-gate (`DCQUIC_TX_ASSEMBLE_METRICS`) around the per-packet TX-assemble
  histogram recordings + `on_tx_packet`, to A/B the send-event recording cost (expected to matter only in
  the CPU-bound small-object regime). **PR #565.**

---

## 4. Refuted hypotheses (built + measured, did not support the hypothesis)

Recorded honestly so the ideas are not re-attempted blind. Retained default-off or as a documented
negative.

- **Adaptive busy-poll backoff** (`DCQUIC_BUSY_POLL_BACKOFF_K`) — sleep after K consecutive no-work polls.
  **REFUTED:** an 8 KiB / c64 K-sweep was monotonically *worse* as the backoff sharpened (K-off p50
  335 µs / p99 657 → K=8 p50 395 µs / p99 721; throughput 177k → 151k). Parked to the idle-worker regime.
  **PR #568** (retained as a documented negative).
- **Ready-object burst past pacing** (`DCQUIC_READY_BURST`) — burst a ready object ≤ cwnd to remove the
  pacing delay. **REFUTED:** 1 MiB / c1, 3 reps — TTLB p50 byte-identical on/off; the single stream stayed
  ~4.6 GB/s (about half line rate) and the burst self-gated on the CCA's `is_app_limited()` signal.
  Landed default-off (harmless); no PR.

---

## PR index & retarget status

| PR | Patch | Base today | Target |
|----|-------|-----------|--------|
| _pending_ | ACK/timer-wheel priority (§1.1, **measured win**) | — | open atomic PR against `main` |
| #560 | Lock-free channel has-items (§2.1) | perf-staging | retarget `main` |
| #558 | Lock-free waker set (§2.2) | perf-staging | retarget `main` |
| #543 | RX-dispatch wake coalescing (§2.3) | (staged) | ensure `main` |
| #545 | Recycle-channel drain-per-pass (§2.4) | (staged) | ensure `main` |
| #548 | RX peer-Context front cache (§2.5) | (staged) | ensure `main` |
| #564 | RX payload prefetch (§2.6) | (staged) | ensure `main` |
| #544 | recv ring SINGLE_ISSUER+DEFER_TASKRUN (§2.7) | (staged) | ensure `main` |
| #535 | Per-sweep clock cache (§2.8) | perf-staging | retarget `main` |
| #553 | NAPI busy-poll knob (§2.9) | perf-staging | retarget `main` |
| #559 | Advertised recv-window override (§2.10) | perf-staging | retarget `main` |
| #554 | CCA-bypass experiment knob (§2.11) | perf-staging | retarget `main` |
| #546 | RX decrypt fast/slow counters (§3) | (staged) | ensure `main` |
| #547 | RX ring exhaustion counters (§3) | (staged) | ensure `main` |
| #565 | TX-assemble metrics gate (§3) | (staged) | ensure `main` |
| #568 | Busy-poll backoff (§4, **refuted**) | (staged) | keep as documented negative |
