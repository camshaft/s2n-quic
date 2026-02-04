// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

// DO NOT MODIFY THIS FILE
// This file was generated with the `s2n-events` crate and any required
// changes should be made there.

use crate::event::{
    self, api,
    metrics::aggregate::{
        info::{self, Str},
        AsVariant, BoolRecorder, Info, Metric, NominalRecorder, Recorder, Registry, Units,
    },
};
use core::sync::atomic::{AtomicU64, Ordering};
static INFO: &[Info; 339usize] = &[
    info::Builder {
        id: 0usize,
        name: Str::new("acceptor_tcp_started\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 1usize,
        name: Str::new("acceptor_tcp_loop_iteration_completed\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 2usize,
        name: Str::new("acceptor_tcp_loop_iteration_completed.pending_streams\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 3usize,
        name: Str::new("acceptor_tcp_loop_iteration_completed.slots_idle\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 4usize,
        name: Str::new("acceptor_tcp_loop_iteration_completed.slot_utilization\0"),
        units: Units::Percent,
    }
    .build(),
    info::Builder {
        id: 5usize,
        name: Str::new("acceptor_tcp_loop_iteration_completed.processing_duration\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 6usize,
        name: Str::new("acceptor_tcp_loop_iteration_completed.max_sojourn_time\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 7usize,
        name: Str::new("acceptor_tcp_fresh_enqueued\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 8usize,
        name: Str::new("acceptor_tcp_fresh_batch_completed\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 9usize,
        name: Str::new("acceptor_tcp_fresh_batch_completed.enqueued\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 10usize,
        name: Str::new("acceptor_tcp_fresh_batch_completed.dropped\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 11usize,
        name: Str::new("acceptor_tcp_fresh_batch_completed.errored\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 12usize,
        name: Str::new("acceptor_tcp_stream_dropped\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 13usize,
        name: Str::new("acceptor_tcp_stream_dropped.reason\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 14usize,
        name: Str::new("acceptor_tcp_stream_replaced\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 15usize,
        name: Str::new("acceptor_tcp_stream_replaced.sojourn_time\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 16usize,
        name: Str::new("acceptor_tcp_stream_replaced.buffer_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 17usize,
        name: Str::new("acceptor_tcp_packet_received\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 18usize,
        name: Str::new("acceptor_tcp_packet_received.payload_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 19usize,
        name: Str::new("acceptor_tcp_packet_received.is_fin\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 20usize,
        name: Str::new("acceptor_tcp_packet_received.is_fin_known\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 21usize,
        name: Str::new("acceptor_tcp_packet_received.sojourn_time\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 22usize,
        name: Str::new("acceptor_tcp_packet_dropped\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 23usize,
        name: Str::new("acceptor_tcp_packet_dropped.reason\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 24usize,
        name: Str::new("acceptor_tcp_packet_dropped.sojourn_time\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 25usize,
        name: Str::new("acceptor_tcp_stream_enqueued\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 26usize,
        name: Str::new("acceptor_tcp_stream_enqueued.sojourn_time\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 27usize,
        name: Str::new("acceptor_tcp_stream_enqueued.blocked_count\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 28usize,
        name: Str::new("acceptor_tcp_io_error\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 29usize,
        name: Str::new("acceptor_tcp_socket_sent\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 30usize,
        name: Str::new("acceptor_tcp_socket_sent.sojourn_time\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 31usize,
        name: Str::new("acceptor_tcp_socket_sent.blocked_count_host\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 32usize,
        name: Str::new("acceptor_tcp_socket_sent.blocked_count_stream\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 33usize,
        name: Str::new("acceptor_tcp_socket_sent.len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 34usize,
        name: Str::new("acceptor_tcp_socket_received\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 35usize,
        name: Str::new("acceptor_tcp_socket_received.transfer_time\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 36usize,
        name: Str::new("acceptor_tcp_socket_received.len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 37usize,
        name: Str::new("acceptor_udp_started\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 38usize,
        name: Str::new("acceptor_udp_datagram_received\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 39usize,
        name: Str::new("acceptor_udp_datagram_received.len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 40usize,
        name: Str::new("acceptor_udp_packet_received\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 41usize,
        name: Str::new("acceptor_udp_packet_received.payload_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 42usize,
        name: Str::new("acceptor_udp_packet_received.is_zero_offset\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 43usize,
        name: Str::new("acceptor_udp_packet_received.is_retransmission\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 44usize,
        name: Str::new("acceptor_udp_packet_received.is_fin\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 45usize,
        name: Str::new("acceptor_udp_packet_received.is_fin_known\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 46usize,
        name: Str::new("acceptor_udp_packet_dropped\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 47usize,
        name: Str::new("acceptor_udp_packet_dropped.reason\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 48usize,
        name: Str::new("acceptor_udp_stream_enqueued\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 49usize,
        name: Str::new("acceptor_udp_io_error\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 50usize,
        name: Str::new("acceptor_stream_pruned\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 51usize,
        name: Str::new("acceptor_stream_pruned.sojourn_time\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 52usize,
        name: Str::new("acceptor_stream_pruned.reason\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 53usize,
        name: Str::new("acceptor_stream_dequeued\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 54usize,
        name: Str::new("acceptor_stream_dequeued.sojourn_time\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 55usize,
        name: Str::new("stream_write_flushed\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 56usize,
        name: Str::new("stream_write_flushed.latency\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 57usize,
        name: Str::new("stream_write_flushed.conn\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 58usize,
        name: Str::new("stream_write_flushed.provided\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 59usize,
        name: Str::new("stream_write_flushed.committed.total\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 60usize,
        name: Str::new("stream_write_flushed.committed\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 61usize,
        name: Str::new("stream_write_flushed.committed.conn\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 62usize,
        name: Str::new("stream_write_flushed.processing_duration\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 63usize,
        name: Str::new("stream_write_flushed.processing_duration.conn\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 64usize,
        name: Str::new("stream_write_fin_flushed\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 65usize,
        name: Str::new("stream_write_fin_flushed.latency\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 66usize,
        name: Str::new("stream_write_fin_flushed.conn\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 67usize,
        name: Str::new("stream_write_fin_flushed.provided\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 68usize,
        name: Str::new("stream_write_fin_flushed.committed.total\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 69usize,
        name: Str::new("stream_write_fin_flushed.committed\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 70usize,
        name: Str::new("stream_write_fin_flushed.committed.conn\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 71usize,
        name: Str::new("stream_write_fin_flushed.processing_duration\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 72usize,
        name: Str::new("stream_write_fin_flushed.processing_duration.conn\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 73usize,
        name: Str::new("stream_write_blocked\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 74usize,
        name: Str::new("stream_write_blocked.latency\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 75usize,
        name: Str::new("stream_write_blocked.conn\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 76usize,
        name: Str::new("stream_write_blocked.provided\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 77usize,
        name: Str::new("stream_write_blocked.processing_duration\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 78usize,
        name: Str::new("stream_write_blocked.processing_duration.conn\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 79usize,
        name: Str::new("stream_write_errored\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 80usize,
        name: Str::new("stream_write_errored.latency\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 81usize,
        name: Str::new("stream_write_errored.provided\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 82usize,
        name: Str::new("stream_write_errored.processing_duration\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 83usize,
        name: Str::new("stream_write_errored.processing_duration.conn\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 84usize,
        name: Str::new("stream_write_key_updated\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 85usize,
        name: Str::new("stream_write_allocated\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 86usize,
        name: Str::new("stream_write_allocated.conn\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 87usize,
        name: Str::new("stream_write_allocated.allocated_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 88usize,
        name: Str::new("stream_write_allocated.allocated_len.conn\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 89usize,
        name: Str::new("stream_write_shutdown\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 90usize,
        name: Str::new("stream_write_shutdown.latency\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 91usize,
        name: Str::new("stream_write_shutdown.buffer_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 92usize,
        name: Str::new("stream_write_shutdown.background\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 93usize,
        name: Str::new("stream_write_socket_flushed\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 94usize,
        name: Str::new("stream_write_socket_flushed.conn\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 95usize,
        name: Str::new("stream_write_socket_flushed.provided\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 96usize,
        name: Str::new("stream_write_socket_flushed.committed.total\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 97usize,
        name: Str::new("stream_write_socket_flushed.committed\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 98usize,
        name: Str::new("stream_write_socket_flushed.committed.conn\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 99usize,
        name: Str::new("stream_write_socket_blocked\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 100usize,
        name: Str::new("stream_write_socket_blocked.conn\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 101usize,
        name: Str::new("stream_write_socket_blocked.provided\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 102usize,
        name: Str::new("stream_write_socket_errored\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 103usize,
        name: Str::new("stream_write_socket_errored.provided\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 104usize,
        name: Str::new("stream_read_flushed\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 105usize,
        name: Str::new("stream_read_flushed.latency\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 106usize,
        name: Str::new("stream_read_flushed.conn\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 107usize,
        name: Str::new("stream_read_flushed.capacity\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 108usize,
        name: Str::new("stream_read_flushed.committed.total\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 109usize,
        name: Str::new("stream_read_flushed.committed\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 110usize,
        name: Str::new("stream_read_flushed.committed.conn\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 111usize,
        name: Str::new("stream_read_flushed.processing_duration\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 112usize,
        name: Str::new("stream_read_flushed.processing_duration.conn\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 113usize,
        name: Str::new("stream_read_fin_flushed\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 114usize,
        name: Str::new("stream_read_fin_flushed.latency\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 115usize,
        name: Str::new("stream_read_fin_flushed.conn\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 116usize,
        name: Str::new("stream_read_fin_flushed.capacity\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 117usize,
        name: Str::new("stream_read_fin_flushed.processing_duration\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 118usize,
        name: Str::new("stream_read_fin_flushed.processing_duration.conn\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 119usize,
        name: Str::new("stream_read_blocked\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 120usize,
        name: Str::new("stream_read_blocked.latency\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 121usize,
        name: Str::new("stream_read_blocked.capacity\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 122usize,
        name: Str::new("stream_read_blocked.processing_duration\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 123usize,
        name: Str::new("stream_read_blocked.processing_duration.conn\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 124usize,
        name: Str::new("stream_read_errored\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 125usize,
        name: Str::new("stream_read_errored.latency\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 126usize,
        name: Str::new("stream_read_errored.capacity\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 127usize,
        name: Str::new("stream_read_errored.processing_duration\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 128usize,
        name: Str::new("stream_read_errored.processing_duration.conn\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 129usize,
        name: Str::new("stream_read_key_updated\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 130usize,
        name: Str::new("stream_read_shutdown\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 131usize,
        name: Str::new("stream_read_shutdown.latency\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 132usize,
        name: Str::new("stream_read_shutdown.background\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 133usize,
        name: Str::new("stream_read_socket_flushed\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 134usize,
        name: Str::new("stream_read_socket_flushed.conn\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 135usize,
        name: Str::new("stream_read_socket_flushed.capacity\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 136usize,
        name: Str::new("stream_read_socket_flushed.committed.total\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 137usize,
        name: Str::new("stream_read_socket_flushed.committed\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 138usize,
        name: Str::new("stream_read_socket_flushed.committed.conn\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 139usize,
        name: Str::new("stream_read_socket_blocked\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 140usize,
        name: Str::new("stream_read_socket_blocked.conn\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 141usize,
        name: Str::new("stream_read_socket_blocked.capacity\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 142usize,
        name: Str::new("stream_read_socket_errored\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 143usize,
        name: Str::new("stream_read_socket_errored.capacity\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 144usize,
        name: Str::new("stream_decrypt_packet\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 145usize,
        name: Str::new("stream_decrypt_packet.decrypted_in_place\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 146usize,
        name: Str::new("stream_decrypt_packet.forced_copy\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 147usize,
        name: Str::new("stream_decrypt_packet.required_application_buffer\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 148usize,
        name: Str::new("stream_tcp_connect\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 149usize,
        name: Str::new("stream_tcp_connect.error\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 150usize,
        name: Str::new("stream_tcp_connect.tcp_latency\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 151usize,
        name: Str::new("stream_connect\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 152usize,
        name: Str::new("stream_connect.error\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 153usize,
        name: Str::new("stream_connect.tcp\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 154usize,
        name: Str::new("stream_connect.handshake\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 155usize,
        name: Str::new("stream_connect_error\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 156usize,
        name: Str::new("stream_connect_error.reason\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 157usize,
        name: Str::new("stream_packet_transmitted\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 158usize,
        name: Str::new("stream_packet_transmitted.packet_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 159usize,
        name: Str::new("stream_packet_transmitted.payload_len.total\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 160usize,
        name: Str::new("stream_packet_transmitted.payload_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 161usize,
        name: Str::new("stream_packet_transmitted.payload_len.conn\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 162usize,
        name: Str::new("stream_packet_transmitted.retransmission\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 163usize,
        name: Str::new("stream_probe_transmitted\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 164usize,
        name: Str::new("stream_probe_transmitted.packet_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 165usize,
        name: Str::new("stream_packet_received\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 166usize,
        name: Str::new("stream_packet_received.packet_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 167usize,
        name: Str::new("stream_packet_received.payload_len.total\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 168usize,
        name: Str::new("stream_packet_received.payload_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 169usize,
        name: Str::new("stream_packet_received.payload_len.conn\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 170usize,
        name: Str::new("stream_packet_received.retransmission\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 171usize,
        name: Str::new("stream_packet_lost\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 172usize,
        name: Str::new("stream_packet_lost.packet_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 173usize,
        name: Str::new("stream_packet_lost.payload_len.total\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 174usize,
        name: Str::new("stream_packet_lost.payload_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 175usize,
        name: Str::new("stream_packet_lost.payload_len.conn\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 176usize,
        name: Str::new("stream_packet_lost.lifetime\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 177usize,
        name: Str::new("stream_packet_lost.retransmission\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 178usize,
        name: Str::new("stream_packet_acked\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 179usize,
        name: Str::new("stream_packet_acked.packet_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 180usize,
        name: Str::new("stream_packet_acked.payload_len.total\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 181usize,
        name: Str::new("stream_packet_acked.payload_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 182usize,
        name: Str::new("stream_packet_acked.payload_len.conn\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 183usize,
        name: Str::new("stream_packet_acked.lifetime\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 184usize,
        name: Str::new("stream_packet_acked.retransmission\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 185usize,
        name: Str::new("stream_packet_abandoned\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 186usize,
        name: Str::new("stream_packet_abandoned.packet_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 187usize,
        name: Str::new("stream_packet_abandoned.payload_len.total\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 188usize,
        name: Str::new("stream_packet_abandoned.payload_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 189usize,
        name: Str::new("stream_packet_abandoned.payload_len.conn\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 190usize,
        name: Str::new("stream_packet_abandoned.lifetime\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 191usize,
        name: Str::new("stream_packet_abandoned.retransmission\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 192usize,
        name: Str::new("stream_packet_spuriously_retransmitted\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 193usize,
        name: Str::new("stream_packet_spuriously_retransmitted.packet_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 194usize,
        name: Str::new("stream_packet_spuriously_retransmitted.payload_len.total\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 195usize,
        name: Str::new("stream_packet_spuriously_retransmitted.payload_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 196usize,
        name: Str::new("stream_packet_spuriously_retransmitted.payload_len.conn\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 197usize,
        name: Str::new("stream_packet_spuriously_retransmitted.retransmission\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 198usize,
        name: Str::new("stream_max_data_received\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 199usize,
        name: Str::new("stream_max_data_received.increase.total\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 200usize,
        name: Str::new("stream_max_data_received.increase\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 201usize,
        name: Str::new("stream_control_packet_transmitted\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 202usize,
        name: Str::new("stream_control_packet_transmitted.packet_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 203usize,
        name: Str::new("stream_control_packet_transmitted.control_data_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 204usize,
        name: Str::new("stream_control_packet_received\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 205usize,
        name: Str::new("stream_control_packet_received.packet_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 206usize,
        name: Str::new("stream_control_packet_received.control_data_len\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 207usize,
        name: Str::new("stream_control_packet_received.authenticated\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 208usize,
        name: Str::new("stream_receiver_errored\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 209usize,
        name: Str::new("stream_sender_errored\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 210usize,
        name: Str::new("connection_closed\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 211usize,
        name: Str::new("endpoint_initialized\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 212usize,
        name: Str::new("endpoint_initialized.acceptor.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 213usize,
        name: Str::new("endpoint_initialized.handshake.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 214usize,
        name: Str::new("endpoint_initialized.tcp\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 215usize,
        name: Str::new("endpoint_initialized.udp\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 216usize,
        name: Str::new("endpoint_udp_immediate_transmission_scheduled\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 217usize,
        name: Str::new("endpoint_udp_immediate_transmission_scheduled.peer.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 218usize,
        name: Str::new("endpoint_udp_immediate_transmission_scheduled.buffer_size\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 219usize,
        name: Str::new("endpoint_udp_immediate_transmission_scheduled.segment_size\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 220usize,
        name: Str::new("endpoint_udp_immediate_transmission_scheduled.segment_count\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 221usize,
        name: Str::new("endpoint_udp_transmission_scheduled\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 222usize,
        name: Str::new("endpoint_udp_transmission_scheduled.peer.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 223usize,
        name: Str::new("endpoint_udp_transmission_scheduled.buffer_size\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 224usize,
        name: Str::new("endpoint_udp_transmission_scheduled.segment_size\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 225usize,
        name: Str::new("endpoint_udp_transmission_scheduled.segment_count\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 226usize,
        name: Str::new("endpoint_udp_transmission_scheduled.delay\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 227usize,
        name: Str::new("endpoint_udp_transmission_rejected\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 228usize,
        name: Str::new("endpoint_udp_transmission_rejected.peer.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 229usize,
        name: Str::new("endpoint_udp_transmission_rejected.buffer_size\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 230usize,
        name: Str::new("endpoint_udp_transmission_rejected.segment_size\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 231usize,
        name: Str::new("endpoint_udp_transmission_rejected.segment_count\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 232usize,
        name: Str::new("endpoint_udp_transmission_rejected.delay\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 233usize,
        name: Str::new("endpoint_udp_transmission_rejected.backoff\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 234usize,
        name: Str::new("endpoint_udp_packet_transmitted\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 235usize,
        name: Str::new("endpoint_udp_packet_transmitted.peer.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 236usize,
        name: Str::new("endpoint_udp_packet_transmitted.buffer_size\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 237usize,
        name: Str::new("endpoint_udp_packet_transmitted.segment_size\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 238usize,
        name: Str::new("endpoint_udp_packet_transmitted.segment_count\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 239usize,
        name: Str::new("endpoint_udp_transmit_errored\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 240usize,
        name: Str::new("endpoint_udp_transmit_errored.peer.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 241usize,
        name: Str::new("endpoint_udp_transmit_errored.buffer_size\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 242usize,
        name: Str::new("endpoint_udp_transmit_errored.segment_size\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 243usize,
        name: Str::new("endpoint_udp_transmit_errored.segment_count\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 244usize,
        name: Str::new("endpoint_udp_packet_received\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 245usize,
        name: Str::new("endpoint_udp_packet_received.peer.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 246usize,
        name: Str::new("endpoint_udp_packet_received.buffer_size\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 247usize,
        name: Str::new("endpoint_udp_packet_received.segment_size\0"),
        units: Units::Bytes,
    }
    .build(),
    info::Builder {
        id: 248usize,
        name: Str::new("endpoint_udp_packet_received.segment_count\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 249usize,
        name: Str::new("endpoint_udp_receive_errored\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 250usize,
        name: Str::new("path_secret_map_initialized\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 251usize,
        name: Str::new("path_secret_map_initialized.capacity\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 252usize,
        name: Str::new("path_secret_map_uninitialized\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 253usize,
        name: Str::new("path_secret_map_uninitialized.capacity\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 254usize,
        name: Str::new("path_secret_map_uninitialized.entries\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 255usize,
        name: Str::new("path_secret_map_uninitialized.lifetime\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 256usize,
        name: Str::new("path_secret_map_background_handshake_requested\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 257usize,
        name: Str::new("path_secret_map_background_handshake_requested.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 258usize,
        name: Str::new("path_secret_map_entry_inserted\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 259usize,
        name: Str::new("path_secret_map_entry_inserted.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 260usize,
        name: Str::new("path_secret_map_entry_ready\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 261usize,
        name: Str::new("path_secret_map_entry_ready.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 262usize,
        name: Str::new("path_secret_map_entry_replaced\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 263usize,
        name: Str::new("path_secret_map_entry_replaced.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 264usize,
        name: Str::new("path_secret_map_id_entry_evicted\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 265usize,
        name: Str::new("path_secret_map_id_entry_evicted.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 266usize,
        name: Str::new("path_secret_map_id_entry_evicted.age\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 267usize,
        name: Str::new("path_secret_map_address_entry_evicted\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 268usize,
        name: Str::new("path_secret_map_address_entry_evicted.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 269usize,
        name: Str::new("path_secret_map_address_entry_evicted.age\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 270usize,
        name: Str::new("unknown_path_secret_packet_sent\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 271usize,
        name: Str::new("unknown_path_secret_packet_sent.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 272usize,
        name: Str::new("unknown_path_secret_packet_received\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 273usize,
        name: Str::new("unknown_path_secret_packet_received.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 274usize,
        name: Str::new("unknown_path_secret_packet_accepted\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 275usize,
        name: Str::new("unknown_path_secret_packet_accepted.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 276usize,
        name: Str::new("unknown_path_secret_packet_rejected\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 277usize,
        name: Str::new("unknown_path_secret_packet_rejected.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 278usize,
        name: Str::new("unknown_path_secret_packet_dropped\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 279usize,
        name: Str::new("unknown_path_secret_packet_dropped.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 280usize,
        name: Str::new("key_accepted\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 281usize,
        name: Str::new("key_accepted.gap\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 282usize,
        name: Str::new("key_accepted.forward_shift\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 283usize,
        name: Str::new("replay_definitely_detected\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 284usize,
        name: Str::new("replay_potentially_detected\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 285usize,
        name: Str::new("replay_potentially_detected.gap\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 286usize,
        name: Str::new("replay_detected_packet_sent\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 287usize,
        name: Str::new("replay_detected_packet_sent.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 288usize,
        name: Str::new("replay_detected_packet_received\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 289usize,
        name: Str::new("replay_detected_packet_received.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 290usize,
        name: Str::new("replay_detected_packet_accepted\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 291usize,
        name: Str::new("replay_detected_packet_accepted.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 292usize,
        name: Str::new("replay_detected_packet_rejected\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 293usize,
        name: Str::new("replay_detected_packet_rejected.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 294usize,
        name: Str::new("replay_detected_packet_dropped\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 295usize,
        name: Str::new("replay_detected_packet_dropped.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 296usize,
        name: Str::new("stale_key_packet_sent\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 297usize,
        name: Str::new("stale_key_packet_sent.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 298usize,
        name: Str::new("stale_key_packet_received\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 299usize,
        name: Str::new("stale_key_packet_received.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 300usize,
        name: Str::new("stale_key_packet_accepted\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 301usize,
        name: Str::new("stale_key_packet_accepted.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 302usize,
        name: Str::new("stale_key_packet_rejected\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 303usize,
        name: Str::new("stale_key_packet_rejected.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 304usize,
        name: Str::new("stale_key_packet_dropped\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 305usize,
        name: Str::new("stale_key_packet_dropped.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 306usize,
        name: Str::new("path_secret_map_address_cache_accessed\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 307usize,
        name: Str::new("path_secret_map_address_cache_accessed.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 308usize,
        name: Str::new("path_secret_map_address_cache_accessed.hit\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 309usize,
        name: Str::new("path_secret_map_address_cache_accessed_hit\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 310usize,
        name: Str::new("path_secret_map_address_cache_accessed_hit.peer_address.protocol\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 311usize,
        name: Str::new("path_secret_map_address_cache_accessed_hit.age\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 312usize,
        name: Str::new("path_secret_map_id_cache_accessed\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 313usize,
        name: Str::new("path_secret_map_id_cache_accessed.hit\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 314usize,
        name: Str::new("path_secret_map_id_cache_accessed_hit\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 315usize,
        name: Str::new("path_secret_map_id_cache_accessed_hit.age\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 316usize,
        name: Str::new("path_secret_map_cleaner_cycled\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 317usize,
        name: Str::new("path_secret_map_cleaner_cycled.entries.id\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 318usize,
        name: Str::new("path_secret_map_cleaner_cycled.entries.id.retired\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 319usize,
        name: Str::new("path_secret_map_cleaner_cycled.entries.id.active\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 320usize,
        name: Str::new("path_secret_map_cleaner_cycled.entries.id.active.utilization\0"),
        units: Units::Percent,
    }
    .build(),
    info::Builder {
        id: 321usize,
        name: Str::new("path_secret_map_cleaner_cycled.entries.id.utilization\0"),
        units: Units::Percent,
    }
    .build(),
    info::Builder {
        id: 322usize,
        name: Str::new("path_secret_map_cleaner_cycled.entries.id.utilization.initial\0"),
        units: Units::Percent,
    }
    .build(),
    info::Builder {
        id: 323usize,
        name: Str::new("path_secret_map_cleaner_cycled.entries.address\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 324usize,
        name: Str::new("path_secret_map_cleaner_cycled.entries.address.active\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 325usize,
        name: Str::new("path_secret_map_cleaner_cycled.entries.address.active.utilization\0"),
        units: Units::Percent,
    }
    .build(),
    info::Builder {
        id: 326usize,
        name: Str::new("path_secret_map_cleaner_cycled.entries.address.retired\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 327usize,
        name: Str::new("path_secret_map_cleaner_cycled.entries.address.utilization\0"),
        units: Units::Percent,
    }
    .build(),
    info::Builder {
        id: 328usize,
        name: Str::new("path_secret_map_cleaner_cycled.entries.address.utilization.initial\0"),
        units: Units::Percent,
    }
    .build(),
    info::Builder {
        id: 329usize,
        name: Str::new("path_secret_map_cleaner_cycled.handshake_requests\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 330usize,
        name: Str::new("path_secret_map_cleaner_cycled.handshake_requests.retired\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 331usize,
        name: Str::new("path_secret_map_cleaner_cycled.handshake_lock_duration\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 332usize,
        name: Str::new("path_secret_map_cleaner_cycled.total_duration\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 333usize,
        name: Str::new("path_secret_map_id_write_lock\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 334usize,
        name: Str::new("path_secret_map_id_write_lock.acquire\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 335usize,
        name: Str::new("path_secret_map_id_write_lock.duration\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 336usize,
        name: Str::new("path_secret_map_address_write_lock\0"),
        units: Units::None,
    }
    .build(),
    info::Builder {
        id: 337usize,
        name: Str::new("path_secret_map_address_write_lock.acquire\0"),
        units: Units::Duration,
    }
    .build(),
    info::Builder {
        id: 338usize,
        name: Str::new("path_secret_map_address_write_lock.duration\0"),
        units: Units::Duration,
    }
    .build(),
];
#[derive(Debug)]
#[allow(dead_code)]
pub struct ConnectionContext {
    start_time: crate::event::Timestamp,
    ctr_15: AtomicU64,
    ctr_18: AtomicU64,
    ctr_20: AtomicU64,
    ctr_21: AtomicU64,
    ctr_24: AtomicU64,
    ctr_26: AtomicU64,
    ctr_27: AtomicU64,
    ctr_30: AtomicU64,
    ctr_33: AtomicU64,
    ctr_34: AtomicU64,
    ctr_36: AtomicU64,
    ctr_38: AtomicU64,
    ctr_41: AtomicU64,
    ctr_42: AtomicU64,
    ctr_45: AtomicU64,
    ctr_48: AtomicU64,
    ctr_50: AtomicU64,
    ctr_51: AtomicU64,
    ctr_54: AtomicU64,
    ctr_57: AtomicU64,
    ctr_60: AtomicU64,
    ctr_61: AtomicU64,
    ctr_64: AtomicU64,
    ctr_65: AtomicU64,
    ctr_72: AtomicU64,
    ctr_76: AtomicU64,
    ctr_79: AtomicU64,
    ctr_83: AtomicU64,
    ctr_87: AtomicU64,
    ctr_91: AtomicU64,
}
pub struct Subscriber<R: Registry> {
    #[allow(dead_code)]
    counters: Box<[R::Counter; 110usize]>,
    #[allow(dead_code)]
    bool_counters: Box<[R::BoolCounter; 22usize]>,
    #[allow(dead_code)]
    nominal_counters: Box<[R::NominalCounter]>,
    #[allow(dead_code)]
    nominal_counter_offsets: Box<[usize; 38usize]>,
    #[allow(dead_code)]
    measures: Box<[R::Measure; 149usize]>,
    #[allow(dead_code)]
    gauges: Box<[R::Gauge; 0usize]>,
    #[allow(dead_code)]
    timers: Box<[R::Timer; 20usize]>,
    #[allow(dead_code)]
    nominal_timers: Box<[R::NominalTimer]>,
    #[allow(dead_code)]
    nominal_timer_offsets: Box<[usize; 0usize]>,
    #[allow(dead_code)]
    registry: R,
}
impl<R: Registry + Default> Default for Subscriber<R> {
    fn default() -> Self {
        Self::new(R::default())
    }
}
impl<R: Registry> Subscriber<R> {
    #[doc = r" Creates a new subscriber with the given registry"]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" All of the recorders are registered on initialization and cached for the lifetime"]
    #[doc = r" of the subscriber."]
    #[allow(unused_mut)]
    #[inline]
    pub fn new(registry: R) -> Self {
        let mut counters = Vec::with_capacity(110usize);
        let mut bool_counters = Vec::with_capacity(22usize);
        let mut nominal_counters = Vec::with_capacity(38usize);
        let mut nominal_counter_offsets = Vec::with_capacity(38usize);
        let mut measures = Vec::with_capacity(149usize);
        let mut gauges = Vec::with_capacity(0usize);
        let mut timers = Vec::with_capacity(20usize);
        let mut nominal_timers = Vec::with_capacity(0usize);
        let mut nominal_timer_offsets = Vec::with_capacity(0usize);
        counters.push(registry.register_counter(&INFO[0usize]));
        counters.push(registry.register_counter(&INFO[1usize]));
        counters.push(registry.register_counter(&INFO[7usize]));
        counters.push(registry.register_counter(&INFO[8usize]));
        counters.push(registry.register_counter(&INFO[12usize]));
        counters.push(registry.register_counter(&INFO[14usize]));
        counters.push(registry.register_counter(&INFO[17usize]));
        counters.push(registry.register_counter(&INFO[22usize]));
        counters.push(registry.register_counter(&INFO[25usize]));
        counters.push(registry.register_counter(&INFO[28usize]));
        counters.push(registry.register_counter(&INFO[29usize]));
        counters.push(registry.register_counter(&INFO[31usize]));
        counters.push(registry.register_counter(&INFO[34usize]));
        counters.push(registry.register_counter(&INFO[37usize]));
        counters.push(registry.register_counter(&INFO[38usize]));
        counters.push(registry.register_counter(&INFO[40usize]));
        counters.push(registry.register_counter(&INFO[46usize]));
        counters.push(registry.register_counter(&INFO[48usize]));
        counters.push(registry.register_counter(&INFO[49usize]));
        counters.push(registry.register_counter(&INFO[50usize]));
        counters.push(registry.register_counter(&INFO[53usize]));
        counters.push(registry.register_counter(&INFO[55usize]));
        counters.push(registry.register_counter(&INFO[59usize]));
        counters.push(registry.register_counter(&INFO[64usize]));
        counters.push(registry.register_counter(&INFO[68usize]));
        counters.push(registry.register_counter(&INFO[73usize]));
        counters.push(registry.register_counter(&INFO[79usize]));
        counters.push(registry.register_counter(&INFO[84usize]));
        counters.push(registry.register_counter(&INFO[85usize]));
        counters.push(registry.register_counter(&INFO[89usize]));
        counters.push(registry.register_counter(&INFO[93usize]));
        counters.push(registry.register_counter(&INFO[96usize]));
        counters.push(registry.register_counter(&INFO[99usize]));
        counters.push(registry.register_counter(&INFO[102usize]));
        counters.push(registry.register_counter(&INFO[104usize]));
        counters.push(registry.register_counter(&INFO[108usize]));
        counters.push(registry.register_counter(&INFO[113usize]));
        counters.push(registry.register_counter(&INFO[119usize]));
        counters.push(registry.register_counter(&INFO[124usize]));
        counters.push(registry.register_counter(&INFO[129usize]));
        counters.push(registry.register_counter(&INFO[130usize]));
        counters.push(registry.register_counter(&INFO[133usize]));
        counters.push(registry.register_counter(&INFO[136usize]));
        counters.push(registry.register_counter(&INFO[139usize]));
        counters.push(registry.register_counter(&INFO[142usize]));
        counters.push(registry.register_counter(&INFO[144usize]));
        counters.push(registry.register_counter(&INFO[148usize]));
        counters.push(registry.register_counter(&INFO[151usize]));
        counters.push(registry.register_counter(&INFO[155usize]));
        counters.push(registry.register_counter(&INFO[157usize]));
        counters.push(registry.register_counter(&INFO[159usize]));
        counters.push(registry.register_counter(&INFO[163usize]));
        counters.push(registry.register_counter(&INFO[165usize]));
        counters.push(registry.register_counter(&INFO[167usize]));
        counters.push(registry.register_counter(&INFO[171usize]));
        counters.push(registry.register_counter(&INFO[173usize]));
        counters.push(registry.register_counter(&INFO[178usize]));
        counters.push(registry.register_counter(&INFO[180usize]));
        counters.push(registry.register_counter(&INFO[185usize]));
        counters.push(registry.register_counter(&INFO[187usize]));
        counters.push(registry.register_counter(&INFO[192usize]));
        counters.push(registry.register_counter(&INFO[194usize]));
        counters.push(registry.register_counter(&INFO[198usize]));
        counters.push(registry.register_counter(&INFO[199usize]));
        counters.push(registry.register_counter(&INFO[201usize]));
        counters.push(registry.register_counter(&INFO[204usize]));
        counters.push(registry.register_counter(&INFO[208usize]));
        counters.push(registry.register_counter(&INFO[209usize]));
        counters.push(registry.register_counter(&INFO[210usize]));
        counters.push(registry.register_counter(&INFO[211usize]));
        counters.push(registry.register_counter(&INFO[216usize]));
        counters.push(registry.register_counter(&INFO[221usize]));
        counters.push(registry.register_counter(&INFO[227usize]));
        counters.push(registry.register_counter(&INFO[234usize]));
        counters.push(registry.register_counter(&INFO[239usize]));
        counters.push(registry.register_counter(&INFO[244usize]));
        counters.push(registry.register_counter(&INFO[249usize]));
        counters.push(registry.register_counter(&INFO[250usize]));
        counters.push(registry.register_counter(&INFO[252usize]));
        counters.push(registry.register_counter(&INFO[256usize]));
        counters.push(registry.register_counter(&INFO[258usize]));
        counters.push(registry.register_counter(&INFO[260usize]));
        counters.push(registry.register_counter(&INFO[262usize]));
        counters.push(registry.register_counter(&INFO[264usize]));
        counters.push(registry.register_counter(&INFO[267usize]));
        counters.push(registry.register_counter(&INFO[270usize]));
        counters.push(registry.register_counter(&INFO[272usize]));
        counters.push(registry.register_counter(&INFO[274usize]));
        counters.push(registry.register_counter(&INFO[276usize]));
        counters.push(registry.register_counter(&INFO[278usize]));
        counters.push(registry.register_counter(&INFO[280usize]));
        counters.push(registry.register_counter(&INFO[283usize]));
        counters.push(registry.register_counter(&INFO[284usize]));
        counters.push(registry.register_counter(&INFO[286usize]));
        counters.push(registry.register_counter(&INFO[288usize]));
        counters.push(registry.register_counter(&INFO[290usize]));
        counters.push(registry.register_counter(&INFO[292usize]));
        counters.push(registry.register_counter(&INFO[294usize]));
        counters.push(registry.register_counter(&INFO[296usize]));
        counters.push(registry.register_counter(&INFO[298usize]));
        counters.push(registry.register_counter(&INFO[300usize]));
        counters.push(registry.register_counter(&INFO[302usize]));
        counters.push(registry.register_counter(&INFO[304usize]));
        counters.push(registry.register_counter(&INFO[306usize]));
        counters.push(registry.register_counter(&INFO[309usize]));
        counters.push(registry.register_counter(&INFO[312usize]));
        counters.push(registry.register_counter(&INFO[314usize]));
        counters.push(registry.register_counter(&INFO[316usize]));
        counters.push(registry.register_counter(&INFO[333usize]));
        counters.push(registry.register_counter(&INFO[336usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[19usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[20usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[42usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[43usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[44usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[45usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[92usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[132usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[145usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[149usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[152usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[162usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[170usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[177usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[184usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[191usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[197usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[207usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[214usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[215usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[308usize]));
        bool_counters.push(registry.register_bool_counter(&INFO[313usize]));
        {
            #[allow(unused_imports)]
            use api::*;
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <AcceptorTcpStreamDropReason as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[13usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <AcceptorPacketDropReason as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[23usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <AcceptorPacketDropReason as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[47usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <AcceptorStreamPruneReason as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[52usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <MaybeBoolCounter as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[153usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <MaybeBoolCounter as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[154usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <StreamTcpConnectErrorReason as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[156usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[212usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[213usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[217usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[222usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[228usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[235usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[240usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[245usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[257usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[259usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[261usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[263usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[265usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[268usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[271usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[273usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[275usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[277usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[279usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[287usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[289usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[291usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[293usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[295usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[297usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[299usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[301usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[303usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[305usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[307usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
            {
                let offset = nominal_counters.len();
                let mut count = 0;
                for variant in <SocketAddress as AsVariant>::VARIANTS.iter() {
                    nominal_counters
                        .push(registry.register_nominal_counter(&INFO[310usize], variant));
                    count += 1;
                }
                debug_assert_ne!(count, 0, "field type needs at least one variant");
                nominal_counter_offsets.push(offset);
            }
        }
        measures.push(registry.register_measure(&INFO[2usize]));
        measures.push(registry.register_measure(&INFO[3usize]));
        measures.push(registry.register_measure(&INFO[4usize]));
        measures.push(registry.register_measure(&INFO[6usize]));
        measures.push(registry.register_measure(&INFO[9usize]));
        measures.push(registry.register_measure(&INFO[10usize]));
        measures.push(registry.register_measure(&INFO[11usize]));
        measures.push(registry.register_measure(&INFO[16usize]));
        measures.push(registry.register_measure(&INFO[18usize]));
        measures.push(registry.register_measure(&INFO[27usize]));
        measures.push(registry.register_measure(&INFO[32usize]));
        measures.push(registry.register_measure(&INFO[33usize]));
        measures.push(registry.register_measure(&INFO[36usize]));
        measures.push(registry.register_measure(&INFO[39usize]));
        measures.push(registry.register_measure(&INFO[41usize]));
        measures.push(registry.register_measure(&INFO[57usize]));
        measures.push(registry.register_measure(&INFO[58usize]));
        measures.push(registry.register_measure(&INFO[60usize]));
        measures.push(registry.register_measure(&INFO[61usize]));
        measures.push(registry.register_measure(&INFO[62usize]));
        measures.push(registry.register_measure(&INFO[63usize]));
        measures.push(registry.register_measure(&INFO[66usize]));
        measures.push(registry.register_measure(&INFO[67usize]));
        measures.push(registry.register_measure(&INFO[69usize]));
        measures.push(registry.register_measure(&INFO[70usize]));
        measures.push(registry.register_measure(&INFO[71usize]));
        measures.push(registry.register_measure(&INFO[72usize]));
        measures.push(registry.register_measure(&INFO[75usize]));
        measures.push(registry.register_measure(&INFO[76usize]));
        measures.push(registry.register_measure(&INFO[77usize]));
        measures.push(registry.register_measure(&INFO[78usize]));
        measures.push(registry.register_measure(&INFO[81usize]));
        measures.push(registry.register_measure(&INFO[82usize]));
        measures.push(registry.register_measure(&INFO[83usize]));
        measures.push(registry.register_measure(&INFO[86usize]));
        measures.push(registry.register_measure(&INFO[87usize]));
        measures.push(registry.register_measure(&INFO[88usize]));
        measures.push(registry.register_measure(&INFO[91usize]));
        measures.push(registry.register_measure(&INFO[94usize]));
        measures.push(registry.register_measure(&INFO[95usize]));
        measures.push(registry.register_measure(&INFO[97usize]));
        measures.push(registry.register_measure(&INFO[98usize]));
        measures.push(registry.register_measure(&INFO[100usize]));
        measures.push(registry.register_measure(&INFO[101usize]));
        measures.push(registry.register_measure(&INFO[103usize]));
        measures.push(registry.register_measure(&INFO[106usize]));
        measures.push(registry.register_measure(&INFO[107usize]));
        measures.push(registry.register_measure(&INFO[109usize]));
        measures.push(registry.register_measure(&INFO[110usize]));
        measures.push(registry.register_measure(&INFO[111usize]));
        measures.push(registry.register_measure(&INFO[112usize]));
        measures.push(registry.register_measure(&INFO[115usize]));
        measures.push(registry.register_measure(&INFO[116usize]));
        measures.push(registry.register_measure(&INFO[117usize]));
        measures.push(registry.register_measure(&INFO[118usize]));
        measures.push(registry.register_measure(&INFO[121usize]));
        measures.push(registry.register_measure(&INFO[122usize]));
        measures.push(registry.register_measure(&INFO[123usize]));
        measures.push(registry.register_measure(&INFO[126usize]));
        measures.push(registry.register_measure(&INFO[127usize]));
        measures.push(registry.register_measure(&INFO[128usize]));
        measures.push(registry.register_measure(&INFO[134usize]));
        measures.push(registry.register_measure(&INFO[135usize]));
        measures.push(registry.register_measure(&INFO[137usize]));
        measures.push(registry.register_measure(&INFO[138usize]));
        measures.push(registry.register_measure(&INFO[140usize]));
        measures.push(registry.register_measure(&INFO[141usize]));
        measures.push(registry.register_measure(&INFO[143usize]));
        measures.push(registry.register_measure(&INFO[146usize]));
        measures.push(registry.register_measure(&INFO[147usize]));
        measures.push(registry.register_measure(&INFO[158usize]));
        measures.push(registry.register_measure(&INFO[160usize]));
        measures.push(registry.register_measure(&INFO[161usize]));
        measures.push(registry.register_measure(&INFO[164usize]));
        measures.push(registry.register_measure(&INFO[166usize]));
        measures.push(registry.register_measure(&INFO[168usize]));
        measures.push(registry.register_measure(&INFO[169usize]));
        measures.push(registry.register_measure(&INFO[172usize]));
        measures.push(registry.register_measure(&INFO[174usize]));
        measures.push(registry.register_measure(&INFO[175usize]));
        measures.push(registry.register_measure(&INFO[176usize]));
        measures.push(registry.register_measure(&INFO[179usize]));
        measures.push(registry.register_measure(&INFO[181usize]));
        measures.push(registry.register_measure(&INFO[182usize]));
        measures.push(registry.register_measure(&INFO[183usize]));
        measures.push(registry.register_measure(&INFO[186usize]));
        measures.push(registry.register_measure(&INFO[188usize]));
        measures.push(registry.register_measure(&INFO[189usize]));
        measures.push(registry.register_measure(&INFO[190usize]));
        measures.push(registry.register_measure(&INFO[193usize]));
        measures.push(registry.register_measure(&INFO[195usize]));
        measures.push(registry.register_measure(&INFO[196usize]));
        measures.push(registry.register_measure(&INFO[200usize]));
        measures.push(registry.register_measure(&INFO[202usize]));
        measures.push(registry.register_measure(&INFO[203usize]));
        measures.push(registry.register_measure(&INFO[205usize]));
        measures.push(registry.register_measure(&INFO[206usize]));
        measures.push(registry.register_measure(&INFO[218usize]));
        measures.push(registry.register_measure(&INFO[219usize]));
        measures.push(registry.register_measure(&INFO[220usize]));
        measures.push(registry.register_measure(&INFO[223usize]));
        measures.push(registry.register_measure(&INFO[224usize]));
        measures.push(registry.register_measure(&INFO[225usize]));
        measures.push(registry.register_measure(&INFO[226usize]));
        measures.push(registry.register_measure(&INFO[229usize]));
        measures.push(registry.register_measure(&INFO[230usize]));
        measures.push(registry.register_measure(&INFO[231usize]));
        measures.push(registry.register_measure(&INFO[232usize]));
        measures.push(registry.register_measure(&INFO[233usize]));
        measures.push(registry.register_measure(&INFO[236usize]));
        measures.push(registry.register_measure(&INFO[237usize]));
        measures.push(registry.register_measure(&INFO[238usize]));
        measures.push(registry.register_measure(&INFO[241usize]));
        measures.push(registry.register_measure(&INFO[242usize]));
        measures.push(registry.register_measure(&INFO[243usize]));
        measures.push(registry.register_measure(&INFO[246usize]));
        measures.push(registry.register_measure(&INFO[247usize]));
        measures.push(registry.register_measure(&INFO[248usize]));
        measures.push(registry.register_measure(&INFO[251usize]));
        measures.push(registry.register_measure(&INFO[253usize]));
        measures.push(registry.register_measure(&INFO[254usize]));
        measures.push(registry.register_measure(&INFO[255usize]));
        measures.push(registry.register_measure(&INFO[266usize]));
        measures.push(registry.register_measure(&INFO[269usize]));
        measures.push(registry.register_measure(&INFO[281usize]));
        measures.push(registry.register_measure(&INFO[282usize]));
        measures.push(registry.register_measure(&INFO[285usize]));
        measures.push(registry.register_measure(&INFO[311usize]));
        measures.push(registry.register_measure(&INFO[315usize]));
        measures.push(registry.register_measure(&INFO[317usize]));
        measures.push(registry.register_measure(&INFO[318usize]));
        measures.push(registry.register_measure(&INFO[319usize]));
        measures.push(registry.register_measure(&INFO[320usize]));
        measures.push(registry.register_measure(&INFO[321usize]));
        measures.push(registry.register_measure(&INFO[322usize]));
        measures.push(registry.register_measure(&INFO[323usize]));
        measures.push(registry.register_measure(&INFO[324usize]));
        measures.push(registry.register_measure(&INFO[325usize]));
        measures.push(registry.register_measure(&INFO[326usize]));
        measures.push(registry.register_measure(&INFO[327usize]));
        measures.push(registry.register_measure(&INFO[328usize]));
        measures.push(registry.register_measure(&INFO[329usize]));
        measures.push(registry.register_measure(&INFO[330usize]));
        measures.push(registry.register_measure(&INFO[331usize]));
        measures.push(registry.register_measure(&INFO[332usize]));
        measures.push(registry.register_measure(&INFO[334usize]));
        measures.push(registry.register_measure(&INFO[335usize]));
        measures.push(registry.register_measure(&INFO[337usize]));
        measures.push(registry.register_measure(&INFO[338usize]));
        timers.push(registry.register_timer(&INFO[5usize]));
        timers.push(registry.register_timer(&INFO[15usize]));
        timers.push(registry.register_timer(&INFO[21usize]));
        timers.push(registry.register_timer(&INFO[24usize]));
        timers.push(registry.register_timer(&INFO[26usize]));
        timers.push(registry.register_timer(&INFO[30usize]));
        timers.push(registry.register_timer(&INFO[35usize]));
        timers.push(registry.register_timer(&INFO[51usize]));
        timers.push(registry.register_timer(&INFO[54usize]));
        timers.push(registry.register_timer(&INFO[56usize]));
        timers.push(registry.register_timer(&INFO[65usize]));
        timers.push(registry.register_timer(&INFO[74usize]));
        timers.push(registry.register_timer(&INFO[80usize]));
        timers.push(registry.register_timer(&INFO[90usize]));
        timers.push(registry.register_timer(&INFO[105usize]));
        timers.push(registry.register_timer(&INFO[114usize]));
        timers.push(registry.register_timer(&INFO[120usize]));
        timers.push(registry.register_timer(&INFO[125usize]));
        timers.push(registry.register_timer(&INFO[131usize]));
        timers.push(registry.register_timer(&INFO[150usize]));
        {
            #[allow(unused_imports)]
            use api::*;
        }
        Self {
            counters: counters
                .try_into()
                .unwrap_or_else(|_| panic!("invalid len")),
            bool_counters: bool_counters
                .try_into()
                .unwrap_or_else(|_| panic!("invalid len")),
            nominal_counters: nominal_counters.into(),
            nominal_counter_offsets: nominal_counter_offsets
                .try_into()
                .unwrap_or_else(|_| panic!("invalid len")),
            measures: measures
                .try_into()
                .unwrap_or_else(|_| panic!("invalid len")),
            gauges: gauges.try_into().unwrap_or_else(|_| panic!("invalid len")),
            timers: timers.try_into().unwrap_or_else(|_| panic!("invalid len")),
            nominal_timers: nominal_timers.into(),
            nominal_timer_offsets: nominal_timer_offsets
                .try_into()
                .unwrap_or_else(|_| panic!("invalid len")),
            registry,
        }
    }
    #[doc = r" Returns all of the registered counters"]
    #[inline]
    pub fn counters(&self) -> impl Iterator<Item = (&'static Info, &R::Counter)> + '_ {
        self.counters
            .iter()
            .enumerate()
            .map(|(idx, entry)| match idx {
                0usize => (&INFO[0usize], entry),
                1usize => (&INFO[1usize], entry),
                2usize => (&INFO[7usize], entry),
                3usize => (&INFO[8usize], entry),
                4usize => (&INFO[12usize], entry),
                5usize => (&INFO[14usize], entry),
                6usize => (&INFO[17usize], entry),
                7usize => (&INFO[22usize], entry),
                8usize => (&INFO[25usize], entry),
                9usize => (&INFO[28usize], entry),
                10usize => (&INFO[29usize], entry),
                11usize => (&INFO[31usize], entry),
                12usize => (&INFO[34usize], entry),
                13usize => (&INFO[37usize], entry),
                14usize => (&INFO[38usize], entry),
                15usize => (&INFO[40usize], entry),
                16usize => (&INFO[46usize], entry),
                17usize => (&INFO[48usize], entry),
                18usize => (&INFO[49usize], entry),
                19usize => (&INFO[50usize], entry),
                20usize => (&INFO[53usize], entry),
                21usize => (&INFO[55usize], entry),
                22usize => (&INFO[59usize], entry),
                23usize => (&INFO[64usize], entry),
                24usize => (&INFO[68usize], entry),
                25usize => (&INFO[73usize], entry),
                26usize => (&INFO[79usize], entry),
                27usize => (&INFO[84usize], entry),
                28usize => (&INFO[85usize], entry),
                29usize => (&INFO[89usize], entry),
                30usize => (&INFO[93usize], entry),
                31usize => (&INFO[96usize], entry),
                32usize => (&INFO[99usize], entry),
                33usize => (&INFO[102usize], entry),
                34usize => (&INFO[104usize], entry),
                35usize => (&INFO[108usize], entry),
                36usize => (&INFO[113usize], entry),
                37usize => (&INFO[119usize], entry),
                38usize => (&INFO[124usize], entry),
                39usize => (&INFO[129usize], entry),
                40usize => (&INFO[130usize], entry),
                41usize => (&INFO[133usize], entry),
                42usize => (&INFO[136usize], entry),
                43usize => (&INFO[139usize], entry),
                44usize => (&INFO[142usize], entry),
                45usize => (&INFO[144usize], entry),
                46usize => (&INFO[148usize], entry),
                47usize => (&INFO[151usize], entry),
                48usize => (&INFO[155usize], entry),
                49usize => (&INFO[157usize], entry),
                50usize => (&INFO[159usize], entry),
                51usize => (&INFO[163usize], entry),
                52usize => (&INFO[165usize], entry),
                53usize => (&INFO[167usize], entry),
                54usize => (&INFO[171usize], entry),
                55usize => (&INFO[173usize], entry),
                56usize => (&INFO[178usize], entry),
                57usize => (&INFO[180usize], entry),
                58usize => (&INFO[185usize], entry),
                59usize => (&INFO[187usize], entry),
                60usize => (&INFO[192usize], entry),
                61usize => (&INFO[194usize], entry),
                62usize => (&INFO[198usize], entry),
                63usize => (&INFO[199usize], entry),
                64usize => (&INFO[201usize], entry),
                65usize => (&INFO[204usize], entry),
                66usize => (&INFO[208usize], entry),
                67usize => (&INFO[209usize], entry),
                68usize => (&INFO[210usize], entry),
                69usize => (&INFO[211usize], entry),
                70usize => (&INFO[216usize], entry),
                71usize => (&INFO[221usize], entry),
                72usize => (&INFO[227usize], entry),
                73usize => (&INFO[234usize], entry),
                74usize => (&INFO[239usize], entry),
                75usize => (&INFO[244usize], entry),
                76usize => (&INFO[249usize], entry),
                77usize => (&INFO[250usize], entry),
                78usize => (&INFO[252usize], entry),
                79usize => (&INFO[256usize], entry),
                80usize => (&INFO[258usize], entry),
                81usize => (&INFO[260usize], entry),
                82usize => (&INFO[262usize], entry),
                83usize => (&INFO[264usize], entry),
                84usize => (&INFO[267usize], entry),
                85usize => (&INFO[270usize], entry),
                86usize => (&INFO[272usize], entry),
                87usize => (&INFO[274usize], entry),
                88usize => (&INFO[276usize], entry),
                89usize => (&INFO[278usize], entry),
                90usize => (&INFO[280usize], entry),
                91usize => (&INFO[283usize], entry),
                92usize => (&INFO[284usize], entry),
                93usize => (&INFO[286usize], entry),
                94usize => (&INFO[288usize], entry),
                95usize => (&INFO[290usize], entry),
                96usize => (&INFO[292usize], entry),
                97usize => (&INFO[294usize], entry),
                98usize => (&INFO[296usize], entry),
                99usize => (&INFO[298usize], entry),
                100usize => (&INFO[300usize], entry),
                101usize => (&INFO[302usize], entry),
                102usize => (&INFO[304usize], entry),
                103usize => (&INFO[306usize], entry),
                104usize => (&INFO[309usize], entry),
                105usize => (&INFO[312usize], entry),
                106usize => (&INFO[314usize], entry),
                107usize => (&INFO[316usize], entry),
                108usize => (&INFO[333usize], entry),
                109usize => (&INFO[336usize], entry),
                _ => unsafe { core::hint::unreachable_unchecked() },
            })
    }
    #[allow(dead_code)]
    #[inline(always)]
    fn count<T: Metric>(&self, info: usize, id: usize, value: T) {
        let info = &INFO[info];
        let counter = &self.counters[id];
        counter.record(info, value);
    }
    #[doc = r" Returns all of the registered bool counters"]
    #[inline]
    pub fn bool_counters(&self) -> impl Iterator<Item = (&'static Info, &R::BoolCounter)> + '_ {
        self.bool_counters
            .iter()
            .enumerate()
            .map(|(idx, entry)| match idx {
                0usize => (&INFO[19usize], entry),
                1usize => (&INFO[20usize], entry),
                2usize => (&INFO[42usize], entry),
                3usize => (&INFO[43usize], entry),
                4usize => (&INFO[44usize], entry),
                5usize => (&INFO[45usize], entry),
                6usize => (&INFO[92usize], entry),
                7usize => (&INFO[132usize], entry),
                8usize => (&INFO[145usize], entry),
                9usize => (&INFO[149usize], entry),
                10usize => (&INFO[152usize], entry),
                11usize => (&INFO[162usize], entry),
                12usize => (&INFO[170usize], entry),
                13usize => (&INFO[177usize], entry),
                14usize => (&INFO[184usize], entry),
                15usize => (&INFO[191usize], entry),
                16usize => (&INFO[197usize], entry),
                17usize => (&INFO[207usize], entry),
                18usize => (&INFO[214usize], entry),
                19usize => (&INFO[215usize], entry),
                20usize => (&INFO[308usize], entry),
                21usize => (&INFO[313usize], entry),
                _ => unsafe { core::hint::unreachable_unchecked() },
            })
    }
    #[allow(dead_code)]
    #[inline(always)]
    fn count_bool(&self, info: usize, id: usize, value: bool) {
        let info = &INFO[info];
        let counter = &self.bool_counters[id];
        counter.record(info, value);
    }
    #[doc = r" Returns all of the registered nominal counters"]
    #[inline]
    pub fn nominal_counters(
        &self,
    ) -> impl Iterator<Item = (&'static Info, &[R::NominalCounter], &[info::Variant])> + '_ {
        #[allow(unused_imports)]
        use api::*;
        self.nominal_counter_offsets
            .iter()
            .enumerate()
            .map(|(idx, entry)| match idx {
                0usize => {
                    let offset = *entry;
                    let variants = <AcceptorTcpStreamDropReason as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[13usize], entries, variants)
                }
                1usize => {
                    let offset = *entry;
                    let variants = <AcceptorPacketDropReason as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[23usize], entries, variants)
                }
                2usize => {
                    let offset = *entry;
                    let variants = <AcceptorPacketDropReason as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[47usize], entries, variants)
                }
                3usize => {
                    let offset = *entry;
                    let variants = <AcceptorStreamPruneReason as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[52usize], entries, variants)
                }
                4usize => {
                    let offset = *entry;
                    let variants = <MaybeBoolCounter as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[153usize], entries, variants)
                }
                5usize => {
                    let offset = *entry;
                    let variants = <MaybeBoolCounter as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[154usize], entries, variants)
                }
                6usize => {
                    let offset = *entry;
                    let variants = <StreamTcpConnectErrorReason as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[156usize], entries, variants)
                }
                7usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[212usize], entries, variants)
                }
                8usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[213usize], entries, variants)
                }
                9usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[217usize], entries, variants)
                }
                10usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[222usize], entries, variants)
                }
                11usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[228usize], entries, variants)
                }
                12usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[235usize], entries, variants)
                }
                13usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[240usize], entries, variants)
                }
                14usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[245usize], entries, variants)
                }
                15usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[257usize], entries, variants)
                }
                16usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[259usize], entries, variants)
                }
                17usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[261usize], entries, variants)
                }
                18usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[263usize], entries, variants)
                }
                19usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[265usize], entries, variants)
                }
                20usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[268usize], entries, variants)
                }
                21usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[271usize], entries, variants)
                }
                22usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[273usize], entries, variants)
                }
                23usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[275usize], entries, variants)
                }
                24usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[277usize], entries, variants)
                }
                25usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[279usize], entries, variants)
                }
                26usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[287usize], entries, variants)
                }
                27usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[289usize], entries, variants)
                }
                28usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[291usize], entries, variants)
                }
                29usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[293usize], entries, variants)
                }
                30usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[295usize], entries, variants)
                }
                31usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[297usize], entries, variants)
                }
                32usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[299usize], entries, variants)
                }
                33usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[301usize], entries, variants)
                }
                34usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[303usize], entries, variants)
                }
                35usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[305usize], entries, variants)
                }
                36usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[307usize], entries, variants)
                }
                37usize => {
                    let offset = *entry;
                    let variants = <SocketAddress as AsVariant>::VARIANTS;
                    let entries = &self.nominal_counters[offset..offset + variants.len()];
                    (&INFO[310usize], entries, variants)
                }
                _ => unsafe { core::hint::unreachable_unchecked() },
            })
    }
    #[allow(dead_code)]
    #[inline(always)]
    fn count_nominal<T: AsVariant>(&self, info: usize, id: usize, value: &T) {
        let info = &INFO[info];
        let idx = self.nominal_counter_offsets[id] + value.variant_idx();
        let counter = &self.nominal_counters[idx];
        counter.record(info, value.as_variant(), 1usize);
    }
    #[doc = r" Returns all of the registered measures"]
    #[inline]
    pub fn measures(&self) -> impl Iterator<Item = (&'static Info, &R::Measure)> + '_ {
        self.measures
            .iter()
            .enumerate()
            .map(|(idx, entry)| match idx {
                0usize => (&INFO[2usize], entry),
                1usize => (&INFO[3usize], entry),
                2usize => (&INFO[4usize], entry),
                3usize => (&INFO[6usize], entry),
                4usize => (&INFO[9usize], entry),
                5usize => (&INFO[10usize], entry),
                6usize => (&INFO[11usize], entry),
                7usize => (&INFO[16usize], entry),
                8usize => (&INFO[18usize], entry),
                9usize => (&INFO[27usize], entry),
                10usize => (&INFO[32usize], entry),
                11usize => (&INFO[33usize], entry),
                12usize => (&INFO[36usize], entry),
                13usize => (&INFO[39usize], entry),
                14usize => (&INFO[41usize], entry),
                15usize => (&INFO[57usize], entry),
                16usize => (&INFO[58usize], entry),
                17usize => (&INFO[60usize], entry),
                18usize => (&INFO[61usize], entry),
                19usize => (&INFO[62usize], entry),
                20usize => (&INFO[63usize], entry),
                21usize => (&INFO[66usize], entry),
                22usize => (&INFO[67usize], entry),
                23usize => (&INFO[69usize], entry),
                24usize => (&INFO[70usize], entry),
                25usize => (&INFO[71usize], entry),
                26usize => (&INFO[72usize], entry),
                27usize => (&INFO[75usize], entry),
                28usize => (&INFO[76usize], entry),
                29usize => (&INFO[77usize], entry),
                30usize => (&INFO[78usize], entry),
                31usize => (&INFO[81usize], entry),
                32usize => (&INFO[82usize], entry),
                33usize => (&INFO[83usize], entry),
                34usize => (&INFO[86usize], entry),
                35usize => (&INFO[87usize], entry),
                36usize => (&INFO[88usize], entry),
                37usize => (&INFO[91usize], entry),
                38usize => (&INFO[94usize], entry),
                39usize => (&INFO[95usize], entry),
                40usize => (&INFO[97usize], entry),
                41usize => (&INFO[98usize], entry),
                42usize => (&INFO[100usize], entry),
                43usize => (&INFO[101usize], entry),
                44usize => (&INFO[103usize], entry),
                45usize => (&INFO[106usize], entry),
                46usize => (&INFO[107usize], entry),
                47usize => (&INFO[109usize], entry),
                48usize => (&INFO[110usize], entry),
                49usize => (&INFO[111usize], entry),
                50usize => (&INFO[112usize], entry),
                51usize => (&INFO[115usize], entry),
                52usize => (&INFO[116usize], entry),
                53usize => (&INFO[117usize], entry),
                54usize => (&INFO[118usize], entry),
                55usize => (&INFO[121usize], entry),
                56usize => (&INFO[122usize], entry),
                57usize => (&INFO[123usize], entry),
                58usize => (&INFO[126usize], entry),
                59usize => (&INFO[127usize], entry),
                60usize => (&INFO[128usize], entry),
                61usize => (&INFO[134usize], entry),
                62usize => (&INFO[135usize], entry),
                63usize => (&INFO[137usize], entry),
                64usize => (&INFO[138usize], entry),
                65usize => (&INFO[140usize], entry),
                66usize => (&INFO[141usize], entry),
                67usize => (&INFO[143usize], entry),
                68usize => (&INFO[146usize], entry),
                69usize => (&INFO[147usize], entry),
                70usize => (&INFO[158usize], entry),
                71usize => (&INFO[160usize], entry),
                72usize => (&INFO[161usize], entry),
                73usize => (&INFO[164usize], entry),
                74usize => (&INFO[166usize], entry),
                75usize => (&INFO[168usize], entry),
                76usize => (&INFO[169usize], entry),
                77usize => (&INFO[172usize], entry),
                78usize => (&INFO[174usize], entry),
                79usize => (&INFO[175usize], entry),
                80usize => (&INFO[176usize], entry),
                81usize => (&INFO[179usize], entry),
                82usize => (&INFO[181usize], entry),
                83usize => (&INFO[182usize], entry),
                84usize => (&INFO[183usize], entry),
                85usize => (&INFO[186usize], entry),
                86usize => (&INFO[188usize], entry),
                87usize => (&INFO[189usize], entry),
                88usize => (&INFO[190usize], entry),
                89usize => (&INFO[193usize], entry),
                90usize => (&INFO[195usize], entry),
                91usize => (&INFO[196usize], entry),
                92usize => (&INFO[200usize], entry),
                93usize => (&INFO[202usize], entry),
                94usize => (&INFO[203usize], entry),
                95usize => (&INFO[205usize], entry),
                96usize => (&INFO[206usize], entry),
                97usize => (&INFO[218usize], entry),
                98usize => (&INFO[219usize], entry),
                99usize => (&INFO[220usize], entry),
                100usize => (&INFO[223usize], entry),
                101usize => (&INFO[224usize], entry),
                102usize => (&INFO[225usize], entry),
                103usize => (&INFO[226usize], entry),
                104usize => (&INFO[229usize], entry),
                105usize => (&INFO[230usize], entry),
                106usize => (&INFO[231usize], entry),
                107usize => (&INFO[232usize], entry),
                108usize => (&INFO[233usize], entry),
                109usize => (&INFO[236usize], entry),
                110usize => (&INFO[237usize], entry),
                111usize => (&INFO[238usize], entry),
                112usize => (&INFO[241usize], entry),
                113usize => (&INFO[242usize], entry),
                114usize => (&INFO[243usize], entry),
                115usize => (&INFO[246usize], entry),
                116usize => (&INFO[247usize], entry),
                117usize => (&INFO[248usize], entry),
                118usize => (&INFO[251usize], entry),
                119usize => (&INFO[253usize], entry),
                120usize => (&INFO[254usize], entry),
                121usize => (&INFO[255usize], entry),
                122usize => (&INFO[266usize], entry),
                123usize => (&INFO[269usize], entry),
                124usize => (&INFO[281usize], entry),
                125usize => (&INFO[282usize], entry),
                126usize => (&INFO[285usize], entry),
                127usize => (&INFO[311usize], entry),
                128usize => (&INFO[315usize], entry),
                129usize => (&INFO[317usize], entry),
                130usize => (&INFO[318usize], entry),
                131usize => (&INFO[319usize], entry),
                132usize => (&INFO[320usize], entry),
                133usize => (&INFO[321usize], entry),
                134usize => (&INFO[322usize], entry),
                135usize => (&INFO[323usize], entry),
                136usize => (&INFO[324usize], entry),
                137usize => (&INFO[325usize], entry),
                138usize => (&INFO[326usize], entry),
                139usize => (&INFO[327usize], entry),
                140usize => (&INFO[328usize], entry),
                141usize => (&INFO[329usize], entry),
                142usize => (&INFO[330usize], entry),
                143usize => (&INFO[331usize], entry),
                144usize => (&INFO[332usize], entry),
                145usize => (&INFO[334usize], entry),
                146usize => (&INFO[335usize], entry),
                147usize => (&INFO[337usize], entry),
                148usize => (&INFO[338usize], entry),
                _ => unsafe { core::hint::unreachable_unchecked() },
            })
    }
    #[allow(dead_code)]
    #[inline(always)]
    fn measure<T: Metric>(&self, info: usize, id: usize, value: T) {
        let info = &INFO[info];
        let measure = &self.measures[id];
        measure.record(info, value);
    }
    #[doc = r" Returns all of the registered gauges"]
    #[inline]
    pub fn gauges(&self) -> impl Iterator<Item = (&'static Info, &R::Gauge)> + '_ {
        core::iter::empty()
    }
    #[allow(dead_code)]
    #[inline(always)]
    fn gauge<T: Metric>(&self, info: usize, id: usize, value: T) {
        let info = &INFO[info];
        let gauge = &self.gauges[id];
        gauge.record(info, value);
    }
    #[doc = r" Returns all of the registered timers"]
    #[inline]
    pub fn timers(&self) -> impl Iterator<Item = (&'static Info, &R::Timer)> + '_ {
        self.timers
            .iter()
            .enumerate()
            .map(|(idx, entry)| match idx {
                0usize => (&INFO[5usize], entry),
                1usize => (&INFO[15usize], entry),
                2usize => (&INFO[21usize], entry),
                3usize => (&INFO[24usize], entry),
                4usize => (&INFO[26usize], entry),
                5usize => (&INFO[30usize], entry),
                6usize => (&INFO[35usize], entry),
                7usize => (&INFO[51usize], entry),
                8usize => (&INFO[54usize], entry),
                9usize => (&INFO[56usize], entry),
                10usize => (&INFO[65usize], entry),
                11usize => (&INFO[74usize], entry),
                12usize => (&INFO[80usize], entry),
                13usize => (&INFO[90usize], entry),
                14usize => (&INFO[105usize], entry),
                15usize => (&INFO[114usize], entry),
                16usize => (&INFO[120usize], entry),
                17usize => (&INFO[125usize], entry),
                18usize => (&INFO[131usize], entry),
                19usize => (&INFO[150usize], entry),
                _ => unsafe { core::hint::unreachable_unchecked() },
            })
    }
    #[allow(dead_code)]
    #[inline(always)]
    fn time(&self, info: usize, id: usize, value: core::time::Duration) {
        let info = &INFO[info];
        let timer = &self.timers[id];
        timer.record(info, value);
    }
    #[allow(dead_code)]
    #[inline(always)]
    fn time_nominal<T: AsVariant>(
        &self,
        info: usize,
        id: usize,
        value: &T,
        duration: core::time::Duration,
    ) {
        let info = &INFO[info];
        let idx = self.nominal_timer_offsets[id] + value.variant_idx();
        let counter = &self.nominal_timers[idx];
        counter.record(info, value.as_variant(), duration);
    }
}
impl<R: Registry> event::Subscriber for Subscriber<R> {
    type ConnectionContext = ConnectionContext;
    fn create_connection_context(
        &self,
        meta: &api::ConnectionMeta,
        _info: &api::ConnectionInfo,
    ) -> Self::ConnectionContext {
        Self::ConnectionContext {
            start_time: meta.timestamp,
            ctr_15: AtomicU64::new(0),
            ctr_18: AtomicU64::new(0),
            ctr_20: AtomicU64::new(0),
            ctr_21: AtomicU64::new(0),
            ctr_24: AtomicU64::new(0),
            ctr_26: AtomicU64::new(0),
            ctr_27: AtomicU64::new(0),
            ctr_30: AtomicU64::new(0),
            ctr_33: AtomicU64::new(0),
            ctr_34: AtomicU64::new(0),
            ctr_36: AtomicU64::new(0),
            ctr_38: AtomicU64::new(0),
            ctr_41: AtomicU64::new(0),
            ctr_42: AtomicU64::new(0),
            ctr_45: AtomicU64::new(0),
            ctr_48: AtomicU64::new(0),
            ctr_50: AtomicU64::new(0),
            ctr_51: AtomicU64::new(0),
            ctr_54: AtomicU64::new(0),
            ctr_57: AtomicU64::new(0),
            ctr_60: AtomicU64::new(0),
            ctr_61: AtomicU64::new(0),
            ctr_64: AtomicU64::new(0),
            ctr_65: AtomicU64::new(0),
            ctr_72: AtomicU64::new(0),
            ctr_76: AtomicU64::new(0),
            ctr_79: AtomicU64::new(0),
            ctr_83: AtomicU64::new(0),
            ctr_87: AtomicU64::new(0),
            ctr_91: AtomicU64::new(0),
        }
    }
    #[inline]
    fn on_acceptor_tcp_started(&self, meta: &api::EndpointMeta, event: &api::AcceptorTcpStarted) {
        #[allow(unused_imports)]
        use api::*;
        self.count(0usize, 0usize, 1usize);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_tcp_loop_iteration_completed(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorTcpLoopIterationCompleted,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(1usize, 1usize, 1usize);
        self.measure(2usize, 0usize, event.pending_streams);
        self.measure(3usize, 1usize, event.slots_idle);
        self.measure(4usize, 2usize, event.slot_utilization);
        self.time(5usize, 0usize, event.processing_duration);
        self.measure(6usize, 3usize, event.max_sojourn_time);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_tcp_fresh_enqueued(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorTcpFreshEnqueued,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(7usize, 2usize, 1usize);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_tcp_fresh_batch_completed(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorTcpFreshBatchCompleted,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(8usize, 3usize, 1usize);
        self.measure(9usize, 4usize, event.enqueued);
        self.measure(10usize, 5usize, event.dropped);
        self.measure(11usize, 6usize, event.errored);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_tcp_stream_dropped(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorTcpStreamDropped,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(12usize, 4usize, 1usize);
        self.count_nominal(13usize, 0usize, &event.reason);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_tcp_stream_replaced(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorTcpStreamReplaced,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(14usize, 5usize, 1usize);
        self.time(15usize, 1usize, event.sojourn_time);
        self.measure(16usize, 7usize, event.buffer_len);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_tcp_packet_received(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorTcpPacketReceived,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(17usize, 6usize, 1usize);
        self.measure(18usize, 8usize, event.payload_len);
        self.count_bool(19usize, 0usize, event.is_fin);
        self.count_bool(20usize, 1usize, event.is_fin_known);
        self.time(21usize, 2usize, event.sojourn_time);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_tcp_packet_dropped(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorTcpPacketDropped,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(22usize, 7usize, 1usize);
        self.count_nominal(23usize, 1usize, &event.reason);
        self.time(24usize, 3usize, event.sojourn_time);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_tcp_stream_enqueued(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorTcpStreamEnqueued,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(25usize, 8usize, 1usize);
        self.time(26usize, 4usize, event.sojourn_time);
        self.measure(27usize, 9usize, event.blocked_count);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_tcp_io_error(&self, meta: &api::EndpointMeta, event: &api::AcceptorTcpIoError) {
        #[allow(unused_imports)]
        use api::*;
        self.count(28usize, 9usize, 1usize);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_tcp_socket_sent(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorTcpSocketSent,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(29usize, 10usize, 1usize);
        self.time(30usize, 5usize, event.sojourn_time);
        self.count(31usize, 11usize, event.blocked_count);
        self.measure(32usize, 10usize, event.blocked_count);
        self.measure(33usize, 11usize, event.payload_len);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_tcp_socket_received(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorTcpSocketReceived,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(34usize, 12usize, 1usize);
        self.time(35usize, 6usize, event.transfer_time);
        self.measure(36usize, 12usize, event.payload_len);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_udp_started(&self, meta: &api::EndpointMeta, event: &api::AcceptorUdpStarted) {
        #[allow(unused_imports)]
        use api::*;
        self.count(37usize, 13usize, 1usize);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_udp_datagram_received(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorUdpDatagramReceived,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(38usize, 14usize, 1usize);
        self.measure(39usize, 13usize, event.len);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_udp_packet_received(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorUdpPacketReceived,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(40usize, 15usize, 1usize);
        self.measure(41usize, 14usize, event.payload_len);
        self.count_bool(42usize, 2usize, event.is_zero_offset);
        self.count_bool(43usize, 3usize, event.is_retransmission);
        self.count_bool(44usize, 4usize, event.is_fin);
        self.count_bool(45usize, 5usize, event.is_fin_known);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_udp_packet_dropped(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorUdpPacketDropped,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(46usize, 16usize, 1usize);
        self.count_nominal(47usize, 2usize, &event.reason);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_udp_stream_enqueued(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorUdpStreamEnqueued,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(48usize, 17usize, 1usize);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_udp_io_error(&self, meta: &api::EndpointMeta, event: &api::AcceptorUdpIoError) {
        #[allow(unused_imports)]
        use api::*;
        self.count(49usize, 18usize, 1usize);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_stream_pruned(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorStreamPruned,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(50usize, 19usize, 1usize);
        self.time(51usize, 7usize, event.sojourn_time);
        self.count_nominal(52usize, 3usize, &event.reason);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_acceptor_stream_dequeued(
        &self,
        meta: &api::EndpointMeta,
        event: &api::AcceptorStreamDequeued,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(53usize, 20usize, 1usize);
        self.time(54usize, 8usize, event.sojourn_time);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_stream_write_flushed(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteFlushed,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(55usize, 21usize, 1usize);
        self.time(
            56usize,
            9usize,
            meta.timestamp.saturating_duration_since(context.start_time),
        );
        context.ctr_15.fetch_add(1, Ordering::Relaxed);
        self.measure(58usize, 16usize, event.provided_len);
        self.count(59usize, 22usize, event.committed_len);
        self.measure(60usize, 17usize, event.committed_len);
        context
            .ctr_18
            .fetch_add(event.committed_len.as_u64(), Ordering::Relaxed);
        self.measure(62usize, 19usize, event.processing_duration);
        context
            .ctr_20
            .fetch_add(event.processing_duration.as_u64(), Ordering::Relaxed);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_write_fin_flushed(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteFinFlushed,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(64usize, 23usize, 1usize);
        self.time(
            65usize,
            10usize,
            meta.timestamp.saturating_duration_since(context.start_time),
        );
        context.ctr_21.fetch_add(1, Ordering::Relaxed);
        self.measure(67usize, 22usize, event.provided_len);
        self.count(68usize, 24usize, event.committed_len);
        self.measure(69usize, 23usize, event.committed_len);
        context
            .ctr_24
            .fetch_add(event.committed_len.as_u64(), Ordering::Relaxed);
        self.measure(71usize, 25usize, event.processing_duration);
        context
            .ctr_26
            .fetch_add(event.processing_duration.as_u64(), Ordering::Relaxed);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_write_blocked(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteBlocked,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(73usize, 25usize, 1usize);
        self.time(
            74usize,
            11usize,
            meta.timestamp.saturating_duration_since(context.start_time),
        );
        context.ctr_27.fetch_add(1, Ordering::Relaxed);
        self.measure(76usize, 28usize, event.provided_len);
        self.measure(77usize, 29usize, event.processing_duration);
        context
            .ctr_30
            .fetch_add(event.processing_duration.as_u64(), Ordering::Relaxed);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_write_errored(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteErrored,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(79usize, 26usize, 1usize);
        self.time(
            80usize,
            12usize,
            meta.timestamp.saturating_duration_since(context.start_time),
        );
        self.measure(81usize, 31usize, event.provided_len);
        self.measure(82usize, 32usize, event.processing_duration);
        context
            .ctr_33
            .fetch_add(event.processing_duration.as_u64(), Ordering::Relaxed);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_write_key_updated(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteKeyUpdated,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(84usize, 27usize, 1usize);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_write_allocated(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteAllocated,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(85usize, 28usize, 1usize);
        context.ctr_34.fetch_add(1, Ordering::Relaxed);
        self.measure(87usize, 35usize, event.allocated_len);
        context
            .ctr_36
            .fetch_add(event.allocated_len.as_u64(), Ordering::Relaxed);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_write_shutdown(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteShutdown,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(89usize, 29usize, 1usize);
        self.time(
            90usize,
            13usize,
            meta.timestamp.saturating_duration_since(context.start_time),
        );
        self.measure(91usize, 37usize, event.buffer_len);
        self.count_bool(92usize, 6usize, event.background);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_write_socket_flushed(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteSocketFlushed,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(93usize, 30usize, 1usize);
        context.ctr_38.fetch_add(1, Ordering::Relaxed);
        self.measure(95usize, 39usize, event.provided_len);
        self.count(96usize, 31usize, event.committed_len);
        self.measure(97usize, 40usize, event.committed_len);
        context
            .ctr_41
            .fetch_add(event.committed_len.as_u64(), Ordering::Relaxed);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_write_socket_blocked(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteSocketBlocked,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(99usize, 32usize, 1usize);
        context.ctr_42.fetch_add(1, Ordering::Relaxed);
        self.measure(101usize, 43usize, event.provided_len);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_write_socket_errored(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamWriteSocketErrored,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(102usize, 33usize, 1usize);
        self.measure(103usize, 44usize, event.provided_len);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_read_flushed(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadFlushed,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(104usize, 34usize, 1usize);
        self.time(
            105usize,
            14usize,
            meta.timestamp.saturating_duration_since(context.start_time),
        );
        context.ctr_45.fetch_add(1, Ordering::Relaxed);
        self.measure(107usize, 46usize, event.capacity);
        self.count(108usize, 35usize, event.committed_len);
        self.measure(109usize, 47usize, event.committed_len);
        context
            .ctr_48
            .fetch_add(event.committed_len.as_u64(), Ordering::Relaxed);
        self.measure(111usize, 49usize, event.processing_duration);
        context
            .ctr_50
            .fetch_add(event.processing_duration.as_u64(), Ordering::Relaxed);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_read_fin_flushed(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadFinFlushed,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(113usize, 36usize, 1usize);
        self.time(
            114usize,
            15usize,
            meta.timestamp.saturating_duration_since(context.start_time),
        );
        context.ctr_51.fetch_add(1, Ordering::Relaxed);
        self.measure(116usize, 52usize, event.capacity);
        self.measure(117usize, 53usize, event.processing_duration);
        context
            .ctr_54
            .fetch_add(event.processing_duration.as_u64(), Ordering::Relaxed);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_read_blocked(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadBlocked,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(119usize, 37usize, 1usize);
        self.time(
            120usize,
            16usize,
            meta.timestamp.saturating_duration_since(context.start_time),
        );
        self.measure(121usize, 55usize, event.capacity);
        self.measure(122usize, 56usize, event.processing_duration);
        context
            .ctr_57
            .fetch_add(event.processing_duration.as_u64(), Ordering::Relaxed);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_read_errored(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadErrored,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(124usize, 38usize, 1usize);
        self.time(
            125usize,
            17usize,
            meta.timestamp.saturating_duration_since(context.start_time),
        );
        self.measure(126usize, 58usize, event.capacity);
        self.measure(127usize, 59usize, event.processing_duration);
        context
            .ctr_60
            .fetch_add(event.processing_duration.as_u64(), Ordering::Relaxed);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_read_key_updated(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadKeyUpdated,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(129usize, 39usize, 1usize);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_read_shutdown(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadShutdown,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(130usize, 40usize, 1usize);
        self.time(
            131usize,
            18usize,
            meta.timestamp.saturating_duration_since(context.start_time),
        );
        self.count_bool(132usize, 7usize, event.background);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_read_socket_flushed(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadSocketFlushed,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(133usize, 41usize, 1usize);
        context.ctr_61.fetch_add(1, Ordering::Relaxed);
        self.measure(135usize, 62usize, event.capacity);
        self.count(136usize, 42usize, event.committed_len);
        self.measure(137usize, 63usize, event.committed_len);
        context
            .ctr_64
            .fetch_add(event.committed_len.as_u64(), Ordering::Relaxed);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_read_socket_blocked(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadSocketBlocked,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(139usize, 43usize, 1usize);
        context.ctr_65.fetch_add(1, Ordering::Relaxed);
        self.measure(141usize, 66usize, event.capacity);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_read_socket_errored(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReadSocketErrored,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(142usize, 44usize, 1usize);
        self.measure(143usize, 67usize, event.capacity);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_decrypt_packet(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamDecryptPacket,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(144usize, 45usize, 1usize);
        self.count_bool(145usize, 8usize, event.decrypted_in_place);
        self.measure(146usize, 68usize, event.forced_copy);
        self.measure(147usize, 69usize, event.required_application_buffer);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_tcp_connect(&self, meta: &api::EndpointMeta, event: &api::StreamTcpConnect) {
        #[allow(unused_imports)]
        use api::*;
        self.count(148usize, 46usize, 1usize);
        self.count_bool(149usize, 9usize, event.error);
        self.time(150usize, 19usize, event.latency);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_stream_connect(&self, meta: &api::EndpointMeta, event: &api::StreamConnect) {
        #[allow(unused_imports)]
        use api::*;
        self.count(151usize, 47usize, 1usize);
        self.count_bool(152usize, 10usize, event.error);
        self.count_nominal(153usize, 4usize, &event.tcp_success);
        self.count_nominal(154usize, 5usize, &event.handshake_success);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_stream_connect_error(&self, meta: &api::EndpointMeta, event: &api::StreamConnectError) {
        #[allow(unused_imports)]
        use api::*;
        self.count(155usize, 48usize, 1usize);
        self.count_nominal(156usize, 6usize, &event.reason);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_stream_packet_transmitted(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamPacketTransmitted,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(157usize, 49usize, 1usize);
        self.measure(158usize, 70usize, event.packet_len);
        self.count(159usize, 50usize, event.payload_len);
        self.measure(160usize, 71usize, event.payload_len);
        context
            .ctr_72
            .fetch_add(event.payload_len.as_u64(), Ordering::Relaxed);
        self.count_bool(162usize, 11usize, event.is_retransmission);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_probe_transmitted(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamProbeTransmitted,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(163usize, 51usize, 1usize);
        self.measure(164usize, 73usize, event.packet_len);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_packet_received(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamPacketReceived,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(165usize, 52usize, 1usize);
        self.measure(166usize, 74usize, event.packet_len);
        self.count(167usize, 53usize, event.payload_len);
        self.measure(168usize, 75usize, event.payload_len);
        context
            .ctr_76
            .fetch_add(event.payload_len.as_u64(), Ordering::Relaxed);
        self.count_bool(170usize, 12usize, event.is_retransmission);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_packet_lost(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamPacketLost,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(171usize, 54usize, 1usize);
        self.measure(172usize, 77usize, event.packet_len);
        self.count(173usize, 55usize, event.payload_len);
        self.measure(174usize, 78usize, event.payload_len);
        context
            .ctr_79
            .fetch_add(event.payload_len.as_u64(), Ordering::Relaxed);
        self.measure(176usize, 80usize, event.lifetime);
        self.count_bool(177usize, 13usize, event.is_retransmission);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_packet_acked(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamPacketAcked,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(178usize, 56usize, 1usize);
        self.measure(179usize, 81usize, event.packet_len);
        self.count(180usize, 57usize, event.payload_len);
        self.measure(181usize, 82usize, event.payload_len);
        context
            .ctr_83
            .fetch_add(event.payload_len.as_u64(), Ordering::Relaxed);
        self.measure(183usize, 84usize, event.lifetime);
        self.count_bool(184usize, 14usize, event.is_retransmission);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_packet_abandoned(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamPacketAbandoned,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(185usize, 58usize, 1usize);
        self.measure(186usize, 85usize, event.packet_len);
        self.count(187usize, 59usize, event.payload_len);
        self.measure(188usize, 86usize, event.payload_len);
        context
            .ctr_87
            .fetch_add(event.payload_len.as_u64(), Ordering::Relaxed);
        self.measure(190usize, 88usize, event.lifetime);
        self.count_bool(191usize, 15usize, event.is_retransmission);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_packet_spuriously_retransmitted(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamPacketSpuriouslyRetransmitted,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(192usize, 60usize, 1usize);
        self.measure(193usize, 89usize, event.packet_len);
        self.count(194usize, 61usize, event.payload_len);
        self.measure(195usize, 90usize, event.payload_len);
        context
            .ctr_91
            .fetch_add(event.payload_len.as_u64(), Ordering::Relaxed);
        self.count_bool(197usize, 16usize, event.is_retransmission);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_max_data_received(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamMaxDataReceived,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(198usize, 62usize, 1usize);
        self.count(199usize, 63usize, event.increase);
        self.measure(200usize, 92usize, event.increase);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_control_packet_transmitted(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamControlPacketTransmitted,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(201usize, 64usize, 1usize);
        self.measure(202usize, 93usize, event.packet_len);
        self.measure(203usize, 94usize, event.control_data_len);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_control_packet_received(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamControlPacketReceived,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(204usize, 65usize, 1usize);
        self.measure(205usize, 95usize, event.packet_len);
        self.measure(206usize, 96usize, event.control_data_len);
        self.count_bool(207usize, 17usize, event.is_authenticated);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_receiver_errored(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamReceiverErrored,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(208usize, 66usize, 1usize);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_stream_sender_errored(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::StreamSenderErrored,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(209usize, 67usize, 1usize);
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_connection_closed(
        &self,
        context: &Self::ConnectionContext,
        meta: &api::ConnectionMeta,
        event: &api::ConnectionClosed,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(210usize, 68usize, 1usize);
        self.measure(57usize, 15usize, context.ctr_15.load(Ordering::Relaxed));
        self.measure(61usize, 18usize, context.ctr_18.load(Ordering::Relaxed));
        self.measure(
            63usize,
            20usize,
            core::time::Duration::from_micros(context.ctr_20.load(Ordering::Relaxed)),
        );
        self.measure(66usize, 21usize, context.ctr_21.load(Ordering::Relaxed));
        self.measure(70usize, 24usize, context.ctr_24.load(Ordering::Relaxed));
        self.measure(
            72usize,
            26usize,
            core::time::Duration::from_micros(context.ctr_26.load(Ordering::Relaxed)),
        );
        self.measure(75usize, 27usize, context.ctr_27.load(Ordering::Relaxed));
        self.measure(
            78usize,
            30usize,
            core::time::Duration::from_micros(context.ctr_30.load(Ordering::Relaxed)),
        );
        self.measure(
            83usize,
            33usize,
            core::time::Duration::from_micros(context.ctr_33.load(Ordering::Relaxed)),
        );
        self.measure(86usize, 34usize, context.ctr_34.load(Ordering::Relaxed));
        self.measure(88usize, 36usize, context.ctr_36.load(Ordering::Relaxed));
        self.measure(94usize, 38usize, context.ctr_38.load(Ordering::Relaxed));
        self.measure(98usize, 41usize, context.ctr_41.load(Ordering::Relaxed));
        self.measure(100usize, 42usize, context.ctr_42.load(Ordering::Relaxed));
        self.measure(106usize, 45usize, context.ctr_45.load(Ordering::Relaxed));
        self.measure(110usize, 48usize, context.ctr_48.load(Ordering::Relaxed));
        self.measure(
            112usize,
            50usize,
            core::time::Duration::from_micros(context.ctr_50.load(Ordering::Relaxed)),
        );
        self.measure(115usize, 51usize, context.ctr_51.load(Ordering::Relaxed));
        self.measure(
            118usize,
            54usize,
            core::time::Duration::from_micros(context.ctr_54.load(Ordering::Relaxed)),
        );
        self.measure(
            123usize,
            57usize,
            core::time::Duration::from_micros(context.ctr_57.load(Ordering::Relaxed)),
        );
        self.measure(
            128usize,
            60usize,
            core::time::Duration::from_micros(context.ctr_60.load(Ordering::Relaxed)),
        );
        self.measure(134usize, 61usize, context.ctr_61.load(Ordering::Relaxed));
        self.measure(138usize, 64usize, context.ctr_64.load(Ordering::Relaxed));
        self.measure(140usize, 65usize, context.ctr_65.load(Ordering::Relaxed));
        self.measure(161usize, 72usize, context.ctr_72.load(Ordering::Relaxed));
        self.measure(169usize, 76usize, context.ctr_76.load(Ordering::Relaxed));
        self.measure(175usize, 79usize, context.ctr_79.load(Ordering::Relaxed));
        self.measure(182usize, 83usize, context.ctr_83.load(Ordering::Relaxed));
        self.measure(189usize, 87usize, context.ctr_87.load(Ordering::Relaxed));
        self.measure(196usize, 91usize, context.ctr_91.load(Ordering::Relaxed));
        let _ = context;
        let _ = meta;
        let _ = event;
    }
    #[inline]
    fn on_endpoint_initialized(&self, meta: &api::EndpointMeta, event: &api::EndpointInitialized) {
        #[allow(unused_imports)]
        use api::*;
        self.count(211usize, 69usize, 1usize);
        self.count_nominal(212usize, 7usize, &event.acceptor_addr);
        self.count_nominal(213usize, 8usize, &event.handshake_addr);
        self.count_bool(214usize, 18usize, event.tcp);
        self.count_bool(215usize, 19usize, event.udp);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_endpoint_udp_immediate_transmission_scheduled(
        &self,
        meta: &api::EndpointMeta,
        event: &api::EndpointUdpImmediateTransmissionScheduled,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(216usize, 70usize, 1usize);
        self.count_nominal(217usize, 9usize, &event.peer_address);
        self.measure(218usize, 97usize, event.buffer_size);
        self.measure(219usize, 98usize, event.segment_size);
        self.measure(220usize, 99usize, event.segment_count);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_endpoint_udp_transmission_scheduled(
        &self,
        meta: &api::EndpointMeta,
        event: &api::EndpointUdpTransmissionScheduled,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(221usize, 71usize, 1usize);
        self.count_nominal(222usize, 10usize, &event.peer_address);
        self.measure(223usize, 100usize, event.buffer_size);
        self.measure(224usize, 101usize, event.segment_size);
        self.measure(225usize, 102usize, event.segment_count);
        self.measure(226usize, 103usize, event.delay);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_endpoint_udp_transmission_rejected(
        &self,
        meta: &api::EndpointMeta,
        event: &api::EndpointUdpTransmissionRejected,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(227usize, 72usize, 1usize);
        self.count_nominal(228usize, 11usize, &event.peer_address);
        self.measure(229usize, 104usize, event.buffer_size);
        self.measure(230usize, 105usize, event.segment_size);
        self.measure(231usize, 106usize, event.segment_count);
        self.measure(232usize, 107usize, event.delay);
        self.measure(233usize, 108usize, event.backoff);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_endpoint_udp_packet_transmitted(
        &self,
        meta: &api::EndpointMeta,
        event: &api::EndpointUdpPacketTransmitted,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(234usize, 73usize, 1usize);
        self.count_nominal(235usize, 12usize, &event.peer_address);
        self.measure(236usize, 109usize, event.buffer_size);
        self.measure(237usize, 110usize, event.segment_size);
        self.measure(238usize, 111usize, event.segment_count);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_endpoint_udp_transmit_errored(
        &self,
        meta: &api::EndpointMeta,
        event: &api::EndpointUdpTransmitErrored,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(239usize, 74usize, 1usize);
        self.count_nominal(240usize, 13usize, &event.peer_address);
        self.measure(241usize, 112usize, event.buffer_size);
        self.measure(242usize, 113usize, event.segment_size);
        self.measure(243usize, 114usize, event.segment_count);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_endpoint_udp_packet_received(
        &self,
        meta: &api::EndpointMeta,
        event: &api::EndpointUdpPacketReceived,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(244usize, 75usize, 1usize);
        self.count_nominal(245usize, 14usize, &event.peer_address);
        self.measure(246usize, 115usize, event.buffer_size);
        self.measure(247usize, 116usize, event.segment_size);
        self.measure(248usize, 117usize, event.segment_count);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_endpoint_udp_receive_errored(
        &self,
        meta: &api::EndpointMeta,
        event: &api::EndpointUdpReceiveErrored,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(249usize, 76usize, 1usize);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_initialized(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapInitialized,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(250usize, 77usize, 1usize);
        self.measure(251usize, 118usize, event.capacity);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_uninitialized(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapUninitialized,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(252usize, 78usize, 1usize);
        self.measure(253usize, 119usize, event.capacity);
        self.measure(254usize, 120usize, event.entries);
        self.measure(255usize, 121usize, event.lifetime);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_background_handshake_requested(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapBackgroundHandshakeRequested,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(256usize, 79usize, 1usize);
        self.count_nominal(257usize, 15usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_entry_inserted(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapEntryInserted,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(258usize, 80usize, 1usize);
        self.count_nominal(259usize, 16usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_entry_ready(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapEntryReady,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(260usize, 81usize, 1usize);
        self.count_nominal(261usize, 17usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_entry_replaced(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapEntryReplaced,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(262usize, 82usize, 1usize);
        self.count_nominal(263usize, 18usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_id_entry_evicted(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapIdEntryEvicted,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(264usize, 83usize, 1usize);
        self.count_nominal(265usize, 19usize, &event.peer_address);
        self.measure(266usize, 122usize, event.age);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_address_entry_evicted(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapAddressEntryEvicted,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(267usize, 84usize, 1usize);
        self.count_nominal(268usize, 20usize, &event.peer_address);
        self.measure(269usize, 123usize, event.age);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_unknown_path_secret_packet_sent(
        &self,
        meta: &api::EndpointMeta,
        event: &api::UnknownPathSecretPacketSent,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(270usize, 85usize, 1usize);
        self.count_nominal(271usize, 21usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_unknown_path_secret_packet_received(
        &self,
        meta: &api::EndpointMeta,
        event: &api::UnknownPathSecretPacketReceived,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(272usize, 86usize, 1usize);
        self.count_nominal(273usize, 22usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_unknown_path_secret_packet_accepted(
        &self,
        meta: &api::EndpointMeta,
        event: &api::UnknownPathSecretPacketAccepted,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(274usize, 87usize, 1usize);
        self.count_nominal(275usize, 23usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_unknown_path_secret_packet_rejected(
        &self,
        meta: &api::EndpointMeta,
        event: &api::UnknownPathSecretPacketRejected,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(276usize, 88usize, 1usize);
        self.count_nominal(277usize, 24usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_unknown_path_secret_packet_dropped(
        &self,
        meta: &api::EndpointMeta,
        event: &api::UnknownPathSecretPacketDropped,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(278usize, 89usize, 1usize);
        self.count_nominal(279usize, 25usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_key_accepted(&self, meta: &api::EndpointMeta, event: &api::KeyAccepted) {
        #[allow(unused_imports)]
        use api::*;
        self.count(280usize, 90usize, 1usize);
        self.measure(281usize, 124usize, event.gap);
        self.measure(282usize, 125usize, event.forward_shift);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_replay_definitely_detected(
        &self,
        meta: &api::EndpointMeta,
        event: &api::ReplayDefinitelyDetected,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(283usize, 91usize, 1usize);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_replay_potentially_detected(
        &self,
        meta: &api::EndpointMeta,
        event: &api::ReplayPotentiallyDetected,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(284usize, 92usize, 1usize);
        self.measure(285usize, 126usize, event.gap);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_replay_detected_packet_sent(
        &self,
        meta: &api::EndpointMeta,
        event: &api::ReplayDetectedPacketSent,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(286usize, 93usize, 1usize);
        self.count_nominal(287usize, 26usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_replay_detected_packet_received(
        &self,
        meta: &api::EndpointMeta,
        event: &api::ReplayDetectedPacketReceived,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(288usize, 94usize, 1usize);
        self.count_nominal(289usize, 27usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_replay_detected_packet_accepted(
        &self,
        meta: &api::EndpointMeta,
        event: &api::ReplayDetectedPacketAccepted,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(290usize, 95usize, 1usize);
        self.count_nominal(291usize, 28usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_replay_detected_packet_rejected(
        &self,
        meta: &api::EndpointMeta,
        event: &api::ReplayDetectedPacketRejected,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(292usize, 96usize, 1usize);
        self.count_nominal(293usize, 29usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_replay_detected_packet_dropped(
        &self,
        meta: &api::EndpointMeta,
        event: &api::ReplayDetectedPacketDropped,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(294usize, 97usize, 1usize);
        self.count_nominal(295usize, 30usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_stale_key_packet_sent(&self, meta: &api::EndpointMeta, event: &api::StaleKeyPacketSent) {
        #[allow(unused_imports)]
        use api::*;
        self.count(296usize, 98usize, 1usize);
        self.count_nominal(297usize, 31usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_stale_key_packet_received(
        &self,
        meta: &api::EndpointMeta,
        event: &api::StaleKeyPacketReceived,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(298usize, 99usize, 1usize);
        self.count_nominal(299usize, 32usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_stale_key_packet_accepted(
        &self,
        meta: &api::EndpointMeta,
        event: &api::StaleKeyPacketAccepted,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(300usize, 100usize, 1usize);
        self.count_nominal(301usize, 33usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_stale_key_packet_rejected(
        &self,
        meta: &api::EndpointMeta,
        event: &api::StaleKeyPacketRejected,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(302usize, 101usize, 1usize);
        self.count_nominal(303usize, 34usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_stale_key_packet_dropped(
        &self,
        meta: &api::EndpointMeta,
        event: &api::StaleKeyPacketDropped,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(304usize, 102usize, 1usize);
        self.count_nominal(305usize, 35usize, &event.peer_address);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_address_cache_accessed(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapAddressCacheAccessed,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(306usize, 103usize, 1usize);
        self.count_nominal(307usize, 36usize, &event.peer_address);
        self.count_bool(308usize, 20usize, event.hit);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_address_cache_accessed_hit(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapAddressCacheAccessedHit,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(309usize, 104usize, 1usize);
        self.count_nominal(310usize, 37usize, &event.peer_address);
        self.measure(311usize, 127usize, event.age);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_id_cache_accessed(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapIdCacheAccessed,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(312usize, 105usize, 1usize);
        self.count_bool(313usize, 21usize, event.hit);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_id_cache_accessed_hit(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapIdCacheAccessedHit,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(314usize, 106usize, 1usize);
        self.measure(315usize, 128usize, event.age);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_cleaner_cycled(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapCleanerCycled,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(316usize, 107usize, 1usize);
        self.measure(317usize, 129usize, event.id_entries);
        self.measure(318usize, 130usize, event.id_entries_retired);
        self.measure(319usize, 131usize, event.id_entries_active);
        self.measure(320usize, 132usize, event.id_entries_active_utilization);
        self.measure(321usize, 133usize, event.id_entries_utilization);
        self.measure(322usize, 134usize, event.id_entries_initial_utilization);
        self.measure(323usize, 135usize, event.address_entries);
        self.measure(324usize, 136usize, event.address_entries_active);
        self.measure(325usize, 137usize, event.address_entries_active_utilization);
        self.measure(326usize, 138usize, event.address_entries_retired);
        self.measure(327usize, 139usize, event.address_entries_utilization);
        self.measure(
            328usize,
            140usize,
            event.address_entries_initial_utilization,
        );
        self.measure(329usize, 141usize, event.handshake_requests);
        self.measure(330usize, 142usize, event.handshake_requests_retired);
        self.measure(331usize, 143usize, event.handshake_lock_duration);
        self.measure(332usize, 144usize, event.duration);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_id_write_lock(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapIdWriteLock,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(333usize, 108usize, 1usize);
        self.measure(334usize, 145usize, event.acquire);
        self.measure(335usize, 146usize, event.duration);
        let _ = event;
        let _ = meta;
    }
    #[inline]
    fn on_path_secret_map_address_write_lock(
        &self,
        meta: &api::EndpointMeta,
        event: &api::PathSecretMapAddressWriteLock,
    ) {
        #[allow(unused_imports)]
        use api::*;
        self.count(336usize, 109usize, 1usize);
        self.measure(337usize, 147usize, event.acquire);
        self.measure(338usize, 148usize, event.duration);
        let _ = event;
        let _ = meta;
    }
}
