# UDP Send Path Improvements

This document outlines the refactoring work needed to improve the UDP send path for streams by integrating the timing wheel, descriptor pool, and flow control mechanisms.

## Overview

The current implementation uses a direct queue-to-socket model where the application layer buffers packets in a VecDeque and flushes them directly to the socket. The refactored implementation will use a wheel-based pacing system where transmissions are scheduled with timestamps, sent by a dedicated socket worker, and completed asynchronously with feedback to the application layer for flow control.

**Architecture Change:**

- **Before:** Application → Queue (VecDeque) → Direct Socket Send
- **After:** Application → Wheel (Scheduled by Timestamp) → Socket Worker → Completion Queue → Flow Credit Release → Application

---

## Work Item 1: Pool Integration in Environment Layer

### Current Implementation

The environment setup code in `dc/s2n-quic-dc/src/stream/environment/udp.rs` creates sockets and channels but does not manage a descriptor pool. The `Pooled` struct contains socket references and channels but no pool reference.

**Relevant Files:**

- `dc/s2n-quic-dc/src/stream/environment/udp.rs`
- `dc/s2n-quic-dc/src/stream/environment/tokio/pool.rs`
- `dc/s2n-quic-dc/src/stream/environment/bach/pool.rs`

### Work Required

Add a `Pool` instance to the environment layer that will be shared across all streams created from that environment. The pool should be created during environment initialization and passed through to stream creation.

1. Add a `pool` field to the `Pooled` struct that holds an `Arc<Pool>`.
2. Create the pool during environment setup using the existing `max_packet_size` and `packet_count` configuration values from `Config`.
3. Pass the pool reference through the `setup()` method so it's available when creating stream state.
4. The pool should be shared (Arc-wrapped) since multiple streams will allocate from it concurrently.

**Reasoning:**
The pool manages reusable descriptor memory for all packet allocations. By placing it in the environment layer, we ensure:

- A single pool per socket/environment, reducing memory fragmentation
- Shared pool across all streams on that socket
- Proper lifecycle management tied to the environment

---

## Work Item 2: Stream Completion Queue Setup

### Current Implementation

The `stream/send/queue.rs` file manages a `VecDeque<Segment>` that holds packets waiting to be sent. There is no completion mechanism - once packets are flushed to the socket, they're forgotten about.

**Relevant Files:**

- `dc/s2n-quic-dc/src/stream/send/queue.rs`
- `dc/s2n-quic-dc/src/stream/send/state.rs`
- `dc/s2n-quic-dc/src/stream/send/worker.rs`

### Work Required

Each stream needs its own completion queue that receives transmissions after the socket worker has sent them. This queue will be used for:

1. Updating RTT estimates
2. Detecting loss
3. Releasing flow credits back to the application
4. Managing retransmissions

Implementation steps:

1. Create an `Arc<sync::intrusive_queue::Queue<Transmission<Vec<Info>>>>` in the stream's sender state during initialization.
2. When creating transmission entries for the wheel, downgrade this Arc to a `Weak` reference and attach it to the `Transmission.completion` field.
3. The wheel's socket worker will use this weak reference to push completed transmissions back to the stream's queue.
4. The stream's worker will poll this completion queue and process each completed transmission.

**Reasoning:**
Per-stream completion queues are necessary because:

- Each stream needs independent RTT/loss tracking
- Flow control is per-stream
- The weak reference pattern prevents memory leaks if a stream is destroyed while transmissions are in-flight
- The intrusive queue pattern avoids allocations and provides lock-free operations

---

## Work Item 3: Replace Queue with Wheel-Based Transmission in stream/send/queue.rs

### Current Implementation

The `Queue` struct in `dc/s2n-quic-dc/src/stream/send/queue.rs` manages a `VecDeque<Segment>` and directly flushes packets to the socket via `poll_flush_segments`. The `Segment` struct wraps a `descriptor::Filled` with ECN and offset information. The queue accepts packets from the application via `push_buffer` and flushes them synchronously.

**Relevant Files:**

- `dc/s2n-quic-dc/src/stream/send/queue.rs`
- `dc/s2n-quic-dc/src/socket/send/wheel.rs`
- `dc/s2n-quic-dc/src/socket/send/udp.rs`

### Work Required

Transform the queue from a buffering mechanism into a wheel insertion mechanism:

1. **Remove VecDeque buffering:** Delete the `segments: VecDeque<Segment>` field and all associated queue management code.

2. **Remove direct socket flushing:** Delete `poll_flush_segments`, `poll_flush_segments_stream`, `poll_flush_segments_datagram`, and `consume_segments` methods. The socket worker will handle all actual transmission.

3. **Transform Message::push:** Instead of pushing segments onto a VecDeque, the `push` method should:

   - Allocate a descriptor from the pool
   - Fill it with packet data
   - Create a `Transmission` struct containing the filled descriptor and metadata
   - Wrap it in a wheel `Entry`
   - Calculate the transmission timestamp based on the current time and CCA pacing rate
   - Insert the entry into the wheel with that timestamp

4. **Update accepted_len tracking:** The `accepted_len` field was used to ensure all packets were flushed before returning to the application. This mechanism needs to be replaced with flow control based on pending transmission counts (covered in a separate work item).

5. **Maintain transmission metadata:** When pushing to the wheel, create a `Vec<transmission::Info<descriptor::Filled>>` that contains metadata for each QUIC packet within the GSO transmission. This vector will be attached to the `Transmission` struct and returned via the completion queue.

**Reasoning:**
The wheel provides:

- Natural pacing based on timestamps rather than artificial yielding
- Separation of concerns between packet creation (application) and transmission (socket worker)
- Priority-based scheduling if using multiple wheels
- Integration with token bucket rate limiting

By removing direct socket interaction, the application layer becomes purely concerned with packet construction while the socket layer handles timing and transmission.

---

## Work Item 4: Refactor msg/send.rs to Use Descriptor Pool

### Current Implementation

The `Message` struct in `dc/s2n-quic-dc/src/msg/send.rs` implements a custom allocator using `Vec<Vec<u8>>` for buffer storage. It manages its own free list with `free` and `pending_free` vectors, handling reference counting manually. The `Segment` and `Retransmission` types are indices into the buffer array. Packets are sent directly via `sendmsg` syscalls.

**Relevant Files:**

- `dc/s2n-quic-dc/src/msg/send.rs`
- `dc/s2n-quic-dc/src/socket/pool/descriptor.rs`
- `dc/s2n-quic-dc/src/allocator.rs`

### Work Required

Replace the custom allocator with pool-based descriptor allocation:

1. **Replace buffer storage:**

   - Remove the `buffers: Vec<Vec<u8>>` field
   - Remove the `free` and `pending_free` vectors
   - Add a reference to the `Pool`

2. **Update Segment type:**

   - Change `Segment` from an index-based handle to wrap `descriptor::Unfilled` during filling and `descriptor::Filled` after filling
   - The descriptor types already handle their own reference counting and lifecycle

3. **Update Retransmission type:**

   - Change `Retransmission` to wrap `descriptor::Filled` directly
   - When `retransmit_copy` is called, allocate a new descriptor from the pool and copy the payload from the original
   - This simplifies the design at the cost of always copying on retransmission, which is acceptable

4. **Remove direct sending:**

   - Remove the `send()` and `send_with()` methods that directly call `sendmsg`
   - Add a method like `send_transmission()` that creates wheel entries and inserts them into the wheel instead
   - Remove the `on_transmit` method since partial sends won't occur with the wheel

5. **Update push methods:**

   - `push` should wrap the filled descriptor and prepare it for wheel insertion
   - `push_with_retransmission` should return a handle that wraps the filled descriptor for potential later retransmission

6. **Simplify force_clear:**
   - The pool handles descriptor lifecycle, so cleanup becomes simpler
   - Just clear the payload vector and any wheel-related state

**Reasoning:**
Using the descriptor pool provides:

- Unified memory management across the send and receive paths
- Proper handling of GSO/GRO with segment metadata
- Reference counting built into the descriptor type
- Integration with completion queues via weak references

The always-copy-on-retransmit strategy is simpler than trying to implement sharing of descriptor regions. While it's less efficient for retransmissions, retransmissions should be rare in healthy network conditions, and the simplicity benefit outweighs the performance cost.

---

## Work Item 5: Remove Pacer and Implement Flow-Based Backpressure

### Current Implementation

The `stream/pacer.rs` file implements a naive pacer that yields every 5 transmissions with a 1ms window. This doesn't account for actual network capacity or congestion control. The flow controller in `stream/send/flow.rs` only manages stream offset credits, not transmission capacity.

**Relevant Files:**

- `dc/s2n-quic-dc/src/stream/pacer.rs` (to be deleted)
- `dc/s2n-quic-dc/src/stream/send/flow.rs`
- `dc/s2n-quic-dc/src/stream/send/worker.rs`

### Work Required

**Part A: Delete the Pacer**

1. Delete the entire `dc/s2n-quic-dc/src/stream/pacer.rs` file.
2. Remove the `pacer` field from the worker in `stream/send/worker.rs`.
3. Remove all `poll_pacing` calls from the worker's transmit logic.

**Part B: Add Transmission Tracking to Flow Controller**

1. Add a `pending_transmissions: AtomicUsize` field to track how many transmissions are in-flight at the socket layer.
2. Add a `max_in_flight_transmissions: usize` configuration parameter (default to 16-32).
3. When the application requests flow credits, check if `pending_transmissions < max_in_flight_transmissions`.
4. If the limit is reached, return `Poll::Pending` and register a waker to be notified when transmissions complete.

**Part C: Process Completions in Worker**

1. In the worker's poll loop, add a step to poll the completion queue.
2. For each completed transmission:

   - Process the `Vec<Info>` to update RTT estimates, loss detection, and recovery state
   - Decrement `pending_transmissions` by the number of packets in that transmission
   - Wake any tasks waiting for flow credits

3. Ensure completions are processed before checking if more transmissions can be created.

**Reasoning:**
Real pacing should be handled by the wheel's timestamp-based scheduling, driven by the CCA's bandwidth estimate. The application layer should be concerned with:

- Stream offset flow control (how much stream data can be in-flight)
- Transmission capacity flow control (how many packets can be in-flight)

By tracking pending transmissions and blocking when the socket queue is full, we create natural backpressure that prevents the application from overwhelming the socket layer. The completion queue provides the feedback loop needed to release this backpressure.

This approach maintains a small, bounded number of transmissions in-flight, which:

- Reduces memory usage
- Provides accurate RTT measurements
- Enables quick reaction to congestion
- Prevents head-of-line blocking

---

## Work Item 6: Update Worker Transmission Logic

### Current Implementation

The worker in `dc/s2n-quic-dc/src/stream/send/worker.rs` calls `poll_transmit_flush` which directly sends packets from a transmit queue to the socket using `poll_send`. It uses the pacer to yield between transmissions. The worker manages both the application queue and the transmit queue.

**Relevant Files:**

- `dc/s2n-quic-dc/src/stream/send/worker.rs`
- `dc/s2n-quic-dc/src/stream/send/state.rs`

### Work Required

1. **Remove application queue flushing:**

   - The worker currently flushes `application_queue` in the `Detached` state
   - This logic should be removed since applications now insert directly into the wheel

2. **Replace transmit queue logic:**

   - Instead of calling `poll_send` on the socket, transmit queue items should be converted into wheel entries
   - The state's `fill_transmit_queue` method should create these wheel entries and insert them into the wheel
   - Remove the `poll_transmit_flush` method entirely

3. **Add completion processing:**

   - Add a new poll step that processes the stream's completion queue
   - For each completed transmission, extract the `Vec<Info>` and call appropriate handler methods on the state
   - Update flow credits after processing completions

4. **Pass pool reference:**

   - The worker needs access to the pool for retransmissions and control packet generation
   - Add a pool reference to the worker during construction
   - Pass this pool to `fill_transmit_queue` and other methods that need to allocate descriptors

5. **Update snapshot and apply logic:**
   - The `Snapshot::apply` method releases flow credits based on changes
   - This needs to account for the new `pending_transmissions` counter
   - Only release credits when transmissions complete, not immediately when queued

**Reasoning:**
The worker's role changes from "send packets to socket" to "coordinate between application, wheel, and completion processing". It becomes more of an orchestrator:

- It tells the state to create wheel entries (not transmit directly)
- It processes completions to provide feedback
- It manages the lifecycle between different waiting states

This separation allows the socket worker to handle actual transmission timing independently of the stream's logic.

---

## Work Item 7: Update State's Transmission Creation

### Current Implementation

The state in `dc/s2n-quic-dc/src/stream/send/state.rs` has a `fill_transmit_queue` method that creates transmission metadata and stores buffers using the old allocator pattern. It manages retransmissions by storing buffer indices. Several `todo!()` calls are present where pool allocation should occur.

**Relevant Files:**

- `dc/s2n-quic-dc/src/stream/send/state.rs`
- `dc/s2n-quic-dc/src/stream/send/state/transmission.rs`

### Work Required

1. **Update transmission creation:**

   - Replace `todo!()` calls with actual pool allocation
   - Allocate `descriptor::Unfilled` from the pool
   - Fill descriptors with packet data (headers + payload)
   - Convert to `descriptor::Filled` and wrap in `descriptor::Segments`

2. **Update retransmission storage:**

   - Instead of storing buffer indices, store `Vec<transmission::Info<descriptor::Filled>>`
   - Each retransmission info contains a handle to the filled descriptor
   - When retransmitting, use the allocator's `retransmit_copy` to create a new descriptor

3. **Create wheel entries:**

   - Instead of pushing to a transmit queue, create `Transmission<Vec<Info>>` structs
   - Calculate pacing timestamp from current time + interval derived from CCA bandwidth
   - Create wheel `Entry` and insert into the wheel
   - Attach the stream's completion queue weak reference

4. **Handle GSO packets:**

   - A single transmission may contain multiple QUIC packets (GSO)
   - Create a `Vec<transmission::Info<descriptor::Filled>>` with one entry per packet
   - Store metadata like stream offset, payload length, FIN flag, ECN for each packet
   - This vector becomes the `info` field of the `Transmission`

5. **Update recovery logic:**
   - When a packet is marked lost, the recovery state needs to retransmit
   - Use the stored `transmission::Info<descriptor::Filled>` to access the original packet
   - Call `retransmit_copy` to create a new descriptor with the same payload
   - Create a new wheel entry for the retransmission

**Reasoning:**
The state layer is responsible for:

- Deciding what packets need to be sent (new data, retransmissions, control packets)
- Creating those packets with proper headers and encryption
- Providing metadata for RTT/loss tracking

By having it create wheel entries rather than directly transmittable packets, we maintain this separation of concerns. The state knows what needs to be sent and when (relative time), but doesn't directly interact with the socket.

The `Vec<Info>` per transmission is critical for GSO support where a single syscall sends multiple packets. Each packet needs independent tracking for loss detection and RTT measurement.

---

## Work Item 8: Integrate CCA Pacing with Wheel

### Current Implementation

The congestion controller in the state calculates bandwidth and maintains transmission windows, but this isn't currently used for pacing. The naive pacer just yields periodically without considering actual bandwidth.

**Relevant Files:**

- `dc/s2n-quic-dc/src/stream/send/state.rs` (CCA integration)
- `dc/s2n-quic-dc/src/stream/send/queue.rs` (where pacing delay is calculated)
- `quic/s2n-quic-core/src/recovery/bandwidth.rs`

### Work Required

1. **Extract bandwidth from CCA:**

   - The state's CCA (congestion control algorithm) maintains a bandwidth estimate
   - Before creating a wheel entry, query the CCA for current bandwidth
   - Calculate the pacing interval: `interval = packet_size / bandwidth`

2. **Calculate transmission timestamp:**

   - Get current time from the clock
   - Add the pacing interval: `transmission_time = now + interval`
   - Pass this timestamp to `wheel.insert(entry, transmission_time)`

3. **Handle burst allowance:**

   - The CCA may allow bursts of multiple packets
   - For the first packet in a burst, use current time
   - For subsequent packets, spread them over the interval
   - Example: if burst size is 10, packet N gets `now + (interval * N / 10)`

4. **Respect minimum pacing interval:**

   - Even at very high bandwidths, maintain some minimum interval (e.g., 1 microsecond)
   - This prevents all packets from getting the same timestamp

5. **Handle rate changes:**
   - If the CCA adjusts its rate (due to ACKs or loss), newly created transmissions will automatically get new timestamps
   - In-flight transmissions keep their original timestamps (the wheel doesn't support rescheduling)

**Reasoning:**
Pacing prevents bursts that can cause buffer bloat and self-induced congestion. By integrating with the CCA:

- Pacing rate automatically adjusts to network conditions
- Rate increases after ACKs and decreases after loss
- Bandwidth utilization is maximized while avoiding congestion

The wheel's timestamp-based scheduling naturally implements pacing without additional yielding or timers. Each packet gets scheduled at a specific time determined by the CCA, and the socket worker ensures packets are sent at those times.

---

## Work Item 9: Configure Transmission Limits

### Current Implementation

The environment configuration in `dc/s2n-quic-dc/src/stream/environment/udp.rs` includes parameters for wheel horizon and max bitrate, but doesn't include transmission limit configuration.

**Relevant Files:**

- `dc/s2n-quic-dc/src/stream/environment/udp.rs`
- `dc/s2n-quic-dc/src/stream/send/flow.rs`

### Work Required

1. **Add configuration parameter:**

   - Add `max_in_flight_transmissions: usize` to the `Config` struct
   - Default to a reasonable value like 16 or 32
   - This should be tunable for different deployment scenarios

2. **Pass to flow controller:**

   - When creating the flow controller during stream setup, pass this limit
   - Store it in the flow controller's state
   - Use it when checking whether to grant flow credits

3. **Consider RTT scaling:**

   - Optionally, make this limit dynamic based on RTT
   - Higher RTT connections might benefit from more in-flight transmissions
   - Lower RTT connections might need fewer to maintain responsiveness
   - This could be a future enhancement after the basic implementation

4. **Document trade-offs:**
   - Smaller limits: lower memory usage, faster reaction to congestion, more application blocking
   - Larger limits: higher throughput on high-BDP links, more memory usage, slower reaction
   - The default should balance these concerns for typical internet connections

**Reasoning:**
A configurable limit allows deployment-specific tuning. For example:

- Data center environments with low RTT might use 8-16
- High-bandwidth satellite links might use 64-128
- General internet might use 16-32

The limit prevents unbounded growth of the transmission queue while still maintaining enough in-flight data to keep the pipe full. It's a critical parameter for the flow control system.

---

## Work Item 10: Handle Retransmissions with Copying

### Current Implementation

Retransmissions currently use `Arc::make_mut` to potentially share the underlying buffer if no other references exist, or copy if references remain. This works with the `Vec<u8>` buffer model.

**Relevant Files:**

- `dc/s2n-quic-dc/src/msg/send.rs` (Allocator trait implementation)
- `dc/s2n-quic-dc/src/stream/send/state.rs` (retransmission logic)

### Work Required

1. **Always copy on retransmission:**

   - When `retransmit_copy` is called, always allocate a new descriptor from the pool
   - Copy the payload from the original `descriptor::Filled` to the new descriptor
   - This is simpler than trying to implement sharing semantics

2. **Implementation details:**

   - Get the original descriptor from `transmission::Info<descriptor::Filled>`
   - Call `pool.alloc_or_grow()` to get a new `Unfilled` descriptor
   - Use `fill_with` to copy data: read from original's `payload()`, write to new buffer
   - Set the same remote address and metadata
   - Return the new `Filled` descriptor

3. **Handle GSO retransmissions:**

   - When retransmitting a lost packet from a GSO burst, only retransmit that specific packet
   - Don't retransmit the entire GSO burst
   - Create a single-packet transmission for the retransmission

4. **Memory considerations:**
   - Copying increases memory usage during retransmissions
   - However, retransmissions should be rare (<1% in healthy networks)
   - The pool will reuse descriptors after transmission completes
   - Monitor pool exhaustion and consider increasing pool size if retransmission-heavy workloads are common

**Reasoning:**
The alternative of sharing descriptor regions is complex because:

- The `descriptor::Filled` type uses reference counting for the entire descriptor
- Splitting it into shareable sub-regions would require a new layer of indirection
- The GSO sending code in `stream/socket/fd/udp.rs` assumes each iovec entry is an independent segment
- Sharing would break this assumption and require significant refactoring

By always copying, we keep the implementation simple and correct. The performance cost is acceptable because:

- Retransmissions are rare in good network conditions
- Memory copying is fast for typical packet sizes (1-4KB)
- The alternative complexity would impact all code paths, not just retransmissions

---

## Work Item 11: Update Receive Side for Pool-Based ACKs

### Current Implementation

The receive side in `dc/s2n-quic-dc/src/stream/recv/` generates ACK packets using the old allocator pattern. Several `todo!()` calls indicate where pool allocation should be used.

**Relevant Files:**

- `dc/s2n-quic-dc/src/stream/recv/state.rs`
- `dc/s2n-quic-dc/src/stream/recv/shared.rs`
- `dc/s2n-quic-dc/src/stream/recv/worker.rs`

### Work Required

1. **Add pool reference to receive state:**

   - The receive state needs access to the pool to allocate ACK packets
   - Pass the pool reference during receive state initialization
   - Store it in the state for use when generating control packets

2. **Replace ACK buffer allocation:**

   - Find all locations where ACK packets are created (marked with `todo!()`)
   - Replace with `pool.alloc_or_grow()`
   - Fill the descriptor with ACK packet data (frame header + ACK ranges)

3. **Send ACKs through wheel:**

   - Instead of directly sending ACKs, create wheel entries
   - ACKs should use the highest priority wheel (priority 0) to ensure timely delivery
   - Calculate timestamp as `now` (immediate) since ACKs are latency-sensitive

4. **Handle ACK-only packets:**

   - ACK-only packets don't contribute to flow control
   - They should not increment `pending_transmissions` since they're not governed by application flow control
   - Consider a separate counter or flag to distinguish control vs data packets

5. **Reuse ACK descriptors:**
   - After an ACK is sent and completed, its descriptor returns to the pool
   - This allows efficient reuse without allocating new memory each time
   - Monitor that the pool has sufficient size to handle ACK traffic patterns

**Reasoning:**
Using the pool for ACKs provides:

- Consistent memory management across send and receive
- Integration with the wheel for proper prioritization
- Reuse of memory buffers for repeated ACKs

ACKs are critical for throughput and latency, so they need priority treatment in the wheel. By using priority 0, we ensure ACKs are sent before application data when both are ready, which helps the sender's congestion control react quickly.

---

## Work Item 12: Wire Up Pool Through Stream Creation

### Current Implementation

Stream creation happens in `dc/s2n-quic-dc/src/stream/endpoint.rs` where a `todo!()` marks where the pool should be provided. The pool needs to flow from the environment through endpoint to the state.

**Relevant Files:**

- `dc/s2n-quic-dc/src/stream/endpoint.rs`
- `dc/s2n-quic-dc/src/stream/environment/udp.rs` (environment setup)
- `dc/s2n-quic-dc/src/stream/send/state.rs` (state initialization)

### Work Required

1. **Update endpoint to accept pool:**

   - Modify stream creation functions to accept a pool reference
   - This likely comes from the environment's `Pooled` struct
   - Store the pool or pass it through to state initialization

2. **Pass pool to state constructors:**

   - Update `State::new` and related constructors to accept a pool reference
   - Store `Arc<Pool>` in the state for use during transmission creation
   - Both send and receive states need pool access

3. **Pass pool to workers:**

   - Workers need pool access for creating transmissions and processing completions
   - Add pool parameter to worker construction
   - Store it in the worker for use during polling

4. **Handle different environments:**

   - Tokio and Bach environments may create pools differently
   - Ensure both paths properly initialize and pass the pool
   - The pool configuration should come from the environment's `Config`

5. **Lifetime considerations:**
   - The pool is Arc-wrapped and cloned when creating streams
   - The pool must outlive all streams using it
   - Typically the environment owns the pool, and streams hold Arc references

**Reasoning:**
The pool must be accessible everywhere packets are allocated:

- Application layer when creating new transmissions
- Receive layer when generating ACKs
- State layer when creating retransmissions
- Worker layer when processing completions

By threading it through the initialization path, we ensure every component that needs it has access. The Arc wrapper allows sharing without explicit lifetime management.

---

## Work Item 13: Update Transmission Info for GSO

### Current Implementation

The `transmission::Info` type in `dc/s2n-quic-dc/src/stream/send/state/transmission.rs` represents metadata for a single packet. The wheel's `Transmission` type currently expects a single info value.

**Relevant Files:**

- `dc/s2n-quic-dc/src/stream/send/state/transmission.rs`
- `dc/s2n-quic-dc/src/socket/send/wheel.rs`
- `dc/s2n-quic-dc/src/stream/send/application/transmission.rs`

### Work Required

1. **Change Transmission info type:**

   - The wheel's `Transmission<Info>` currently has a single `info: Info` field
   - Change this to `info: Vec<Info>` to support GSO packets containing multiple QUIC packets

2. **Create info vector during transmission:**

   - When creating a transmission with GSO, build a Vec with one `Info` per QUIC packet
   - Each info should contain:
     - `packet_len`: The size of this specific QUIC packet
     - `stream_offset`: Where in the stream this packet's data starts
     - `payload_len`: How much stream data is in this packet
     - `included_fin`: Whether this packet includes the FIN flag
     - `retransmission`: A handle to the filled descriptor for potential retransmission
     - `ecn`: ECN marking (may be same for all packets in the GSO burst)
     - `time_sent`: Set when transmission completes

3. **Process info vector on completion:**

   - When processing completions in the worker, iterate through the Vec
   - Update RTT based on the `time_sent` vs completion time
   - Mark each packet's stream offset range as acknowledged
   - Handle FIN acknowledgment if present
   - Update congestion control based on all packets' `cca_len()` values

4. **Handle partial GSO sends:**

   - With the wheel model, partial sends won't occur (all or nothing)
   - Document this assumption and ensure wheel never partially accepts packets
   - If a transmission fails, all packets in it should be retransmitted

5. **Retransmit individual packets:**
   - When a packet is lost (not the whole GSO burst), retransmit just that packet
   - Extract the specific `Info` from the vector
   - Use its retransmission handle to create a new single-packet transmission
   - Schedule it in the wheel as a separate entry

**Reasoning:**
GSO allows sending multiple packets in a single syscall, dramatically improving throughput. However, loss detection and RTT measurement must operate at the individual packet level, not the GSO burst level. By tracking info per packet:

- We can detect loss of individual packets within a burst
- RTT measurements remain accurate (per-packet timing)
- Retransmissions only resend lost packets, not entire bursts
- Flow control operates on stream offsets, which are per-packet

The Vec adds minimal overhead (packets per GSO burst is typically 1-16) while providing necessary granularity.

---

## Work Item 14: Remove Partial Send Handling

### Current Implementation

The `msg/send.rs` file has a `todo!()` in the `on_transmit` method for handling cases where `total_len > len`, indicating a partial send. The current code attempts to track which segments were partially sent.

**Relevant Files:**

- `dc/s2n-quic-dc/src/msg/send.rs`

### Work Required

1. **Remove partial send logic:**

   - Delete the `if self.total_len as usize > len { todo!(); }` check
   - Remove any code that tries to handle partial sends

2. **Document wheel behavior:**

   - Add comments explaining that the wheel guarantees all-or-nothing transmission
   - If a transmission can't be sent in full, it remains in the wheel
   - The socket worker will retry later

3. **Simplify force_clear:**

   - With no partial sends, cleanup becomes simpler
   - Just mark all pending descriptors as freed
   - Reset counters to zero

4. **Error handling:**
   - If a send fails with an error (not EWOULDBLOCK), the transmission is dropped
   - The completion queue won't receive it
   - This will trigger retransmission via normal loss detection
   - Document this behavior for debugging purposes

**Reasoning:**
The wheel-based model eliminates partial sends because:

- The wheel holds transmissions until they can be fully sent
- The socket worker doesn't try to send if the socket is full
- GSO sends are atomic (all segments or none)
- Blocking on token bucket happens before attempting to send

This simplification removes a significant source of complexity and edge cases. The wheel-based architecture guarantees that either all packets in a transmission are sent or none are sent, eliminating the need to track partial progress. This makes the code more reliable and easier to reason about, while also simplifying error handling and recovery logic.

---

## Status Tracking

See `WORK_ITEM_STATUS.md` for detailed status of each work item, assignments, and progress tracking.

See `TEST_COVERAGE_PLAN.md` for comprehensive test plans for each work item.

See `INVESTIGATION_SUMMARY.md` for high-level overview and implementation plan.

### Quick Status Overview

| # | Work Item | Status |
|---|-----------|--------|
| 1 | Pool Integration in Environment Layer | 🔴 Not Started |
| 2 | Stream Completion Queue Setup | 🔴 Not Started |
| 3 | Replace Queue with Wheel-Based Transmission | 🔴 Not Started |
| 4 | Refactor msg/send.rs to Use Descriptor Pool | 🔴 Not Started |
| 5 | Remove Pacer and Implement Flow-Based Backpressure | 🔴 Not Started |
| 6 | Update Worker Transmission Logic | 🔴 Not Started |
| 7 | Update State's Transmission Creation | 🔴 Not Started |
| 8 | Integrate CCA Pacing with Wheel | 🔴 Not Started |
| 9 | Configure Transmission Limits | 🔴 Not Started |
| 10 | Handle Retransmissions with Copying | 🔴 Not Started |
| 11 | Update Receive Side for Pool-Based ACKs | 🔴 Not Started |
| 12 | Wire Up Pool Through Stream Creation | 🔴 Not Started |
| 13 | Update Transmission Info for GSO | 🔴 Not Started |
| 14 | Remove Partial Send Handling | 🔴 Not Started |

**Status Legend**: 🔴 Not Started | 🟡 In Progress | 🟢 Complete | 🔵 Blocked
