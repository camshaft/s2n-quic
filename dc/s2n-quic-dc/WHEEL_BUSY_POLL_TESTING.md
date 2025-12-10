# UDP Wheel and Busy Poll Testing Guide

This guide explains how to test the timing wheel, busy poll runtime, and clock abstractions in the s2n-quic-dc crate.

## Architecture Overview

The s2n-quic-dc crate provides several key abstractions for low-latency UDP packet processing:

1. **Timing Wheel** (`socket::send::wheel::Wheel`) - Schedules packet transmissions with precise timing
2. **Busy Poll Runtime** (`busy_poll::Handle` and `busy_poll::Pool`) - Provides dedicated worker threads for low-latency packet processing
3. **Clock Abstractions** (`clock::tokio::Clock` and `busy_poll::clock::Timer`) - Accurate timing for packet scheduling
4. **Socket Workers** - Integrate wheel, busy poll, and sockets for packet transmission/reception

## Testing with Existing Infrastructure

### Using the Test Helpers

The crate provides test helpers in `src/testing.rs`:

```rust
use s2n_quic_dc::testing::{send_busy_poll, recv_busy_poll};

// Get busy poll pools (automatically creates worker threads)
let send_pool = send_busy_poll();
let recv_pool = recv_busy_poll();
```

### Creating a Test Client/Server

```rust
use s2n_quic_dc::{
    psk::{client::Provider as ClientProvider, server::Provider as ServerProvider},
    stream::{
        client::tokio::Client as ClientTokio,
        server::tokio::Server as ServerTokio,
        socket::Protocol,
    },
    testing::{send_busy_poll, recv_busy_poll, NoopSubscriber},
};

// Server with busy poll
let server = ServerTokio::<ServerProvider, NoopSubscriber>::builder()
    .with_address("0.0.0.0:8080".parse().unwrap())
    .with_protocol(Protocol::Udp)
    .with_workers(1.try_into().unwrap())
    .with_send_socket_workers(send_busy_poll().into())
    .with_recv_socket_workers(recv_busy_poll().into())
    .build(server_provider, NoopSubscriber::default())
    .unwrap();

// Client with busy poll
let client = ClientTokio::<ClientProvider, NoopSubscriber>::builder()
    .with_default_protocol(Protocol::Udp)
    .with_send_socket_workers(send_busy_poll().into())
    .with_recv_socket_workers(recv_busy_poll().into())
    .build(client_provider, NoopSubscriber::default())
    .unwrap();
```

## What Gets Tested

When you use the infrastructure with busy poll workers enabled:

1. **Timing Wheel**:
   - Packets are scheduled based on precise timestamps
   - The wheel has configurable granularity (default 128µs) and horizon
   - Packets are sent at their scheduled time, not immediately
   - Multiple priority levels can be used

2. **Busy Poll Runtime**:
   - Dedicated worker threads run in tight loops
   - No blocking on async runtime - pure busy polling
   - Low-latency packet processing (microsecond level)
   - Priority-based task scheduling

3. **Clock Abstractions**:
   - `busy_poll::clock::Timer` for busy poll workers
   - Accurate timestamp tracking for RTT measurements
   - Drift compensation for precise timing

4. **Socket Workers**:
   - Send workers use `socket::send::udp::non_blocking` with the wheel
   - Receive workers process incoming packets via busy poll
   - Token bucket rate limiting integrates with the wheel
   - GSO/GRO support for batching

## Running Tests

### Existing Tests

Run the existing tests that use busy poll infrastructure:

```bash
cd dc/s2n-quic-dc
cargo test --features testing -- --nocapture
```

Look for tests in:
- `src/stream/tests/` - Integration tests using busy poll
- `src/socket/send/udp.rs` - Wheel and send worker tests

### Creating Custom Tests

To create a custom test with the wheel and busy poll:

```rust
#[tokio::test]
async fn test_wheel_busy_poll() {
    use s2n_quic_dc::testing::{send_busy_poll, recv_busy_poll};
    
    // Your test code here using the busy poll pools
    let send_pool = send_busy_poll();
    let recv_pool = recv_busy_poll();
    
    // Use these when building client/server...
}
```

## Validation

### What to Look For

1. **Zero Packet Loss**: With proper traffic shaping (leaky bucket), packet loss should be zero at the UDP layer
2. **Low Latency**: RTT should be in the microsecond range for local connections
3. **Accurate Timing**: Wheel should schedule packets at precise intervals
4. **Consistent Performance**: Busy poll should provide consistent low-latency processing

### Debugging

If you see issues:

1. **High Packet Loss**: 
   - Check token bucket configuration
   - Verify wheel horizon is appropriate
   - Check for socket buffer overflow

2. **High Latency**:
   - Verify busy poll workers are running
   - Check clock implementation
   - Look for blocking operations in the busy poll loop

3. **Timing Issues**:
   - Check wheel granularity setting
   - Verify clock drift compensation
   - Look at timestamp accuracy

## Implementation Details

### Wheel Configuration

```rust
// In environment setup (src/stream/environment/tokio/pool.rs)
let wheel = send::wheel::Wheel::new(config.send_wheel_horizon, clock);
```

- `send_wheel_horizon`: How far into the future the wheel can schedule (default 10ms)
- Clock: Provides current time for scheduling

### Busy Poll Worker Spawning

```rust
// Send worker with busy poll
socket.spawn_busy_poll_send_worker(config, env, handle);

// Receive worker with busy poll  
socket.spawn_busy_poll_recv_worker(config, env, alloc, router, handle);
```

These spawn tasks on the busy poll runtime instead of the tokio runtime.

### Token Bucket Integration

```rust
let token_bucket = send::udp::LeakyBucket::new(gigabits_per_second);
```

The leaky bucket rate limiter integrates with the wheel to shape traffic and prevent congestion.

## References

- Timing wheel implementation: `src/socket/send/wheel.rs`
- Busy poll runtime: `src/busy_poll.rs`
- Clock abstractions: `src/clock/` and `src/busy_poll/clock.rs`
- Socket workers: `src/socket/send/udp.rs`
- Environment setup: `src/stream/environment/tokio/pool.rs`
- Test helpers: `src/testing.rs`

## Conclusion

The s2n-quic-dc crate provides comprehensive infrastructure for testing low-latency UDP processing. By using the busy poll runtime, timing wheel, and clock abstractions together, you can validate that packets are processed with microsecond-level latency and accurate timing.

For specific testing needs, create tests that use the `send_busy_poll()` and `recv_busy_poll()` helpers, which automatically set up the full infrastructure including wheel, busy poll workers, and clocks.
