# s2n-quic-dc-cli Project Inventory

**Date:** December 10, 2025  
**Status:** Early development - basic structure complete, ready for DC QUIC integration

## Current Implementation Status

### ✅ Completed Components

#### 1. Project Structure

- ✅ Cargo.toml with all necessary dependencies
- ✅ Multi-file modular architecture
- ✅ Proper licensing (Apache-2.0)

#### 2. CLI Framework (`main.rs`)

- ✅ Clap-based command line interface
- ✅ Server/Client subcommands implemented
- ✅ Configuration file support via `-c/--config` flag
- ✅ Workload selection for client mode
- ✅ Tokio async runtime setup
- ✅ Tracing initialization

#### 3. Configuration System (`config.rs`)

- ✅ TOML-based configuration
- ✅ ServerConfig with listen_address
- ✅ ClientConfig (placeholder)
- ✅ TuiConfig with refresh_rate_ms
- ✅ WorkloadConfig with num_streams, request_size, response_size, delay_ms
- ✅ HashMap of named workloads
- ✅ Default values for all configs
- ✅ File loading via `Config::from_file()`

#### 4. Protocol Definition (`protocol.rs`)

- ✅ 16-byte Request header (8 bytes delay_ms + 8 bytes response_size)
- ✅ Request encoding/decoding with bytes crate
- ✅ RequestStorage implementing s2n_quic_core::buffer::reader::Storage
- ✅ Support for variable-length responses

#### 5. Metrics Tracking (`metrics.rs`)

- ✅ GoodputStats structure tracking:
  - acked_payload_bytes
  - stream_packet_bytes
  - control_packet_bytes
  - goodput_percentage() calculation
- ✅ MetricsSubscriber implementing event::Subscriber
- ✅ Atomic counters for thread-safe metrics
- ✅ Snapshot system with reset
- ✅ Packet loss rate tracking
- ✅ Latency tracking with average calculation
- ✅ Event handlers for:
  - stream_packet_acked
  - stream_packet_transmitted
  - stream_control_packet_received
  - stream_packet_lost

#### 6. Example Configuration

- ✅ example-config.toml with two workloads:
  - large_burst: 10,000 streams
  - small_quick: 100 streams

#### 7. Documentation

- ✅ Comprehensive README.md
- ✅ Usage examples
- ✅ Architecture documentation

### 🚧 Partially Implemented

#### 1. Server (`server.rs`)

- ✅ Basic structure with Config
- ✅ MetricsSubscriber integration
- ⚠️ **Using TCP placeholder instead of DC QUIC**
- ✅ Request decoding
- ✅ Configurable delay implementation
- ✅ Response size support
- ❌ Not using actual s2n-quic-dc stream server
- ❌ Not integrated with endpoint.rs

#### 2. Client (`client.rs`)

- ✅ Basic structure with Config and server_addr
- ✅ MetricsSubscriber integration
- ✅ Workload execution logic
- ✅ Concurrent stream launching
- ✅ Success/failure tracking
- ⚠️ **Using TCP connections instead of DC QUIC streams**
- ❌ Not using actual s2n-quic-dc stream client
- ❌ Not integrated with endpoint.rs

#### 3. Endpoint Setup (`endpoint.rs`)

- ✅ Code structure exists
- ✅ Server endpoint with PSK and stream server
- ✅ Client endpoint with PSK and stream client
- ✅ TLS provider using test certificates
- ✅ Path secret Map configuration
- ⚠️ **Commented out in main.rs** (not currently compiled)
- ❌ Not integrated with server.rs or client.rs

### ❌ Not Yet Implemented

#### 1. TUI (Terminal User Interface)

- ❌ No ratatui implementation yet
- ❌ No tab system (Logs, Goodput, Latency, Overview)
- ❌ No real-time graph rendering
- ❌ No histogram visualization
- ❌ No dynamic tracing level control

#### 2. Integration Testing

- ❌ No integration tests
- ❌ No end-to-end testing

#### 3. DC QUIC Integration

- ❌ Server not using s2n-quic-dc stream server
- ❌ Client not using s2n-quic-dc stream client
- ❌ No actual QUIC stream handling
- ❌ Metrics not connected to real DC QUIC events

## Compilation Status

**Result:** ✅ Compiles successfully with warnings

**Warnings (11 total):**

- Dead code warnings for unused metrics fields/methods (expected until integrated)
- Unused function `default_client_server_address`
- Unused `RequestStorage::new()` method

**No errors** - project is buildable.

## Technical Debt

1. **endpoint.rs commented out:** Main blocker for DC QUIC integration
2. **TCP placeholder:** Both server and client use TCP instead of DC QUIC streams
3. **Metrics not wired up:** MetricsSubscriber created but events not flowing through it
4. **No TUI:** Core feature missing entirely

## Dependencies Installed

- ✅ s2n-quic (path dependency)
- ✅ s2n-quic-dc with "testing" feature (path dependency)
- ✅ s2n-quic-core with "testing" feature (path dependency)
- ✅ s2n-codec (path dependency)
- ✅ tokio with "full" features
- ✅ clap with "derive" features
- ✅ serde with "derive" features
- ✅ toml
- ✅ ratatui
- ✅ crossterm
- ✅ tracing
- ✅ tracing-subscriber with "env-filter", "fmt", "json" features
- ✅ anyhow
- ✅ bytes
- ✅ parking_lot

## Next Priority Tasks

According to README's "In Progress" section:

### 🎯 HIGH PRIORITY (Next to implement)

**1. Endpoint Setup Integration** ⭐ IMMEDIATE NEXT TASK

- Uncomment endpoint module in main.rs
- Resolve any version conflicts or compilation issues
- Integrate endpoint::Server with server::Server
- Integrate endpoint::Client with client::Client
- Test PSK handshake works
- Verify UDP transport operational
- Confirm test certificates work

**2. Server Implementation Enhancement**

- Replace TCP listener with DC QUIC stream server from endpoint
- Accept streams using stream::server::accept
- Wire up MetricsSubscriber to actual DC events
- Implement proper stream handling loop
- Handle multiple concurrent connections

**3. Client Implementation Enhancement**

- Replace TCP connections with DC QUIC streams from endpoint
- Open streams to server endpoint
- Wire up MetricsSubscriber to actual DC events
- Implement burst workload with thousands of concurrent streams
- Track per-stream metrics

### 🎯 MEDIUM PRIORITY

**4. TUI Implementation (ratatui)**

- Create TUI module with tab system
- Implement Logs tab (scrollable log viewer)
- Implement Goodput Graph tab (line chart over time)
- Implement Latency Histogram tab (distribution visualization)
- Implement Overview tab (summary statistics)
- Wire up metrics to TUI refresh loop
- Add dynamic tracing level control

**5. Testing & Validation**

- Create integration tests
- Test with various workload sizes
- Validate goodput calculations
- Test packet loss tracking
- Verify latency measurements

### 🎯 NICE TO HAVE

**6. Additional Features**

- Add more workload patterns
- Support for custom protocols
- Enhanced error handling
- Better logging
- Performance profiling

## Key Design Goals (from README)

The CLI should demonstrate:

1. ✅ Traffic shaping works across all streams (protocol ready)
2. ❌ High goodput (>90%) with bursty workloads (not yet tested)
3. ❌ Minimal packet loss with 10K+ concurrent streams (not yet tested)
4. ❌ Predictable latencies under load (not yet tested)

## Files Overview

```
dc/s2n-quic-dc-cli/
├── Cargo.toml                 ✅ Complete with all deps
├── README.md                  ✅ Comprehensive documentation
├── example-config.toml        ✅ Working example
├── INVENTORY.md              ✅ This file
└── src/
    ├── main.rs               ✅ CLI framework complete
    ├── config.rs             ✅ TOML config system complete
    ├── protocol.rs           ✅ Request/Response protocol complete
    ├── metrics.rs            ✅ Metrics tracking complete
    ├── endpoint.rs           ⚠️  Complete but commented out
    ├── server.rs             ⚠️  TCP placeholder, needs DC QUIC
    └── client.rs             ⚠️  TCP placeholder, needs DC QUIC
```

## Conclusion

**Current State:** Foundation is solid with ~60% of core functionality implemented.

**Blockers:**

1. endpoint.rs needs to be uncommented and integrated
2. Server and client need to migrate from TCP to DC QUIC streams

**Ready for:** Immediate work on integrating the DC QUIC endpoint code to replace TCP placeholders.

**Estimated completion:**

- DC QUIC integration: 1-2 days
- TUI implementation: 2-3 days
- Testing & polish: 1 day
- **Total: ~5-6 days to MVP**
