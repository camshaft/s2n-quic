# Architecture Diagrams

This document provides visual representations of the UDP send path improvements.

## Current vs New Architecture

### Before: Direct Queue-to-Socket Model

```mermaid
graph TD
    A[Application Layer] -->|push data| B[VecDeque Buffer]
    B -->|poll_flush| C[Direct Socket Send]
    C -->|sendmsg| D[UDP Socket]
    D --> E[Network]
    
    style B fill:#f9f,stroke:#333
    style C fill:#f99,stroke:#333
```

**Problems:**
- Custom memory allocator (complex)
- No flow control backpressure
- Naive pacing (yield every 5 packets)
- No completion feedback

### After: Wheel-Based Pacing Model

```mermaid
graph TD
    A[Application Layer] -->|allocate| B[Descriptor Pool]
    B -->|filled descriptor| C[Wheel Entry]
    C -->|timestamp-based| D[Wheel Scheduler]
    D -->|ready to send| E[Socket Worker]
    E -->|sendmsg| F[UDP Socket]
    F --> G[Network]
    E -->|completed| H[Completion Queue]
    H -->|process| I[Flow Controller]
    I -->|release credits| A
    
    style B fill:#9f9,stroke:#333
    style D fill:#9f9,stroke:#333
    style H fill:#9f9,stroke:#333
    style I fill:#9f9,stroke:#333
```

**Benefits:**
- Unified memory management
- Flow control with backpressure
- CCA-driven pacing
- Feedback loop for RTT/loss

---

## Work Item Dependencies

```mermaid
graph TD
    WI1[#1 Pool Integration] --> WI4[#4 Pool Refactor]
    WI1 --> WI12[#12 Pool Wiring]
    WI1 --> WI11[#11 ACK Updates]
    
    WI12 --> WI7[#7 State Updates]
    WI4 --> WI7
    
    WI2[#2 Completion Queues] --> WI3[#3 Wheel Integration]
    WI2 --> WI5[#5 Flow Control]
    WI2 --> WI6[#6 Worker Updates]
    WI2 --> WI7
    
    WI3 --> WI6
    WI4 --> WI6
    WI3 --> WI8[#8 CCA Pacing]
    WI3 --> WI13[#13 GSO Support]
    WI3 --> WI14[#14 Partial Send Removal]
    
    WI5 --> WI9[#9 Configuration]
    
    WI7 --> WI8
    WI7 --> WI10[#10 Retransmissions]
    WI4 --> WI10
    
    WI1 --> WI11
    WI3 --> WI11
    
    style WI1 fill:#f99,stroke:#333
    style WI2 fill:#f99,stroke:#333
    style WI3 fill:#f99,stroke:#333
    style WI4 fill:#f99,stroke:#333
    style WI6 fill:#f99,stroke:#333
    style WI7 fill:#f99,stroke:#333
    style WI12 fill:#f99,stroke:#333
```

**Legend:**
- Red nodes: P0 (Critical) - Must be done first
- Default nodes: P1-P2 - Can be done later

---

## Implementation Phases

```mermaid
gantt
    title UDP Send Path Improvements Timeline
    dateFormat  YYYY-MM-DD
    section Phase 1: Foundation
    Pool Integration           :p1-1, 2025-12-17, 5d
    Pool Wiring               :p1-2, after p1-1, 3d
    Completion Queues         :p1-3, after p1-1, 5d
    Pool Refactor             :p1-4, after p1-2, 5d
    
    section Phase 2: Core
    Wheel Integration         :p2-1, after p1-3, 7d
    State Updates             :p2-2, after p1-4, 7d
    Worker Updates            :p2-3, after p2-1, 5d
    
    section Phase 3: Features
    Flow Control              :p3-1, after p2-3, 5d
    CCA Pacing               :p3-2, after p2-2, 5d
    Retransmissions          :p3-3, after p2-2, 4d
    ACK Updates              :p3-4, after p2-1, 4d
    
    section Phase 4: Polish
    GSO Support              :p4-1, after p3-2, 5d
    Configuration            :p4-2, after p3-1, 3d
    Partial Send Removal     :p4-3, after p2-1, 3d
```

---

## Data Flow

### Sending Data (New Architecture)

```mermaid
sequenceDiagram
    participant App as Application
    participant Pool as Descriptor Pool
    participant Wheel as Wheel
    participant Worker as Socket Worker
    participant Socket as UDP Socket
    participant CompQ as Completion Queue
    participant Flow as Flow Controller
    
    App->>Flow: Request credits
    Flow->>App: Grant (if available)
    App->>Pool: Allocate descriptor
    Pool->>App: Unfilled descriptor
    App->>App: Fill with packet data
    App->>Wheel: Insert entry (timestamp)
    
    Note over Wheel: Time passes...
    
    Worker->>Wheel: Poll ready entries
    Wheel->>Worker: Entry (timestamp reached)
    Worker->>Socket: sendmsg
    Socket->>Worker: Success
    Worker->>CompQ: Push completion
    
    App->>CompQ: Poll completions
    CompQ->>App: Completed transmission
    App->>Flow: Update (decrement pending)
    Flow->>App: More credits available
```

### Retransmission Flow

```mermaid
sequenceDiagram
    participant App as Application
    participant State as Send State
    participant Pool as Descriptor Pool
    participant Wheel as Wheel
    participant Loss as Loss Detector
    
    Note over Loss: Detects packet loss
    Loss->>State: Mark packet lost
    State->>State: Get retransmission info
    State->>Pool: Allocate new descriptor
    Pool->>State: Unfilled descriptor
    State->>State: Copy payload
    State->>Wheel: Insert retransmission
    
    Note over Wheel: Retransmission sent normally
```

---

## Flow Control State Machine

```mermaid
stateDiagram-v2
    [*] --> Ready: Initialize
    
    Ready --> Waiting: pending >= max_in_flight
    Ready --> Ready: Request credits (granted)
    
    Waiting --> Ready: Completion processed
    Waiting --> Waiting: Request credits (pending)
    
    Ready --> [*]: Stream closed
    Waiting --> [*]: Stream closed
```

---

## Pool Lifecycle

```mermaid
stateDiagram-v2
    [*] --> Created: Environment setup
    
    Created --> Available: Descriptor freed
    Created --> InUse: Descriptor allocated
    
    Available --> InUse: Allocate
    InUse --> Filling: Application fills data
    Filling --> Filled: Fill complete
    Filled --> InWheel: Inserted to wheel
    InWheel --> Sending: Socket worker sends
    Sending --> Available: Send complete
    Sending --> InWheel: Send failed (retry)
    
    Available --> [*]: Pool destroyed
    InUse --> Available: Descriptor dropped
```

---

## Component Interactions

```mermaid
graph LR
    subgraph "Application Layer"
        A1[Stream Send]
        A2[Flow Controller]
    end
    
    subgraph "Memory Management"
        M1[Descriptor Pool]
        M2[Allocator]
    end
    
    subgraph "Scheduling"
        S1[Wheel]
        S2[CCA]
    end
    
    subgraph "Socket Layer"
        SK1[Socket Worker]
        SK2[UDP Socket]
    end
    
    subgraph "Feedback"
        F1[Completion Queue]
        F2[RTT Tracker]
        F3[Loss Detector]
    end
    
    A1 -->|allocate| M1
    A1 -->|check credits| A2
    M1 -->|descriptor| A1
    A1 -->|insert| S1
    S2 -->|bandwidth| S1
    S1 -->|ready| SK1
    SK1 -->|sendmsg| SK2
    SK1 -->|completed| F1
    F1 -->|process| A1
    F1 -->|update| F2
    F1 -->|detect| F3
    F3 -->|retransmit| A1
    A2 -->|credits| A1
    F1 -->|release| A2
    
    style M1 fill:#9f9
    style S1 fill:#9f9
    style F1 fill:#9f9
    style A2 fill:#9f9
```

---

## Test Coverage Hierarchy

```mermaid
graph TD
    A[Test Suite] --> B[Unit Tests 140]
    A --> C[Integration Tests 70]
    A --> D[E2E Tests 20]
    A --> E[Performance Tests 10]
    A --> F[Stress Tests 10]
    
    B --> B1[Pool Tests]
    B --> B2[Wheel Tests]
    B --> B3[Flow Control Tests]
    B --> B4[State Tests]
    
    C --> C1[Pool + Wheel]
    C --> C2[Wheel + Worker]
    C --> C3[Flow + Completions]
    
    D --> D1[Full Send Path]
    D --> D2[Retransmissions]
    D --> D3[High BDP]
    
    E --> E1[Throughput]
    E --> E2[Latency]
    E --> E3[Memory]
    
    F --> F1[1000+ Streams]
    F --> F2[Packet Loss]
    F --> F3[Pool Exhaustion]
    
    style B fill:#9f9
    style C fill:#99f
    style D fill:#f99
    style E fill:#ff9
    style F fill:#f9f
```

---

## Risk Areas

```mermaid
mindmap
  root((Risks))
    High Risk
      Wheel Integration
        Core architecture change
        Timing sensitive
      Pool Refactor
        Memory management
        Potential leaks
    Medium Risk
      State Updates
        Many todo calls
        Complex logic
      Worker Updates
        State coordination
        Lifecycle management
    Low Risk
      Configuration
        Simple parameters
      Documentation
        No code changes
```

---

## Success Metrics Dashboard

```mermaid
graph LR
    subgraph "Functional Metrics"
        F1[All todo removed]
        F2[No memory leaks]
        F3[All tests pass]
        F4[Coverage >90%]
    end
    
    subgraph "Performance Metrics"
        P1[Throughput maintained]
        P2[Latency P99 <10ms]
        P3[Memory stable]
        P4[1000+ streams OK]
    end
    
    subgraph "Quality Metrics"
        Q1[Zero critical bugs]
        Q2[Docs updated]
        Q3[Reviews complete]
        Q4[Benchmarks done]
    end
    
    style F1 fill:#9f9
    style F2 fill:#9f9
    style F3 fill:#9f9
    style F4 fill:#9f9
```

---

## GitHub Rendering

These Mermaid diagrams are rendered automatically by GitHub when viewing this file in the repository. For best viewing experience, use the GitHub web interface.

## Diagram Sources

All diagrams in this document are generated using Mermaid.js syntax. To edit:

1. Copy the mermaid code block
2. Use the [Mermaid Live Editor](https://mermaid.live)
3. Make changes
4. Copy back to this file

## Related Documents

- See `INVESTIGATION_SUMMARY.md` for detailed architecture discussion
- See `WORK_ITEM_STATUS.md` for current status
- See `TEST_COVERAGE_PLAN.md` for test specifications
- See `udp-improvements.md` for technical details
