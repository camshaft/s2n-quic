# Queue Init Simplification: Review Findings

This document captures the results of a thorough review of the queue-init-simplification branch, focused on correctness, performance, and scalability under high core counts (200+ cores, 500k+ flows/sec).

## Critical: Must Fix Before Ship

### 1. Server allocates wrong slot on implicit binding

The live code path in `handle_queue_data` (dispatch.rs:554) uses `alloc_or_grow` which pops whatever slot is next in the free list. The client specifies a `dest_queue_id` expecting the server to allocate at that exact index. A dead function `create_binding` at dispatch.rs:611 correctly uses `alloc_at_or_grow(local_queue_id.as_u64() as usize, ...)` but is never called.

This works by coincidence today because both the client's `peer_free_list` and the server's `FreeVec` start at slot 0 and advance sequentially. The invariant breaks under any of: concurrent stream creation from multiple tasks, slot recycling via QueueFree, or a failed init that frees a slot asymmetrically. Once slot ordering diverges, all subsequent non-init frames hit the `Unallocated` branch and are silently dropped.

The fix is straightforward: replace `alloc_or_grow` with `alloc_at_or_grow(local_queue_id.as_u64() as usize, ...)` in the live path, matching what the dead code already does. Then remove the dead `create_binding` function.

### 2. `alloc_at` is O(n) under mutex

The `FreeVec::alloc_at` method (free_list.rs:96) performs a linear scan of the entire VecDeque to find the descriptor matching a given slot index:

```rust
let pos = inner.descriptors.iter().position(|d| unsafe { d.queue_id_index() } == slot_index);
```

With `INITIAL_PAGE_SIZE = 65535`, this is an O(65k) scan on every new server-side binding, serialized behind the FreeVec mutex. After fixing issue #1 to call `alloc_at_or_grow`, this becomes the hot-path bottleneck for new flow creation.

The free list currently uses a VecDeque which has no indexing capability. The fix requires a data structure that supports O(1) positional allocation. Options include a direct-indexed array with a free bitmap (each slot is either in-use or free, checked by index), or threading a free-list chain through the descriptors themselves so that removal by index is O(1).

### 3. Inline waker firing from `drop_sender` bypasses waker offload

The waker offload infrastructure (endpoint/waker.rs) correctly defers waker invocations off busy-poll dispatch threads. Producers push wakers into per-worker Slots via a Sink, and a separate Drain task fires them in bulk. This prevents futex/eventfd syscalls from stalling packet processing.

However, `Descriptor::drop_sender` (descriptor.rs:269-270) completely bypasses this mechanism:

```rust
inner.control.close();  // returns AutoWake, dropped immediately
inner.stream.close();   // same — fires waker.wake() inline
```

When the last sender reference drops, `Queue::close()` extracts the receiver's waker and returns it as an `AutoWake`. Since `drop_sender` has no access to a waker sink, the `AutoWake` drops on the spot, invoking `waker.wake()` synchronously on whatever thread is dropping the sender. If this is a dispatch thread (which happens when sender references are held in pipeline channels), the syscall stalls processing.

At 500k flows/sec churn, this is 500k inline wake syscalls per second distributed across the dispatch threads. The fix requires giving `drop_sender` a deferred-wake path. Approaches include storing a `Weak<Slot>` in each DescriptorInner (16 bytes overhead, gives direct access to the waker offload), using a thread-local waker accumulator that the drain task sweeps, or making `Queue::close()` push the waker directly to a shared lock-free queue rather than returning it.

## High: Significant Performance Impact

### 4. `free_notify` Mutex causes cross-thread contention

The freed-slot notification mechanism uses `Arc<std::sync::Mutex<Vec<FreedSlot>>>` (descriptor.rs:403). This mutex is acquired from two distinct thread populations: receiver task threads (any tokio worker) push freed slots during `drop_receiver`, and the recv dispatch worker drains them via `drain_freed()`.

Under high flow churn with hundreds of cores, many receiver drops happen simultaneously on different cores, all contending with each other and with the dispatch thread's drain. The critical section is short (push to Vec), but the cross-core cache-line bouncing and potential futex waits add up.

The communication pattern is textbook MPSC (many producers, single consumer). Replacing the `Mutex<Vec>` with a lock-free MPSC queue (crossbeam's `SegQueue` or a custom Treiber stack) eliminates the contention entirely.

### 5. `drain_freed()` locks unconditionally on every packet

In `dispatch::process` (dispatch.rs:305), `peer.queue_dispatcher.drain_freed()` is called after processing every packet. The method acquires the `free_notify` mutex even when the Vec is empty (the common case). The lock/unlock touches the futex word, bouncing the cache line between cores.

The minimal fix is an atomic flag (or an `AtomicUsize` length counter) checked before acquiring the mutex. The producer sets the flag after pushing; the consumer clears it after draining. When the flag reads zero, the mutex is skipped entirely.

### 6. Per-PathSecretEntry `queue_allocator` Mutex serializes flow creation

Each `PathSecretEntry` holds `queue_allocator: std::sync::Mutex<Allocator>` (entry.rs:86). The `alloc_queue()` method (called by client connect) and `queue_dispatcher()` (called on recv cache miss) both acquire this mutex. For a hot peer with many concurrent flows being established, all recv dispatch workers contend here.

Combined with issue #2 (O(n) scan inside this lock), the result is a cascading bottleneck: mutex acquisition + O(n) scan, serialized across all cores handling the same peer. The code has a TODO comment acknowledging this: "replace Mutex with a lock-free grow strategy."

Short-term, fixing the O(n) scan (issue #2) reduces the critical section to O(1) and makes the mutex tolerable. Long-term, the allocator should use a lock-free structure.

### 7. `FreeVec` single Mutex serializes all alloc and free operations

The `FreeVec` (free_list.rs:34) protects all descriptor allocation and deallocation with a single `Mutex<FreeInner>`. Every new stream creation (alloc) and every stream completion (free, via `drop_receiver`) goes through this one lock across all cores.

The free list comment says it prefers recently-used descriptors for cache locality (LIFO behavior). A lock-free Treiber stack naturally provides LIFO behavior without any lock. Alternatively, the free list could be sharded per-NUMA-node or per-worker, with work-stealing when a local shard is empty.

### 8. `sync::FreeList` high_water_mark overshoot permanently kills the fast path

The client-side peer free list (sync/free_list.rs:64-70) uses a speculative `fetch_add` to allocate fresh IDs:

```rust
let current = self.high_water_mark.load(Ordering::Relaxed);
if current < self.max_queues {
    let prev = self.high_water_mark.fetch_add(1, Ordering::Relaxed);
    if prev < self.max_queues {
        return VarInt::new(prev).ok();
    }
}
```

Multiple threads racing past `max_queues` inflate the counter permanently (it never decrements). After the initial allocation burst, `high_water_mark >= max_queues` forever, so every subsequent `try_alloc` and `poll_alloc` falls through to the mutex path even if fresh IDs would have been available had the counter not overshot.

The fix is a CAS loop:

```rust
loop {
    let current = self.high_water_mark.load(Ordering::Relaxed);
    if current >= self.max_queues { break; }
    match self.high_water_mark.compare_exchange_weak(
        current, current + 1, Ordering::Relaxed, Ordering::Relaxed
    ) {
        Ok(_) => return VarInt::new(current).ok(),
        Err(_) => continue,
    }
}
```

## Medium: Correctness or Performance Concern

### 9. RwLock write during `grow()` blocks all dispatch lookups

During `Pool::grow()` (pool.rs:190), a write lock is held on `state.pages` while performing a `Vec::push(pending_senders)`. The Vec push may trigger reallocation (memcpy of the pages vector). While the write lock is held, all dispatch workers that need to `refresh_pages()` (because they see a queue_id in a not-yet-cached page) block.

The data is prepared outside the lock, which is good. But the `pages.push(...)` inside the lock should never trigger a realloc if the Vec has pre-allocated capacity. Consider initializing the pages Vec with a capacity of `ceil(log2(MAX_SLOTS / INITIAL_PAGE_SIZE))` (about 10 entries) to guarantee no allocation under the write lock.

### 10. Relaxed ordering on `queue_id` is unsound on ARM

`into_receiver_pair` (descriptor.rs:222) stores `queue_id` with `Ordering::Relaxed`. The dispatch path (descriptor.rs:134) loads it with `Relaxed` on a potentially different core. On x86 (TSO), stores are visible to other cores quickly. On ARM/Graviton (weak ordering), a dispatch thread that already cached the sender page (skipping `refresh_pages` and its RwLock acquire) can read stale `REMOTE_QUEUE_ID_UNKNOWN` indefinitely, rejecting valid packets as Unallocated.

The indirect happens-before chain through the RwLock makes this extremely unlikely in practice (pages are only cached after a `refresh_pages` call which acquires the read lock, providing a fence). But the guarantee depends on `refresh_pages` being called between the `into_receiver_pair` store and the dispatch load, which is not formally enforced.

The fix is either: make the store use `Release` and the load use `Acquire`, or document that `refresh_pages` (and its RwLock acquire) must always be called before accessing a newly-grown page's queue_ids.

### 11. Dead counters never wired up

Nine `queue_bind` counters and six `rx_init_*` counters are defined and registered but never incremented. The QueueBind frame type was removed, so its counters are dead. The init rejection counters (`rx_init_no_acceptor`, `rx_init_acceptor_closed`, `rx_init_acceptor_no_slots`) should be incremented in `create_binding_with_queues` at the NotFound/Closed/NoSlots arms (dispatch.rs:699-747).

### 12. Unbounded waiter growth in sync::FreeList

When queue IDs are exhausted, every blocked `poll_alloc` pushes a waker clone into `inner.waiters` (sync/free_list.rs:99). Under sustained exhaustion (e.g., peer stops sending QueueFree due to bug or network partition), the waiter VecDeque grows without bound. Consider capping the waiter count or using a fixed-size ring buffer with backpressure.

## Low: Cleanup

### 13. Dead `create_binding` function

The function at dispatch.rs:611-645 is defined but never called. After fixing issue #1 to use `alloc_at_or_grow` in the live path, this dead code should be removed.

### 14. Per-frame Box allocation

Every dispatched frame allocates an intrusive `Entry` via `Box::new(Inner { ... })`. At millions of frames per second per worker, this creates significant allocator pressure. A per-worker slab or object pool for Entry nodes would amortize the cost.

## Lock Chain Summary

A single incoming init frame from a hot peer acquires these locks in sequence:

1. `PathSecretEntry::queue_allocator` Mutex — to get a dispatcher or allocate
2. `FreeVec::inner` Mutex — to find and remove the specific slot (O(n) scan)
3. `RwLock<SenderPages>` read — if pages need refreshing (write on grow)
4. `Queue<S>::inner` parking_lot Mutex — to push the data frame
5. `free_notify` Mutex — to drain freed slots (every packet)

On stream completion (receiver drops):

6. `Queue::inner` Mutex — to close receiver and check other half
7. `free_notify` Mutex — to notify freed slot
8. `FreeVec::inner` Mutex — to return descriptor to free list
9. Inline `waker.wake()` syscall — bypasses offload infrastructure

At 200 cores with 500k flows/sec, the number of distinct serialization points in this chain is the fundamental scalability ceiling. Each lock is short-held in isolation, but the aggregate latency of acquiring 5 locks per new flow and 4 locks per completed flow bounds maximum throughput.

The priority order for fixes should follow the critical path: (1) correctness of slot allocation, (2) O(1) alloc_at, (3) inline waking fix, (4) replace free_notify with lock-free MPSC, (5) atomic guard on drain_freed, (6) CAS loop for high_water_mark.
