# s2n-quic-dc Architecture

`s2n-quic-dc` is a datagram-based transport for data center networks. It reuses the QUIC
handshake to derive strongly authenticated, long-lived shared secrets between peers, then uses
those secrets to protect individual short-lived streams without requiring any per-stream
cryptographic negotiation or centralized state.

## Crate Layout

```
src/
  credentials.rs        -- Credential ID and key-ID types
  crypto/               -- Encrypt/decrypt traits and AWS-LC implementation
  packet/               -- Wire formats (stream, control, secret_control)
  path/
    secret/             -- Shared-secret scheduling, sender/receiver state, secret map
  psk/                  -- QUIC PSK handshake wrappers (client / server)
  stream/
    application.rs      -- Public Builder/Stream/Reader/Writer types
    endpoint.rs         -- open_stream() entry point
    server/             -- Accept-side plumbing (handshake map, acceptor, manager)
    send/               -- Outbound state machine, queue, and worker
    recv/               -- Inbound dispatch, buffer, state machine, and worker
    shared/             -- State shared between the send and receive halves
  sync/                 -- Lock-free primitives (ring_deque, mpsc, mpmc)
  task/                 -- Waker helpers
  event/                -- Telemetry subscriber traits
  socket/               -- UDP socket abstraction and descriptor pool
  control.rs            -- Control-channel setup
```

---

## 1. Handshake and Credential Establishment

### The core idea

The handshake runs **once per peer pair** using a standard TLS 1.3 QUIC connection. Both
sides export a shared secret from the TLS session using the TLS Exporter (RFC 5705), then
store that secret in a local map. Every stream opened to that peer thereafter derives its
encryption material directly from the cached secret — no additional handshake is required.

### What gets exported

After the QUIC handshake completes, `HandshakingPath` (see
[`path/secret/map/handshake.rs`](src/path/secret/map/handshake.rs)) calls
`s2n_quic::provider::dc::Path::on_dc_handshake_complete`, which uses `TLS_EXPORTER_LABEL =
"EXPERIMENTAL EXPORTER s2n-quic-dc"` to derive a shared `schedule::Secret`.

This secret, together with a `credentials::Id` (a 16-byte random identifier), forms a
`path/secret/map/entry::Entry` that is stored in the `path::secret::Map`
([`path/secret/map.rs`](src/path/secret/map.rs)).

### Per-stream key derivation

Every stream packet carries a `Credentials` value
([`credentials.rs`](src/credentials.rs)) consisting of:

- `Id` — 16-byte identifier for the shared entry in the map
- `KeyId` — a monotonically incrementing varint

The sender side (`path/secret/sender::State`,
[`path/secret/sender.rs`](src/path/secret/sender.rs)) atomically claims the next `KeyId`
and hands it to the stream. The crypto layer derives the actual AEAD key for that `KeyId`
on the fly from the base `schedule::Secret`
([`path/secret/schedule.rs`](src/path/secret/schedule.rs)).

The receiver side (`path/secret/receiver::State`,
[`path/secret/receiver.rs`](src/path/secret/receiver.rs)) tracks the maximum `KeyId` seen
and maintains a sliding bitmap window (65,535 bits) to detect replayed packets.

### Why this avoids excess state and synchronization

- **One entry per peer**, not per stream. Streams are ephemeral; the map entry lives as long
  as the peer relationship does, bounded by a configurable capacity.
- **Atomic key-ID claiming**. The sender increments a single `AtomicU64`; no locks are
  needed to allocate a key for a new stream.
- **Lazy key derivation**. AEAD keys are derived from the base secret and the `KeyId` only
  when a packet is actually being sealed or opened; no keys are pre-generated.
- **Lock-free replay detection**. The bitmap window is guarded by a narrow `Mutex` only for
  bit-level updates; reads of `max_seen_key_id` are atomic.
- **Periodic re-handshake**. The cleaner task
  ([`path/secret/map/cleaner.rs`](src/path/secret/map/cleaner.rs)) evicts old entries and
  triggers re-handshakes to rotate the base secret.

### Out-of-band credential signals

If a receiver encounters a packet it cannot decode — because the credential ID is unknown,
the key is stale, or a replay is detected — it sends a small authenticated
`packet::secret_control` datagram
([`packet/secret_control.rs`](src/packet/secret_control.rs)) back to the sender. These
signals (`UnknownPathSecret`, `StaleKey`, `ReplayDetected`, `FlowReset`) allow the two
sides to resynchronize without a full re-handshake in most cases.

---

## 2. Stream Lifecycle

### Overview

A stream is a single bidirectional, reliable, ordered byte channel. Each stream is
associated with one `Credentials` value (and hence one peer entry) and runs on a UDP socket.
The word *stream* here refers to the application-level abstraction; the underlying transport
uses unreliable UDP datagrams plus a reliability layer inside `s2n-quic-dc`.

### Queue-ID observation protocol

Before either side can deliver data reliably, each needs to know which queue slot the other
has allocated for inbound packets. This is tracked by a small state machine in
[`stream/shared/handshake.rs`](src/stream/shared/handshake.rs):

| State | Meaning |
|---|---|
| `ClientInit` | Client has chosen its own `queue_id`; server's is not yet known |
| `ClientQueueIdObserved` | Client has seen at least one packet from the server and learned its `queue_id` |
| `ServerInit` | Server has seen the client's first packet and chosen its own `queue_id` |
| `ServerQueueIdObserved` | Server has seen evidence the client received its `queue_id` |
| `Finished` | Both sides have confirmed each other's `queue_id`; steady state |

Early packets include a `source_queue_id` field so the peer can update its routing table.
Once both sides have observed the other's queue ID the field is omitted to save space.

### Client: opening a stream

1. **Credential lookup** — `endpoint::open_stream`
   ([`stream/endpoint.rs`](src/stream/endpoint.rs)) looks up the peer in the
   `path::secret::Map`. If no entry exists, a new QUIC handshake is triggered first.
2. **Rate limiting** — `entry.next_connection_time()` enforces a per-peer connection rate
   limit using a single atomic compare-and-swap; no global lock is taken.
3. **Builder construction** — A `stream::application::Builder`
   ([`stream/application.rs`](src/stream/application.rs)) is assembled. It holds:
   - a `send::application::Builder` (outbound half)
   - a `recv::application::Builder` (inbound half)
   - an `ArcShared` containing credentials, clocks, sockets, and wakers
4. **Worker spawning** — Calling `Builder::build()` spawns a send worker and a receive
   worker as async tasks on the environment's runtime
   ([`stream/environment.rs`](src/stream/environment.rs)).
5. **Stream returned** — A `Stream` is returned to the caller, providing an async `Reader`
   and `Writer`.

The first outgoing packet uses `queue_id = 0` as a sentinel, telling the server to assign
a real queue ID. Subsequent packets use the queue ID learned from the server's first reply.

### Server: accepting a stream

1. **Socket receive loop** — A per-socket task reads UDP datagrams continuously.
2. **Decrypt and route** — The recv dispatch layer
   ([`stream/recv/dispatch.rs`](src/stream/recv/dispatch.rs)) decrypts each packet using
   the credential from the packet header and routes it to the appropriate per-stream channel
   by `queue_id`.
3. **New stream detection** — If a packet arrives with `queue_id = 0` (the sentinel for a
   new stream), the server handshake map
   ([`stream/server/handshake.rs`](src/stream/server/handshake.rs)) allocates a fresh
   dispatch channel and enqueues the stream into the acceptor.
4. **Acceptor** — The `stream::server::accept::Acceptor`
   ([`stream/server/accept.rs`](src/stream/server/accept.rs)) dequeues streams and applies
   back-pressure (maximum sojourn time, queue capacity). Stale or errored streams are pruned
   before being handed to the application.
5. **Application `accept()`** — The application calls `server.accept()` to receive a
   `Stream` (same `Reader`/`Writer` type as the client side).

---

## 3. End-to-End Data Flow

### Send path (application → network)

```
Application
  │  writes bytes into Writer (async)
  ▼
send::application::State          (stream/send/application/)
  │  segments data, applies flow-control window,
  │  stores in a lock-free send queue
  ▼
send::worker::Worker              (stream/send/worker.rs)
  │  polls the queue, builds packet headers, calls crypto seal
  │  (path/secret/schedule → crypto/awslc)
  │  hands sealed packets to the socket
  ▼
socket::Socket                    (stream/socket.rs)
  │  writes UDP datagrams; uses GSO/sendmmsg where available
  ▼
Network
```

**Key contracts:**

- The `Writer` interacts with `send::application::State` through a shared, lock-free queue
  ([`stream/send/queue.rs`](src/stream/send/queue.rs)) and an `AtomicWaker` so that the
  writer future and the send worker wake each other without a mutex.
- The send worker tracks outstanding packet numbers and handles retransmissions via the
  transmission state machine ([`stream/send/state.rs`](src/stream/send/state.rs)).
- Flow control credits are managed in `send::flow`
  ([`stream/send/flow.rs`](src/stream/send/flow.rs)); the worker only transmits while
  credits are available and parks itself otherwise.
- Keep-alive PINGs are generated by the send worker when the stream is idle and the
  `keep_alive` option is enabled ([`stream/send/state/keep_alive.rs`](src/stream/send/state/keep_alive.rs)).

### Receive path (network → application)

```
Network
  │  UDP datagrams arrive at socket
  ▼
recv::dispatch::Dispatch          (stream/recv/dispatch.rs)
  │  authenticates + decrypts each packet
  │  (path/secret/receiver — replay check, then crypto open)
  │  routes to the per-stream channel by queue_id
  ▼
recv::worker::Worker              (stream/recv/worker.rs)
  │  reassembles out-of-order packets via recv::buffer::Buffer
  │  (stream/recv/buffer.rs)
  │  delivers in-order data to the application
  │  tracks receive state machine (stream/recv/state.rs)
  │  sends ACK control packets back to the sender
  ▼
recv::application::Reader         (stream/recv/application.rs)
  │  application reads bytes (async)
  ▼
Application
```

**Key contracts:**

- The `Dispatch` and each stream's receive worker communicate via a bounded channel from the
  lock-free `sync::mpsc` ([`sync/mpsc.rs`](src/sync/mpsc.rs)) / `sync::ring_deque`
  ([`sync/ring_deque.rs`](src/sync/ring_deque.rs)) family. Back-pressure is inherent: if
  the per-stream channel is full, new packets for that stream are dropped and the sender's
  congestion control reacts.
- The receive worker sends ACK packets through the same `socket::Socket` abstraction used
  by the send worker; ACKs are plain `packet::control` datagrams
  ([`packet/control.rs`](src/packet/control.rs)) authenticated with HMAC.
- The receive state machine ([`stream/recv/state.rs`](src/stream/recv/state.rs)) drives an
  idle timer; ACKs are only generated when that timer fires or new data arrives, preventing
  ACK storms.

### Control packets

Two categories of out-of-band packets exist alongside stream data:

| Category | Packet types | Purpose |
|---|---|---|
| **Stream control** | `packet::control` | ACKs, flow-credit updates from receiver to sender |
| **Secret control** | `packet::secret_control` | `UnknownPathSecret`, `StaleKey`, `ReplayDetected`, `FlowReset` — credential resynchronization signals |

Both categories are authenticated (HMAC or AEAD) and are small enough to fit in a single
datagram. They travel on the same UDP socket as data packets and are demultiplexed by their
tag byte before any per-stream routing.

---

## Component Interaction Summary

```
┌──────────────────────────────────────┐
│            Application               │
│  Writer ──────────────── Reader      │
└──────┬───────────────────────┬───────┘
       │                       │
┌──────▼──────┐         ┌──────▼──────┐
│ send worker │         │ recv worker │
│  (per stream)│        │  (per stream)│
└──────┬──────┘         └──────▲──────┘
       │  packet::stream        │ packet::stream
       │  (sealed)              │ (opened)
┌──────▼────────────────────────┴──────┐
│          socket::Socket              │
│       (UDP, GSO, sendmmsg)           │
└──────┬────────────────────────▲──────┘
       │  datagrams             │ datagrams
       │          Network       │
       └────────────────────────┘

Credential / key material flows through:
  path::secret::Map  →  credentials::Credentials  →  packet headers
  (one map entry per peer, shared by all streams to that peer)
```

The design keeps the data plane lock-free and the credential plane nearly so. Streams are
created and destroyed rapidly; the per-peer map entry is the only long-lived state.
