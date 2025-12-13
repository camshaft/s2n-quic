# Libfabric Polyfill and Runtime Fallback

This document describes the libfabric support strategy in s2n-quic-dc, which includes both compile-time polyfills and runtime fallback.

## Overview

S2n-quic-dc supports three levels of libfabric integration:

1. **Compile-time polyfill** - For platforms that never have libfabric (macOS, Windows, embedded)
2. **Runtime fallback** - For platforms that might have libfabric, with automatic detection
3. **Native libfabric** - When available and successfully initialized

This multi-level approach ensures the system works everywhere while utilizing hardware acceleration when available.

## Three Levels of Support

### 1. Compile-Time Polyfill

The `ofi-libfabric-sys-polyfill` crate provides FFI-compatible replacements for all libfabric C functions. This is used when:
- Building for platforms without libfabric support (macOS, Windows)
- Compile-time decision to avoid libfabric dependency

The polyfill implements the libfabric FFI layer, allowing high-level bindings in `src/libfabric.rs` to work unchanged.

### 2. Runtime Fallback

The `transport` module provides a provider abstraction that detects capabilities at runtime:
- Attempts to initialize libfabric
- If initialization fails (library not found, no RDMA hardware, permissions), falls back to UDP
- Transparent to application code

This is the **recommended approach** for Linux deployments that may or may not have EFA/RDMA.

### 3. Native Libfabric

When libfabric is available and successfully initializes, the system uses hardware RDMA for maximum performance.

## Feature Flags

```toml
[features]
default = ["tokio", "libfabric-polyfill"]
libfabric = ["dep:ofi-libfabric-sys"]           # Real libfabric with runtime fallback
libfabric-polyfill = ["dep:ofi-libfabric-sys-polyfill"]  # Compile-time polyfill
```

### Recommended: Runtime Fallback (Linux)

```bash
# Build with libfabric support + automatic UDP fallback
cargo build --features libfabric --no-default-features --features tokio

# At runtime:
# - Tries to initialize libfabric
# - Falls back to UDP if libfabric unavailable
# - Transparent to application
```

### Compile-Time Polyfill (macOS, Windows, embedded)

```bash
# Build with UDP-only (no libfabric dependency)
cargo build  # default includes libfabric-polyfill
```

### Testing Both Paths

```bash
# Test with compile-time polyfill
cargo test --package s2n-quic-dc

# Test with libfabric (if available on your system)
cargo test --package s2n-quic-dc --features libfabric --no-default-features --features tokio
```

## Architecture

### Layer 1: FFI Layer (Compile-Time)

**Files:** `dc/ofi-libfabric-sys-polyfill/`

Provides C-compatible functions matching libfabric's API. Used when `libfabric-polyfill` feature is enabled (default).

### Layer 2: High-Level Bindings

**Files:** `dc/s2n-quic-dc/src/libfabric.rs`

Rust-safe wrappers over FFI layer. Works with both real libfabric and polyfill.

### Layer 3: Transport Provider Abstraction (Runtime)

**Files:** `dc/s2n-quic-dc/src/transport.rs`

Runtime provider selection with automatic fallback:

```rust
pub trait TransportProvider {
    fn transport_type(&self) -> TransportType;  // Libfabric or Udp
    fn initialize(&mut self) -> io::Result<()>; // May fail
    fn send(&self, ...) -> io::Result<usize>;
    // ... other operations
}

// Automatically selects best available provider
let provider = select_provider()?;
```

The provider abstraction maps transport concepts to implementations:

### Memory Registration

- **Libfabric**: Registers memory with the NIC for RDMA operations, returns hardware memory key
- **Polyfill**: Tracks buffers in userspace with synthetic keys generated atomically

```rust
// Both real and polyfill have the same API
let buffer = ByteVec::from(vec![1, 2, 3, 4]);
let mr = memory_registration::Send::register(&domain, buffer, Access::SEND)?;
let key = mr.key(); // Synthetic key in polyfill, hardware key with libfabric
```

### RDMA Write

- **Libfabric**: Hardware performs direct memory write to remote peer
- **Polyfill**: Sends buffer contents over UDP (similar to DATA_CHUNKS in the V2 protocol design)

```rust
// API is identical
endpoint.write(&mr, len, dest_addr, remote_addr, remote_key)?;
```

### Send/Receive

- **Libfabric**: Uses RDMA reliable datagram transport
- **Polyfill**: Uses UDP socket operations

```rust
endpoint.send(&send_mr, len, dest_addr)?;
endpoint.recv(recv_mr, src_addr)?;
```

### Completion Queue

- **Libfabric**: Hardware notifies of operation completion via CQ
- **Polyfill**: Tracks operations in thread-safe queue, provides async completions

```rust
let count = cq.read::<_, 32>(limit, |memory_region| {
    // Handle completion
})?;
```

### Address Vector

- **Libfabric**: Maps addresses to hardware endpoint IDs
- **Polyfill**: Maps fi_addr_t handles to socket addresses

```rust
let handle = av.insert(addr_bytes)?;
endpoint.send(&mr, len, handle)?; // Uses handle to resolve destination
```

## Implementation Details

### Thread Safety

All polyfill types are thread-safe:
- Memory registration uses atomic counters for key generation
- Completion queues use `Mutex<Vec<...>>` for pending operations
- Address vectors use `Mutex<HashMap<...>>` for address storage
- Reference counting via `Arc` for shared ownership

### Memory Management

The polyfill uses Rust's standard memory safety guarantees:
- Send memory regions use `Arc<SendInner>` for cloneable buffers
- Receive memory regions use `Box<ReceiveInner>` for unique ownership
- Buffers are automatically freed when all references are dropped

### Non-blocking Operations

The UDP socket is set to non-blocking mode to match libfabric semantics:

```rust
socket.set_nonblocking(true)?;
```

Operations return immediately and completions are delivered via the completion queue.

## Implementation Status

The polyfill currently provides:

✅ **Fully Implemented:**
- Memory registration with synthetic key generation
- Address vector with handle-to-address mapping
- Completion queue structure
- Endpoint lifecycle management
- UDP socket creation and binding
- Type-safe memory region reconstruction

⚠️ **Partial Implementation:**
- Send/recv operations (structure in place, needs protocol integration)
- RDMA write/read (stubs present, needs DATA_CHUNKS/REQUEST protocol)

The partial implementations provide API compatibility and basic structure. Full functionality requires integration with the transport layer's message protocol described in V2.md (DATA_CHUNKS, READ_REQUEST/RESPONSE, WRITE_DATA messages).

## Limitations

The polyfill provides functional implementations but with some differences from hardware RDMA:

1. **Performance**: UDP has higher latency and lower throughput than RDMA
2. **Ordering**: UDP may deliver packets out of order
3. **Reliability**: UDP requires application-level retransmission for reliability
4. **Memory**: No true zero-copy - data is copied through userspace buffers
5. **Bandwidth**: Limited by UDP socket buffer sizes and kernel networking stack
6. **Protocol integration**: Send/recv operations need full message protocol implementation

These limitations are acceptable for:
- Development and testing environments
- Platforms without RDMA hardware
- API compatibility testing
- Scenarios where functionality is more important than peak performance

## When to Use Which Approach

### Use Runtime Fallback When:
- ✅ Deploying to Linux that **might** have EFA/RDMA
- ✅ Want maximum performance when available
- ✅ Need graceful degradation when hardware unavailable
- ✅ AWS EC2 instances (some have EFA, some don't)

### Use Compile-Time Polyfill When:
- ✅ Building for macOS or Windows
- ✅ Embedded or constrained environments
- ✅ Know for certain no RDMA hardware will be present
- ✅ Want smallest binary (no libfabric linking)

### Example: AWS Deployment

```rust
// Initialize transport with automatic fallback
let provider = s2n_quic_dc::transport::select_provider()?;

match provider {
    ProviderSelection::Libfabric(_) => {
        log::info!("Running on EFA-enabled instance");
    }
    ProviderSelection::Udp(_) => {
        log::info!("Running on non-EFA instance, using UDP");
    }
}
```

## Testing

Build and test with the polyfill:

```bash
# Build with polyfill (default)
cargo build --package s2n-quic-dc

# Build with polyfill explicitly
cargo build --package s2n-quic-dc --features libfabric-polyfill --no-default-features

# Run tests with polyfill  
cargo test --package s2n-quic-dc

# Build with real libfabric (requires libfabric installed)
cargo build --package s2n-quic-dc --features libfabric --no-default-features
```

## Future Enhancements

Potential improvements to the polyfill:

1. **Implement actual UDP send/recv**: Currently stubs, needs integration with socket operations
2. **Add protocol messages**: Implement READ_REQUEST/RESPONSE and WRITE_DATA messages
3. **Connection tracking**: Maintain per-peer connection state
4. **Reliability layer**: Add sequence numbers and retransmission
5. **Flow control**: Implement congestion control and backpressure
6. **Performance optimization**: Use io_uring or other async I/O when available

## Compatibility

The polyfill maintains API compatibility with the real libfabric module. Code using the libfabric module should work unchanged with the polyfill, though with different performance characteristics.

## See Also

- [V2.md](V2.md) - Overall transport architecture and protocol design
- [libfabric documentation](https://ofiwg.github.io/libfabric/) - Official libfabric docs
- `src/libfabric.rs` - High-level Rust bindings (works with both real and polyfill)
- `../ofi-libfabric-sys-polyfill/src/lib.rs` - FFI-level UDP polyfill implementation
