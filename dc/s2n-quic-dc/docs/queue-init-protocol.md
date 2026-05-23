# Queue Init Protocol

This document describes the simplified queue initialization protocol. The design eliminates explicit bind frames entirely, using implicit binding creation analogous to IETF QUIC's stream creation model. The first QueueData frame arriving at an unbound server queue atomically creates the binding.

## Background

The original protocol required a multi-phase handshake: the client sent FlowInit, waited for the server to allocate and respond, then transitioned to Open. A subsequent simplification introduced QueueBind as an explicit binding frame, but this still introduced unnecessary round-trip latency for the first packet. The current design removes QueueBind entirely — any QueueData frame can implicitly create a binding on the server.

## Protocol Overview

The client allocates a local source queue ID, allocates a destination queue ID from its model of the server's free list (seeded by `initial_max_queues` from the handshake, recycled via QueueFree), then sends a QueueData frame with `dest_acceptor_id` set. The server sees the first frame for that unbound queue, atomically sets the binding_id as the key, creates the stream (Writer + Reader), registers with the acceptor, and delivers the data. No separate bind frame is needed; no response is required before data can flow.

### Wire Frames

**QueueData** — Bidirectional. Stream payload with offset, FIN, and optional acceptor_id.

When `dest_acceptor_id` is present (init frame), the server can create a new binding if the queue is unbound. Once the server confirms the binding (by sending MaxData back), subsequent frames omit acceptor_id. On the wire, init frames use distinct type tags (`QUEUE_DATA_INIT_NO_FIN_TYPE`, `QUEUE_DATA_INIT_WITH_FIN_TYPE`) that include the extra VarInt.

**QueueControl** — Bidirectional. Flow control frames (encoded in payload).

**QueueMaxData** — Bidirectional. Advertises increased receive window. Implicitly confirms the binding to the client.

**QueueReset** — Bidirectional. Aborts a stream with an error code.

**QueueFree** — Server → Client. Returns queue slot credits.

Contains binding_id and largest_queue_id with ACK-style range encoding in the payload. Tells the client that the server has finished with a queue slot, allowing the client to reuse that slot index for a future stream.

### Key Fields

| Field | Purpose | Lifetime |
|-------|---------|----------|
| source_queue_id | Route packets TO the sender | Slot index, reusable via QueueFree |
| dest_queue_id | Route packets TO the receiver | Slot index, reusable via QueueFree |
| binding_id | Anti-ABA validation for recycled slots | Per-entry monotonic, never reused |
| dest_acceptor_id | Identify the server's accept channel | Present until binding confirmed |

The binding_id is allocated from a per-PathSecretEntry atomic counter (`next_binding_id`). It serves as the queue key — when a packet is dispatched to a queue slot, the slot's binding_id is compared against the packet's binding_id via ordered comparison.

## Binding Validation

The binding_id comparison is ordered, not just equality:

- **Equal**: accept the frame, deliver to the stream
- **Received < current** (stale): packet was meant for a previous binding that has been freed and recycled. Drop silently.
- **Received > current** (future): protocol bug — the client rebound the queue before receiving QueueFree from the server. Panic in debug builds, log ERROR and emit counter in release.

On the server, if the queue is unbound (key is None) and the frame carries a `dest_acceptor_id`, the binding is created atomically under a single lock acquisition. This is the implicit binding creation path.

On the client side, unbound queues simply drop frames (the client never creates bindings — only the server does).

## Dispatch Architecture

Queue dispatch is per-context: each recv::Context owns its own Dispatcher instance. Since recv contexts are keyed by (credential_id, source_sender_id), queue IDs are effectively namespaced per-peer-per-sender. No credential_id validation is needed during dispatch — the context lookup already guarantees the packet belongs to this peer.

### Server-Side Implicit Binding

When a QueueData (or QueueReset) frame arrives at the server for a queue slot that has no binding:

1. The queue lock is taken
2. The key is checked — it's None (unbound)
3. The binding_id is set as the new key (atomically, under the same lock)
4. The data is pushed into the queue
5. The lock is released
6. Writer + Reader are created and registered with the acceptor identified by `dest_acceptor_id`

This is a single lock acquisition for the entire operation. The server's queue allocator grows on demand — it doesn't allocate from a free list, it just expands pages as needed.

## Queue ID Allocation

The client maintains an MPMC free list channel for dest_queue_id allocation:

- `initial_max_queues` (from ApplicationParams in the handshake) provides the initial range of IDs the client can use
- A high-water mark counter provides lock-free fresh allocation up to `initial_max_queues`
- Once the initial range is exhausted, the client waits for recycled IDs from QueueFree frames
- Multiple client tasks can concurrently allocate from this channel (MPMC)

The server doesn't allocate from its own free list — it sends freed queue_ids back to the client via QueueFree frames. Server queues grow on demand.

## QueueFree: Credit Return

When both the stream and control receivers for a queue slot are dropped (stream completed), the descriptor notifies a shared freed-slot sink. The recv worker drains this sink after processing each packet and emits QueueFree frames back to the client via the response path.

On the client side, receiving a QueueFree pushes that queue_id back into the MPMC free list channel, waking any consumer that was waiting for an available slot.

## Per-Entry State

Each PathSecretEntry maintains:

- `next_binding_id: AtomicU64` — monotonic counter for unique binding_ids
- `queue_allocator: Mutex<Allocator>` — per-entry pool for queue slot allocation
- `peer_free_list: Arc<FreeList>` — MPMC channel of available dest_queue_ids

An endpoint acts as both server and client. Path secret entries are unidirectional — each direction requires its own handshake and maintains its own state.

## Writer State Machine

The client writer has three states:

- **Init** → first QueueData-init frame sent → **Binding**
- **Binding** → MaxData received from server → **Open**
- **Open** → normal data flow, acceptor_id omitted from frames

While in the Binding state, all frames include `dest_acceptor_id` so the server can create the binding from any retransmitted frame (in case the first one was lost).

## Removed Mechanisms

The following were removed as part of this simplification:

- QueueBind frame type (implicit binding via QueueData)
- FlowInitSent writer state (no intermediate state)
- FlowInitReset and FlowInitFin frames (no handshake to abort)
- FlowValidateRequest and FlowInitValidate (validation is immediate)
- AttemptDedup bitmap
- flow::Tracker (stream_id → queue_id map, replaced by per-context dispatch)
- credential_id validation in queue dispatch (already namespaced by context)
- QueueBind sender pinning on the completion channel
- Generation encoding in queue_ids (binding_id provides anti-ABA instead)
- PeerQueueState with Notify+Mutex (replaced by MPMC FreeList)
