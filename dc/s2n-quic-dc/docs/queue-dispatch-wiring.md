# Queue dispatch wiring — status and remaining work

## Completed

### Client connect (`stream/client.rs`)

Wired. The client gets `Arc<ClientState>` from the Entry's `QueueState::Client`
variant, calls `client_state.alloc().await` to get an `AllocResult { stream,
control, local_queue_id, dest_queue_id, binding_id }`, then passes those to
`Writer::new_client` and `Reader::new_client`.

### Server dispatch (`endpoint/dispatch.rs`)

Wired. When a QueueData-init frame arrives (dest_acceptor_id is Some):

1. Checks `acceptor_registry.get(acceptor_id)` first — if not found, calls
   `server_view.record_freed()` to enqueue the queue_id for QueueFree emission,
   sends a reset with `ACCEPTOR_NOT_FOUND`, and returns early (no bind, no
   Writer/Reader allocation).
2. Binds via `server_view.bind_and_send_stream(...)`.
3. On `NewBinding`: creates Writer/Reader, wraps in PendingValidation, sends
   directly to the pre-looked-up `acceptor_sender` (avoiding double lookup).
4. On `Bound`: wakes and continues.

### Sim connect (`endpoint/testing/sim.rs`)

Wired. Same pattern as the real client. The fast-path now checks
`QueueState::Client` before returning — in bidirectional P2P scenarios the
address map can contain a Server entry from the peer's prior connection to us.

### recv::Context changes

Done. `QueueView` enum has `as_server_mut()` and `as_client_mut()` accessors.
`ServerView` is stored per-context and has `record_freed()` for direct freed-ID
submission without binding.

### QueueFree frame type (`endpoint/frame.rs`)

Added. `Header::QueueFree { free_request_id, largest_queue_id }` with:
- Priority level 0 (highest — transmitted before all other frame types)
- Wire tag 23
- `has_payload_length() = true` (payload carries range-encoded freed queue_ids)
- Full encode/decode support
- Per-frame-type counters (tx, probe, acked, rx)

### Range codec (`endpoint/range_codec.rs`)

New module. Zero-allocation ACK-style range encoder/decoder for VarInt range
sets. Used by QueueFree (and eventually ACK frames — TODO).

- `encode(largest, ranges_descending, buffer)` — encodes directly into an Encoder
- `RangeDecoder::new(largest, payload)` — lazy iterator yielding
  `Result<RangeInclusive<VarInt>, DecoderError>`

### QueueFree receive path (`endpoint/dispatch.rs`)

Wired. `handle_queue_free` decodes the payload lazily via `RangeDecoder`, passes
the iterator to `ClientDispatch::free()`, which calls `FreeList::free()`. Wakers
from unblocked alloc futures are forwarded through `waker_sink`. Emits
`rx.queue_free.slots` and `rx.queue_free.ranges` distribution counters.

### PTO invariant fix (`endpoint/send.rs`)

`invariants()` now returns early when `self.invalidated` is true. An invalidated
context is dead — its PTO/inflight consistency is irrelevant since the idle wheel
already drained it.

### Acceptor reset (`endpoint/dispatch.rs`)

Generic `send_reset()` helper sends a QueueReset frame directly from dispatch.
Used in the acceptor-not-found path. Takes `&mut SubmissionSender` (all callers
updated to pass `&mut`).

### acceptor::LocalRegistry improvements (`acceptor.rs`)

Added `get(&mut self, acceptor_id) -> Option<&mut Sender<T>>` for checking
acceptor existence before doing expensive work. The caller can then call
`sender.send(item)` directly, avoiding a second lookup through
`LocalRegistry::send()`.

---

## Remaining work

### QueueFree emission task

The `freed_batch_rx` channel receiver at `endpoint.rs:405` is created but
dropped immediately (`_freed_batch_rx`). A background task needs to:

1. Receive `FreedBatch` tokens from `freed_batch_rx`.
2. At encoding time (as late as possible), call `batch.take(&mut interval_set)`
   to snapshot all accumulated queue_ids.
3. Encode using `range_codec::encode(largest, ranges_descending, buffer)` into
   a `ByteVec` payload.
4. Submit a `Frame { header: Header::QueueFree { free_request_id, largest_queue_id }, payload, ... }` to `frame_tx`.
5. After transmission, call `batch.check_and_resubmit(&freed_batch_tx)` — if
   more IDs accumulated while encoding/transmitting, the token requeues itself.

This is what blocks the `init_uniqueness_all_duplicated` test (currently capped
at 65535 streams = u16::MAX because freed IDs never reach the client).

### Peer-dead broadcast wiring

Done. The `peer_dead_broadcast` task no longer uses `msg::queue::Dispatcher`.
Instead it accesses the `Arc<Entry>`'s `QueueState` directly from the
`PeerDead` struct and calls `broadcast_reset` on the appropriate state:

- `ClientState::broadcast_reset` iterates all allocated slots via
  `SenderView::for_each_slot`, pushes `Reset { IDLE_TIMEOUT }` into both
  halves (skipping unallocated slots and halves without a receiver), then
  calls `FreeList::wake_all` to wake any clients blocked in `alloc()`.

- `ServerState::broadcast_reset` does the same slot iteration and reset push
  for server-owned slots.

- `Slot::broadcast_reset` pushes resets without binding validation and without
  clearing `HAS_SENDER` — this is a transient notification, not a permanent
  close. After cooldown expires, the slots remain usable for new bindings.

- `ClientAllocFuture` now borrows `&Entry` and a `cooldown: Duration`. On
  each poll it checks `entry.is_dead_during_cooldown(now, cooldown)` and
  returns `None` if the peer is dead — callers blocked in `alloc().await`
  bail promptly when `wake_all` fires.

- `FreeList::wake_all` drains all waiters without closing the list, so after
  cooldown expires new `alloc()` calls proceed normally against the unchanged
  free list state.

Design note: `broadcast_close` (which permanently removes `HAS_SENDER`) is
reserved for entry eviction, not peer-dead. The `peer_free` list is not closed
or reset either — if the peer was merely transiently unreachable, those server
slots remain valid after cooldown. If the peer truly died and reconnects, a
new TLS handshake creates a new Entry with a fresh `ClientState`.

This unblocks `peer_dead_cooldown_blocks_new_connects`.
`total_packet_loss_surfaces_read_timeout` may still need additional work if
idle-timeout resets from the PTO path need similar wiring.

### Entry eviction must broadcast_close

When a path secret Entry is evicted from the map, `ClientState` /
`ServerState` should have `broadcast_close` called to permanently clear
`HAS_SENDER` on all slots. Currently `ClientDispatch::close` and
`ServerView::close` exist but are never invoked from the eviction path.
Without this, receivers on evicted entries may block forever waiting for data.

### FreedInner leak on peer-dead

When the send context is invalidated (peer-dead or unknown-path-secret),
QueueFree frames already taken from `FreedInner` via `batch.take()` and
submitted to `frame_tx` are abandoned. Those server queue_ids are lost from
the client's perspective — `peer_free` never learns about them.

Additionally, if `check_and_resubmit` is never called for an in-flight token
(because the emission task's frame was cancelled), `in_flight` stays
permanently true, blocking all future freed-ID emission for that peer.

The fix requires the emission task (when wired) to handle cancellation
gracefully: either re-inserting taken IDs on failure, or having peer-dead
reset `in_flight` and re-accumulate any slots that the server considers free
but the client doesn't know about.

### Cleanup (follow-up PR)

Once all tests pass, remove:
- `handle_queue_init` function (old path) and the three `todo!()` arms
- `AttemptDedup` struct and all references
- `flow::Tracker` and `flow::Handle`
- `QueueInit`, `QueueInitReset`, `QueueInitFin`, `QueueValidateRequest`,
  `QueueInitValidate` frame variants
- `PendingValidation` reader status variant
- `msg::queue::Allocator` / `msg::queue::Dispatcher` type aliases (Dispatcher
  is no longer used by `peer_dead_broadcast`; Allocator remains in idle wheel
  recv tests until those are ported)
- The old `flow::queue` module
- `Endpoint.queue_allocator` and `Endpoint.next_binding_id`

### Range codec consolidation (nice-to-have)

Replace the `recv/ack_ranges.rs` ACK encoding (which uses `frame::Ack` from
s2n-quic-core) with `range_codec::encode`. Saves a few bytes per ACK frame by
dropping the redundant `ack_delay` field from the body (it's already in our
`Header::Ack`).
