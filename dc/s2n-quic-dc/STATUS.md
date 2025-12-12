# dcQUIC v2 Implementation Status

This document tracks the implementation status of the dcQUIC v2 design as outlined in the [design specification](https://gist.githubusercontent.com/camshaft/de91fef78f485814cd7cdc3b4829d5f6/raw/15171708bd9fa5acb0ed5064e025f231c7e1d183/README.md).

**Note:** Backward compatibility with the current implementation is explicitly not a goal.

## Overview

The dcQUIC v2 design addresses fundamental performance and scalability limitations through:
- Receiver-driven protocol
- Explicit transfer semantics
- Dedicated worker architecture with busy polling
- Multi-NIC support
- Causality tracking system
- Priority-based scheduling

### Target Metrics
- Support 10,000+ concurrent streams to hundreds of peers
- Achieve near 100% goodput (vs. current 20%)
- Utilize up to 8 NICs providing 3.2 Tbps aggregate bandwidth
- Scale to 192 CPU cores

## Requirements Status

### Performance Requirements
- [ ] Support burst transmission of 10,000+ concurrent streams
- [ ] Maintain near 100% goodput under burst conditions
- [ ] Utilize all available NICs (up to 8 NICs)
- [ ] Scale to 192 CPU cores
- [ ] Handle aggregate bandwidth up to 3.2 Tbps
- [ ] Minimize latency for high-priority transfers
- [ ] Support dedicated CPU cores with busy polling

### Protocol Features
- [ ] Unary RPC (single request, single response)
- [ ] Streaming response (single request, multiple responses)
- [ ] Streaming request (multiple requests, single response)
- [ ] Bidirectional (multiple requests and responses)
- [ ] Raw bulk transfer (direct memory/disk placement with cache teeing)
- [ ] Optional request and response headers for all patterns
- [ ] Priority-based scheduling across all transfers
- [ ] Causality tracking with critical dependencies (failure propagates)
- [ ] Causality tracking with optional dependencies (failure does not propagate)
- [ ] Automatic retry for transient failures
- [ ] Graceful handling of permanent dependency failures
- [ ] Broadcast transfers (identical payload to multiple peers)
- [ ] Single encryption for broadcast with payload reuse
- [ ] RDMA support with single buffer allocation for multiple peers

### Transport Support
- [ ] EFA via libfabric (primary transport for EC2)
- [ ] UDP fallback on all platforms
- [ ] macOS support for developer workstations
- [ ] Bach simulator integration for deterministic testing

### Security Requirements
- [ ] Maintain existing path secret map mechanism
- [ ] Replay protection without per-stream overhead
- [ ] Key rotation without interrupting active transfers
- [ ] Minimize key derivation overhead vs. per-stream approach

### End-to-End Backpressure
- [ ] Backpressure propagation from slow receivers
- [ ] Prevent unbounded message buffer buildup
- [ ] Application-level flow control integration

### Operational Requirements
- [ ] Configurable dedicated core count at initialization
- [ ] Core pinning to specific CPU IDs
- [ ] Kernel scheduler exclusion for dedicated cores
- [ ] Graceful handling of missing EFA capability
- [ ] Graceful degradation on resource unavailability

### Events and Observability
- [ ] Event emission to application-provided subscribers
- [ ] Deep visibility into system operations
- [ ] Metrics derivation from events

## Core Components

### 1. Transport Pool
**Status:** Not Started

The single entry point for all application transfer requests.

#### Tasks
- [ ] Design Arc-based cloneable pool structure
- [ ] Implement thread-safe concurrent access
- [ ] Create distinct APIs for each transfer type:
  - [ ] unary_rpc()
  - [ ] streaming_response()
  - [ ] streaming_request()
  - [ ] bidirectional()
  - [ ] raw_bulk_transfer()
- [ ] Implement transfer options (priority, causality dependencies)
- [ ] Create ring buffers for app-worker communication (per priority level)
- [ ] Implement backpressure handling with wakers
- [ ] Design response delivery mechanisms:
  - [ ] Oneshot channels for single responses
  - [ ] Async streams for streaming responses
  - [ ] Separate channels for headers
- [ ] Implement buffer allocation and encryption interface
- [ ] Add opt-out encryption for pre-encrypted data

### 2. Request Router
**Status:** Not Started

Maps transfer requests to NIC workers and manages causality token allocation.

#### Tasks
- [ ] Design lock-free or minimal-locking routing structure
- [ ] Implement flow hashing for peer-to-worker assignment
- [ ] Create causality token allocation system
- [ ] Implement load-aware routing for new peer assignments
- [ ] Add stable peer assignment (all transfers to peer use same worker)
- [ ] Handle NIC failure and worker reassignment
- [ ] Implement routing metrics and monitoring

### 3. NIC Worker
**Status:** Not Started

Manages all transfer activity for a specific network interface with dedicated cores.

#### Tasks
- [ ] Design single-threaded busy polling loop
- [ ] Implement CPU core pinning
- [ ] Add kernel scheduler exclusion support
- [ ] Create non-blocking socket I/O operations
- [ ] Implement continuous polling loop:
  - [ ] Poll UDP socket for incoming packets
  - [ ] Process received packets
  - [ ] Advance timing wheel
  - [ ] Check priority queue for ready transfers
  - [ ] Write outgoing packets
- [ ] Add separate receive and transmit buffers
- [ ] Implement bandwidth configuration and traffic shaping
- [ ] Add receiver-side bandwidth control
- [ ] Create worker health monitoring (iteration counter)

### 4. Timing Wheel
**Status:** Not Started

Schedules future events with low overhead for high-frequency operations.

#### Tasks
- [ ] Design hierarchical timing wheel structure
- [ ] Implement microsecond-granularity timing
- [ ] Add event scheduling interface
- [ ] Create event firing mechanism
- [ ] Integrate with worker polling loop
- [ ] Optimize for high-frequency timer operations

### 5. Priority Queue
**Status:** Not Started

Schedules transfers based on priority and readiness.

#### Tasks
- [ ] Design multi-level priority queue structure
- [ ] Implement priority-based dequeuing
- [ ] Add transfer readiness tracking
- [ ] Integrate causality dependency checking
- [ ] Create queue depth monitoring
- [ ] Implement backpressure signaling

### 6. Causality Map
**Status:** Not Started

Tracks dependencies between transfers with lock-free operations.

#### Tasks
- [ ] Design growable slot array structure
- [ ] Implement atomic operations for dependency tracking
- [ ] Create critical dependency support (failure propagates)
- [ ] Add optional dependency support (failure isolated)
- [ ] Implement slot allocation and management
- [ ] Add slot reuse after transfer completion
- [ ] Create DAG enforcement (prevent cycles)
- [ ] Implement cycle detection algorithm
- [ ] Add dependency resolution on completion
- [ ] Handle failure propagation for critical dependencies
- [ ] Create causality token API

### 7. Receiver-Driven Protocol
**Status:** Not Started

Core protocol implementation with receiver control.

#### Tasks
- [ ] Design TRANSFER_NOTIFY message format
- [ ] Implement PULL_REQUEST message handling
- [ ] Create DATA_CHUNKS transmission
- [ ] Add RANGE_ACK message processing
- [ ] Implement receiver state management
- [ ] Design transfer identifier system (64-bit counter + generation)
- [ ] Add replay protection window
- [ ] Implement challenge-response mechanism:
  - [ ] TRANSFER_CHALLENGE message
  - [ ] TRANSFER_CHALLENGE_RESPONSE message
- [ ] Create replay tracking data structures
- [ ] Add inline data support in TRANSFER_NOTIFY

### 8. Encryption Key Pool
**Status:** Not Started

Shared key pool to minimize derivation overhead.

#### Tasks
- [ ] Design key pool structure with per-generation keys
- [ ] Implement control key management
- [ ] Add data key management
- [ ] Create key rotation mechanism
- [ ] Implement key retirement:
  - [ ] RETIRE_CONTROL_KEY message
  - [ ] RETIRE_DATA_KEY message
- [ ] Add nonce management (per-key counter)
- [ ] Create key derivation from path secret
- [ ] Implement key caching on receiver side
- [ ] Add UNKNOWN_PATH_SECRET handling
- [ ] Design key pool sizing and cleanup

### 9. Multi-NIC Load Distribution
**Status:** Not Started

Distribute transfers across multiple NICs.

#### Tasks
- [ ] Implement NIC capability detection
- [ ] Create per-NIC worker instantiation
- [ ] Add flow-based routing (consistent peer assignment)
- [ ] Implement load-aware NIC selection
- [ ] Create NIC failure detection and failover
- [ ] Add bandwidth monitoring per NIC
- [ ] Design NIC health state tracking

### 10. Transfer State Management
**Status:** Not Started

Track lifecycle of each transfer.

#### Tasks
- [ ] Design transfer state structure
- [ ] Implement state machine for transfer lifecycle
- [ ] Add timeout and retry logic with exponential backoff
- [ ] Create maximum retry limit handling
- [ ] Implement transfer completion tracking
- [ ] Add transfer cancellation support
- [ ] Create transfer metrics (latency, retries, etc.)

### 11. Buffer Management
**Status:** Not Started

Manage memory buffers and lifecycle for transfers.

#### Tasks
- [ ] Design buffer registry with slot map structure
- [ ] Implement reference counting for buffers
- [ ] Create buffer allocation interface
- [ ] Add range-based deallocation
- [ ] Implement RDMA memory registration (for EFA)
- [ ] Add buffer pool with configurable size limits
- [ ] Create transfer independence (separate priorities/causality per buffer)
- [ ] Implement buffer lifecycle tracking
- [ ] Add buffer cleanup on all references released

### 12. Peer Health Monitoring
**Status:** Not Started

Track peer health and handle failures gracefully.

#### Tasks
- [ ] Design health state machine (healthy/degraded/probing/failed)
- [ ] Implement peer liveness monitoring (PROBE/PROBE_RESPONSE)
- [ ] Add last-activity timestamp tracking
- [ ] Create failure rate tracking (EWMA)
- [ ] Implement state transitions based on metrics
- [ ] Add backoff in degraded state
- [ ] Create probe scheduling in probing state
- [ ] Implement application thread failure handling (RAII cleanup)
- [ ] Add worker health monitoring (iteration counter check)
- [ ] Implement broadcast failure handling (per-stream independence)
- [ ] Add ICMP signal processing (Destination Unreachable, etc.)
- [ ] Create health state event emission

### 13. Flow Control Messages
**Status:** Not Started

Implement flow control protocol messages.

#### Tasks
- [ ] Implement CANCEL_PULL message
- [ ] Add FLOW_RESET message
- [ ] Create cancellation handling (with in-flight awareness)
- [ ] Implement flow reset on transfer ID mismatch

### 14. Streaming Lifecycle
**Status:** Not Started

Manage stream lifecycle with reliable closure.

#### Tasks
- [ ] Implement STREAM_CLOSE message
- [ ] Add STREAM_CLOSE_ACK message
- [ ] Create status code for closure reasons
- [ ] Implement sender-initiated closure
- [ ] Add receiver-initiated closure
- [ ] Create automatic cancellation of active transfers on closure
- [ ] Implement reliable closure with acknowledgment

### 15. Congestion Control
**Status:** Not Started

Integrate BBR congestion control for bandwidth management.

#### Tasks
- [ ] Integrate BBR congestion controller per peer
- [ ] Implement transmission pacing
- [ ] Add bandwidth probe mechanism
- [ ] Create congestion state tracking
- [ ] Implement ECN marking support
- [ ] Add congestion event emission for observability

### 16. Bach Simulator Integration
**Status:** Not Started

Support deterministic testing via Bach simulator.

#### Tasks
- [ ] Add simulation mode detection
- [ ] Implement simulated timers (vs. actual time)
- [ ] Create simulated socket I/O
- [ ] Disable core pinning in simulation mode
- [ ] Ensure protocol logic remains unchanged
- [ ] Add simulator-specific test harness

### 17. Cross-Platform UDP Support
**Status:** Not Started

Universal UDP fallback for all platforms.

#### Tasks
- [ ] Implement UDP socket configuration
- [ ] Add large socket buffer allocation
- [ ] Enable GSO (Generic Segmentation Offload) where available
- [ ] Add ECN marking support
- [ ] Create platform-specific optimizations
- [ ] Implement macOS-specific support
- [ ] Add Linux-specific optimizations
- [ ] Abstract platform differences behind common interface

### 18. EFA/libfabric Integration
**Status:** Not Started

Primary high-performance transport for EC2.

#### Tasks
- [ ] Add EFA capability detection at initialization
- [ ] Integrate libfabric library
- [ ] Implement memory region registration for RDMA
- [ ] Add RDMA read operations for receivers
- [ ] Create memory key inclusion in TRANSFER_NOTIFY
- [ ] Implement fallback to UDP when EFA unavailable
- [ ] Add EFA-specific error handling
- [ ] Create EFA performance monitoring

### 19. Core Allocation and Pinning
**Status:** Not Started

Dedicated core management for workers.

#### Tasks
- [ ] Implement core allocation during pool initialization
- [ ] Add CPU core assignment based on NUMA topology
- [ ] Create core pinning using sched_setaffinity (Linux)
- [ ] Add kernel scheduler exclusion (isolcpus/cgroup cpuset)
- [ ] Implement worker-to-core mapping
- [ ] Add configuration for core count and IDs
- [ ] Create graceful handling of unavailable cores

### 20. Scalability and Configuration
**Status:** Not Started

Configurable limits for deployment-specific tuning.

#### Tasks
- [ ] Add causality map sizing configuration (initial/maximum)
- [ ] Implement buffer pool size limits
- [ ] Create worker queue depth configuration per priority
- [ ] Add per-peer concurrent transfer limits
- [ ] Implement backpressure on limit approach
- [ ] Create configuration validation
- [ ] Add default configurations for common scenarios
- [ ] Ensure 64-bit identifiers/counters (prevent overflow)

### 21. Monitoring and Observability
**Status:** Not Started

Comprehensive event and metrics framework.

#### Tasks
- [ ] Implement event framework for instrumentation
- [ ] Create worker metrics (queue depth, bandwidth, congestion, errors)
- [ ] Add transfer lifecycle events
- [ ] Implement protocol message exchange events
- [ ] Create congestion control action events
- [ ] Add error condition events
- [ ] Implement tracing integration
- [ ] Use causality tokens as correlation IDs
- [ ] Create metrics aggregation
- [ ] Add application-provided subscriber support

## Testing

### Unit Tests
- [ ] Transport pool tests
- [ ] Router tests
- [ ] Worker tests (simulated)
- [ ] Timing wheel tests
- [ ] Priority queue tests
- [ ] Causality map tests
- [ ] Encryption key pool tests
- [ ] Buffer management tests
- [ ] Peer health state machine tests
- [ ] Protocol message encoding/decoding tests

### Integration Tests
- [ ] End-to-end transfer tests
- [ ] Multi-NIC utilization tests
- [ ] Causality dependency tests
- [ ] Broadcast transfer tests
- [ ] Failure and recovery tests
- [ ] Congestion control tests
- [ ] Backpressure tests
- [ ] Stream lifecycle tests

### Performance Tests
- [ ] 10,000+ concurrent stream tests
- [ ] Goodput measurement tests
- [ ] Multi-NIC bandwidth tests
- [ ] Latency measurement tests
- [ ] Scalability tests (CPU/memory)
- [ ] Load testing under burst conditions

### Bach Simulator Tests
- [ ] Deterministic scenario tests
- [ ] Race condition reproduction tests
- [ ] Timing-dependent behavior tests
- [ ] Failure injection tests

## Documentation

- [ ] API documentation for Transport Pool
- [ ] Transfer patterns usage guide
- [ ] Configuration guide
- [ ] Deployment guide (EFA setup, core allocation)
- [ ] Monitoring and observability guide
- [ ] Migration guide (from v1, noting breaking changes)
- [ ] Architecture documentation
- [ ] Protocol specification document
- [ ] Performance tuning guide

## Current Phase

**Phase:** Planning and Design Review

**Next Steps:**
1. Review and validate this status document
2. Prioritize components for initial implementation
3. Set up development environment and build infrastructure
4. Begin implementation with foundational components

## Implementation Strategy

### Phase 1: Foundation (Recommended Order)
1. Transport Pool (basic structure)
2. Request Router (basic routing)
3. NIC Worker (basic polling loop)
4. Transfer State Management (basic lifecycle)

### Phase 2: Core Protocol
1. Receiver-Driven Protocol (messages)
2. Encryption Key Pool
3. Buffer Management
4. Flow Control Messages

### Phase 3: Advanced Features
1. Causality Map
2. Priority Queue
3. Timing Wheel
4. Multi-NIC Load Distribution

### Phase 4: Reliability
1. Peer Health Monitoring
2. Congestion Control
3. Streaming Lifecycle
4. End-to-End Backpressure

### Phase 5: Platform Support
1. UDP Transport (all platforms)
2. EFA/libfabric Integration
3. Bach Simulator Integration
4. Core Allocation and Pinning

### Phase 6: Scale and Polish
1. Scalability Configuration
2. Monitoring and Observability
3. Performance Optimization
4. Testing and Documentation

## Notes

- All work items should be tracked and updated in this document
- Breaking changes from v1 are acceptable and expected
- Performance targets must be validated through testing
- Platform-specific code should be isolated behind abstractions
