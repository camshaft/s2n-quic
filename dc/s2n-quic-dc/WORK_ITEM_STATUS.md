# UDP Send Path Improvements - Work Item Status Tracker

## Overview

This document tracks the status of all work items for the UDP send path improvements. Use this as a quick reference for project management and task assignment.

**Last Updated**: 2025-12-10

---

## Status Legend

- 🔴 **Not Started**: Work has not begun
- 🟡 **In Progress**: Work is underway
- 🟢 **Complete**: Work is done and tested
- 🔵 **Blocked**: Cannot proceed due to dependency or issue
- ⚪ **Skipped**: Decided not to implement

---

## Work Items Summary Table

| # | Work Item | Status | Priority | Assigned To | Depends On | Test Coverage |
|---|-----------|--------|----------|-------------|------------|---------------|
| 1 | Pool Integration in Environment Layer | 🔴 | P0 | _________ | None | 0% |
| 2 | Stream Completion Queue Setup | 🔴 | P0 | _________ | #1 | 0% |
| 3 | Replace Queue with Wheel-Based Transmission | 🔴 | P0 | _________ | #1, #2 | 0% |
| 4 | Refactor msg/send.rs to Use Descriptor Pool | 🔴 | P0 | _________ | #1 | 0% |
| 5 | Remove Pacer and Implement Flow-Based Backpressure | 🔴 | P1 | _________ | #2 | 0% |
| 6 | Update Worker Transmission Logic | 🔴 | P0 | _________ | #2, #3, #4 | 0% |
| 7 | Update State's Transmission Creation | 🔴 | P0 | _________ | #1, #2, #4 | 0% |
| 8 | Integrate CCA Pacing with Wheel | 🔴 | P1 | _________ | #3, #7 | 0% |
| 9 | Configure Transmission Limits | 🔴 | P2 | _________ | #5 | 0% |
| 10 | Handle Retransmissions with Copying | 🔴 | P1 | _________ | #4, #7 | 0% |
| 11 | Update Receive Side for Pool-Based ACKs | 🔴 | P1 | _________ | #1, #3 | 0% |
| 12 | Wire Up Pool Through Stream Creation | 🔴 | P0 | _________ | #1 | 0% |
| 13 | Update Transmission Info for GSO | 🔴 | P1 | _________ | #3, #7 | 0% |
| 14 | Remove Partial Send Handling | 🔴 | P2 | _________ | #3 | 0% |

---

## Priority Definitions

- **P0 (Critical)**: Must be completed first, blocks other work
- **P1 (High)**: Important for functionality, should be done early
- **P2 (Medium)**: Nice to have, can be done later

---

## Suggested Implementation Order

Based on dependencies, the recommended implementation order is:

### Phase 1: Foundation (P0 - Week 1-2)
1. Work Item #1: Pool Integration in Environment Layer
2. Work Item #12: Wire Up Pool Through Stream Creation
3. Work Item #2: Stream Completion Queue Setup
4. Work Item #4: Refactor msg/send.rs to Use Descriptor Pool

### Phase 2: Core Transmission Changes (P0 - Week 3-4)
5. Work Item #3: Replace Queue with Wheel-Based Transmission
6. Work Item #7: Update State's Transmission Creation
7. Work Item #6: Update Worker Transmission Logic

### Phase 3: Flow Control & Pacing (P1 - Week 5-6)
8. Work Item #5: Remove Pacer and Implement Flow-Based Backpressure
9. Work Item #8: Integrate CCA Pacing with Wheel
10. Work Item #10: Handle Retransmissions with Copying
11. Work Item #11: Update Receive Side for Pool-Based ACKs

### Phase 4: Advanced Features (P1-P2 - Week 7-8)
12. Work Item #13: Update Transmission Info for GSO
13. Work Item #9: Configure Transmission Limits
14. Work Item #14: Remove Partial Send Handling

---

## Detailed Status

### Work Item 1: Pool Integration in Environment Layer

**Status**: 🔴 Not Started  
**Priority**: P0  
**Assigned To**: _________  
**Estimated Effort**: 3 days  

**Files to Modify**:
- `dc/s2n-quic-dc/src/stream/environment/udp.rs`
- `dc/s2n-quic-dc/src/stream/environment/tokio/pool.rs`
- `dc/s2n-quic-dc/src/stream/environment/bach/pool.rs`

**Test Status**:
- [ ] Unit tests (6 planned)
- [ ] Integration tests (2 planned)

**Blockers**: None

**Notes**: This is the foundation for all other work items. Must be completed first.

---

### Work Item 2: Stream Completion Queue Setup

**Status**: 🔴 Not Started  
**Priority**: P0  
**Assigned To**: _________  
**Estimated Effort**: 3 days  

**Files to Modify**:
- `dc/s2n-quic-dc/src/stream/send/queue.rs`
- `dc/s2n-quic-dc/src/stream/send/state.rs`
- `dc/s2n-quic-dc/src/stream/send/worker.rs`

**Test Status**:
- [ ] Unit tests (4 planned)
- [ ] Integration tests (4 planned)

**Blockers**: Work Item #1

**Notes**: Critical for feedback loop from socket worker to stream.

---

### Work Item 3: Replace Queue with Wheel-Based Transmission

**Status**: 🔴 Not Started  
**Priority**: P0  
**Assigned To**: _________  
**Estimated Effort**: 5 days  

**Files to Modify**:
- `dc/s2n-quic-dc/src/stream/send/queue.rs` (major refactor)
- `dc/s2n-quic-dc/src/socket/send/wheel.rs`
- `dc/s2n-quic-dc/src/socket/send/udp.rs`

**Test Status**:
- [ ] Unit tests (5 planned)
- [ ] Integration tests (3 planned)

**Blockers**: Work Items #1, #2

**Notes**: This is the core architectural change. High risk, needs careful testing.

---

### Work Item 4: Refactor msg/send.rs to Use Descriptor Pool

**Status**: 🔴 Not Started  
**Priority**: P0  
**Assigned To**: _________  
**Estimated Effort**: 4 days  

**Files to Modify**:
- `dc/s2n-quic-dc/src/msg/send.rs` (major refactor)
- `dc/s2n-quic-dc/src/socket/pool/descriptor.rs`
- `dc/s2n-quic-dc/src/allocator.rs`

**Test Status**:
- [ ] Unit tests (5 planned)
- [ ] Integration tests (4 planned)

**Blockers**: Work Item #1

**Notes**: Removes custom allocator. Impacts memory management significantly.

---

### Work Item 5: Remove Pacer and Implement Flow-Based Backpressure

**Status**: 🔴 Not Started  
**Priority**: P1  
**Assigned To**: _________  
**Estimated Effort**: 4 days  

**Files to Modify**:
- `dc/s2n-quic-dc/src/stream/pacer.rs` (DELETE)
- `dc/s2n-quic-dc/src/stream/send/flow.rs`
- `dc/s2n-quic-dc/src/stream/send/worker.rs`

**Test Status**:
- [ ] Unit tests (6 planned)
- [ ] Integration tests (3 planned)

**Blockers**: Work Item #2

**Notes**: Improves flow control. Should help with high concurrency issues.

---

### Work Item 6: Update Worker Transmission Logic

**Status**: 🔴 Not Started  
**Priority**: P0  
**Assigned To**: _________  
**Estimated Effort**: 4 days  

**Files to Modify**:
- `dc/s2n-quic-dc/src/stream/send/worker.rs`
- `dc/s2n-quic-dc/src/stream/send/state.rs`

**Test Status**:
- [ ] Unit tests (5 planned)
- [ ] Integration tests (3 planned)

**Blockers**: Work Items #2, #3, #4

**Notes**: Changes worker from sender to orchestrator. Key architectural change.

---

### Work Item 7: Update State's Transmission Creation

**Status**: 🔴 Not Started  
**Priority**: P0  
**Assigned To**: _________  
**Estimated Effort**: 5 days  

**Files to Modify**:
- `dc/s2n-quic-dc/src/stream/send/state.rs`
- `dc/s2n-quic-dc/src/stream/send/state/transmission.rs`

**Test Status**:
- [ ] Unit tests (5 planned)
- [ ] Integration tests (3 planned)

**Blockers**: Work Items #1, #2, #4

**Notes**: Removes todo!() calls. Critical for functionality.

---

### Work Item 8: Integrate CCA Pacing with Wheel

**Status**: 🔴 Not Started  
**Priority**: P1  
**Assigned To**: _________  
**Estimated Effort**: 4 days  

**Files to Modify**:
- `dc/s2n-quic-dc/src/stream/send/state.rs`
- `dc/s2n-quic-dc/src/stream/send/queue.rs`
- `quic/s2n-quic-core/src/recovery/bandwidth.rs`

**Test Status**:
- [ ] Unit tests (6 planned)
- [ ] Integration tests (4 planned)

**Blockers**: Work Items #3, #7

**Notes**: Proper pacing should improve throughput and reduce congestion.

---

### Work Item 9: Configure Transmission Limits

**Status**: 🔴 Not Started  
**Priority**: P2  
**Assigned To**: _________  
**Estimated Effort**: 2 days  

**Files to Modify**:
- `dc/s2n-quic-dc/src/stream/environment/udp.rs`
- `dc/s2n-quic-dc/src/stream/send/flow.rs`

**Test Status**:
- [ ] Unit tests (3 planned)
- [ ] Integration tests (3 planned)

**Blockers**: Work Item #5

**Notes**: Allows tuning for different network conditions.

---

### Work Item 10: Handle Retransmissions with Copying

**Status**: 🔴 Not Started  
**Priority**: P1  
**Assigned To**: _________  
**Estimated Effort**: 3 days  

**Files to Modify**:
- `dc/s2n-quic-dc/src/msg/send.rs`
- `dc/s2n-quic-dc/src/stream/send/state.rs`

**Test Status**:
- [ ] Unit tests (5 planned)
- [ ] Integration tests (3 planned)

**Blockers**: Work Items #4, #7

**Notes**: Simplifies implementation at cost of retransmission performance.

---

### Work Item 11: Update Receive Side for Pool-Based ACKs

**Status**: 🔴 Not Started  
**Priority**: P1  
**Assigned To**: _________  
**Estimated Effort**: 3 days  

**Files to Modify**:
- `dc/s2n-quic-dc/src/stream/recv/state.rs`
- `dc/s2n-quic-dc/src/stream/recv/shared.rs`
- `dc/s2n-quic-dc/src/stream/recv/worker.rs`

**Test Status**:
- [ ] Unit tests (5 planned)
- [ ] Integration tests (4 planned)

**Blockers**: Work Items #1, #3

**Notes**: Ensures ACKs are sent with proper priority.

---

### Work Item 12: Wire Up Pool Through Stream Creation

**Status**: 🔴 Not Started  
**Priority**: P0  
**Assigned To**: _________  
**Estimated Effort**: 3 days  

**Files to Modify**:
- `dc/s2n-quic-dc/src/stream/endpoint.rs`
- `dc/s2n-quic-dc/src/stream/environment/udp.rs`
- `dc/s2n-quic-dc/src/stream/send/state.rs`

**Test Status**:
- [ ] Unit tests (5 planned)
- [ ] Integration tests (4 planned)

**Blockers**: Work Item #1

**Notes**: Plumbing work. Essential for all components to access pool.

---

### Work Item 13: Update Transmission Info for GSO

**Status**: 🔴 Not Started  
**Priority**: P1  
**Assigned To**: _________  
**Estimated Effort**: 4 days  

**Files to Modify**:
- `dc/s2n-quic-dc/src/stream/send/state/transmission.rs`
- `dc/s2n-quic-dc/src/socket/send/wheel.rs`
- `dc/s2n-quic-dc/src/stream/send/application/transmission.rs`

**Test Status**:
- [ ] Unit tests (5 planned)
- [ ] Integration tests (4 planned)

**Blockers**: Work Items #3, #7

**Notes**: Critical for GSO performance benefits. Enables per-packet tracking.

---

### Work Item 14: Remove Partial Send Handling

**Status**: 🔴 Not Started  
**Priority**: P2  
**Assigned To**: _________  
**Estimated Effort**: 2 days  

**Files to Modify**:
- `dc/s2n-quic-dc/src/msg/send.rs`

**Test Status**:
- [ ] Unit tests (4 planned)
- [ ] Integration tests (4 planned)

**Blockers**: Work Item #3

**Notes**: Simplification. Can be done after wheel is working.

---

## Team Meeting Discussion Topics

### High Concurrency Issues

Based on the problem statement, the high stream concurrency issues may be related to:

1. **Flow Control**: Work Item #5 (Remove Pacer and Implement Flow-Based Backpressure) should help by preventing overwhelming the socket layer
2. **Memory Management**: Work Items #1, #4, #12 (Pool integration) should improve memory efficiency
3. **Pacing**: Work Item #8 (Integrate CCA Pacing) should prevent bursts that cause congestion

**Recommendation**: Prioritize Work Items #5 and #8 once the foundation is in place.

### Testing Strategy

The test plan includes 250+ tests across 14 work items. Key recommendations:

1. **Implement tests alongside code**: Don't defer testing to the end
2. **Use mock utilities**: Build reusable test infrastructure early
3. **Run tests incrementally**: Validate each work item before moving to next
4. **Performance benchmarks**: Establish baseline before changes

### Risk Areas

High-risk work items that need extra attention:

1. **Work Item #3** (Wheel integration): Core architectural change
2. **Work Item #4** (Pool refactor): Memory management changes
3. **Work Item #7** (State transmission): Removes many todo!() calls

These should be:
- Code reviewed by multiple team members
- Tested extensively before merge
- Documented with rationale for design decisions

---

## Progress Tracking

### Weekly Updates

Update this section weekly with progress:

**Week of 2025-12-10**:
- ✅ Created test coverage plan
- ✅ Created work item status tracker
- ⏳ Waiting for team assignments

**Week of 2025-12-17**:
- [ ] TBD

---

## Questions & Issues

Use this section to track blocking questions or issues:

1. **Q**: What should max_in_flight_transmissions default be?  
   **A**: TBD - need benchmarking

2. **Q**: Should we support runtime pool resizing?  
   **A**: TBD - discuss with team

3. **Q**: How to handle pool exhaustion?  
   **A**: TBD - may need backpressure mechanism

---

## Success Metrics

Track these metrics to measure improvement:

- [ ] **Baseline Performance**: Measure current throughput/latency (before changes)
- [ ] **High Concurrency Test**: Run with 1000+ concurrent streams
- [ ] **Memory Usage**: Track pool memory vs old allocator
- [ ] **Retransmission Rate**: Should remain low (<1% in normal conditions)
- [ ] **CPU Usage**: Should not increase significantly
- [ ] **Test Coverage**: Target >90% line coverage for changed code

---

## Notes

Add any additional notes or observations here:

- The original issue mentioned "hunting down exactly what the cause is" for high stream concurrency issues. We should add specific stress tests that reproduce the problem.
- Consider adding instrumentation/metrics to track pool usage, wheel depth, pending transmissions, etc. for debugging in production.
- The test plan is comprehensive but may need adjustment based on implementation findings.

