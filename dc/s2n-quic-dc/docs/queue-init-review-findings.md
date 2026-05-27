# Queue Init Simplification: Review Findings

Deep analysis of the `queue-init-simplification` branch for correctness and performance issues under production workloads (10-30 busy-poll threads, hundreds of application threads per process).

## Critical / High Severity

### 1. Single Mutex Bottleneck on All Alloc/Free Operations

**Location:** `flow/queue/free_list.rs` — `FreeVec` struct

Every `alloc()`, `alloc_at()`, `free()`, and `record_region()` call acquires a single `std::sync::Mutex<FreeInner>`. With hundreds of application threads creating/destroying flows simultaneously plus dispatch threads freeing slots, this serializes all flow lifecycle operations through one lock.

Under a burst of 10,000 flow creations (e.g., service restart causing reconnect storm), all threads serialize here. `parking_lot::Mutex` is used elsewhere in the codebase but not here.

**Potential fix:** Sharded free list (per-thread local caches with global fallback), or at minimum switch to `parking_lot::Mutex`. A thread-local cache of pre-allocated descriptors would eliminate most contention.

### 2. Per-Packet `BytesMut::with_capacity(decrypt_len)` Allocation

**Location:** `endpoint/dispatch.rs` — decrypt path

Every authenticated packet triggers a heap allocation of `decrypt_len` bytes for the decrypted frame payload. At 30Gbps with small packets, this is approximately 18 million allocations/second across all recv workers.

The allocation enables zero-copy frame slicing (each frame in a multi-frame packet gets a `BytesMut` slice without copying), which is the intended design. The cost is amortized over multiple frames per packet due to frame aggregation.

**Potential fix:** Per-worker buffer pool or reusable `BytesMut`. The zero-copy slicing semantics make this non-trivial since downstream holds references.

### 3. Stale `alloc_at` Entries Accumulate in Free Indices

**Location:** `flow/queue/free_list.rs` — `alloc()` loop

When `alloc_at` takes a specific slot (server-side indexed allocation), its entry remains in `free_indices` as a stale reference. The `alloc()` loop must pop and discard these before finding a valid one. Under workloads where the server uses `alloc_at` heavily, `free_indices` grows indefinitely with stale entries.

Worst case: O(stale_count) scan on every `alloc()` call, plus unbounded memory growth of the VecDeque.

**Potential fix:** Maintain a stale count and compact periodically, or use a bitmap that supports both indexed and sequential allocation without stale entries.

---

## Medium Severity

### 4. QueueFree Frame Emits `binding_id: 0` Instead of Actual Value

**Location:** `endpoint/dispatch.rs` — QueueFree frame construction

```rust
header: Header::QueueFree {
    binding_id: VarInt::ZERO,
    largest_queue_id: slot.queue_id,
},
```

The `FreedSlot` struct only carries `queue_id`, not the `binding_id` that was active when the slot was freed. If the peer uses `binding_id` from QueueFree to validate which binding was freed (to avoid ABA on slot reuse), the zero value breaks that mechanism.

**Fix:** Store `binding_id` in `FreedSlot` at the time `drop_receiver` captures the slot's key.

### 5. `alloc_or_grow` Allocates Arbitrary Slot Instead of at `dest_queue_id`

**Location:** `endpoint/dispatch.rs` — Unallocated QueueData handling

When a QueueData arrives for an unallocated slot with `dest_acceptor_id`, the code allocates a new queue via `alloc_or_grow` (any free slot), not at the specific `dest_queue_id` from the packet. Until the peer receives the server's first response with the correct queue_id, subsequent retransmits of the initial QueueData hit `Unallocated` again and could allocate duplicate streams.

**Fix:** Use `alloc_at_or_grow(local_queue_id, ...)` to allocate at the exact slot index the client specified, so retransmits route correctly to the already-allocated slot.

### 6. Waiter Queue Eviction Silently Loses Wakers

**Location:** `sync/free_list.rs` — `poll_alloc`

```rust
if inner.waiters.len() >= MAX_WAITERS {
    inner.waiters.pop_front();
}
inner.waiters.push_back(cx.waker().clone());
```

When `MAX_WAITERS` (4096) is reached, the oldest waker is silently evicted. That task will never be woken and hangs indefinitely unless it has an independent timeout. Under sustained exhaustion with more than 4096 waiting tasks, some tasks permanently lose their waker registration.

**Fix:** Wake the evicted waker before dropping it so the evicted task can re-register, or return `Poll::Ready(None)` to signal allocation failure when the queue is full.

### 7. `UnsafeCell<Option<Key>>` Soundness Relies on Undocumented Lock Ordering

**Location:** `flow/queue/descriptor.rs` — `DescriptorInner::key`

The key field is accessed through `UnsafeCell` without a directly guarding synchronization primitive. Safety depends on the invariant that all key reads happen under the queue mutex (via `push`) and all writes happen either during single-threaded allocation or under both queue mutexes (during `close_receiver_inner`). This invariant is currently maintained but undocumented — a refactor that changes lock ordering could silently break soundness.

**Fix:** Add a `// SAFETY:` comment block documenting the exact lock-ordering invariant that makes this sound.

### 8. Per-Queue Mutex Contention Between Dispatch and Application Threads

**Location:** `flow/queue/inner.rs` — `Queue::push` and `Queue::poll_pop`

Every frame dispatch requires the per-queue `parking_lot::Mutex`. The application thread draining the queue contends on the same lock. Under heavy fan-out (one application thread owning 100+ sub-streams from the same peer), the dispatch thread must acquire 100+ mutexes per multi-frame packet, potentially finding each held by the application thread.

This is an inherent tradeoff of the per-queue mutex design. `parking_lot::Mutex` with short critical sections mitigates it well for typical workloads, but extreme fan-out adds latency jitter to the dispatch thread.

### 9. 65535 Initial Page Size Allocates ~16MB Upfront

**Location:** `flow/queue/pool.rs` — `INITIAL_PAGE_SIZE`

With `INITIAL_PAGE_SIZE = 65535`, the first `grow()` allocates 65535 `DescriptorInner` structs. Each contains two `parking_lot::Mutex<Inner>`, multiple `Arc`s, atomics, and an `UnsafeCell<Option<Key>>`. Conservative estimate: ~256+ bytes per descriptor = ~16MB for the first page alone.

For small deployments or testing, this is wasteful. The second grow doubles to ~32MB.

**Fix:** Make initial page size configurable or start smaller (1024-4096) for non-production contexts.

---

## Low Severity

### 10. FIFO Documented as LIFO in Free List

**Location:** `flow/queue/free_list.rs` — struct comment says "LIFO", code does `push_back` + `pop_front` (FIFO)

FIFO provides better ABA protection (maximizes reuse delay) but worse cache locality (recently-freed hot descriptors cycle through the entire list before reuse). Since ABA protection is handled by `binding_id` rather than reuse delay, LIFO would be the better performance choice.

**Fix:** Either fix the comment to say FIFO (if the delay is intentional), or switch to `pop_back` for LIFO.

### 11. `control_out` Vec Allocated Per Packet

**Location:** `endpoint/dispatch.rs` — `process()` entry

`Vec::new()` is constructed per packet for `control_out`. While `Vec::new()` doesn't allocate backing storage until pushed to, a per-worker pre-allocated and cleared buffer would be marginally more efficient.

### 12. Hash Quality in `recv::Key`

**Location:** `endpoint/recv.rs` — `Hash` impl for `Key`

```rust
state.write_u64(self.id.to_hash() ^ self.remote_sender_id.as_varint().as_u64());
```

XOR with small sequential values (0, 1, 2...) only flips low-order bits, which may cause clustering in power-of-two-sized hash tables. The `routing.rs` hash uses proper multiplication-based mixing. FxHash's internal multiply partially mitigates this.

### 13. Reader `stream_id` Should Be `binding_id`

**Location:** `stream/reader.rs` — field naming

The reader stores `stream_id: VarInt` which is actually the `binding_id` (verified by tracing the `client.rs` construction). The writer uses the correct name. Cosmetic inconsistency that could confuse future developers.

### 14. `drain_freed()` Returns Owned Vec

**Location:** `endpoint/dispatch.rs` — `drain_freed()` call site

`core::mem::take()` on the inner Vec means the next `push()` must reallocate. Under burst teardown (1000 flows closing simultaneously) this causes repeated small allocations. A `SmallVec` or reusable buffer would help, though this path is rare.

### 15. Panic-Drop Sends QueueReset for Never-Bound Queue

**Location:** `stream/writer.rs` — `Drop` impl

If the writer is dropped during a panic while still in `Init` state (no frame ever sent), it sends a QueueReset to `dest_queue_id`. The server will reject this since the slot was never bound. Wasteful but harmless.

---

## Confirmed Correct (No Issues Found)

These areas were specifically investigated and found to be well-designed:

- **Waker protocol**: No lost wakes possible. The `take_waker` pattern provides natural deduplication. Deferred waker latency is bounded by executor poll interval.
- **Counter infrastructure**: rseq-based per-CPU buffering on Linux means zero atomic contention for counters on the hot path.
- **Queue dispatch lookup**: O(1) via page-based index arithmetic with no lock acquisition on the steady-state path.
- **Frame parsing**: Zero-allocation lazy iterator with proper bounds checking.
- **Descriptor state machine**: Transitions are properly ordered with Release/Acquire pairs. The binding_id monotonicity invariant is sound and provides complete ABA protection.
- **Teardown sequencing**: Descriptors are unreachable between `close_receiver_inner` and `free_list.free()` because `queue_id` is invalidated under the queue locks before the free list returns the slot.
- **Writer state machine**: Init → Open → FinSent/Shutdown transitions are correct with no deadlock-capable states. Early data is properly bounded by MTU.
- **Idle wheel**: Budget-limited processing prevents starvation. 1-second granularity early expiry is acceptable for 30-60s timeouts.
- **Task lifecycle**: Clean shutdown propagation via channel closure. No zombie accumulation.

---

## Scalability Assessment

The architecture scales well for the target deployment (10-30 busy-poll threads + hundreds of application threads) because:

1. **Dispatch is single-threaded per worker** — no cross-thread contention on the hot receive path
2. **Queue lookup is O(1)** — constant-time arithmetic, no scanning
3. **Counters use rseq** — zero cache-line contention
4. **Waker notification is deferred** — no syscalls on the dispatch thread

The primary scalability bottleneck is **Finding #1** (single mutex on alloc/free). Under sustained high connection rates, this will serialize all flow lifecycle operations. For steady-state data transfer (where alloc/free is rare), the system scales linearly with worker count.

The secondary concern is **Finding #2** (per-packet allocation). This is an inherent cost of the zero-copy frame slicing design and acceptable given the frame aggregation amortization. A buffer pool could eliminate it if profiling shows it's a bottleneck.
