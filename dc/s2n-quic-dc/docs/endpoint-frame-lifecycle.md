# Endpoint Frame Lifecycle (endpoint.rs + tasks.rs)

This document explains the high-level send-side lifecycle implemented by
`dc/s2n-quic-dc/src/endpoint.rs` and
`dc/s2n-quic-dc/src/endpoint/tasks.rs`, with supporting behavior in
`frame.rs`, `combinator.rs`, `send.rs`, `assemble.rs`, and `ack.rs`.

The goal is to describe how a submitted frame moves through dispatch, pacing, congestion control, transmission,
ack/loss processing, retransmission, timeout handling, and completion notification.

## Scope and mental model

At a high level, the endpoint is a task graph with one shared frame-submission ingress and multiple worker-local
send/recv pipelines. The key split is:

- **Submission + global scheduling**: accepts frames from writers/readers/dispatch and chooses a send socket worker.
- **Per-send-worker state machine**: owns per-peer send contexts (crypto, CCA, inflight map, timers) and performs
  assembly, ACK/loss processing, retransmission, PTO probing, and completion emission.

A **frame** is the application-level unit. A **packet** (encoded into a UDP **segment** in this codebase’s send
path) is the transport-level unit that may contain many frames.

## 1) Application submission

Application components create `Frame` values and submit them through the endpoint’s shared `SubmissionSender`
(`Endpoint::frame_tx`).

`Frame` includes:

- routing/header metadata,
- payload,
- optional completion sender (`completion: Option<CompletionSender>`),
- transmission status (`Pending`, `Acknowledged`, `Failed(reason)`),
- TTL (for bounded retransmission),
- optional transmission-time hint.

If `completion` is present, `completion.should_transmit()` acts as a cancellation gate. If it is absent, the frame
is best-effort and still transmitted, but no completion event is delivered.

## 2) Global frame dispatch (priority, batching, pacing, worker selection)

`tasks::frame_dispatch` runs as two cooperating tasks.

First, the **priority router** swaps shard-local submission storage and drains frames into per-priority lanes.
Second, the **batch/distributor**:

1. batches consecutive frames by path-secret entry (`BatchFramesByPathSecret`),
2. applies an endpoint-wide pacing stage (`Paced` with `overall_send_rate`),
3. routes each batch to a send worker via pick-two load balancing (`PickTwo`).

Sticky sender IDs bypass pick-two and preserve sender affinity (used when retransmissions or flow-init follow-up
must stay on the same sender socket).

## 3) Send worker context resolution

Each send worker (`tasks::send_worker`) receives frame batches and ACK messages and owns a set of send sockets.

`tasks::context_resolver` maps each frame batch to a per-peer `send::Context` (creating it on first use), appends
batch contents to that context’s priority queues, and computes `WheelInterest` (whether tx, PTO, and/or idle wheels
should be armed/re-armed).

`send::Context` is where send-side state lives:

- sealer + credentials,
- congestion controller (CCA) and RTT estimator,
- inflight packet map,
- pending frame queues,
- pending ACK queue (for direct ACK sends),
- tx/PTO/idle wheel links and target times.

## 4) Tx wheel → assembler → socket send

The tx wheel (`send_tx_wheel_drain`) releases expired contexts to their owning socket assembler task.

`tasks::send_socket_assembler` then runs:

1. `Assembler` (`combinator.rs`), which calls `assemble::assemble`,
2. wheel re-dispatch (new tx/PTO/idle interest),
3. per-socket pacing (`Paced` with `per_socket_send_rate`),
4. socket transmission (`SocketSender`).

### What `assemble::assemble` actually does

For each segment, assembly proceeds in phases:

1. drain pending direct ACK submissions (`pending_acks`) into `Header::Ack` frames,
2. if PTO probe is requested and no pending data exists, build a probe from oldest inflight data (`assemble_probe`),
3. drain pending data/control frames (subject to CWND, except PTO probe bypass behavior).

Frames that fail `should_transmit()` are not sent; they are routed to cancelled output.

Encoded ack-eliciting packets with data are inserted into the inflight map with transmission metadata, and CCA is
updated via `on_packet_sent`. PTO state is updated when an inflight packet is created. Sender load score is refreshed
after queue/CCA changes so pick-two sees current pressure.

## 5) ACK processing, loss detection, retransmission

ACK messages from recv workers are handled by `AckProcessor` (`combinator.rs`), which resolves the context and calls
`Context::process_ack_payload` (`send.rs`), which delegates to `ack::process_ack` (`ack.rs`).

ACK processing performs:

- inflight removal for ACKed ranges,
- completion marking (`TransmissionStatus::Acknowledged`) for ACKed frames,
- RTT/CCA updates (including ACK-only RTT sampling path),
- ECN feedback handling,
- PN-threshold loss detection.

On loss, each frame is handled independently:

- cancelled (`!should_transmit`) → `Failed(Cancelled)` and routed to cancelled path,
- TTL exhausted (`ttl == 0`) → `Failed(TransmissionError)` and completed as failure,
- otherwise, the frame’s TTL is decremented and the frame is requeued to submission
  (`frame_tx.send_batch(lost_queue)`).

That requeue path sends lost frames back through the same global dispatch pipeline instead of bypassing it.

## 6) Timeout and probe behavior

The PTO wheel (`send_pto_timeout`) calls `Context::on_pto_timeout`, which may transition probe state to
`Requested` and re-arm wheels.

When probe state is requested, assembly ensures an ack-eliciting probe is sent:

- pending data can serve as the probe (bypassing CWND per current logic),
- otherwise the assembler retransmits from the oldest non-shell inflight packet under a new packet number, linking
  old and new entries via `probed_to`.

Idle wheel handling (`send_idle_wheel_drain`) expires inactive contexts or re-schedules them with the next idle
deadline.

## 7) Invalidation and secret-control effects

Background invalidation validation (`invalidation_validator`) parses secret-control packets and emits validated
`Invalidation` events.

Send-side invalidation behavior (`send_invalidation`) is intentionally split:

- `UnknownPathSecret` invalidates matching send contexts and fails drained frames with
  `FailureReason::UnknownPathSecret`.
- `StaleKey`/`ReplayDetected` map to `Invalidation::StaleKey` and requeue drained frames as pending work
  (status remains pending) so they can be retransmitted under refreshed context state.

Routing is sender-targeted for stale/replay invalidations using `sender_id_to_worker`.

## 8) Completion delivery

Completed/failed frames are sent to `completion_dispatcher`, which groups by completion queue and uses batch
`send_batch` to reduce locking overhead. Only non-pending statuses are emitted. `FailuresOnly` subscriptions only
receive failed frames.

A separate cancelled drain task drops frames whose owning writer no longer needs notification.

## End-to-end summary

A frame is submitted once, then repeatedly cycled through:

submission → global dispatch/pacing/pick-two → per-peer context queues → tx wheel → assembly/send →
ack/loss/timeout/invalidation updates.

It exits the lifecycle when one of the following occurs:

- peer ACKs it (`Acknowledged` completion),
- retry budget/cancellation/peer condition fails it (`Failed(reason)` completion, when subscribed),
- writer-side cancellation causes it to be dropped by the cancelled path.

This design keeps admission/global scheduling centralized while keeping transport state and recovery logic local to
the send context that owns the peer/socket relationship.
