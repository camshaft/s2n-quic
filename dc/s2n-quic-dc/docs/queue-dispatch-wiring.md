# Remaining work: dispatch and client wiring

The Writer and Reader now use the new queue module types (`ControlReceiver`,
`StreamReceiver`) and send QueueData-init frames instead of QueueInit. The
three `todo!()` sites block the endpoint-level tests from passing. This doc
describes what each site needs to become.

## 1. Client connect (`stream/client.rs`)

Currently `todo!("wire client to new queue module")`.

The client needs to:

1. Get `Arc<ClientState>` from the Entry's `QueueState::Client` variant.
2. Create a `ClientAllocator` (or reuse one cached on the Client struct).
3. Call `allocator.try_alloc()` to get `AllocResult { stream, control,
   local_queue_id, dest_queue_id, binding_id }`.
4. Pass `control` + `dest_queue_id` to `Writer::new_client`.
5. Pass `stream` + `dest_queue_id` to `Reader::new_client`.
   (For the reader, `dest_queue_id` is the peer's source_queue_id — i.e. the
   slot the peer will use to send control frames back to us. This is the same
   `local_queue_id` from the alloc result since the client allocated both sides.)

Open question: `try_alloc` is non-blocking and returns `None` if the peer's
free list is exhausted. The old code used `alloc_or_grow` which always
succeeds by growing the pool. We may need an async `alloc` path that waits for
a QueueFree from the server — or just grow locally for now (the peer free list
starts at `max_queues` capacity and won't exhaust until QueueFree is needed).

Actually — the peer free list starts with `max_queues` worth of fresh IDs via
`try_alloc_fresh`, so it won't return None until all slots are in use. For the
initial wiring this is fine.

## 2. Server dispatch (`endpoint/dispatch.rs`)

Currently `todo!("wire dispatch to new queue module")` inside the
`create_stream` closure in `handle_queue_init`.

When a QueueData-init frame arrives (dest_acceptor_id is Some), the dispatch
needs to:

1. Get `Arc<ServerState>` from the peer's `recv::Context` (which gets it from
   the Entry's `QueueState::Server` variant).
2. Get or create a `ServerView` on the recv::Context (cached for the lifetime
   of the context).
3. Call `server_view.bind_and_send_stream(queue_id, binding_id, entry,
   &path_entry, &endpoint_tx)`.
4. On `BindResult::NewBinding { stream, control, .. }`:
   - Create `Writer::new_server(frame_tx, path_entry, source_queue_id,
     acceptor_id, control)`
   - Create `Reader::new_server(frame_tx, path_entry, source_queue_id,
     stream, peer_fin)`
   - Wrap in `Stream::new(reader, writer)` → `PendingValidation::new(stream)`
   - Dispatch to the acceptor registry
5. On `BindResult::Bound(waker)`: the frame was already pushed to an existing
   stream — just wake and continue.

The `source_queue_id` for the server's Writer/Reader `dest_queue_id` param is
the client's `queue_pair.source_queue_id` from the incoming frame — that's
the slot the client is listening on for control/data responses.

The old `handle_queue_init` function also handles:
- AttemptDedup (can be removed once QueueInit is gone — binding_id on the
  slot handles dedup)
- flow::Tracker (maps binding_id → queue_id — replaced by ServerView's
  bind_and_push_stream which does this atomically)
- QueueValidateRequest/QueueInitValidate handshake (removed — no longer needed)

For the initial wiring, the existing `handle_queue_init` path can remain for
backward compat with old clients. A new `handle_queue_data_init` branch handles
QueueData frames where `dest_acceptor_id.is_some()`.

## 3. Sim connect (`endpoint/testing/sim.rs`)

Currently `todo!("wire sim connect to new queue module")`.

Same pattern as the client: get `ClientState` from the Entry, allocate, pass
results to Writer/Reader constructors. The sim's connect function just needs
the same 5 lines as the real client.

## 4. recv::Context changes

The `recv::Context` needs:
- A `ServerView` field (created from the Entry's `ServerState` on first use)
- Access to `FreedBatchTx` (the endpoint-level channel for freed-batch
  emission) — passed in at Context creation or stored on the Context

## 5. What can be removed after wiring (follow-up PR)

Once the dispatch is wired and all tests pass:
- `handle_queue_init` function (old path)
- `AttemptDedup` struct and all references
- `flow::Tracker` and `flow::Handle`
- `QueueInit`, `QueueInitReset`, `QueueInitFin`, `QueueValidateRequest`,
  `QueueInitValidate` frame variants
- `PendingValidation` reader state
- `msg::queue::Allocator` / `msg::queue::Dispatcher` type aliases
- The old `flow::queue` module
- `Endpoint.queue_allocator` and `Endpoint.next_binding_id`
