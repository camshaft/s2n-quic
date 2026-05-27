# Queue Descriptor Lifecycle: Current Problems

## The Fundamental Cycle

Every `DescriptorInner` contains a field `free_list: Arc<dyn FreeList>`. This Arc points back to the pool infrastructure that owns the Region that owns the raw memory containing that DescriptorInner. The ownership graph forms a cycle:

    Pool/Tracker → Vec<Region> → raw allocation → DescriptorInner.free_list → Arc<...> → Pool/Tracker

This cycle exists because the design uses a callback-on-drop pattern: when a receiver handle is dropped, it calls `free_list.free(descriptor)` to notify the pool. The descriptor must therefore carry a pointer to its parent. But the parent owns the descriptor's memory. Neither can be freed first.

The client pool works around this with a two-phase shutdown: `Memory` drops → sets `open = false` → waits for all descriptors to recycle back → then drops `FreeInner` which runs `drop_in_place` on each descriptor and finally deallocates the Region. This works because descriptors are always returned to the free list on the client path. The invariant `free_count == total` gates the final deallocation.

The server pool cannot use this pattern. Server descriptors are forgotten (never returned to a free list), so the "wait for all to come back" gate never opens. The DescriptorInner destructors never run. The Arcs inside them never decrement. The cycle is permanent.

## The on_freed Callback Cycle

`ServerFreedTracker` stores an `on_freed: OnceLock<Arc<dyn Fn()>>` callback. The dispatch code wires this to a closure that captures a clone of the `Dispatch` struct (which holds `Arc<ServerFreedTracker>`). This creates a second cycle independent of the Region/descriptor issue:

    ServerFreedTracker.on_freed → closure → Dispatch.freed → Arc<ServerFreedTracker>

Using `Weak` here breaks the immediate cycle but introduces a liveness problem: if the only strong reference to the tracker comes from the dispatch path, and dispatch is reconstructed per-packet-batch from the PathSecretEntry, the tracker could be dropped between batches. The `Weak` upgrade would then fail and freed queue_ids would be silently lost.

## Memory Ownership Confusion

A Region is a raw allocation (alloc_zeroed) that holds N DescriptorInners laid out contiguously. Ownership of this memory must last until both conditions are true: no sender references exist (no more pushes possible), and no receiver references exist (no more pops possible). Today, three different things claim to manage this lifetime:

The `Memory` struct (client path) holds `Arc<FreeVec>` and signals shutdown on drop. The `ServerFreedTracker` holds `Vec<Region>` directly. The descriptor's own `free_list` Arc indirectly keeps the pool alive. When these disagree about who outlives whom, memory either leaks (server) or panics (double-free if the Region drops while senders still hold descriptor pointers).

## The Server Doesn't Allocate But Shares Allocation Infrastructure

The gap analysis correctly identified that the server never allocates queue IDs from a free list. Queue IDs arrive from the wire. Yet until this refactor, both client and server shared `FreeVec` — a data structure built entirely around allocation. The current split (ClientPool + ServerPool) separates the pool types but they still share `DescriptorInner`, which carries the `Arc<dyn FreeList>` field regardless of which side it serves. A server descriptor that will never be "freed back into a free list" still pays the cost of carrying and reference-counting the callback machinery.

## Requirements

The descriptor memory (the Region) must stay alive as long as any sender or receiver reference to any descriptor within that Region exists. Senders are cloned into the sender page table and live as long as the page table lives. Receivers are handed out to stream tasks and live until the stream completes. Both can outlive the pool, the dispatch handle, and the PathSecretEntry.

When the last receiver for a queue drops, the server must record that queue_id for QueueFree emission. This notification must reach a structure that outlives the receiver but is reachable by the frame emission path.

The frame emission path (QueueFree encoding) must be able to drain accumulated freed queue_ids without holding any lock that contends with the server dispatch hot path.

The server dispatch hot path (bind_and_send_stream) must acquire exactly one lock: the stream queue's parking_lot::Mutex. No other synchronization primitive should be acquired. The bind operation itself (validate_or_bind) is a lock-free atomic CAS on the descriptor's binding_id word.

The descriptor memory layout must support both client and server use cases without the descriptor itself needing to know which pool type created it. The per-descriptor overhead for server descriptors (which are never recycled) should be minimal — ideally zero beyond what both sides need (the queues, binding_id, remote_queue_id, sender refcount).

There must be no reference cycles. Specifically: no path from the thing that owns Region memory, through the Region, through a DescriptorInner field, back to the thing that owns Region memory. The drop-notification mechanism must not create a cycle between the notifier and the notification target.

The pool infrastructure must be droppable. When a PathSecretEntry is evicted, all pool state for that peer must eventually be reclaimed — after in-flight streams complete, not before, but also not "never" due to a cycle.
