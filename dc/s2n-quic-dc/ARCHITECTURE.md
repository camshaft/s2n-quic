# s2n-quic-dc Architecture

`s2n-quic-dc` is a transport for data center networks. It reuses the QUIC handshake to
derive strongly authenticated, long-lived shared secrets between peers, then uses those
secrets to protect individual short-lived streams without requiring any per-stream
cryptographic negotiation or centralized state.

---

## 1. Handshake and Credential Establishment

The handshake runs once per peer pair using a standard TLS 1.3 QUIC connection. Both
sides export a shared secret from the TLS session using the TLS Exporter (RFC 5705,
label `"EXPERIMENTAL EXPORTER s2n-quic-dc"`), then store that secret in a local map.
Every stream opened to that peer thereafter derives its encryption material directly from
the cached secret — no additional handshake is required.

`HandshakingPath` ([`path/secret/map/handshake.rs`](src/path/secret/map/handshake.rs))
implements the `dc::Path` trait from `s2n_quic_core`. After the QUIC handshake completes,
it exports the shared `schedule::Secret` and stores it in a `path/secret/map/entry::Entry`
inside the `path::secret::Map` ([`path/secret/map.rs`](src/path/secret/map.rs)).

### Per-stream key derivation

Every stream packet carries a `Credentials` value
([`credentials.rs`](src/credentials.rs)) consisting of a 16-byte `Id` that identifies the
shared map entry and a monotonically incrementing `KeyId`. The actual AEAD key for a given
`KeyId` is derived on the fly from the base `schedule::Secret`
([`path/secret/schedule.rs`](src/path/secret/schedule.rs)) — no keys are pre-generated and
none are stored beyond the base secret.

The sender claims the next `KeyId` by atomically incrementing a single counter in
`path/secret/sender::State` ([`path/secret/sender.rs`](src/path/secret/sender.rs)). No
locks are taken when opening a new stream; there is nothing to coordinate with other
streams to the same peer. This is the primary reason that the scheme scales: a single
`AtomicU64` does all the work that a per-stream key negotiation would otherwise require.

The receiver maintains a sliding bitmap window (65,535 bits) in
`path/secret/receiver::State` ([`path/secret/receiver.rs`](src/path/secret/receiver.rs))
to detect replayed `KeyId` values. The window is protected by a narrow `Mutex` only during
bit-level updates; reading the maximum-seen key ID is purely atomic.

Because the map entry is shared by every stream to the same peer, the scheme has one fixed
cost per peer relationship rather than one per stream. The entry's lifetime is managed by a
periodic cleaner task ([`path/secret/map/cleaner.rs`](src/path/secret/map/cleaner.rs)) that
evicts stale entries and triggers re-handshakes to rotate the base secret when necessary.

### Out-of-band credential signals

When a receiver cannot authenticate a packet — because the credential ID is unknown,
the `KeyId` is outside the current replay window, or a replay is positively detected — it
sends a small, authenticated `packet::secret_control` datagram back to the sender
([`packet/secret_control.rs`](src/packet/secret_control.rs)).

There are four signal types:

- `UnknownPathSecret` — the receiver does not recognise the credential ID; the sender
  should trigger a fresh handshake before retrying.
- `StaleKey` — the `KeyId` is so far behind the receiver's window that it cannot determine
  whether the packet was replayed; the sender should move to a newer key.
- `ReplayDetected` — the receiver has definitively seen this `KeyId` before; the packet is
  dropped and the sender is notified so it can take corrective action.
- `FlowReset` — used to reset the state of a stream when one side detects an inconsistency
  or hard error. Carries a `queue_id` so it can be routed to the right stream.

These signals allow the two sides to resynchronize quickly without a full re-handshake in
most cases.

---

## 2. Stream Lifecycle

A stream is a bidirectional, reliable, ordered byte channel. Each stream is associated with
one `Credentials` value (and hence one peer map entry). The underlying transport can be UDP
or TCP: the socket abstraction ([`stream/socket.rs`](src/stream/socket.rs)) reports its
capabilities via `TransportFeatures`, and the implementation fills in whatever reliability
and ordering guarantees the application layer requires that the socket does not already
provide. On UDP that means the dc layer handles ACKs, retransmission, and flow control. On
TCP those are provided by the kernel and the dc layer skips them.

### Queue-ID observation protocol

Every stream endpoint has a local *queue ID* that it publishes in the `source_queue_id`
field of outgoing packets. The peer uses this value to direct future packets to the right
per-stream receive queue. Before each side has observed the other's queue ID, packets
include this field so the peer can learn it. The state machine that tracks this exchange
lives in [`stream/shared/handshake.rs`](src/stream/shared/handshake.rs):

| State | Meaning |
|---|---|
| `ClientInit` | Client has chosen its own `queue_id`; server's is not yet known |
| `ClientQueueIdObserved` | Client has seen at least one packet from the server and learned its `queue_id` |
| `ServerInit` | Server has seen the client's first packet and chosen its own `queue_id` |
| `ServerQueueIdObserved` | Server has received evidence that the client has seen its `queue_id` |
| `Finished` | Both sides are fully aware of each other's `queue_id`; steady state |

The `source_queue_id` field is always included in packets — even in the `Finished` state —
because omitting it breaks the `FlowReset` flow that needs the field for routing.

### Client: opening a stream

Opening a stream begins in `endpoint::open_stream`
([`stream/endpoint.rs`](src/stream/endpoint.rs)), which looks up the peer in the
`path::secret::Map`. If no entry exists, a QUIC handshake is triggered first and the caller
waits for it to complete. Once credentials are available, a per-peer rate limit is enforced
with a single atomic compare-and-swap on `entry.next_connection_time()`; no global lock is
taken.

The function then assembles a `stream::application::Builder`
([`stream/application.rs`](src/stream/application.rs)) that pairs a send-side and
receive-side builder with an `ArcShared` value containing the credentials, clock, sockets,
and wakers that the two halves share. Calling `Builder::build()` spawns a send worker and a
receive worker as async tasks on the environment's runtime
([`stream/environment.rs`](src/stream/environment.rs)) and returns a `Stream` to the
caller.

The client's first outgoing packet uses `queue_id = 0` in the destination field, which
tells the server-side acceptor that this is a new stream. The protocol allows several
packets to be sent before the client observes the server's chosen `queue_id`; until then
the client continues using `0` as the destination while advertising its own `queue_id` in
the `source_queue_id` field.

### Server: accepting a stream

The server runs a receive loop on a shared acceptor socket. On UDP, the acceptor
([`stream/server/udp.rs`](src/stream/server/udp.rs)) reads raw packets and first checks
whether the credentials in the packet header are already associated with an active stream. If
so, the packet is forwarded directly to that stream's receive queue by queue ID. If the
credentials are new, the acceptor allocates a fresh pair of dispatch channels
([`stream/recv/dispatch.rs`](src/stream/recv/dispatch.rs)), injects the packet, and calls
`endpoint::open_stream` to open the server side of the stream. On TCP, incoming connections
are accepted at the TLS/QUIC level and each connection corresponds to one stream.

In both cases the resulting `stream::application::Builder` is enqueued in the accept queue.
The `stream::server::accept::Acceptor`
([`stream/server/accept.rs`](src/stream/server/accept.rs)) dequeues builders, applies
back-pressure via a configurable maximum sojourn time and queue capacity, and prunes stale
or already-errored streams before the application calls `server.accept()` and receives the
same `Stream` type used by the client.

---

## 3. End-to-End Data Flow

### Send path (application → network)

The application calls `Writer::write` (or the `AsyncWrite` adapter), which calls into
`send::application::Writer` ([`stream/send/application.rs`](src/stream/send/application.rs)).
The writer first acquires flow credits by polling `send::flow`
([`stream/send/flow.rs`](src/stream/send/flow.rs)), which gates transmission on the
receiver's advertised window. When credits are granted, the writer seals each packet
in-place using the AEAD key derived for the current `KeyId`, then pushes the sealed
descriptor into a per-stream `Queue` ([`stream/send/queue.rs`](src/stream/send/queue.rs)).

The `Queue` is backed by a hierarchical timing wheel
([`stream/send/state/transmission.rs`](src/stream/send/state/transmission.rs)) that
schedules each segment for transmission at a specific time, enabling pacing. A socket worker
drains the wheel and writes the segments to the network via the `socket::Socket` abstraction
([`stream/socket.rs`](src/stream/socket.rs)), which uses GSO/sendmmsg on Linux when
available. After each segment is physically sent, the wheel posts a completion entry back
through a lock-free completion queue. The send worker
([`stream/send/worker.rs`](src/stream/send/worker.rs)) drains this completion queue via
`State::load_completion_queue`, recording the actual transmission time for RTT estimation
and tracking which packets need to be acknowledged or retransmitted. Keep-alive PING frames
are generated by the send worker when the stream is idle and the `keep_alive` option is
enabled ([`stream/send/state/keep_alive.rs`](src/stream/send/state/keep_alive.rs)).

### Receive path (network → application)

On the receive side the design distinguishes between the *happy path*, where the application
is reading fast enough to keep up with the socket, and the *fallback path*, where the
receive worker takes over.

In the happy path the application task calls `Reader::read`
([`stream/recv/application.rs`](src/stream/recv/application.rs)). This reads raw bytes
directly from the socket into a receive buffer, then drives the packet parser and AEAD
opener inline via `PacketDispatch::on_packet` in
[`stream/recv/shared.rs`](src/stream/recv/shared.rs). Decrypted, validated packet payloads
are fed into the reassembly buffer and delivered to the caller in stream order. Because the
application does the work directly, no inter-thread handoff is needed in steady state.

When the application is not reading quickly enough, the receive worker
([`stream/recv/worker.rs`](src/stream/recv/worker.rs)) detects the lag by observing whether
the application's read epoch is advancing. If not, the worker acquires the shared receiver
lock and drains the socket on behalf of the application, decrypting and reassembling packets
so they are ready when the application eventually calls `read`. The worker is also solely
responsible for sending ACK control packets back to the sender, regardless of which path
processed the data.

The endpoint-level dispatch ([`stream/recv/dispatch.rs`](src/stream/recv/dispatch.rs)) is a
shared component that sits upstream of all per-stream receive queues. It is intentionally
kept as thin as possible: it reads tag bytes and queue IDs from the raw datagram, looks up
the per-stream channel, and forwards the encrypted buffer without touching the payload. Any
decryption or authentication happens later, either in the application thread or in the per-
stream worker. If no channel is registered for a given queue ID the packet is forwarded to
an unroutable queue for handling by the acceptor or the secret-control layer.

### Control packets

Two categories of small, authenticated packets travel alongside stream data:

`packet::control` ([`packet/control.rs`](src/packet/control.rs)) carries ACKs and flow
credit updates from the receiver to the sender. These are authenticated with HMAC, small
enough to always fit in a single datagram, and sent by the receive worker after processing
each epoch of data packets.

`packet::secret_control` ([`packet/secret_control.rs`](src/packet/secret_control.rs))
carries the four credential-resynchronization signals described in section 1. These are
authenticated with AEAD using the map entry's control key and are handled by
`path::secret::Map` before any per-stream routing occurs.

Both packet types share the same UDP socket as stream data and are demultiplexed by their
leading tag byte.

---

## Component Interaction Summary

```text
┌────────────────────────────────────────────────────────────┐
│                        Application                         │
│   Writer (send/application.rs)    Reader (recv/application.rs)
└──────────────┬──────────────────────────────┬─────────────┘
               │ flow credits + sealed packets │ decrypted bytes
               │                               │
    ┌──────────▼──────────┐       ┌────────────▼────────────┐
    │    send::Worker     │       │      recv::Worker        │
    │  (send/worker.rs)   │       │   (recv/worker.rs)       │
    │                     │       │                          │
    │ drains completion   │       │ fallback: drains socket  │
    │ queue; tracks ACKs  │       │ when app reads slowly;   │
    │ and retransmission  │       │ always sends ACKs        │
    └──────────┬──────────┘       └────────────▲────────────┘
               │ sealed segments               │ encrypted descriptors
               │                               │
    ┌──────────▼───────────────────────────────┴────────────┐
    │                   socket::Socket                       │
    │            (stream/socket.rs — UDP or TCP)             │
    │  send: timing wheel → socket worker → completion queue │
    │  recv: dispatch (recv/dispatch.rs) routes by queue_id  │
    └──────────┬────────────────────────────────────────────┘
               │                               │
            Network ──────────────────────► Network
```

Credential and key material flow through:

```text
path::secret::Map  ──►  credentials::Credentials  ──►  every packet header
```

One map entry exists per peer and is shared by every stream to that peer. Streams
themselves are ephemeral; the map entry is the only long-lived per-peer state.
