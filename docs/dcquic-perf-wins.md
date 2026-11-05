# s2n-quic-dc transport performance — findings

This document summarizes a performance investigation of the `s2n-quic-dc` transport used as the datapath
under a high-throughput request/response (RPC-style) workload. It records, for each lever tried, the
**hypothesis**, the **code-grounded mechanism**, and the **measured outcome** on the benchmark rig — kept
faithful: levers that were measured and did **not** help are recorded as such, with the measurement that
ruled them out.

Scope note: this doc covers only the **transport** (`s2n-quic-dc`) levers that belong in this repository.
The larger remaining performance opportunity for the workload was found to be in the **application/RPC
integration layer above the transport**, which is tracked separately and out of scope here.

## Headline finding: the transport is already at line rate

On the benchmark rig, the stock `s2n-quic-dc` datapath (`dc-tester`) reaches **~74–75 Gbps line rate**
(64 KiB objects at concurrency 48–64; 1 MiB at c8–c32), matching the fastest reference stacks. The
transport is therefore **not** the bottleneck for the workload's aggregate throughput. Several transport
micro-optimizations that looked promising in a flamegraph turned out to be measured **nulls** because the
busy-poll datapath is not CPU-bound in the way the profile suggested (it spins at 100% regardless).

## Measurement status legend

- **MEASURED WIN** — an A/B on the rig produced a real improvement (baseline + load config recorded).
- **CORRECTNESS / OBSERVABILITY** — implemented and verified for correctness (or adds counters only); no
  behavior-level perf claim.
- **FALSIFIED** — built and measured; the measurement did not support the hypothesis. Recorded with the
  number that ruled it out, and kept default-off/closed so the idea is not re-attempted blind.

---

## 1. Transport-level result that helped

### 1.1 Prioritize ACK + timer-wheel drains ahead of packet dispatch (tail latency) — MEASURED WIN

- **Hypothesis:** the dominant p99 tail at high concurrency is timer-wheel *lateness* — the single
  recv-dispatch worker services packet dispatch ahead of ACK-completion and PTO/TX timer drains, so ACKs
  and loss timers fire late under load.
- **Mechanism:** an env-gated priority mode that drains the ACK-completion channel and the TX/PTO timer
  wheels *before* packet dispatch on the recv-dispatch worker.
- **Measured delta:** 64 KiB objects at concurrency 64, 3 reps: **p99 1698 µs → 1190 µs (−30%)**,
  throughput **+2.8%** (no regression), p50 flat. A per-poll dispatch-budget follow-on is in progress.
- **Status:** PR #578 (draft, base `main`, pending review). Default-off; the measured delta above was
  taken on an integration rig and the mechanism is ported here behavior-equivalently.

---

## 2. Falsified hypotheses (built + measured, did not help)

These are recorded honestly so they are not re-attempted. The recurring theme: the busy-poll datapath is
not CPU-bound, so cutting per-poll CPU did not move throughput or latency.

- **Per-stream receive-window sizing (transport).** Matching the per-stream recv window down (2 MiB →
  256 KiB) on an otherwise identical config produced **identical** throughput at every cell (64 KiB and
  1 MiB, c16–c64). A 64 KiB response fits inside 256 KiB and aggregate traffic fills the pipe at c6+, so
  the per-stream window is not the throughput limiter at these concurrencies. FALSIFIED.
- **Per-sweep clock cache.** Caching the OS clock once per busy-poll sweep halved `clock_gettime` CPU
  (~17% → ~9% in the profile) but produced **no** throughput or latency change (not CPU-bound). Landed
  anyway as a harmless cleanup (PR #535).
- **Lock-free "has-items" fast path on channel `poll_recv`.** An atomic empty-channel flag read before
  taking the channel mutex, to skip the lock on empty polls (the profile showed `poll_recv` ~23% self).
  Measured a **comprehensive null / small regression** (−1.5% throughput at c64, worse p50 at c16): at
  load the channels are rarely empty, so the empty-skip path almost never fires and the added per-poll
  atomic load is net-negative; the ~23% is intrinsic dequeue work, not empty-poll lock churn. FALSIFIED.
  PR #560 (closed).
- **Adaptive busy-poll backoff.** Sleeping after K consecutive no-work polls was monotonically *worse* as
  the backoff sharpened (8 KiB / c64: p50 335 → 395 µs, p99 657 → 721, throughput 177k → 151k). FALSIFIED.
  PR #568 (kept as a documented negative).
- **Ready-object burst past pacing.** Bursting a ready object ≤ cwnd to remove the pacing delay left TTLB
  p50 byte-identical on/off; the single stream stayed ~half line rate and the burst self-gated on the
  congestion controller's app-limited signal. FALSIFIED (landed default-off, harmless).
- **Other CPU-reduction hypotheses** (atomic-CAS handoffs, per-packet wakeup elision, receive-credit
  starvation, recv-dispatch queue depth) were each measured and found **not** to be the limiter: no
  parking under load, shallow dispatch queues, and no throughput/latency change from removing per-poll
  CPU. FALSIFIED.

---

## 3. Correctness / observability changes (no perf claim)

Implemented and verified; either measurement-only or a correctness fix. Listed for completeness; not
claimed as wins.

- **io_uring recv-ring setup** — build the recv ring with `SINGLE_ISSUER | DEFER_TASKRUN | COOP_TASKRUN`
  (kernel-gated), enabling from the recv thread to bind the issuer correctly (fixes an `-EEXIST`
  issuer-binding bug found during development). Correctness verified; perf deltas not established. PR #544.
- **RX-dispatch wake coalescing** — one splice + wake per worker per recv-completion batch instead of per
  packet; reduces wakes/locks from O(packets) to O(active-workers) (no-op at batch ≈ 1). Structurally
  verified; throughput/latency at concurrency not established. PR #543.
- **Recycle-channel drain-per-pass**, **RX peer-Context front cache**, **RX payload prefetch** — micro-
  optimizations with in-process structural checks; rig perf not established (PRs #545, #548, #564).
- **Counters / gates** — RX decrypt fast/slow split, recv-ring exhaustion counters, TX-assemble metrics
  gate (PRs #546, #547, #565); measurement-only, no behavior change.

---

## 4. Where the real gains are

The transport being at line rate points the remaining opportunity at the **application / RPC integration
layer above the transport** — per-frame deserialization and copies, per-request lookups, async-iterator
indirection on the read path, and per-chunk write wakeups — and at the per-request cryptographic cost (the
send-encrypt and recv-decrypt AEAD operations dominate per-request CPU). That work is tracked separately.
A kernel-bypass (zero-copy) receive datapath is also under evaluation as a small-object / low-concurrency
lever.

## Transport PR status

All transport PRs on this fork are **draft**, pending review; none are self-merged.

| PR | Change | Status |
|----|--------|--------|
| #578 | ACK/timer-wheel priority (§1.1) | **measured win**; draft |
| #535 | Per-sweep clock cache (§2) | falsified (CPU-only); landed as cleanup |
| #560 | Lock-free channel has-items (§2) | falsified (null/regression); closed |
| #568 | Busy-poll backoff (§2) | falsified; kept as documented negative |
| #544 | io_uring recv-ring SINGLE_ISSUER+DEFER_TASKRUN (§3) | correctness; draft |
| #543 | RX-dispatch wake coalescing (§3) | structural; draft |
| #545 | Recycle-channel drain-per-pass (§3) | structural; draft |
| #548 | RX peer-Context front cache (§3) | structural; draft |
| #564 | RX payload prefetch (§3) | structural; draft |
| #546 | RX decrypt fast/slow counters (§3) | observability; draft |
| #547 | RX ring exhaustion counters (§3) | observability; draft |
| #565 | TX-assemble metrics gate (§3) | observability; draft |
| #553 | NAPI busy-poll recv knob (§3) | unmeasured knob; draft |
| #554 | CCA-bypass experiment knob (§3) | diagnostic knob; draft |
| #559 | Recv-window override knob (§2/§3) | window sizing falsified; draft |
