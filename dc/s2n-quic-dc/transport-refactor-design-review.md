# Transport Refactor Design - Review Findings

This document captures gaps, missing details, and areas needing clarification identified during the design review of `transport-refactor-design.md`.

## Critical Missing Details

### 1. Application-Worker Communication Channel

**Issue:** The design describes workers running on dedicated cores with busy polling, but doesn't specify how application threads submit transfer requests to workers.

**Questions:**

- How does the Router communicate with workers? (lock-free queues, channels, shared memory?)
- What happens when a worker's request queue fills up? (backpressure mechanism to application?)
- How are responses delivered back to application threads waiting for transfer completion?
- For broadcast transfers, which worker "owns" the buffer and coordinates acknowledgments?

**Answers:**

1. **Router-to-Worker Communication**: Use ring buffers with separate queues per priority level, allowing workers to process higher-priority items first. Application allocates and associates its own ring buffer for responses. Goal is bidirectional channel with minimal synchronization and maximum allocation reuse.

2. **Queue Saturation**: Apply backpressure when worker's request queue fills up. Since the application is tokio-based, store a waker to notify when new capacity becomes available.

3. **Response Delivery**:

   - Single message responses: async oneshot channel
   - Multiple responses (streaming): async stream
   - Headers: potential separate oneshot channel

4. **Broadcast Coordination**: Router owns the buffer. Application workflow:
   - Allocates a buffer region
   - Encrypts data directly into the region (avoiding copy)
   - Submits buffer ID to multiple RPC requests
   - Buffer management happens before/interleaved with RPC calls

**Impact:** High - Critical for understanding the system's threading model and performance characteristics.

---

### 2. Causality Map Sizing and Exhaustion

**Issue:** The causality map uses fixed-size slot allocation but lacks concrete sizing information.

**Questions:**

- What's the actual slot count? (critical for understanding scale limits)
- What happens when all slots are exhausted? (fail requests, block, wait?)
- How are slots reclaimed? (immediate on completion, delayed garbage collection?)
- Maximum dependency count per slot is mentioned as "sufficient for expected usage" but not specified
- How does the system handle dependency cycles (if at all)?

**Answers:**

1. **Slot Allocation**: Allocate slots only when needed. Application must explicitly indicate they want to use causality tracking for a transfer. Map should be resizable/growable to avoid becoming a bottleneck (backpressure mechanisms exist elsewhere in the system).

2. **Slot Exhaustion**: With a growable map design, exhaustion should be rare. The system can expand capacity as needed.

3. **Slot Reclamation**: Slots are freed immediately when transfers complete.

4. **Dependencies per Slot**: No hard limit. While majority of requests will have only a handful of dependencies, the system should support hundreds if the application requires it.

5. **Dependency Cycles**: Always assume DAG (Directed Acyclic Graph). API structure enforces DAG by design: when creating a request, application provides list of dependency tokens but doesn't yet know the causality token for the request being created. This prevents cycles by construction.

**Impact:** High - Directly affects system scalability and capacity planning.

---

### 3. Receiver State Management

**Issue:** The receiver-driven protocol is well described from the sender's perspective, but receiver implementation is unclear.

**Questions:**

- Does each receiver also run dedicated workers with busy polling?
- How does the receiver's application thread get notified of incoming `TRANSFER_NOTIFY` messages?
- Where does the receiver store received ranges before application consumption?
- How is receiver-side backpressure communicated (slow application consumption)?
- What determines when a receiver sends `PULL_REQUEST` messages (immediately on NOTIFY, batched, application-triggered)?

**Answers:**

1. **Receiver Architecture**: Yes, both sides run dedicated workers with busy polling. Since all communication is bidirectional (even simple request/response patterns), both sides need the same architecture. There's just an initiator (client) that starts the flow.

2. **TRANSFER_NOTIFY Delivery**: Server side uses an acceptor trait that gets called for each new flow. Application decides how to handle:

   - Spawn tokio task to handle it
   - Reject it (e.g., under load)
   - Queue it without immediate processing
   - **Important**: Need mechanism for application to include metadata in TRANSFER_NOTIFY about transfer contents (without decrypting) to enable RPC handler dispatch

3. **Received Data Storage**: Use reassembler buffer for encrypted data. Once ready to decrypt:

   - Decrypt directly into application-provided buffer, OR
   - Handle inline and provide BytesMut directly to application

4. **Receiver-Side Backpressure**: Simply don't send PULL_REQUEST messages. May want ability to cancel outstanding pull requests (though not guaranteed due to in-flight messages).

5. **PULL_REQUEST Timing**: Send immediately if receiver has bandwidth, up to configurable local buffer limit (prevents buffering entire request if application won't process immediately). Should include batching optimization.

**New Gap Identified**: How does sender know if peer received TRANSFER_NOTIFY? Need ACK mechanism for TRANSFER_NOTIFY messages so sender can detect and retransmit missing notifications.

**Impact:** High - Essential for understanding end-to-end flow and symmetry between sender/receiver.

---

### 4. Transfer Identifier Generation and Scoping

**Issue:** Protocol messages reference "transfer identifier" but the design doesn't specify generation and scoping.

**Questions:**

- Who generates transfer IDs? (sender, receiver, router?)
- What's the scope? (global, per-peer, per-worker?)
- What's the format and size?
- How are collisions avoided in high-concurrency scenarios?
- How long must IDs remain unique to avoid ambiguity with retries?

**Answers:**

1. **ID Generation**: Initiator creates identifier before sending TRANSFER_NOTIFY. Both sides create their own transfer IDs - sender has local ID, receiver has local ID. IDs are echoed in protocol messages (e.g., PULL_REQUEST includes receiver's local ID, DATA_CHUNKS echoes it back for routing).

2. **Scope**: Global across all peers and workers to enable easy routing of responses to correct destination.

3. **Format**: 64-bit structure:

   - One half: generation ID (potentially includes worker ID bits to avoid cross-worker coordination)
   - Other half: slot ID
   - Generation wraparound is acceptable - window is large enough to avoid confusion

4. **Collision Avoidance**: Use slot allocator for IDs. Embedding worker ID in generation bits eliminates need for worker coordination, enabling better concurrency.

5. **ID Reuse**: Same ID used for retries to enable receiver deduplication.

**New Gap Identified - Replay Protection with Challenge-Response**:

- How does receiver know TRANSFER_NOTIFY is fresh?
- Need tracking window for replay detection
- For transfers outside tracking window: implement challenge-response mechanism
- Receiver challenges via PULL_REQUEST
- Sender responds via DATA_CHUNKS if transfer is still valid
- If transfer ID now maps to different transfer, sender sends FLOW_RESET message to indicate it's not genuine

**Impact:** High - Critical for protocol correctness and debugging.

---

### 5. Timing Wheel Parameters

**Issue:** The timing wheel is critical for pacing but lacks concrete details.

**Questions:**

- Tick duration (mentioned as "sub-millisecond" but no specific value)
- Number of slots/hierarchy depth
- Maximum scheduleable time into the future
- How does it handle events beyond its time horizon?

**Impact:** Medium - Needed for implementation but can be tuned later.

---

### 6. Priority Queue Starvation Prevention

**Issue:** With 256 priority levels, lower priorities could starve.

**Questions:**

- Can high-priority transfers starve lower priorities indefinitely?
- Is there any fairness mechanism or time-based promotion?
- How does the system ensure progress on all priorities?

**Impact:** Medium - Important for operational fairness and preventing pathological cases.

---

### 7. Key Rotation Mechanics

**Issue:** Key rotation is mentioned but critical details are missing.

**Questions:**

- What triggers rotation? (time-based, transfer count, data volume?)
- How do peers coordinate rotation to ensure sender and receiver agree?
- What happens to in-flight transfers during rotation?
- How is the "next key" communicated and agreed upon?

**Answers:**

The system uses two distinct key types with different rotation mechanisms:

**Control Keys:**

- Derived from TLS handshake with the peer
- Used to authenticate control messages
- Rotation triggered by: `N` bytes transmitted AND `M` packets sent
- Rotation process: Bump key ID, which deterministically derives next key from root path secret
- No coordination needed - key ID determines which key to use
- Each worker derives control keys locally per peer
- Workers need notification when control key invalidated (`UNKNOWN_PATH_SECRET` message)

**Data Keys:**

- Generated on-the-fly, sent across wire (encrypted by control keys)
- Optionally encrypt and authenticate application payloads
- Enable efficient broadcast by distributing same key to multiple peers
- Rotation: Generate new key and include in packet
- Receivers should cache AES key schedule (expensive operation) using key value itself as identifier or hash
- Can rotate independently from control keys

**Transfer Lifecycle:**

- Each transfer pins its data key when starting
- Sender doesn't keep old keys long
- Receiver may need to cache keys longer for decryption

**Key Cleanup:**

- Consider `RETIRE_KEY` message for control keys so receiver knows to stop caching
- Would use separate key derived per peer path secret (already done for secret control messages)

**Worker Coordination:**

- Each worker maintains its own keys
- Need mechanism to notify all workers when peer's control key invalidated
- Continue using `UNKNOWN_PATH_SECRET` message pattern from current implementation

**Impact:** High - Critical for security and operational stability.

---

### 8. Replay Protection Implementation

**Issue:** Replay protection is listed as a requirement but the design doesn't describe the mechanism.

**Questions:**

- How is replay protection achieved with pooled keys?
- What nonce/sequence number scheme is used?
- How does the receiver's replay window work?
- How are gaps in sequence handled (reordering vs. loss)?

**Answers:**

**Primary Concern - TRANSFER_NOTIFY Messages:**

- TRANSFER_NOTIFY is the only message type sensitive to replay attacks
- For inline requests, receiver checks if active transfer exists with that transfer ID for that peer
- `TRANSFER_NOTIFY` should include retransmission flag so receiver knows whether to deduplicate
- If existing transfer found: send `TRANSFER_ACCEPT` (original likely lost)

**Inline Data Handling:**

- Inline data in TRANSFER_NOTIFY needs special handling
- If message outside replay window and inline data present: send `TRANSFER_CHALLENGE`
- Sender responds with `TRANSFER_CHALLENGE_RESPONSE` to validate legitimate request
- Only need to track scenarios with inline data

**Non-Inline Data Protection:**

- For non-inline transfers, peer liveness validated naturally by PULL_REQUEST mechanism
- Buffer ID/handle validation provides implicit replay protection
- For RDMA: invalid handle fails naturally
- For DATA_CHUNKS: if stream doesn't exist, drop as replay and emit event

**Inline Request Optimization:**

- Small requests benefit from inline transmission in TRANSFER_NOTIFY
- However, for many small requests (e.g., 1000 to single peer), bulk transfer with RDMA more efficient
- Need logic to determine when inline vs. bulk is optimal

**Nonce Management:**

- Use monotonic counters for nonces
- Since each worker has own key, no coordination needed between workers
- Prevents nonce reuse naturally

**Replay Window:**

- Configurable size
- Use bitset implementation
- Shift out old values when gaps close or new IDs exceed current window
- Per-peer tracking

**New Protocol Messages Identified:**

- `TRANSFER_ACCEPT`: Acknowledge existing transfer (handle retransmitted TRANSFER_NOTIFY)
- `TRANSFER_CHALLENGE`: Challenge potentially replayed inline TRANSFER_NOTIFY outside window
- `TRANSFER_CHALLENGE_RESPONSE`: Validate legitimate inline transfer request

**Impact:** High - Security requirement that must be satisfied.

---

### 9. Buffer Registry Synchronization

**Issue:** Buffer management describes lock-free access but needs clarification on synchronization.

**Questions:**

- How do workers learn about buffers allocated by application threads?
- Is the registry per-worker or shared across all workers?
- How is the buffer registry invalidated/cleaned up after buffer deallocation?
- For RDMA, who registers the memory regions and how are keys distributed?

**Answers:**

**Registry Scope:**

- Global buffer registry shared across all workers
- Must be highly concurrent (likely partitioned) to support many allocations

**Allocation Flow:**

- Application allocates buffer from global registry (synchronization point)
- Application copies data into allocated buffer, likely by scatter/gather encrypting it to the destination
- Application passes buffer handle to transport layer
- Transport uses buffer for libfabric or UDP transmission

**Streaming Support:**

- Allocate buffers on-demand for streaming messages
- Need batch allocation pattern for streaming messages
- Merge stream messages into same buffer allocation when possible

**Registry Cleanup:**

- Reference counting mechanism
- Buffers released back to pool when reference count reaches zero
- Automatic cleanup on final deregistration

**RDMA Memory Registration:**

- Worker registers buffer with NIC (worker knows which transport is being used)
- For broadcast to multiple NICs: register buffer multiple times with libfabric
- Each deregistration decrements reference count
- Final deregistration frees buffer back to pool

**Performance Requirements:**

- O(1) lookup time
- Minimal indirection for hot path
- Slot map or similar structure for buffer ID → buffer mapping

**New Gaps Identified - Streaming Lifecycle:**

- **Keepalive Mechanism**: Need keepalive for open streams when application has no data to send but holds handle
- **Stream Close Signaling**: Need protocol message to indicate stream closure
  - Sender initiated: distinguish error vs. normal end-of-stream
  - Receiver initiated: notify sender that receiver closed
  - Proposed message: `STREAM_CLOSE` with status/reason code

**Impact:** High - Critical for correct memory management and avoiding leaks.

---

### 10. Bandwidth Configuration and Enforcement

**Issue:** Workers must "shape traffic" with a "leaky bucket" but implementation details are missing.

**Questions:**

- How is NIC bandwidth configured? (per-worker, per-NIC, global?)
- What's the leaky bucket implementation? (token bucket, rate limiter algorithm?)
- How is bandwidth allocated across priorities?
- How does the system detect and respond to NIC rate limit violations?

**Answers:**

**Bandwidth Configuration:**

- Each NIC has its own leaky bucket state
- Application specifies bandwidth at startup (not runtime adjustable)
- No automatic detection - must be configured per deployment
- EC2 example: 5Gbps per-flow limit, 25Gbps per-NIC limit

**Transport-Specific Handling:**

_UDP:_

- Use port randomization to work around per-flow limits
- Allocate multiple ports to aggregate to total NIC bandwidth
- Spread bulk data transmission across all allocated ports
- Example: 5 ports × 5Gbps = 25Gbps total

_RDMA/libfabric/EFA:_

- EFA uses SRD (Scalable Reliable Datagram) protocol
- Per-flow VPC limits don't apply to EFA in same way as TCP/IP
- SRD automatically spreads traffic across available paths
- Can achieve full NIC bandwidth without port tricks
- Still implement application-level rate limiting to avoid overwhelming NIC

**Leaky Bucket Algorithm:**

- Use classic leaky bucket (not token bucket)
- Goal: avoid bursting entirely
- Pace packets evenly at precise rate to achieve target bandwidth
- Microbursts acceptable for GSO coalescing
- Time-based token replenishment in worker polling loop

**Priority Levels:**

- Reduce from 256 to 8 priority levels (8 is sufficient)
- Use weighted fair queueing across priorities
- Ensures all priorities make _some_ progress
- Prevents complete starvation of lower priorities

**Rate Limit Enforcement:**

- Static configuration (no dynamic adjustment)
- Known bandwidth values per deployment that never change
- Worker enforces configured rate through pacing

**Oversubscription Handling:**

- Apply backpressure to entire system when NIC bandwidth exceeded
- Slow down application message submission, especially lower priority
- Throttle opening new RPC flows if can't flush pending ones
- New low-priority flows blocked if high-priority flows pending
- Critical: avoid taking on so much work that nothing makes progress

**Receiver-Side Bandwidth Control:**

- Control receiving bandwidth by rate-limiting PULL_REQUEST messages
- Critical for incast scenarios (e.g., paxos replicated reads)
- Multiple replicas responding simultaneously can overwhelm receiver
- Schedule reads based on pending response queue depth
- Prioritize PULL_REQUESTs same way as sends (use priority levels)
- When many pending responses queued: slow down new PULL_REQUESTs
- Ensures receiver doesn't get hammered by simultaneous data from multiple peers

**Impact:** High - Essential for meeting goodput goals and preventing congestion.

---

## Significant Gaps

### 11. Failure and Recovery Scenarios

**Application thread crash during transfer:**

- How are in-flight transfers cleaned up?
- Who releases buffer references?
- How are causality tokens reclaimed?

**Worker crash/hang detection:**

- How does the system detect a worker has failed?
- What happens to its assigned peers and transfers?
- How are they redistributed?

**Partial broadcast failure handling:**

- Design mentions "failed peers have their transfer references released" but doesn't specify mechanism
- Who determines a peer has failed in a broadcast? (timeout, explicit error?)
- How does the application learn which peers succeeded/failed?

**Answers:**

**Application Thread Crash:**

- Automatic cleanup via RAII: stream handles dropped when thread crashes
- Dropped handles notify workers that application closed
- Workers send closure notification to peer
- Buffers and causality tokens automatically released back to pool
- Dependent transfers with causality links automatically fail (cascade failures)

**Peer Liveness Monitoring:**

- Monitor peers only when outstanding streams/transfers exist
- Track last time packets received from peer
- If no activity: send `PROBE` messages to check liveness
- Peer responds with `PROBE_RESPONSE` if still active
- Avoids redundant monitoring (single monitoring point per peer vs. per-flow)

**Peer Failure Detection:**

- Transfer timeouts trigger peer failure detection
- Timeouts indicate peer becoming unavailable
- Health state machine handles transitions based on timeout patterns

**Worker Crash/Hang Detection:**

- Lightweight monitoring task watches all workers
- Each worker updates iteration counter in polling loop
- If counter stops updating: worker is hung
- Response: Log failure and abort process
- Rationale: Worker hang is extreme failure, difficult to recover from gracefully

**Partial Broadcast Handling:**

- Each stream in broadcast has independent state management
- Only shared component: buffer(s) via reference counting
- Minimal synchronization between broadcast streams
- Per-stream failure handled independently
- Buffer released only when all streams complete or fail
- Application receives per-stream completion/failure status

**NIC Failure Detection:**

- Remove from design - not realistically detectable
- Focus on peer-level and worker-level failure detection instead

**New Protocol Messages Identified:**

- `PROBE`: Check peer liveness when no recent activity
- `PROBE_RESPONSE`: Confirm peer is still active

**Impact:** High - System reliability depends on graceful failure handling.

---

### 12. EFA vs UDP Behavioral Differences

**Protocol adaptation:**

- Do `PULL_REQUEST` messages exist in EFA mode or are they replaced entirely by RDMA reads?
- How does receiver-side pacing work with RDMA? (receiver controls read timing)
- Do causality dependencies work identically across both transports?
- What happens when broadcasting to mixed EFA/UDP peers?

**Failure modes:**

- EFA-specific errors and how they map to transport-level failures
- Network partition handling differences between EFA and UDP

**Impact:** Medium - Important for consistent behavior across environments.

---

### 13. End-to-End Backpressure Details

**Issue:** Requirement states backpressure is essential but implementation is vague.

**Questions:**

- How does a slow application reader stop the receiver from requesting more data?
- How is this signaled back to the sender?
- What happens when backpressure accumulates across many transfers?
- Does this interact with priority scheduling?

**Answers:**

**Receiver Application Slow Reader:**

- Implicit based on queue depth (no explicit API call needed)
- Configured per-stream on creation with two limits: message queue depth AND byte limit
- When queue at capacity: stop issuing `PULL_REQUEST` messages
- Byte limit can be exceeded if don't yet have fully-buffered message (prevents deadlock)

**Receiver-to-Sender Signaling:**

- Absence of `PULL_REQUEST` messages signals backpressure to sender (implicit)
- Additional explicit control: extend `TRANSFER_ACCEPT` message to include credit system
- Server issues higher sequence ID in `TRANSFER_ACCEPT` to indicate allowed `TRANSFER_NOTIFY` credits
- Provides explicit flow control for number of pending `TRANSFER_NOTIFY` messages

**Sender Buffer Backpressure:**

- When sender runs out of buffer space: apply backpressure to application
- Try to clear buffer space by getting acknowledgments from peers
- Application defines max buffer pool size that shouldn't be exceeded
- Prevents unbounded buffer growth on sender side

**Priority Interaction:**

- Backpressure considers priority levels
- Lower priority transfers throttled more aggressively
- Higher priority transfers get preferential treatment even under backpressure
- Ensures critical work makes progress while applying pressure to less important work

**Impact:** High - Critical requirement that's underspecified.

---

### 14. Small Transfer Optimization

**Issue:** Inline payloads in `TRANSFER_NOTIFY` are mentioned but underspecified.

**Questions:**

- What's the size threshold for inline vs. pull-based transfers?
- How does this interact with encryption? (still use ephemeral key?)
- Do inline transfers skip the causality check or still respect dependencies?
- How is the "pacing of inline messages" enforced?

**Impact:** Medium - Performance optimization that needs clear semantics.

---

### 15. Congestion Control Specifics

**BBR integration:**

- Is BBR per-peer, per-worker, or per-NIC?
- How does BBR state survive peer reassignment after NIC failure?
- How do multiple workers sending to the same peer (shouldn't happen per design, but worth clarifying) coordinate BBR state?

**Receiver-driven interaction:**

- When receiver sends `PULL_REQUEST` with desired pacing rate, how is this reconciled with sender's BBR estimate?
- What if they disagree significantly?

**Impact:** Medium - Important for achieving goodput goals.

---

### 16. Path Secret Map Integration

**Issue:** Requirements mention maintaining "existing path secret map mechanism" but integration is unclear.

**Questions:**

- How does the new key pooling interact with the path secret map?
- Is the path secret map per-peer or global?
- How are path secrets established for new peers?
- How does this relate to the control message authentication keys?

**Impact:** Medium - Important for understanding security architecture continuity.

---

### 17. Transfer Timeout Handling

**Issue:** No mention of transfer timeouts in the design.

**Questions:**

- When does the system give up on a transfer?
- How are timeout durations determined? (fixed, adaptive, application-specified?)
- What happens to causality dependencies when a transfer times out?
- How are timed-out buffers cleaned up?

**Answers:**

**Timeout Granularity:**

- Granular timeouts for each protocol phase
- Most critical: retransmit `TRANSFER_NOTIFY` until get `TRANSFER_ACCEPT` or `TRANSFER_CHALLENGE`
- Use exponential backoff starting at 3× RTT (similar to QUIC's PTO timer)
- Same retry-with-backoff approach for `PULL_REQUEST` and `DATA_CHUNKS`
- Quick recovery: when receive `TRANSFER_ACCEPT` for later NOTIFY while earlier pending, indicates earlier was lost, same with PULL_REQUEST/DATA_CHUNKS

**Overall Transfer Timeout:**

- Streams time out when overall peer liveness probes fail
- Individual protocol phases keep backing off exponentially
- Any peer activity resets backoff (indicates peer is alive)
- Per-phase timeouts provide resilience while peer-level timeout provides ultimate failure detection

**Refined Dependency Type Matrix:**
Need more precise naming based on two dimensions:

1. Wait for request or response?
2. Cascade error or not?

Gives 4 combinations:

- Wait for response + cascade error (current "critical")
- Wait for request only + cascade error (current "weak")
- Wait for response + don't cascade (new type?)
- Wait for request only + don't cascade (current "optional")

**Buffer Cleanup:**

- Automatic via reference counting
- When transfer times out: stream/transfer state cleaned up
- Reference count decremented
- If reference count reaches zero: buffer freed back to pool
- For broadcast: buffer stays available for successful streams

**Impact:** High - Essential for preventing resource leaks and hangs.

---

## Consistency Issues

### 18. Worker Core Assignment Ambiguity

**Issue:** Architecture diagram shows "Cores 0-N", "Cores N+1-M" suggesting ranges, but text says workers are assigned cores based on NUMA.

**Questions:**

- Is each worker single-threaded on one core, or does it span multiple cores?
- How many cores per worker?
- Can workers share cores?

**Impact:** Medium - Clarity needed for deployment planning.

---

### 19. Causality Token Ownership

**Issue:** Data flow shows Router allocates causality tokens, but Causality Map section implies workers also interact with it.

**Questions:**

- Is the causality map owned by the Router or shared by all workers?
- Do workers update causality state directly or through Router?
- For completion notification, who updates the causality map?

**Impact:** Medium - Architectural clarity needed.

---

### 20. Buffer Management Responsibility Split

**Issue:** The design states application encrypts data, buffer placed in "shared memory region managed by transport", workers maintain buffer ID registry. This creates ambiguity about responsibility.

**Questions:**

- Who allocates the shared memory region? (application, pool, worker?)
- Where physically does the buffer reside? (per-worker, shared across all workers?)
- How does application know where to place encrypted data?

**Answers:**

**Memory Region Allocation:**

- Transport Pool allocates buffer memory regions during initialization
- NUMA-aware regions desirable (application can request buffer from specific worker's NUMA node)
- Not a hard requirement (especially for broadcast buffers shared across workers)

**Application Buffer API:**

- `allocate_buffer(size)` returns handle to buffer descriptor
- Buffer descriptor is a list (message may span multiple blocks)
- Registry tracks memory address associated with each buffer ID

**Buffer Lifecycle States:**

1. **Allocated**: Buffer reserved but not yet written
2. **Writing**: Application writing data into buffer
3. **Filled**: Data complete, buffer ready for transmission, can be passed to multiple streams (reference counted)

**Two Application Modes:**

Mode 1 - Pre-encrypted data:

- Application copies already-encrypted/authenticated data directly into buffer
- Must specify region to use for authenticating in control messages
- Binds data to control message for security

Mode 2 - Encrypt-on-write:

- Application submits plaintext data to be encrypted into buffer
- Binding region is the GHASH tag
- Transport handles encryption

**Buffer Sharing:**

- Once filled, buffer can be cloned and passed to multiple streams
- Streams hold pre-encrypted/authenticated buffer references
- Reference counted across all streams
- Freed back to pool when all references released

**Worker Access:**

- Workers reference buffers directly via thin pointers
- Can dereference to get buffer metadata
- All workers hold reference-counted set of regions
- Ensures pointer validity during buffer lifetime

**Memory Ownership:**

- Both Application and Transport owns the memory pool with reference counts
- Application allocates from it, transport sends it and frees it back
- Explicit lifecycle transitions
- Reference counting manages shared ownership during transmission

**Impact:** High - Critical for implementation clarity.

---

## Areas Needing More Detail

### 21. Request/Response Header Support

**Issue:** Requirements mention "optional request headers and/or response headers" but no protocol details.

**Questions:**

- How are headers encoded in protocol messages?
- Are headers separate from bulk payload or integrated?
- Do headers have their own encryption/authentication?
- How do headers interact with inline transfers?

**Impact:** Medium - Feature completeness.

---

### 22. Bidirectional Transfer Implementation

**Issue:** Bidirectional transfers mentioned as a pattern but implementation is unclear.

**Questions:**

- How do simultaneous send/receive flows coordinate?
- Do they share the same transfer ID?
- How is completion determined (both directions done)?
- How do priorities apply (per-direction or per-transfer)?

**Impact:** Medium - Feature completeness.

---

### 23. Streaming Transfer Lifecycle

**Issue:** Streaming patterns (request/response) are listed but lifecycle is unclear.

**Questions:**

- How does sender know when streaming is complete vs. more chunks coming?
- How are stream boundaries communicated?
- Can streams be cancelled mid-flight?
- How do causality dependencies work with streams (whole stream or per-chunk)?

**Impact:** Medium - Feature completeness.

---

### 24. Raw Bulk Transfer Details

**Issue:** "Direct memory or disk placement with optional cache teeing" mentioned but underspecified.

**Questions:**

- How is destination memory specified?
- What's the cache teeing mechanism?
- How does this work with encryption?
- Does this require different protocol messages?

**Impact:** Medium - Feature completeness.

---

### 25. Event Framework Specifics

**Issue:** Requirements emphasize event visibility but implementation is vague.

**Questions:**

- What's the event delivery mechanism? (callbacks, channels, queue?)
- Are events per-worker or aggregated?
- How is event overflow handled (slow subscriber)?
- What's the event schema/format?
- Which events are considered critical vs. debug-level?

**Impact:** Medium - Operational visibility requirement.

---

### 26. GSO (Generic Segmentation Offload) Usage

**Issue:** Mentioned for UDP but needs detail.

**Questions:**

- How large are GSO segments?
- How does this interact with MTU and encryption?
- What happens on platforms without GSO support?
- How does GSO affect timing wheel pacing accuracy?

**Impact:** Low - Performance optimization detail.

---

### 27. NUMA Awareness

**Issue:** Core allocation mentions NUMA but details are sparse.

**Questions:**

- How are NICs mapped to NUMA nodes?
- Is memory allocated per-NUMA node?
- How does peer-to-worker assignment consider NUMA?
- What happens in non-NUMA systems?

**Impact:** Medium - Performance optimization for target platforms.

---

## Questions About Meeting Goals

### 28. 10,000+ Concurrent Streams to Hundreds of Peers

**Issue:** Math check: 10,000 streams × hundreds of peers = millions of concurrent transfers.

**Questions:**

- Is the causality map sized for this? (slot count?)
- Can the buffer registry scale to millions of buffers?
- How much memory does this require?
- Are there any implicit limits that would prevent this scale?

**Answers:**

**Scalability Validation:**

- Requires profiling and testing to determine concrete limits
- Will need configurable limits at various levels:
  - Causality map initial/max size
  - Buffer pool capacity
  - Worker queue depths
  - Per-peer concurrent transfer limits
- Apply backpressure when approaching limits
- Tunable parameters based on deployment characteristics

**Key Considerations:**

- Per-transfer memory overhead (state, descriptors, queue entries)
- Causality map memory at scale (if 1M transfers with causality tracking)
- Buffer registry memory (descriptor size × buffer count)
- Worker queue capacity across 8 priority levels
- Total memory footprint needs measurement

**Potential Limits to Address:**

- 32-bit value overflows (ensure 64-bit where needed)
- File descriptor limits for UDP ports
- System resource limits for RDMA memory mappings
- Memory mapping limits per process

**Approach:**

- Design allows scaling to requirement
- Actual limits determined through testing
- Configuration knobs provided for tuning
- Backpressure prevents overload

**Impact:** High - Core scalability requirement validation.

---

### 29. 100% Goodput Target

**Issue:** Extremely aggressive goal from current 20%.

**Questions:**

- What's the theoretical best-case goodput given protocol overhead (NOTIFY, ACK, PULL_REQUEST)?
- How does the design quantify this?
- What conditions must hold to achieve near-100%?

**Impact:** High - Understanding goal feasibility.

---

### 30. Microsecond-Level Latency

**Issue:** Busy polling provides "microsecond-level timing precision" but end-to-end latency unclear.

**Questions:**

- What's the expected end-to-end latency for high-priority small transfers?
- How does this compare to current system?
- What are the latency percentiles under load?

**Impact:** Medium - Performance goal validation.

---

## Security Concerns

### 31. Ephemeral Data Key Distribution

**Issue:** `TRANSFER_NOTIFY` includes ephemeral data key (presumably encrypted at transport level).

**Questions:**

- How is this key generated? (per-transfer, cryptographically secure?)
- Size and algorithm?
- How is replay protection achieved if keys are ephemeral?
- Could an attacker inject a `TRANSFER_NOTIFY` with their own key?

**Impact:** High - Security critical.

---

### 32. Buffer ID Collision Attacks

**Issue:** 64-bit buffer IDs with security implications.

**Questions:**

- Could an attacker send `PULL_REQUEST` with guessed buffer IDs?
- How does AAD binding prevent unauthorized access to buffers?
- What prevents replay of legitimate `TRANSFER_NOTIFY` messages?

**Impact:** Medium - Security hardening.

---

### 33. Control Message Authentication Scope

**Issue:** Control messages authenticated separately from data.

**Questions:**

- Does this enable any attacks where control and data are mixed?
- How are control message sequence numbers managed to prevent replay?
- What prevents control message reordering attacks?

**Impact:** Medium - Security architecture clarity.

---

## Summary and Recommendations

### High Priority Items (13)

Critical for implementation readiness:

1. Application-Worker Communication Channel (#1)
2. Causality Map Sizing and Exhaustion (#2)
3. Receiver State Management (#3)
4. Transfer Identifier Generation and Scoping (#4)
5. Key Rotation Mechanics (#7)
6. Replay Protection Implementation (#8)
7. Buffer Registry Synchronization (#9)
8. Bandwidth Configuration and Enforcement (#10)
9. Failure and Recovery Scenarios (#11)
10. End-to-End Backpressure Details (#13)
11. Transfer Timeout Handling (#17)
12. Buffer Management Responsibility Split (#20)
13. 10,000+ Concurrent Streams Scalability (#28)

### Medium Priority Items (14)

Important for complete design:

- Timing Wheel Parameters (#5)
- Priority Queue Starvation Prevention (#6)
- EFA vs UDP Behavioral Differences (#12)
- Small Transfer Optimization (#14)
- Congestion Control Specifics (#15)
- Path Secret Map Integration (#16)
- Worker Core Assignment Ambiguity (#18)
- Causality Token Ownership (#19)
- Request/Response Header Support (#21)
- Bidirectional Transfer Implementation (#22)
- Streaming Transfer Lifecycle (#23)
- Raw Bulk Transfer Details (#24)
- Event Framework Specifics (#25)
- NUMA Awareness (#27)

### Security Items (3)

- Ephemeral Data Key Distribution (#31)
- Buffer ID Collision Attacks (#32)
- Control Message Authentication Scope (#33)

### Other Items (3)

- GSO Usage (#26)
- 100% Goodput Target (#29)
- Microsecond-Level Latency (#30)

---

## Next Steps

1. Review and discuss each high-priority item
2. Make decisions on design choices for critical missing details
3. Update main design document with clarifications
4. Address medium-priority items that impact implementation
5. Document security model more explicitly
6. Add concrete performance targets and capacity planning guidelines
