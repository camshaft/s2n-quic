# s2n-quic-dc CLI

A CLI tool for testing s2n-quic-dc with artificial workloads to demonstrate traffic shaping, goodput, and predictable latencies under bursty conditions.

## Overview

This tool provides:

- **Server mode**: Accepts connections and responds to client requests with configurable delays and payload sizes
- **Client mode**: Creates burst workloads with thousands of concurrent streams
- **Metrics tracking**: Real-time goodput calculation, packet loss, and latency monitoring
- **TUI interface** (planned): Real-time visualization with multiple views for logs, metrics, and graphs

## Current Status

### ✅ Completed

- [x] Project structure and dependencies
- [x] Request/Response protocol (16-byte header: delay_ms + response_size)
- [x] GoodputSubscriber for metrics tracking
- [x] TOML configuration support with multiple named workloads
- [x] Basic CLI structure with server/client subcommands
- [x] Example configuration file

### ✅ Recently Completed

- [x] Endpoint setup (PSK handshake, UDP transport, test certificates)
- [x] Server implementation with DC QUIC streams
- [x] Client implementation with concurrent burst workloads
- [x] Custom protocol over DC QUIC streams

### 🚧 In Progress

- [ ] End-to-end testing with workloads
- [ ] Wire up MetricsSubscriber to DC events
- [ ] TUI with ratatui (5 tabs: Logs, Goodput Graph, Latency Histogram, Panics, Overview)
- [ ] Panic capture and display in TUI
- [ ] Dynamic tracing level control

## Configuration

Create a `config.toml` file (see `example-config.toml`):

```toml
[server]
listen_address = "0.0.0.0:4433"

[tui]
refresh_rate_ms = 100

[workload.large_burst]
num_streams = 10000
request_size = 1024
response_size = 65536
delay_ms = 100

[workload.small_quick]
num_streams = 100
request_size = 512
response_size = 4096
delay_ms = 10
```

## Usage (Planned)

```bash
# Run server
cargo run -- -c config.toml server

# Run client with specific workload
cargo run -- -c config.toml client --workload large_burst

# Run client with multiple workloads
cargo run -- -c config.toml client --workload small_quick --workload large_burst
```

## Architecture

### Protocol

- **Request**: 16 bytes (8 bytes delay_ms + 8 bytes response_size)
- **Response**: Variable size data payload

### Metrics Tracked

- **Goodput**: `total bytes ACKed / (total bytes transmitted + control bytes)`
- **Packet loss rate**: `packets lost / packets transmitted`
- **Latency**: Per-packet RTT from transmission to ACK
- **Throughput**: Bytes/second

### TUI Design (Planned)

```
┌─────────────────────────────────────────────────────────┐
│ [Logs] [Goodput] [Latency] [Overview]                   │
├─────────────────────────────────────────────────────────┤
│                                                          │
│  Tab content here (logs, graphs, histograms, stats)     │
│                                                          │
└─────────────────────────────────────────────────────────┘
```

## Goals

This tool demonstrates:

1. **Traffic shaping** works across all streams
2. **High goodput** (>90%) even with extremely bursty workloads
3. **Minimal packet loss** with tens of thousands of concurrent streams
4. **Predictable latencies** under load

## Development

```bash
# Check compilation
cargo check

# Build
cargo build --release

# Run with example config
cargo run -- -c example-config.toml server
```
