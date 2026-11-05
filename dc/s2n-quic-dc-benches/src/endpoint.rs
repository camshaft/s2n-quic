// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::bench::{black_box, BatchSize, BenchmarkId, Criterion, Throughput};

pub fn benchmarks(c: &mut Criterion) {
    assemble_benches(c);
    ack_processing_benches(c);
}

fn assemble_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("endpoint/assemble");
    // `segments` is the GSO max_segments the assembler packs per datagram. The
    // segments=1 rows preserve the original (non-GSO-batched) measurements; the
    // segments={8,32,64} rows exercise the GSO-packing path the real send loop
    // uses, so the send-assembler cost is measurable across the GSO regime
    // (32 segments == the busy-poll c32 regime; 64 == the kernel GSO ceiling).
    let scenarios = [
        (16usize, 1usize, 32usize, 1usize),
        (16, 8, 32, 1),
        (64, 8, 32, 1),
        (64, 16, 16, 1),
        (128, 16, 16, 1),
        (64, 8, 32, 8),
        (64, 8, 32, 32),
        (64, 8, 32, 64),
    ];

    for (packets, frames_per_packet, payload_len, segments) in scenarios {
        group.throughput(Throughput::Elements((packets * frames_per_packet) as u64));
        let input_name = format!(
            "packets={packets},frames={frames_per_packet},payload={payload_len},segments={segments}"
        );
        group.bench_with_input(BenchmarkId::new("assemble", &input_name), &(), |b, _| {
            b.iter_batched(
                || {
                    s2n_quic_dc::endpoint::testing::bench::AssembleBenchmark::new_with_segments(
                        packets,
                        frames_per_packet,
                        payload_len,
                        segments,
                    )
                },
                |benchmark| {
                    black_box(benchmark.run());
                },
                BatchSize::SmallInput,
            );
        });
    }
}

fn ack_processing_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("endpoint/ack_processing");
    let scenarios = [
        (16usize, 1usize, 32usize, 1usize),
        (64, 1, 32, 1),
        (64, 8, 32, 1),
        (128, 8, 16, 4),
        (256, 4, 16, 8),
    ];

    for (packets, frames_per_packet, payload_len, ack_frames) in scenarios {
        group.throughput(Throughput::Elements(packets as u64));
        let input_name = format!(
            "packets={packets},frames={frames_per_packet},payload={payload_len},ack_frames={ack_frames}"
        );
        group.bench_with_input(BenchmarkId::new("ack", &input_name), &(), |b, _| {
            b.iter_batched(
                || {
                    s2n_quic_dc::endpoint::testing::bench::AckProcessingBenchmark::new(
                        packets,
                        frames_per_packet,
                        payload_len,
                        ack_frames,
                    )
                },
                |benchmark| {
                    black_box(benchmark.run());
                },
                BatchSize::SmallInput,
            );
        });
    }
}
