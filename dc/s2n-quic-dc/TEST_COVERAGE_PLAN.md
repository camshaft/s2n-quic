# Test Coverage Plan for UDP Send Path Improvements

## Overview

This document provides a comprehensive test coverage plan for the UDP send path improvements outlined in `udp-improvements.md`. Each work item includes:
- **Status**: Current implementation state
- **Test Strategy**: What needs to be tested
- **Test Suite Plan**: Specific tests to implement
- **Assigned To**: (TBD - to be filled in during team planning)

---

## Work Item 1: Pool Integration in Environment Layer

**Status**: 🔴 Not Started

**Description**: Add a `Pool` instance to the environment layer that will be shared across all streams created from that environment.

**Files Affected**:
- `dc/s2n-quic-dc/src/stream/environment/udp.rs`
- `dc/s2n-quic-dc/src/stream/environment/tokio/pool.rs`
- `dc/s2n-quic-dc/src/stream/environment/bach/pool.rs`

### Test Strategy

The pool integration needs tests to verify:
1. Pool is correctly created and shared across streams
2. Pool configuration (max_packet_size, packet_count) is respected
3. Pool lifecycle is tied to environment lifecycle
4. Multiple streams can allocate from the same pool concurrently

### Test Suite Plan

#### Unit Tests

**Test: `test_pool_creation_with_config`**
- Create environment with specific pool config
- Verify pool is created with correct parameters
- Assert pool is not None
- Check max_packet_size and packet_count match config

**Test: `test_pool_shared_across_streams`**
- Create environment with pool
- Create multiple streams from the same environment
- Verify all streams reference the same pool (Arc pointer comparison)
- Allocate from different streams and verify they share the pool

**Test: `test_pool_lifecycle_tied_to_environment`**
- Create environment with pool
- Create weak reference to pool
- Drop environment
- Verify pool is dropped when environment is dropped

#### Integration Tests

**Test: `test_concurrent_pool_allocation`**
- Create environment with pool
- Spawn multiple streams
- Have each stream allocate descriptors concurrently
- Verify no allocation conflicts
- Verify all allocations succeed up to pool capacity

**Test: `test_pool_exhaustion_behavior`**
- Create environment with small pool (e.g., 10 descriptors)
- Create stream and allocate all descriptors
- Attempt to allocate one more
- Verify pool grows or returns appropriate error
- Verify pool recovery after descriptors are freed

**Assigned To**: _________

---

## Work Item 2: Stream Completion Queue Setup

**Status**: 🔴 Not Started

**Description**: Each stream needs its own completion queue that receives transmissions after the socket worker has sent them.

**Files Affected**:
- `dc/s2n-quic-dc/src/stream/send/queue.rs`
- `dc/s2n-quic-dc/src/stream/send/state.rs`
- `dc/s2n-quic-dc/src/stream/send/worker.rs`

### Test Strategy

Completion queues need tests for:
1. Queue creation and initialization per stream
2. Weak reference handling (no memory leaks)
3. Completion pushing from socket worker
4. Completion processing in stream worker
5. Queue behavior when stream is destroyed

### Test Suite Plan

#### Unit Tests

**Test: `test_completion_queue_creation`**
- Create stream send state
- Verify completion queue is created
- Verify it's an Arc<intrusive_queue>
- Create weak reference and verify it works

**Test: `test_weak_reference_prevents_leak`**
- Create stream with completion queue
- Create transmission with weak reference to queue
- Drop stream
- Verify transmission can't push to queue (weak upgrade fails)
- Verify no memory leak

**Test: `test_completion_queue_fifo_order`**
- Create completion queue
- Push multiple completed transmissions
- Poll completions
- Verify FIFO order is maintained

#### Integration Tests

**Test: `test_completion_flow_end_to_end`**
- Create stream
- Create transmission entry with completion queue weak ref
- Simulate socket worker completing transmission
- Push to completion queue via weak ref
- Poll stream worker
- Verify completion is processed

**Test: `test_concurrent_completion_processing`**
- Create multiple streams with their own completion queues
- Create transmissions for each stream
- Complete transmissions concurrently
- Verify each stream processes only its own completions

**Test: `test_completion_queue_with_rtt_tracking`**
- Create transmission with timestamp
- Complete transmission
- Process completion
- Verify RTT is calculated correctly
- Verify RTT stats are updated

**Assigned To**: _________

---

## Work Item 3: Replace Queue with Wheel-Based Transmission

**Status**: 🔴 Not Started

**Description**: Transform the queue from a buffering mechanism into a wheel insertion mechanism, removing direct socket flushing.

**Files Affected**:
- `dc/s2n-quic-dc/src/stream/send/queue.rs`
- `dc/s2n-quic-dc/src/socket/send/wheel.rs`
- `dc/s2n-quic-dc/src/socket/send/udp.rs`

### Test Strategy

Critical to verify:
1. VecDeque is completely removed
2. All transmissions go through wheel
3. Wheel entries are created correctly
4. Timestamp-based scheduling works
5. No direct socket writes from application layer

### Test Suite Plan

#### Unit Tests

**Test: `test_queue_removed_vecdeque`**
- Verify Queue struct no longer has VecDeque field (compile-time)
- Verify no push/pop queue operations exist

**Test: `test_transmission_creates_wheel_entry`**
- Create buffer to send
- Call push method
- Verify wheel entry is created
- Verify entry has correct descriptor, metadata, timestamp

**Test: `test_wheel_entry_has_completion_queue_ref`**
- Create transmission
- Verify wheel entry has weak reference to completion queue
- Verify weak reference is valid

**Test: `test_pacing_timestamp_calculation`**
- Create transmission with known bandwidth
- Verify timestamp = now + (packet_size / bandwidth)
- Test with different packet sizes and bandwidths

#### Integration Tests

**Test: `test_no_direct_socket_writes`**
- Create stream
- Push data to send
- Verify data is NOT directly written to socket
- Verify data IS in wheel
- Manually trigger socket worker
- Verify data is then sent

**Test: `test_wheel_based_transmission_end_to_end`**
- Create stream
- Push multiple packets
- Verify all enter wheel with correct timestamps
- Advance time and trigger socket worker
- Verify packets sent in timestamp order
- Verify completions received

**Test: `test_accepted_len_removed`**
- Verify accepted_len tracking is removed from Queue
- Verify flow control is now based on pending_transmissions

**Assigned To**: _________

---

## Work Item 4: Refactor msg/send.rs to Use Descriptor Pool

**Status**: 🔴 Not Started

**Description**: Replace custom allocator in Message struct with pool-based descriptor allocation.

**Files Affected**:
- `dc/s2n-quic-dc/src/msg/send.rs`
- `dc/s2n-quic-dc/src/socket/pool/descriptor.rs`
- `dc/s2n-quic-dc/src/allocator.rs`

### Test Strategy

Need comprehensive tests for:
1. Custom allocator removal is complete
2. Descriptor pool integration works correctly
3. Segment and Retransmission types work with descriptors
4. Memory lifecycle is correct (no leaks)
5. Retransmission copying works

### Test Suite Plan

#### Unit Tests

**Test: `test_custom_allocator_removed`**
- Verify Message has no buffers: Vec<Vec<u8>> field
- Verify no free/pending_free vectors
- Verify Message has Pool reference

**Test: `test_segment_wraps_descriptor`**
- Create Segment
- Verify it wraps descriptor::Unfilled during filling
- Fill segment
- Verify it becomes descriptor::Filled

**Test: `test_retransmission_wraps_descriptor`**
- Create transmission
- Create retransmission handle
- Verify it wraps descriptor::Filled
- Verify retransmission can be copied

**Test: `test_descriptor_reference_counting`**
- Allocate descriptor
- Create multiple references
- Drop references one by one
- Verify descriptor is freed when last reference drops

#### Integration Tests

**Test: `test_pool_based_allocation_end_to_end`**
- Create Message with pool
- Allocate multiple segments
- Fill segments
- Verify pool provides descriptors
- Free segments
- Verify descriptors return to pool

**Test: `test_retransmission_copy`**
- Create transmission
- Mark for retransmission
- Call retransmit_copy
- Verify new descriptor is allocated
- Verify payload is copied
- Verify original and copy are independent

**Test: `test_no_direct_sending`**
- Verify send() and send_with() methods are removed
- Verify only wheel insertion methods exist

**Test: `test_memory_leak_detection`**
- Create many transmissions
- Drop without sending
- Verify all descriptors return to pool
- Verify pool size is stable

**Assigned To**: _________

---

## Work Item 5: Remove Pacer and Implement Flow-Based Backpressure

**Status**: 🔴 Not Started

**Description**: Delete naive pacer and implement flow control based on pending transmission tracking.

**Files Affected**:
- `dc/s2n-quic-dc/src/stream/pacer.rs` (to be deleted)
- `dc/s2n-quic-dc/src/stream/send/flow.rs`
- `dc/s2n-quic-dc/src/stream/send/worker.rs`

### Test Strategy

Must verify:
1. Pacer is completely removed
2. Pending transmission tracking works
3. Flow control blocks when limit reached
4. Completions unblock flow control
5. Waker registration and notification

### Test Suite Plan

#### Unit Tests

**Test: `test_pacer_removed`**
- Verify stream/pacer.rs file is deleted
- Verify no pacer field in worker
- Verify no poll_pacing calls

**Test: `test_pending_transmissions_tracking`**
- Create flow controller
- Increment pending_transmissions
- Verify counter increments correctly
- Decrement pending_transmissions
- Verify counter decrements correctly

**Test: `test_flow_control_blocks_at_limit`**
- Set max_in_flight_transmissions = 10
- Create 10 transmissions (pending = 10)
- Request flow credits
- Verify Poll::Pending is returned

**Test: `test_flow_control_unblocks_on_completion`**
- Fill pending_transmissions to limit
- Request flow credits (returns Pending)
- Complete one transmission
- Request flow credits again
- Verify Poll::Ready is returned

**Test: `test_waker_registration`**
- Block on flow control
- Verify waker is registered
- Complete transmission
- Verify waker is called

#### Integration Tests

**Test: `test_backpressure_end_to_end`**
- Create stream with small in-flight limit
- Write data rapidly
- Verify writes block when limit reached
- Verify writes resume when completions happen

**Test: `test_no_artificial_yielding`**
- Remove all yield_now() calls
- Verify pacing is purely timestamp-based
- Verify no unnecessary task yields

**Test: `test_completion_processing_before_transmission`**
- Queue multiple transmissions
- Have completions pending
- Poll worker
- Verify completions are processed first
- Then new transmissions are created

**Assigned To**: _________

---

## Work Item 6: Update Worker Transmission Logic

**Status**: 🔴 Not Started

**Description**: Change worker from direct socket sending to wheel coordination and completion processing.

**Files Affected**:
- `dc/s2n-quic-dc/src/stream/send/worker.rs`
- `dc/s2n-quic-dc/src/stream/send/state.rs`

### Test Strategy

Worker changes need tests for:
1. Application queue flushing is removed
2. Transmit queue creates wheel entries
3. Completion processing works correctly
4. Pool reference is available
5. State coordination works

### Test Suite Plan

#### Unit Tests

**Test: `test_application_queue_flushing_removed`**
- Verify no direct queue flushing in Detached state
- Verify no poll_send calls on socket

**Test: `test_transmit_queue_creates_wheel_entries`**
- Call fill_transmit_queue
- Verify wheel entries are created
- Verify no direct sends

**Test: `test_completion_processing_in_poll`**
- Add completion to queue
- Poll worker
- Verify completion is processed
- Verify flow credits updated

**Test: `test_pool_reference_available`**
- Create worker with pool
- Verify pool is accessible
- Use pool for allocation
- Verify allocation works

#### Integration Tests

**Test: `test_worker_orchestration_flow`**
- Create worker
- Add data to send
- Poll worker
- Verify: completions processed → state updated → wheel entries created
- Verify correct ordering

**Test: `test_snapshot_apply_with_pending_transmissions`**
- Create snapshot with changes
- Apply snapshot
- Verify pending_transmissions is checked
- Verify credits only released on completion

**Test: `test_worker_lifecycle_with_pool`**
- Create worker with pool
- Create transmissions
- Destroy worker
- Verify pool descriptors are freed

**Assigned To**: _________

---

## Work Item 7: Update State's Transmission Creation

**Status**: 🔴 Not Started

**Description**: Replace todo!() calls with actual pool allocation and create wheel entries instead of direct transmit queue pushes.

**Files Affected**:
- `dc/s2n-quic-dc/src/stream/send/state.rs`
- `dc/s2n-quic-dc/src/stream/send/state/transmission.rs`

### Test Strategy

State transmission creation needs tests for:
1. Pool allocation works (no todo!() panics)
2. Descriptor filling is correct
3. Wheel entry creation
4. Retransmission storage
5. GSO packet handling

### Test Suite Plan

#### Unit Tests

**Test: `test_pool_allocation_no_todo`**
- Call fill_transmit_queue
- Verify no panic (no todo!() calls)
- Verify descriptors are allocated

**Test: `test_descriptor_filling`**
- Create transmission
- Verify descriptor is filled with packet data
- Verify headers are correct
- Verify payload is correct

**Test: `test_wheel_entry_creation_from_state`**
- Call fill_transmit_queue
- Verify wheel Entry is created
- Verify Entry has Transmission<Vec<Info>>
- Verify timestamp is calculated

**Test: `test_retransmission_storage`**
- Create transmission
- Verify retransmission info is stored
- Verify info contains descriptor::Filled handle
- Mark for retransmission
- Verify descriptor is accessible

#### Integration Tests

**Test: `test_transmission_creation_end_to_end`**
- Create state with pool
- Add data to send
- Call fill_transmit_queue
- Verify wheel entries created
- Verify completion queue reference attached
- Verify pacing timestamp calculated

**Test: `test_gso_packet_transmission`**
- Enable GSO
- Create transmission with multiple packets
- Verify Vec<Info> created with multiple entries
- Verify each packet has correct metadata

**Test: `test_recovery_logic_with_descriptors`**
- Create transmission
- Mark packet as lost
- Trigger retransmission
- Verify retransmit_copy is called
- Verify new descriptor created
- Verify new wheel entry created

**Assigned To**: _________

---

## Work Item 8: Integrate CCA Pacing with Wheel

**Status**: 🔴 Not Started

**Description**: Use congestion controller bandwidth estimate to calculate pacing timestamps for wheel entries.

**Files Affected**:
- `dc/s2n-quic-dc/src/stream/send/state.rs`
- `dc/s2n-quic-dc/src/stream/send/queue.rs`
- `quic/s2n-quic-core/src/recovery/bandwidth.rs`

### Test Strategy

CCA integration needs tests for:
1. Bandwidth extraction from CCA
2. Pacing interval calculation
3. Timestamp calculation
4. Burst handling
5. Rate adjustment on ACK/loss

### Test Suite Plan

#### Unit Tests

**Test: `test_bandwidth_extraction`**
- Create CCA with known bandwidth
- Extract bandwidth
- Verify correct value retrieved

**Test: `test_pacing_interval_calculation`**
- Set bandwidth = 100 Mbps
- Set packet_size = 1500 bytes
- Calculate interval
- Verify interval = 1500 * 8 / 100_000_000 seconds

**Test: `test_transmission_timestamp_calculation`**
- Get current time T
- Calculate interval I
- Verify transmission_time = T + I

**Test: `test_burst_allowance`**
- Set burst size = 10
- Create 10 packets
- Verify first packet: timestamp = now
- Verify packet N: timestamp = now + (interval * N / 10)

**Test: `test_minimum_pacing_interval`**
- Set very high bandwidth (100 Gbps)
- Calculate interval
- Verify interval >= 1 microsecond

#### Integration Tests

**Test: `test_cca_pacing_end_to_end`**
- Create stream with CCA
- Send multiple packets
- Verify wheel entries have staggered timestamps
- Verify spacing matches bandwidth

**Test: `test_rate_adjustment_on_ack`**
- Send packet with timestamp T1 based on rate R1
- Receive ACK (CCA increases rate to R2)
- Send next packet
- Verify timestamp uses R2 (shorter interval)

**Test: `test_rate_adjustment_on_loss`**
- Send packet with timestamp T1 based on rate R1
- Detect loss (CCA decreases rate to R2)
- Send next packet
- Verify timestamp uses R2 (longer interval)

**Test: `test_no_rescheduling_in_wheel`**
- Create transmission with timestamp T1
- CCA changes rate
- Verify transmission keeps timestamp T1 (no rescheduling)

**Assigned To**: _________

---

## Work Item 9: Configure Transmission Limits

**Status**: 🔴 Not Started

**Description**: Add max_in_flight_transmissions configuration parameter to tune flow control.

**Files Affected**:
- `dc/s2n-quic-dc/src/stream/environment/udp.rs`
- `dc/s2n-quic-dc/src/stream/send/flow.rs`

### Test Strategy

Configuration needs tests for:
1. Parameter is added to Config
2. Default value is reasonable
3. Parameter is passed to flow controller
4. Flow control respects the limit
5. Different values have expected effects

### Test Suite Plan

#### Unit Tests

**Test: `test_config_parameter_exists`**
- Create Config
- Verify max_in_flight_transmissions field exists
- Verify default value (16 or 32)

**Test: `test_config_passed_to_flow_controller`**
- Create Config with max_in_flight = 20
- Create flow controller
- Verify flow controller has limit = 20

**Test: `test_flow_controller_respects_limit`**
- Set max_in_flight = 5
- Create 5 transmissions
- Request credits
- Verify blocked

#### Integration Tests

**Test: `test_small_limit_blocks_sooner`**
- Create two streams: one with limit=5, one with limit=20
- Send data on both
- Verify limit=5 stream blocks after 5 packets
- Verify limit=20 stream continues

**Test: `test_large_limit_higher_throughput`**
- Create high-latency simulated connection
- Test with limit=16 and limit=64
- Measure throughput
- Verify limit=64 has higher throughput on high-BDP link

**Test: `test_limit_tuning_for_rtt`**
- Test connections with RTT 10ms, 100ms, 1000ms
- Test different limits
- Document optimal limits for each RTT

**Assigned To**: _________

---

## Work Item 10: Handle Retransmissions with Copying

**Status**: 🔴 Not Started

**Description**: Implement always-copy-on-retransmit strategy using descriptor pool.

**Files Affected**:
- `dc/s2n-quic-dc/src/msg/send.rs`
- `dc/s2n-quic-dc/src/stream/send/state.rs`

### Test Strategy

Retransmission tests need to verify:
1. Copying always occurs
2. Original and copy are independent
3. Payload is correctly copied
4. GSO retransmissions only retransmit lost packets
5. Memory management is correct

### Test Suite Plan

#### Unit Tests

**Test: `test_retransmit_always_copies`**
- Create transmission
- Call retransmit_copy
- Verify new descriptor allocated
- Verify payload copied
- Modify original
- Verify copy unchanged

**Test: `test_retransmit_copy_implementation`**
- Create descriptor with known payload
- Call retransmit_copy
- Verify new descriptor has same payload
- Verify same remote address
- Verify same metadata

**Test: `test_gso_single_packet_retransmission`**
- Create GSO transmission with 5 packets
- Mark packet 3 as lost
- Retransmit
- Verify only packet 3 is retransmitted
- Verify single-packet transmission created

**Test: `test_retransmission_descriptor_lifecycle`**
- Create transmission
- Retransmit multiple times
- Verify each retransmission gets new descriptor
- Free descriptors
- Verify all return to pool

#### Integration Tests

**Test: `test_retransmission_end_to_end`**
- Send packet
- Simulate loss (no ACK)
- Detect loss
- Trigger retransmission
- Verify retransmit_copy called
- Verify new wheel entry created
- Verify packet resent

**Test: `test_retransmission_performance_impact`**
- Simulate lossy network (10% loss)
- Measure copy overhead
- Document performance impact
- Verify acceptable (<5% throughput loss)

**Test: `test_pool_exhaustion_during_retransmissions`**
- Create small pool
- Generate many retransmissions
- Verify pool grows or handles gracefully
- Monitor memory usage

**Assigned To**: _________

---

## Work Item 11: Update Receive Side for Pool-Based ACKs

**Status**: 🔴 Not Started

**Description**: Use descriptor pool for ACK packet allocation and send ACKs through wheel.

**Files Affected**:
- `dc/s2n-quic-dc/src/stream/recv/state.rs`
- `dc/s2n-quic-dc/src/stream/recv/shared.rs`
- `dc/s2n-quic-dc/src/stream/recv/worker.rs`

### Test Strategy

Receive-side ACK tests need to verify:
1. Pool reference is available
2. ACK allocation uses pool
3. ACKs go through wheel with priority
4. ACK descriptor reuse works
5. ACK timing is appropriate

### Test Suite Plan

#### Unit Tests

**Test: `test_recv_state_has_pool_reference`**
- Create receive state
- Verify pool reference exists
- Allocate from pool
- Verify allocation works

**Test: `test_ack_uses_pool_allocation`**
- Receive packets
- Generate ACK
- Verify ACK descriptor allocated from pool
- Verify no todo!() panic

**Test: `test_ack_sent_through_wheel`**
- Generate ACK
- Verify wheel entry created
- Verify ACK not directly sent

**Test: `test_ack_priority`**
- Create ACK wheel entry
- Verify priority = 0 (highest)
- Verify timestamp = now (immediate)

#### Integration Tests

**Test: `test_ack_generation_end_to_end`**
- Receive packets
- Trigger ACK generation
- Verify pool allocation
- Verify wheel insertion
- Verify ACK sent
- Verify descriptor returned to pool

**Test: `test_ack_descriptor_reuse`**
- Generate many ACKs
- Track pool size
- Verify descriptors are reused
- Verify pool doesn't grow unbounded

**Test: `test_ack_timing_requirements`**
- Receive packet
- Measure time to ACK generation
- Verify ACK generated within 25ms (typical requirement)
- Verify ACK sent with high priority

**Test: `test_ack_only_packets_dont_count_flow_control`**
- Send ACK-only packet
- Verify pending_transmissions not incremented
- Verify flow control not blocked by ACKs

**Assigned To**: _________

---

## Work Item 12: Wire Up Pool Through Stream Creation

**Status**: 🔴 Not Started

**Description**: Thread pool reference from environment through endpoint to stream state and workers.

**Files Affected**:
- `dc/s2n-quic-dc/src/stream/endpoint.rs`
- `dc/s2n-quic-dc/src/stream/environment/udp.rs`
- `dc/s2n-quic-dc/src/stream/send/state.rs`

### Test Strategy

Pool wiring tests need to verify:
1. Pool flows from environment to streams
2. All components have pool access
3. Lifetime management is correct
4. Both Tokio and Bach environments work

### Test Suite Plan

#### Unit Tests

**Test: `test_endpoint_accepts_pool`**
- Create endpoint with pool
- Verify pool is stored or passed through

**Test: `test_state_constructor_accepts_pool`**
- Call State::new with pool
- Verify pool is stored
- Verify pool is Arc-wrapped

**Test: `test_worker_has_pool_access`**
- Create worker with pool
- Verify pool accessible
- Allocate from pool
- Verify allocation works

**Test: `test_pool_lifetime_management`**
- Create environment with pool
- Create stream (clones Arc<Pool>)
- Drop environment
- Verify stream still has valid pool
- Drop stream
- Verify pool is dropped

#### Integration Tests

**Test: `test_pool_wiring_tokio_environment`**
- Create Tokio environment
- Verify pool created
- Create stream
- Verify stream has pool reference
- Allocate from stream
- Verify works

**Test: `test_pool_wiring_bach_environment`**
- Create Bach environment
- Verify pool created
- Create stream
- Verify stream has pool reference
- Allocate from stream
- Verify works

**Test: `test_multiple_streams_share_pool`**
- Create environment
- Create 10 streams
- Verify all share same pool (Arc pointer equality)
- Allocate from different streams
- Verify pool state shared

**Test: `test_no_todo_in_stream_creation`**
- Create stream
- Verify no todo!() panics in creation path
- Verify pool is properly wired

**Assigned To**: _________

---

## Work Item 13: Update Transmission Info for GSO

**Status**: 🔴 Not Started

**Description**: Change Transmission<Info> to Transmission<Vec<Info>> to support GSO packets with multiple QUIC packets.

**Files Affected**:
- `dc/s2n-quic-dc/src/stream/send/state/transmission.rs`
- `dc/s2n-quic-dc/src/socket/send/wheel.rs`
- `dc/s2n-quic-dc/src/stream/send/application/transmission.rs`

### Test Strategy

GSO info tests need to verify:
1. Vec<Info> structure works
2. One Info per QUIC packet
3. Completion processing handles Vec
4. Retransmission of individual packets
5. Metadata is correct per packet

### Test Suite Plan

#### Unit Tests

**Test: `test_transmission_info_is_vec`**
- Create Transmission
- Verify info field is Vec<Info>
- Verify can hold multiple Info entries

**Test: `test_info_vec_creation_for_gso`**
- Create GSO transmission with 5 packets
- Verify Vec<Info> has 5 entries
- Verify each Info has correct packet_len, offset, etc.

**Test: `test_single_packet_transmission`**
- Create non-GSO transmission
- Verify Vec<Info> has 1 entry
- Verify Info is correct

**Test: `test_completion_processing_iterates_vec`**
- Create transmission with Vec<Info> (3 entries)
- Process completion
- Verify all 3 Info entries processed
- Verify RTT updated 3 times (or aggregated)

#### Integration Tests

**Test: `test_gso_transmission_end_to_end`**
- Enable GSO
- Send data that creates 10-packet GSO burst
- Verify Transmission<Vec<Info>> created
- Verify Vec has 10 entries
- Complete transmission
- Process completion
- Verify all 10 packets acknowledged

**Test: `test_gso_partial_loss_retransmission`**
- Send GSO transmission (5 packets)
- ACK packets 1, 2, 4, 5
- Mark packet 3 lost
- Trigger retransmission
- Verify only packet 3 retransmitted
- Verify single-packet transmission created

**Test: `test_gso_rtt_measurement`**
- Send GSO transmission (multiple packets)
- Timestamp each packet
- Receive ACK
- Process completion
- Verify RTT calculated per packet
- Verify RTT stats updated correctly

**Test: `test_gso_flow_control_per_packet`**
- Send GSO transmission (5 packets)
- Verify flow control tracks each packet's offset
- ACK some packets
- Verify flow control updated per packet

**Assigned To**: _________

---

## Work Item 14: Remove Partial Send Handling

**Status**: 🔴 Not Started

**Description**: Remove partial send logic since wheel guarantees all-or-nothing transmission.

**Files Affected**:
- `dc/s2n-quic-dc/src/msg/send.rs`

### Test Strategy

Partial send removal needs tests to verify:
1. Partial send code is removed
2. All-or-nothing behavior is documented
3. Error handling works correctly
4. Wheel retry behavior works

### Test Suite Plan

#### Unit Tests

**Test: `test_partial_send_code_removed`**
- Verify no `total_len > len` check
- Verify no partial send tracking
- Verify simplified force_clear

**Test: `test_all_or_nothing_documented`**
- Check for comments explaining wheel behavior
- Verify documentation mentions all-or-nothing

**Test: `test_error_handling_without_partial_sends`**
- Simulate send error
- Verify transmission dropped
- Verify completion queue not notified
- Verify retransmission will trigger via loss detection

#### Integration Tests

**Test: `test_wheel_all_or_nothing_behavior`**
- Fill socket buffer
- Attempt to send transmission
- Verify transmission stays in wheel (not partially sent)
- Drain socket buffer
- Verify transmission sent completely

**Test: `test_gso_atomic_send`**
- Create GSO transmission (5 packets)
- Attempt to send when socket has room for 3 packets
- Verify all 5 packets stay in wheel
- Verify not partially sent

**Test: `test_wheel_retry_on_ewouldblock`**
- Attempt send when socket full (EWOULDBLOCK)
- Verify transmission stays in wheel
- Retry later
- Verify transmission sent

**Test: `test_send_error_triggers_retransmission`**
- Send transmission
- Simulate send error (not EWOULDBLOCK)
- Verify transmission dropped
- Advance time
- Verify loss detection triggers retransmission

**Assigned To**: _________

---

## Testing Infrastructure

### Required Test Utilities

1. **Mock Socket Worker**: Simulate socket worker for unit testing streams
2. **Mock Wheel**: Simulate wheel for unit testing state/worker
3. **Mock Clock**: Control time for testing timestamps and pacing
4. **Pool Test Fixture**: Reusable pool setup with configurable parameters
5. **Lossy Network Simulator**: Simulate packet loss for retransmission tests
6. **Latency Simulator**: Simulate network latency for RTT tests
7. **Bandwidth Simulator**: Simulate bandwidth limits for CCA tests

### Test Categories

- **Unit Tests**: Test individual components in isolation (~140 tests planned)
- **Integration Tests**: Test component interactions (~70 tests planned)
- **End-to-End Tests**: Test full send path with real socket (~20 tests planned)
- **Performance Tests**: Benchmark throughput, latency, memory usage (~10 tests planned)
- **Stress Tests**: Test under high concurrency and error conditions (~10 tests planned)

### Test Execution Plan

1. **Phase 1**: Implement basic unit tests for each work item (2-3 days per item)
2. **Phase 2**: Implement integration tests (1-2 days per item)
3. **Phase 3**: Implement end-to-end tests (1 week)
4. **Phase 4**: Performance and stress tests (1 week)

### Success Criteria

- [ ] All unit tests pass with >90% code coverage
- [ ] All integration tests pass
- [ ] No memory leaks detected in stress tests
- [ ] Performance tests show improvement over current implementation
- [ ] No regressions in existing test suite

---

## Summary Statistics

- **Total Work Items**: 14
- **Total Test Plans**: 250+ tests
- **Estimated Test Implementation Time**: 6-8 weeks
- **Current Status**: Planning phase complete, ready for assignment

## Next Steps

1. Review this test plan with the team
2. Assign work items to team members
3. Set up testing infrastructure (mocks, simulators)
4. Begin implementation starting with Work Item 1
5. Review and adjust test plans based on implementation learnings
