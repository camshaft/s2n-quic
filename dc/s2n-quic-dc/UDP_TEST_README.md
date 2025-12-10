# UDP Busy Poll Testing Tool

A CLI tool for testing UDP socket performance using busy polling in the s2n-quic-dc crate.

## Purpose

This tool helps test UDP socket layer performance by:
- Using busy poll sockets for low-latency packet processing
- Measuring round-trip time (RTT) at the UDP level
- Detecting packet loss
- Separating socket-level issues from stream protocol issues

The tool validates that with proper traffic shaping (leaky bucket), there should be no packet loss at the UDP layer, helping identify whether issues are in the socket layer or in the stream code on top.

## Building

```bash
cd dc/s2n-quic-dc
cargo build --bin udp-test
```

## Usage

### Server Mode

Start a server that echoes packets back:

```bash
cargo run --bin udp-test -- server --port 8080
```

Options:
- `--ip <IP>`: IP address to bind to (default: 0.0.0.0)
- `--port <PORT>`: Port to listen on (default: 8080)
- `--busy-poll-us <MICROSECONDS>`: Busy poll timeout in microseconds (default: 50)

### Client Mode

Send bursts of packets to a server:

```bash
cargo run --bin udp-test -- client \
    --server-ip 127.0.0.1 \
    --server-port 8080 \
    --burst-size 10 \
    --burst-delay-ms 100 \
    --num-bursts 100
```

Options:
- `--server-ip <IP>`: Server IP address (required)
- `--server-port <PORT>`: Server port (required)
- `--burst-size <N>`: Number of packets per burst (default: 10)
- `--burst-delay-ms <MS>`: Delay between bursts in milliseconds (default: 100)
- `--num-bursts <N>`: Number of bursts to send (default: 100)
- `--payload-size <BYTES>`: Payload size in bytes (default: 1024)
- `--busy-poll-us <MICROSECONDS>`: Busy poll timeout in microseconds (default: 50)

## Example Session

Terminal 1 (Server):
```bash
$ cargo run --bin udp-test -- server --port 9999
UDP server listening on 0.0.0.0:9999
Busy polling enabled with 50µs
Stats: 150 packets, 0 MB, 150.32 pps, 1.23 Mbps
```

Terminal 2 (Client):
```bash
$ cargo run --bin udp-test -- client --server-ip 127.0.0.1 --server-port 9999 --burst-size 5 --num-bursts 3
UDP client bound to 0.0.0.0:48245
Busy polling enabled with 50µs
Connecting to 127.0.0.1:9999
Burst size: 5 packets, Delay: 100ms, Payload: 1024 bytes

Burst 0: sent=5, received=5, loss=0.00%, duration=250.12µs
Burst 1: sent=5, received=5, loss=0.00%, duration=305.32µs
Burst 2: sent=5, received=5, loss=0.00%, duration=294.15µs

=== Final Statistics ===
Total sent: 15
Total received: 15
Packet loss: 0.00%
RTT: min=181µs, max=304µs, avg=245µs
```

## Understanding the Output

### Client Statistics
- **sent/received**: Number of packets sent and received per burst
- **loss**: Percentage of packets lost (should be 0% with traffic shaping)
- **duration**: Time taken for the burst
- **RTT (Round-Trip Time)**: 
  - **min**: Fastest packet round trip
  - **max**: Slowest packet round trip
  - **avg**: Average round trip time

### Server Statistics
- **packets**: Total packets received
- **MB**: Total megabytes received
- **pps**: Packets per second
- **Mbps**: Megabits per second

## Busy Polling

Busy polling (SO_BUSY_POLL) is a Linux socket option that reduces latency by polling the network device for incoming packets instead of relying purely on interrupts. This is useful for low-latency applications.

The `--busy-poll-us` parameter sets how long (in microseconds) the kernel will busy-poll when waiting for data. Higher values can reduce latency but increase CPU usage.

**Note**: Setting SO_PREFER_BUSY_POLL may require elevated privileges. If you see warnings about this, the tool will still work, just with slightly different busy poll behavior.

## Troubleshooting

### "Operation not permitted" warnings
These warnings about SO_PREFER_BUSY_POLL are normal in environments without certain privileges. The tool will still function with SO_BUSY_POLL enabled.

### High packet loss
If you see packet loss:
1. Ensure the server is running and reachable
2. Check network configuration
3. Try reducing burst size or increasing burst delay
4. Check if traffic shaping (leaky bucket) is configured correctly

### No response from server
- Verify server is running: `ps aux | grep udp-test`
- Check firewall rules
- Verify IP and port match between client and server
- Try with `127.0.0.1` for local testing first

## Implementation Details

The tool:
- Uses Tokio's async runtime for efficient I/O
- Enables SO_BUSY_POLL socket option for low latency
- Encodes sequence numbers in packet payloads for tracking
- Tracks per-burst and overall statistics
- Uses simple timeout-based loss detection
