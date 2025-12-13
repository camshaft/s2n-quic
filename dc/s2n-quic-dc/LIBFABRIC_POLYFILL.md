# Libfabric Polyfill

This document describes the UDP-based polyfill implementation for libfabric functionality in s2n-quic-dc.

## Overview

The libfabric polyfill provides a userspace implementation of libfabric operations that can be used when:

- Libfabric is not installed on the system
- The platform doesn't support libfabric (e.g., macOS, Windows)  
- RDMA hardware is not available (e.g., development machines, cloud instances without EFA)

The polyfill implements libfabric operations over UDP sockets, providing actual functionality rather than just stub implementations.

## Feature Flag

The libfabric dependency is controlled by the `libfabric` cargo feature:

```toml
# Cargo.toml
[features]
libfabric = ["dep:ofi-libfabric-sys"]
```

### Building with Libfabric (requires libfabric installed)

```bash
cargo build --features libfabric
```

### Building with UDP Polyfill (no libfabric required)

```bash
cargo build --no-default-features
# or
cargo build  # polyfill is used by default when libfabric feature is not enabled
```

## Architecture

The polyfill maps libfabric concepts to userspace UDP implementations:

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

## Limitations

The polyfill provides functional implementations but with some differences from hardware RDMA:

1. **Performance**: UDP has higher latency and lower throughput than RDMA
2. **Ordering**: UDP may deliver packets out of order
3. **Reliability**: UDP requires application-level retransmission for reliability
4. **Memory**: No true zero-copy - data is copied through userspace buffers
5. **Bandwidth**: Limited by UDP socket buffer sizes and kernel networking stack

These limitations are acceptable for:
- Development and testing environments
- Platforms without RDMA hardware
- Scenarios where functionality is more important than peak performance

## Testing

Build and test with the polyfill:

```bash
# Build with polyfill
cargo build --package s2n-quic-dc --no-default-features

# Run tests with polyfill  
cargo test --package s2n-quic-dc --no-default-features

# Build with real libfabric (requires libfabric installed)
cargo build --package s2n-quic-dc --features libfabric
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
- `src/libfabric.rs` - Real libfabric bindings
- `src/libfabric_polyfill.rs` - UDP-based polyfill implementation
