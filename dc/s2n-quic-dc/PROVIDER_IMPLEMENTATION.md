# Custom Libfabric Provider Implementation

## Overview

This document outlines the implementation plan for the custom UDP-based libfabric provider.

## Architecture

### Provider Registration

The custom provider registers with libfabric's provider interface using `fi_register_provider()`. When applications call `fi_getinfo()`, libfabric queries all registered providers (including ours) and returns matching capabilities.

### Provider Structure

```c
struct fi_provider {
    uint32_t version;
    uint32_t fi_version;
    struct fi_context context;
    const char *name;
    int (*getinfo)(uint32_t version, const char *node, const char *service,
                   uint64_t flags, const struct fi_info *hints,
                   struct fi_info **info);
    int (*fabric)(struct fi_fabric_attr *attr, struct fid_fabric **fabric,
                  void *context);
    void (*cleanup)(void);
};
```

## Implementation Phases

### Phase 1: Provider Registration (In Progress)

**File:** `src/libfabric_udp_provider.rs`

**Tasks:**
- [ ] Define provider struct matching libfabric's expectations
- [ ] Implement `getinfo` callback (enumerate capabilities)
- [ ] Implement `fabric` callback (create fabric context)
- [ ] Call `fi_register_provider()` during module initialization
- [ ] Test provider discovery with `fi_getinfo()`

**Success Criteria:**
- Provider appears in `fi_getinfo()` results
- Can create fabric context through provider

### Phase 2: Core Operations

**Tasks:**
- [ ] Implement domain creation
- [ ] Implement endpoint creation with UDP sockets
- [ ] Implement address vector (peer address mapping)
- [ ] Implement memory registration (synthetic keys)
- [ ] Implement completion queues

**Success Criteria:**
- Can open endpoints
- Can register memory
- Basic structure in place

### Phase 3: Data Transfer

**Tasks:**
- [ ] Implement `fi_send` / `fi_recv` over UDP
- [ ] Implement message framing
- [ ] Implement completion notifications
- [ ] Handle non-blocking I/O

**Success Criteria:**
- Can send/receive messages
- Completions work correctly

### Phase 4: RDMA-like Operations

**Tasks:**
- [ ] Implement `fi_read` (pull data from remote)
- [ ] Implement `fi_write` (push data to remote)
- [ ] Implement DATA_CHUNKS message protocol
- [ ] Implement READ_REQUEST/RESPONSE protocol

**Success Criteria:**
- RDMA-like operations work over UDP
- Maintains libfabric semantics

### Phase 5: Integration

**Tasks:**
- [ ] Integrate with worker architecture
- [ ] Performance testing
- [ ] Error handling
- [ ] Documentation

## Key Interfaces

### Provider Callbacks

```rust
// Enumerate provider capabilities
fn getinfo(hints: &fi_info) -> Result<Vec<fi_info>, Error>;

// Create fabric context
fn fabric(attr: &fi_fabric_attr) -> Result<*mut fid_fabric, Error>;

// Cleanup provider resources
fn cleanup();
```

### Fabric Operations

```rust
// Open domain
fn fi_domain(fabric: *mut fid_fabric, info: *mut fi_info) -> Result<*mut fid_domain, Error>;

// Open endpoint  
fn fi_endpoint(domain: *mut fid_domain, info: *mut fi_info) -> Result<*mut fid_ep, Error>;

// Bind resources
fn fi_ep_bind(ep: *mut fid_ep, fid: *mut fid, flags: u64) -> Result<(), Error>;

// Enable endpoint
fn fi_enable(ep: *mut fid_ep) -> Result<(), Error>;
```

### Data Operations

```rust
// Send message
fn fi_send(ep: *mut fid_ep, buf: *const void, len: usize, 
           desc: *mut void, dest: fi_addr_t, context: *mut void) -> Result<(), Error>;

// Receive message
fn fi_recv(ep: *mut fid_ep, buf: *mut void, len: usize,
           desc: *mut void, src: fi_addr_t, context: *mut void) -> Result<(), Error>;

// RDMA write
fn fi_write(ep: *mut fid_ep, buf: *const void, len: usize,
            desc: *mut void, dest: fi_addr_t, addr: u64, key: u64,
            context: *mut void) -> Result<(), Error>;

// RDMA read
fn fi_read(ep: *mut fid_ep, buf: *mut void, len: usize,
           desc: *mut void, src: fi_addr_t, addr: u64, key: u64,
           context: *mut void) -> Result<(), Error>;
```

## Testing Strategy

### Unit Tests

- Provider registration
- Capability enumeration
- Resource creation/cleanup
- Memory management

### Integration Tests

- End-to-end message passing
- RDMA operations
- Completion queue operations
- Error handling

### Interop Tests

- Works alongside real providers (EFA, verbs)
- Proper provider selection
- Fallback behavior

## References

- [Libfabric Programmer's Manual](https://ofiwg.github.io/libfabric/)
- [Libfabric Provider Development Guide](https://github.com/ofiwg/libfabric/wiki)
- V2.md - Transport architecture and protocol design
- LIBFABRIC_POLYFILL.md - Overall strategy

## Current Status

**Phase 1** - In Progress
- Stub created in `src/libfabric_udp_provider.rs`
- Next: Implement provider registration
- Goal: Make provider discoverable through `fi_getinfo()`
