# Queue Init Protocol

This document describes the simplified queue initialization protocol that replaced the original Flow Init handshake. The new design eliminates the FlowInitSent intermediate state, giving the client full routing knowledge at connect() time with no round-trip before data can flow.

## Background

The original protocol required a multi-phase handshake: the client sent FlowInit, waited for the server to allocate a queue and respond, then transitioned to an Open state where data could flow. This intermediate FlowInitSent state caused a class of bugs around writer drops during handshake, FlowInitFin reordering, and server-side dedup complexity.

## Protocol Overview

The client allocates a local source queue ID plus a destination queue ID from its model of the server's free list, then sends a QueueBind frame that carries everything the server needs. The server allocates its own local queue slot and immediately begins accepting data. No response is needed before data can flow.

### Wire Frames

The protocol uses the following frame types:

**QueueBind** — Client → Server. Opens a new stream.

Contains queue_pair (source_queue_id + dest_queue_id), dest_acceptor_id, binding_id, is_fin flag, and optional early data payload. The client knows its own queue IDs before sending because it allocates them from its per-PathSecretEntry pool.

**QueueData** — Bidirectional. Stream payload with offset and FIN.

**QueueControl** — Bidirectional. Flow control frames (encoded in payload).

**QueueMaxData** — Bidirectional. Advertises increased receive window.

**QueueReset** — Bidirectional. Aborts a stream with an error code.

**QueueFree** — Server → Client. Returns queue slot credits.

Contains binding_id and largest_queue_id. Tells the client that the server has finished with a queue slot, allowing the client to reuse that slot index for a future stream.

### Key Fields

| Field | Purpose | Lifetime |
|-------|---------|----------|
| source_queue_id | Route packets TO the sender | Slot index, reusable via QueueFree |
| dest_queue_id | Route packets TO the receiver | Slot index, reusable via QueueFree |
| binding_id | Anti-ABA validation for recycled slots | Per-entry monotonic, never reused |

The binding_id is allocated from a per-PathSecretEntry atomic counter (`next_binding_id`). It serves as the queue key — when a packet is dispatched to a queue slot, the slot's binding_id is compared against the packet's binding_id. A mismatch means a stale packet reached a recycled slot and gets rejected.

## Dispatch Architecture

Queue dispatch is per-context: each recv::Context owns its own Dispatcher instance. Since recv contexts are keyed by (credential_id, source_sender_id), queue IDs are effectively namespaced per-peer-per-sender. This means no credential_id validation is needed during dispatch — the context lookup already guarantees the packet belongs to this peer.

### Queue Association Checks

Queue IDs are associated with a single binding_id at a time. If a frame arrives for an unassigned queue on the server, QueueBind establishes the association. If the queue is already associated with a different binding_id, the frame is dropped.

### Queue Validation

The queue Key is simply the binding_id (a VarInt). On every dispatch to a queue slot, the descriptor validates that the packet's binding_id matches the slot's stored binding_id. This single comparison (inside the queue lock) replaces the previous two-field validation (credential_id + stream_id).

## QueueFree: Credit Return

When both the stream and control receivers for a queue slot are dropped (stream completed), the descriptor notifies a shared freed-slot sink. The recv worker drains this sink after processing each packet and emits QueueFree frames back to the client via the response path.

On the client side, receiving a QueueFree returns that queue_id to an async MPMC free-list queue. `connect()` allocates `dest_queue_id` from this queue and will wait when no slot is currently available.

## Per-Entry State

Each PathSecretEntry maintains:

- `next_binding_id: AtomicU64` — monotonic counter for unique binding_ids
- `queue_allocator: Mutex<Allocator>` — per-entry pool for queue slot allocation
- The allocator produces Dispatchers that share the same underlying pool

The queue pool uses exponential page growth (page sizes double) so it converges quickly to the right capacity without over-allocating on startup.

## Removed Mechanisms

The following were removed as part of this simplification:

- FlowInitSent writer state (no intermediate state)
- FlowInitReset and FlowInitFin frames (no handshake to abort)
- FlowValidateRequest and FlowInitValidate (validation is immediate)
- AttemptDedup bitmap
- flow::Tracker (stream_id → queue_id map, replaced by per-context dispatch)
- credential_id validation in queue dispatch (already namespaced by context)
- QueueBind sender pinning state on the completion channel
- Generation encoding in queue_ids (binding_id provides anti-ABA instead)
