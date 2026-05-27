# ACK Processor Scalability Problem

## Observation

The strss-1-dcquic stress cluster is configured to drive 25 Gbps per instance (10 aggregator hosts, 10 storage hosts). During the recovered steady-state period (after an initial probe black-hole event subsides), per-host throughput peaks at approximately 7.5 Gbps — roughly 3x below target. The bottleneck is not network, NIC, or congestion control. Every host reports `send.app_limited` at ~6000 per metrics interval with `send.cca_limited` near zero. The flows have data to send but the event loop cannot service them fast enough.

## Task Poll Latency

Metrics reveal the event loop on storage hosts is severely overloaded:

| Metric | Storage | Aggregator |
|--------|---------|------------|
| task.*.next_poll_latency p50 | 11.4 ms | 76 us |
| task.*.next_poll_latency p99 | 22.5 ms | 1.4 ms |
| task.*.next_poll_latency peak | 416 ms | 99 ms |

All tasks on storage share the same cooperative scheduler, and all report identical poll latencies. This means one task is monopolizing the loop, starving all others.

## The Culprit: `task.ack_processor`

Breaking down CPU budget consumption per task on storage (estimated from `task.*.time` histograms multiplied by poll count):

| Task | Seconds per 10s report (p50) | Seconds per 10s report (p99) |
|------|------|------|
| `task.ack_processor` | 6.4 | 14.5 |
| `task.assembler` | 0.3 | 1.6 |
| `task.completion` | 0.1 | 0.5 |
| everything else | < 0.1 each | < 0.3 each |

The ack_processor alone consumes 64% of the single-threaded event loop budget at p50, and exceeds the entire budget at p99. This leaves negligible time for the tx_wheel to fire transmissions, the assembler to build packets, or any other task to make progress.

## Workload Characteristics

During the steady-state period, storage processes approximately 186k ACKs per 10-second reporting interval (18.6k ACKs/sec). Each ACK drains ~5 items from the queue at median. The ack_processor is polled 68k times per report interval, with an average per-poll execution time of 92 us at p50 and 210 us at p99.

The per-ACK cost is therefore approximately 6.4s / 186k = 34 microseconds per ACK. For a simple "look up packet number, remove entry, notify completion" operation, 34 us is far too expensive.

## The Pending Queue Asymmetry

A key datapoint explaining the cost:

| Metric | Storage | Aggregator |
|--------|---------|------------|
| `send.pending.depth_dist` p50 | 439 | 43 |
| `send.pending.depth_dist` p99 | 953 | 120 |
| `send.pending.depth_dist` peak | 11,295 | 669 |

Storage has 10x more pending packets per flow than aggregator. Storage is the "big sender" in the RPC model (it sends response payloads), so it accumulates many more unacknowledged packets per flow.

## How the Packet Number Map Works

The inflight map (`s2n_quic_core::packet::number::Map`) is a ring buffer where each slot corresponds to a packet number offset from the base. Packet number N maps to index `(base_index + (N - start)) % capacity`. The map tracks which packet numbers have been sent but not yet acknowledged.

The `remove_range(start..=end)` operation, used when processing an ACK range, creates a `RemoveIter` that walks every slot from the range start to the range end:

```
while self.remaining > 0 {
    self.remaining -= 1;
    // advance to next slot
    if let Some(info) = self.packets.values[index].take() {
        return Some((...));
    }
    // slot was None — already removed — keep scanning
}
```

The iterator visits every packet number in the ACK range regardless of whether that slot still contains an entry. If an ACK covers PNs 1000-5000 but only PNs 4990-5000 are still present (the rest were already acknowledged or declared lost), the iterator still touches all 4000 intermediate empty slots.

Additionally, when entries are removed from the front of the map, `set_start()` performs a linear scan forward to find the next occupied slot:

```
for packet_number in PacketNumberRange::new(packet_number, self.end) {
    if self.get(packet_number).is_some() {
        self.index = ...;
        self.start = packet_number;
        return;
    }
}
```

## Why Storage Is Particularly Affected

The pathology emerges from two interacting factors:

First, storage flows have large congestion windows (cwnd p50 = 256 KB) and send many packets per flow before receiving acknowledgment. This creates a wide span between the lowest and highest packet numbers in the inflight map.

Second, ACKs from the aggregator frequently cover ranges that have already been partially cleared by prior ACKs or loss detection. The cumulative ACK field advances, but the range still spans the entire gap. Each duplicate or overlapping ACK range forces the iterator to walk through hundreds of empty slots to find the few remaining entries, or to find nothing at all.

The result is that the work performed by `remove_range` is proportional to the *span* of packet numbers in the ACK range, not the number of *actual entries* remaining. With 439 pending packets spread across a much larger packet number space (due to gaps from previously cleared entries), each ACK scan touches far more empty slots than occupied ones.

## Downstream Effects

The event loop starvation creates a vicious cycle:

1. The ack_processor consumes most of the loop budget processing ACKs over sparse ranges.
2. The tx_wheel and assembler get starved — they cannot fire sends on time.
3. Pending packets accumulate further because the send path cannot drain them.
4. More pending packets means wider PN spans in the inflight map.
5. Future ACK ranges cover even wider spans with more empty slots.
6. The ack_processor takes even longer.

This also explains why `q.router_to_batcher` on the aggregator side backs up to peak depths of 10,655 — the aggregator sends data to storage, but storage's event loop is too busy to process incoming packets promptly, causing backpressure that propagates back to the aggregator's inbound queue.

## Quantifying the Gap

At 7.5 Gbps peak per host (observed) vs 25 Gbps target, the system is achieving 30% of its bandwidth goal. The ack_processor consuming 64% of the event loop implies that if ACK processing were instantaneous, the available budget for actual data transmission would roughly triple — aligning with the 3x throughput gap.

## Metrics Summary

All measurements from strss-1-dcquic cluster, 60-minute observation window, storage hosts during recovered steady-state (minutes 52-58 relative to observation start):

- 10 hosts per role (aggregator, storage), 1 dead storage host
- Per-host peak bandwidth: 7.5 Gbps (target: 25 Gbps)
- `task.ack_processor.time` p50: 92 us/poll, 68k polls/report = 6.4s/10s
- `send.pending.depth_dist` p50: 439, peak: 11,295
- `send.app_limited`: ~6000 per report (flow starvation)
- `send.cca_limited`: ~2 per report (not congestion-limited)
- `q.ack.drain`: 186k ACKs per 10s report interval
