// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    stream::testing::{Client, Server},
    testing::{ext::*, sim},
};
use bach::time::Instant;
use s2n_quic_core::stream::testing::Data;
use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    vec,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, info_span, Instrument};

fn watchdog() {
    // NOTE: This is a **simulated time** watchdog, NOT wall-clock time.
    // The test uses a discrete event simulation where time advances instantly
    // between events. Wall-clock time (what nextest measures) is completely
    // independent of simulated time - 60s of sim time should complete in
    // milliseconds of wall time. If this watchdog fires, it means the
    // simulation is stuck in an infinite event loop where simulated time
    // cannot advance.
    async move {
        120.s().sleep().await;
        panic!(
            "WATCHDOG: simulation did not complete within 120s of simulated time. \
                 This indicates an infinite event loop preventing time advancement."
        );
    }
    .group("watchdog")
    .spawn();
}

#[test]
fn backlog_rejection() {
    let backlog = 2;
    let rejected = Arc::new(AtomicUsize::new(0));
    sim(|| {
        for idx in 0..(backlog * 2) {
            let rejected = rejected.clone();
            async move {
                let client = Client::builder().build();
                let mut stream = client.connect_sim("server:443").await.unwrap();

                // Alternate between small and large payloads to try and trigger different failure modes
                let small_len = 100;
                let large_len = u32::MAX as u64;
                let payload_len = if idx % 2 == 0 { small_len } else { large_len };
                let is_small = payload_len == small_len;
                let mut payload = Data::new(payload_len);

                let start = Instant::now();

                let write_res = stream.write_all_from_fin(&mut payload).await;

                info!(res = ?write_res, payload_len, "write");

                let write_err = if is_small {
                    // The small payloads shouldn't block on the stream getting accepted
                    write_res.unwrap();
                    None
                } else {
                    Some(write_res.unwrap_err())
                };

                let mut response = vec![];
                let read_res = stream.read_to_end(&mut response).await;

                info!(res = ?read_res, payload_len, "read");
                let read_err = read_res.unwrap_err();

                if let Some(write_err) = write_err {
                    assert_eq!(
                        read_err.kind(),
                        write_err.kind(),
                        "read error ({:?}) should match write error ({:?}); payload_len: {payload_len}",
                        read_err.kind(),
                        write_err.kind(),
                    );
                }

                let elapsed = start.elapsed();

                match read_err.kind() {
                    // Streams evicted from the queue (capacity exceeded) get ConnectionRefused
                    io::ErrorKind::ConnectionRefused => {
                        assert!(elapsed < 1.s(), "connection refused should be fast");
                        rejected.fetch_add(1, Ordering::Relaxed);
                    }
                    // Streams drained when the server closes get a reset with ServerClosed code
                    io::ErrorKind::ConnectionReset => {
                        rejected.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {
                        panic!("unexpected error kind: {read_err:?}");
                    }
                }
            }
            .group(format!("client-{idx}"))
            .instrument(info_span!("client", client = idx))
            .primary()
            .spawn();
        }

        async move {
            let server = Server::udp().port(443).backlog(backlog).build();

            // Give clients time to connect before dropping the server.
            // Dropping the server closes the accept queue receiver, which drains
            // all pending entries and notifies stream workers via Entry::drop.
            1.s().sleep().await;

            drop(server);
        }
        .group("server")
        .instrument(info_span!("server"))
        .spawn();

        watchdog();
    });

    let rejected = rejected.load(Ordering::Relaxed);
    assert_eq!(
        rejected,
        backlog * 2,
        "all {0} streams should have been rejected, but only {1} were",
        backlog * 2,
        rejected,
    );
}

/// Test that when a client drops its stream while it's still sitting in the
/// server's accept queue, the server's accept() filters it out instead of
/// returning a broken stream to the application.
///
/// The flow:
/// 1. Clients connect and immediately drop (no graceful shutdown)
/// 2. Server-side workers idle-timeout, setting errors on shared state
/// 3. Server drops, closing the accept queue. Since entries have errors,
///    they are pruned via `Entry::drop` → `prune(ServerClosed)`.
/// 4. If the server had called `accept()`, the errored entries would be
///    skipped by the `has_error()` filter.
#[test]
fn client_close_while_queued() {
    let backlog = 4;
    let client_count: usize = 4;
    let pruned = Arc::new(AtomicUsize::new(0));
    sim(|| {
        // Spawn clients that connect then drop WITHOUT graceful shutdown.
        for idx in 0..client_count {
            async move {
                let client = Client::builder().build();
                let mut stream = client.connect_sim("server:443").await.unwrap();
                let _ = stream.write_all(b"hello").await;

                // Drop the stream immediately - the server hasn't accepted it yet.
                // The server-side worker will see no activity and eventually idle-timeout.
                drop(stream);
            }
            .group(format!("client-{idx}"))
            .instrument(info_span!("client", client = idx))
            .primary()
            .spawn();
        }

        {
            let pruned = pruned.clone();
            async move {
                let subscriber = crate::event::testing::Subscriber::no_snapshot();
                let server = Server::udp()
                    .port(443)
                    .backlog(backlog)
                    .subscriber(subscriber)
                    .build();

                // Wait for server-side workers to idle-timeout (30s default)
                // so errors are set on all entries' shared state.
                35.s().sleep().await;

                // Count how many entries have errors set before we drop
                // (they would be filtered by accept's has_error check)
                let test_sub = server.subscriber();
                let pruned_before = test_sub.acceptor_stream_pruned.load(Ordering::Relaxed);

                // Drop the server. This closes the accept queue, which drains
                // all remaining entries. Each entry's Drop fires
                // prune(ServerClosed), setting the error and notifying workers.
                drop(server);

                let pruned_after = test_sub.acceptor_stream_pruned.load(Ordering::Relaxed);
                let pruned_count = pruned_after - pruned_before;
                info!(pruned_count, "entries pruned on server close");
                pruned.store(pruned_count as usize, Ordering::Relaxed);
            }
            .group("server")
            .instrument(info_span!("server"))
            .primary()
            .spawn();
        }

        watchdog();
    });

    let pruned_count = pruned.load(Ordering::Relaxed);
    assert_eq!(
        pruned_count, client_count,
        "all {client_count} entries should have been pruned on server close, but only {pruned_count} were"
    );
}
