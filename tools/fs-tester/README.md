# fs-tester

A configurable stress tester for the s2n-quic-dc storage IO scheduler (`s2n_quic_dc::fs`) — the
storage analog of `dc-tester`. It drives a chosen backend with a workload (read/write mix, op size,
device queue depth, concurrency, buffered vs. `O_DIRECT`) and reports achieved IOPS, throughput, and
per-op latency percentiles.

## Usage

```sh
# io_uring, O_DIRECT, 64 streams of 4 KiB mixed read/write, queue depth 256, for 10s (Linux):
cargo run -p fs-tester --release -- \
    --backend uring --direct --op-size 4096 --streams 64 --queue-depth 256 \
    --op-mix mixed --duration 10 --file /mnt/nvme1/fs-tester.dat

# Bounded blocking-syscall backend, buffered, large sequential writes:
cargo run -p fs-tester --release -- \
    --backend syscall --op-size 1048576 --streams 16 --op-mix write --duration 5

# From a TOML config (CLI flags override individual fields):
cargo run -p fs-tester --release -- --config tools/fs-tester/etc/example.toml
```

`--backend uring` is Linux only. `--direct` requires `--op-size` to be a multiple of 4096.

## What it measures

Each submitter stream loops `submit → await completion` against a shared device whose credit pool
(capacity = `queue_depth` op-cost units) governs how many ops are in flight; the rest park
cooperatively. The report shows total ops/errors, IOPS, throughput (Gbps + MiB/s), and latency
percentiles (p50/p90/p99/p999/max, in microseconds), so you can compare backends and workload shapes
and watch how the scheduler's fairness/backpressure behaves under contention.
