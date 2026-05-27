# Phase 2.1 Gap Analysis: Server Queue Dispatch

This document analyzes the gaps between the Phase 2.1 design intent and what commit 88bea9ea actually implemented in the server-side queue dispatch path.

## Design Intent

The design specifies two invariants for server-side queue dispatch:

First, the bind-or-validate plus push operation must execute under a SINGLE LOCK acquisition. The server receives a QueueData-init frame and must atomically determine whether the slot is unbound (new stream) or already bound (retransmit/continuation), then push data into the stream queue. No additional synchronization primitives should be acquired on this path.

Second, the server does not allocate from its own free list. Queue IDs originate from the client, arrive in the packet header as `dest_queue_id`, and the server indexes directly into a page table. When the server frees a queue, it sends `QueueFree` back to the client so the client can reuse that ID. The server never needs to "find the next available slot" because it never chooses slot indices.

## What Was Implemented

Commit 88bea9ea introduced the single-lock bind path with these components:

`bind_and_push` in `inner.rs` (line 473) acquires the stream queue's `parking_lot::Mutex`, calls `validate_or_bind` (an atomic CAS on the descriptor's binding_id word), sets `HAS_RECEIVER` on new bindings, and pushes the entry. This correctly fuses validation and push into one lock acquisition.

`finalize_new_binding` in `descriptor.rs` (line 373) opens the control queue's receiver by calling `open_receiver_single`, which acquires the control queue's `parking_lot::Mutex`. This is a separate lock on a different structure, which is acceptable because it operates on the other half of the queue pair.

`claim_bound_slot` in `free_list.rs` (line 339) removes a slot from the `FreeVec`'s bookkeeping after an atomic bind succeeds. It acquires `self.inner.lock()` on the free list's `std::sync::Mutex`.

The dispatch entry point in `queue.rs` (`bind_and_send_stream`, line 370) orchestrates these: it calls `lookup_unbounded` to find the sender, invokes `bind_and_send_stream` on the sender (which calls `bind_and_push`), then calls `finalize_new_binding` for new bindings, and finally calls `self.pool.claim_bound_slot()` outside the closure.

## Gap 1: Second Mutex on the New-Binding Hot Path

After `bind_and_push` returns `ServerValidation::NewBinding` under the stream queue's `parking_lot::Mutex`, the code at `queue.rs` line 400 calls `self.pool.claim_bound_slot(local_queue_id.as_u64() as usize)`. This calls through to `FreeVec::claim_bound_slot` (free_list.rs line 339), which acquires the free list's `std::sync::Mutex<FreeInner>`.

This mutex is shared across all lifecycle operations on this peer's pool: every `alloc`, `alloc_at`, `record_region`, and `recycle` call contends on the same lock. On a server handling 500k+ flows per second across many cores, this serialization point is problematic. It also violates the single-lock design goal since the new-binding path now acquires two mutexes sequentially (stream queue mutex, then free list mutex).

The root cause is that `claim_bound_slot` exists only to maintain the free list's `slots` vec and `free_count` bookkeeping. The atomic CAS in `validate_or_bind` already claimed the slot logically (the binding_id word transitioned from UNALLOCATED to allocated). The free list removal is bookkeeping for an allocator that the server should not possess.

## Gap 2: Server Possesses a Full FreeVec Allocator

The server's `Pool` (pool.rs line 13) contains an `Arc<FreeVec<S, C>>` with a `VecDeque<usize>` of free indices, `alloc()` and `alloc_at()` methods, and mutex-protected lifecycle management. This exists because `Pool` is shared between client and server via a `server: bool` flag (pool.rs line 27) rather than being structurally different types.

The design says the server never allocates queue IDs. It receives `dest_queue_id` from the wire and indexes directly. Yet the current code provides the server with the full allocation machinery: `alloc` (free_list.rs line 233), `alloc_at` (free_list.rs line 280), `alloc_or_grow` (pool.rs line 120), and `alloc_at_or_grow` (pool.rs line 145). The server only ever uses `alloc_at_or_grow` on the cold path (page doesn't exist yet), but even that function loops through the free list trying to "allocate" a slot whose index was already determined by the packet header.

The `FreeVec` legitimately serves one purpose on the server: tracking `pending_freed` for QueueFree emission. But the slot management (`slots`, `free_indices`, `free_count`) and the allocation methods (`alloc`, `alloc_at`) are dead weight. The server needs only the `pending_freed` IntervalSet and the `on_freed` callback mechanism.

## Gap 3: Cold Path Acquires Multiple Locks

When `lookup_unbounded` returns `NeedsGrow` (the page table hasn't grown to include the requested slot index), the cold path in `handle_queue_data_server_init` (dispatch.rs line 763) calls `peer.queue_dispatcher.alloc_at_or_grow()`. Tracing through this function reveals multiple lock acquisitions:

The `pool.grow()` call (pool.rs line 173) acquires the sender state's `RwLock` in write mode (line 226) and then calls `self.free.record_region()` (line 264), which acquires the free list's `std::sync::Mutex` (free_list.rs line 377). After growing, `alloc_at_or_grow` loops back to call `self.free.alloc_at()` (free_list.rs line 280), which acquires the free list mutex again. If `alloc_at` returns `Err` because the slot was concurrently taken, the loop may grow and retry, acquiring both locks additional times.

After `alloc_at_or_grow` succeeds, the code pushes data via `queue_stream.push()` (dispatch.rs line 800), which acquires the stream queue's `parking_lot::Mutex`.

The total for the cold path when the page must grow is: RwLock write (grow), free list Mutex (record_region inside grow), free list Mutex (alloc_at), stream queue Mutex (push). That is 4 lock acquisitions minimum. With retries it can be 5+.

The server cold path should require only 2 locks: the RwLock write to extend the page table, then the stream queue mutex to bind and push. Everything in between is free-list bookkeeping for an allocator the server doesn't need.

## Gap 4: Dead Code

`push_with_result` in inner.rs (line 198) and `with_key` in inner.rs (line 446) are never called. `push_with_result` was intermediate scaffolding that `bind_and_push` replaced. `with_key` was a lock-guarded validation helper that became unnecessary once `validate_or_bind` moved the validation inside the `bind_and_push` closure. Both should be removed to reduce cognitive load and maintenance surface.

## Lock Count Summary

The following traces count every mutex/lock acquisition on the server dispatch path for a QueueData-init frame.

For the hot path where the binding already exists (Bound): the code acquires the stream queue's parking_lot::Mutex once inside `bind_and_push`. Total: 1 lock. This matches the design intent.

For the hot path where a new binding is created and the slot's page exists (NewBinding): the code acquires the stream queue Mutex (bind_and_push), then the control queue Mutex (finalize_new_binding / open_receiver_single), then the free list std::sync::Mutex (claim_bound_slot). Total: 3 locks, of which only the first is on the data-path critical section. The control queue lock is acceptable (different structure, different contention domain). The free list lock is the violation.

For the cold path where the page doesn't exist (NeedsGrow): the code acquires the sender RwLock in write mode (grow), the free list Mutex (record_region), the free list Mutex again (alloc_at), then the stream queue Mutex (push via the handle), then a second push to the control queue. Total: 5+ locks. The design target is 2 (RwLock grow + stream queue Mutex).

## What the Server Actually Needs

The server's queue infrastructure requires only:

A page table of descriptors, directly indexed by queue_id. Growth is protected by a RwLock (write-side is rare, read-side is free via cached local pages). Each descriptor contains an AtomicU64 binding_id using the MSB-encoded scheme (UNALLOCATED_BIT for free/bound discrimination and stale detection), a stream queue (parking_lot::Mutex), a control queue (parking_lot::Mutex), and an atomic remote_queue_id.

A pending-freed IntervalSet for QueueFree emission, with its own dedicated lock and single-flight dedup. This already exists inside `FreeVec` but is entangled with the slot management code.

The server does not need: `free_indices: VecDeque<usize>`, `slots: Vec<Option<Descriptor>>`, `free_count`, `alloc()`, `alloc_at()`, `alloc_or_grow()`, or `claim_bound_slot()`. When a stream is freed on the server, `drop_receiver` already sets UNALLOCATED_BIT via `fetch_or` (descriptor.rs line 275) and the free list callback records the queue_id in pending_freed. The descriptor never needs to be physically "returned" to a free list because the server never allocates from one.

## Proposed Fix

Split `Pool` into two structurally distinct types: `ClientPool` retains `FreeVec` with sequential allocation and FIFO reuse. `ServerPool` holds only the page table (for growth), the pending-freed buffer with its callback, and the sender state. No free list, no `claim_bound_slot`.

The `ServerFreeList` trait implementation changes: instead of calling `recycle` (which puts the descriptor back into `slots` and `free_indices`), it records the queue_id in pending_freed and then simply forgets the descriptor pointer. The descriptor's memory remains valid (owned by the Region) and the slot can be rebound on the next packet that arrives with that queue_id, gated solely by the atomic CAS on binding_id.

The server dispatch path becomes:

1. `lookup_unbounded` finds the sender by indexed page table access (no lock, just array indexing on cached local pages).
2. If the page is missing: acquire RwLock write to grow, extending the page table to cover the requested slot. This is the rare cold path.
3. `bind_and_push` under the stream queue's parking_lot::Mutex: atomically CAS the binding_id, set HAS_RECEIVER if new, push data. One lock.
4. If NewBinding: `open_receiver_single` on the control queue's parking_lot::Mutex. This is a different lock on a different structure.

Total for the new-binding hot path: 2 locks (stream queue + control queue), both on independent structures with no cross-contention. The free list mutex disappears entirely from the data path.

Total for the cold path: 3 locks (RwLock write for growth + stream queue + control queue). Down from 5+.
