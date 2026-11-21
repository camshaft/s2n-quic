// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests of stream priority in the send pipeline.
//!
//! Lowest-priority (`Background`) bulk uploads fill a 1 Gbps link while a `Highest` priority
//! stream makes small request and reply round trips to the same peer. Send credit admits bulk
//! bytes into the send pipeline well ahead of the pacer, so a pipeline that sends frames in
//! arrival order makes each request wait behind every admitted bulk byte. With the pipeline
//! ordering frames by stream priority, a request waits only for the batches already past that
//! ordering, so its round trip stays near the idle round trip.
//!
//! Each test also checks that every bulk stream still delivers its whole body, so the lower
//! priority keeps making progress next to the higher one.

use crate::{
    credit::Priority,
    socket::rate::Rate,
    stream::endpoint::testing::sim::{Client, Server, SimEndpointConfig},
    testing::{ext::*, sim},
    tracing::*,
};
use bach::time::{sleep, timeout, Instant};
use s2n_quic_core::{stream::testing::Data, varint::VarInt};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

/// The acceptor the bulk streams connect to.
const BULK_ACCEPTOR: u8 = 1;

/// The acceptor the request and reply streams connect to.
const CONTROL_ACCEPTOR: u8 = 2;

/// The client's send rate, in Gbps.
const LINK_GBPS: f64 = 1.0;

/// The bytes each request and each reply carries.
const MESSAGE_LEN: u64 = 200;

/// The bytes all bulk streams upload together: about 270 ms of the link.
const BULK_TOTAL: u64 = 32 * 1024 * 1024;

/// The bytes frame dispatch puts in one batch for a peer, as `BatchFramesByPathSecret` sets it.
const BATCH_BYTES: u64 = u16::MAX as u64 - 3000;

/// The batches a request may find ahead of it once the pipeline orders frames by priority: the
/// batch being filled, the batch the pacer holds and the batches the send workers hold for the
/// peer, with room to spare. At 1 Gbps this is about 4 ms, where arrival order puts every
/// admitted bulk byte ahead of the request.
const BATCHES_AHEAD: u64 = 8;

/// The round trips measured before the bulk streams start.
const IDLE_ROUND_TRIPS: usize = 3;

/// The round trips measured while the bulk streams run.
const LOADED_ROUND_TRIPS: usize = 5;

/// The time the bulk streams run before the first loaded round trip, so their bytes queue up.
const LOAD_DELAY: Duration = Duration::from_millis(50);

/// The time between two loaded round trips.
const LOADED_INTERVAL: Duration = Duration::from_millis(5);

/// The longest a request, a reply or one read may wait without progress.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest a bulk stream's whole upload may take.
const BULK_TIMEOUT: Duration = Duration::from_secs(60);

/// Reads `len` bytes and the end of the stream from `reader`.
async fn read_body(reader: &mut crate::stream::Reader, len: u64, what: &str) {
    let mut rx = Data::new(len);
    loop {
        let result = timeout(CHUNK_TIMEOUT, reader.read_into(&mut rx)).await;
        let n = match result {
            Ok(Ok(n)) => n,
            Ok(Err(error)) => panic!("{what} read failed at offset {}: {error}", rx.offset()),
            Err(_) => panic!(
                "{what} read stalled for {CHUNK_TIMEOUT:?} at offset {}",
                rx.offset()
            ),
        };
        if n == 0 {
            break;
        }
    }
    assert!(rx.is_finished(), "{what} stream ended at offset {} of {len}", rx.offset());
}

/// Makes one request and reply round trip on a new `Highest` priority stream, and returns the
/// time from the connect to the end of the reply.
async fn round_trip(client: &mut Client) -> Duration {
    let start = Instant::now().elapsed_since_start();

    let stream = client
        .connect("server:0", VarInt::from_u8(CONTROL_ACCEPTOR))
        .await
        .expect("control connect failed");
    let (mut reader, mut writer) = stream.into_split();
    writer.set_priority(Priority::Highest);

    let mut request = Data::new(MESSAGE_LEN);
    timeout(CHUNK_TIMEOUT, writer.write_all_from_fin(&mut request))
        .await
        .expect("request stalled")
        .expect("request failed");

    read_body(&mut reader, MESSAGE_LEN, "reply").await;

    Instant::now().elapsed_since_start() - start
}

/// Runs `bulk_streams` `Background` uploads that share [`BULK_TOTAL`] bytes, measures `Highest`
/// round trips before and while they run, and asserts each loaded round trip stays within
/// [`BATCHES_AHEAD`] batches' send time of the slowest idle one.
///
/// Returns how many bulk writers and how many bulk readers finished.
fn run(bulk_streams: usize) -> (usize, usize) {
    let per_stream = BULK_TOTAL / bulk_streams as u64;

    let writers_done = Arc::new(AtomicUsize::new(0));
    let readers_done = Arc::new(AtomicUsize::new(0));

    sim(|| {
        // Server: drains the bulk streams and answers each request.
        {
            let readers_done = readers_done.clone();
            async move {
                let server = Server::new();
                let mut bulk = server
                    .register_acceptor_channel(VarInt::from_u8(BULK_ACCEPTOR), bulk_streams * 2)
                    .expect("bulk acceptor registration failed");
                let mut control = server
                    .register_acceptor_channel(VarInt::from_u8(CONTROL_ACCEPTOR), 8)
                    .expect("control acceptor registration failed");

                async move {
                    while let Some(stream) = control.recv().await {
                        async move {
                            let (mut reader, mut writer) = stream.into_split();
                            writer.set_priority(Priority::Highest);
                            read_body(&mut reader, MESSAGE_LEN, "request").await;

                            let mut reply = Data::new(MESSAGE_LEN);
                            timeout(CHUNK_TIMEOUT, writer.write_all_from_fin(&mut reply))
                                .await
                                .expect("reply stalled")
                                .expect("reply failed");
                        }
                        .spawn();
                    }
                }
                .spawn();

                while let Some(stream) = bulk.recv().await {
                    let readers_done = readers_done.clone();
                    async move {
                        let (mut reader, _writer) = stream.into_split();
                        read_body(&mut reader, per_stream, "bulk").await;
                        readers_done.fetch_add(1, Ordering::Relaxed);
                    }
                    .primary()
                    .spawn();
                }
            }
            .group("server")
            .spawn();
        }

        // Client: idle round trips, then bulk uploads and loaded round trips.
        {
            let writers_done = writers_done.clone();
            let readers_done = readers_done.clone();
            async move {
                let mut client = SimEndpointConfig::default()
                    .overall_send_rate(Rate::new(LINK_GBPS))
                    .client();

                let mut idle = Duration::ZERO;
                for _ in 0..IDLE_ROUND_TRIPS {
                    idle = idle.max(round_trip(&mut client).await);
                }

                let mut streams = Vec::with_capacity(bulk_streams);
                for _ in 0..bulk_streams {
                    let stream = client
                        .connect("server:0", VarInt::from_u8(BULK_ACCEPTOR))
                        .await
                        .expect("bulk connect failed");
                    streams.push(stream);
                }

                for (i, stream) in streams.into_iter().enumerate() {
                    let writers_done = writers_done.clone();
                    async move {
                        let (_reader, mut writer) = stream.into_split();
                        writer.set_priority(Priority::Background);

                        let mut payload = Data::new(per_stream);
                        timeout(BULK_TIMEOUT, writer.write_all_from_fin(&mut payload))
                            .await
                            .unwrap_or_else(|_| {
                                panic!("bulk stream {i} didn't finish within {BULK_TIMEOUT:?}")
                            })
                            .expect("bulk write failed");
                        writers_done.fetch_add(1, Ordering::Relaxed);
                    }
                    .primary()
                    .spawn();
                }

                sleep(LOAD_DELAY).await;

                let ahead = Rate::new(LINK_GBPS).nanos_for_bytes(BATCHES_AHEAD * BATCH_BYTES);
                let bound = idle + Duration::from_nanos(ahead);

                for i in 0..LOADED_ROUND_TRIPS {
                    let loaded = round_trip(&mut client).await;
                    info!(bulk_streams, i, ?idle, ?loaded, ?bound, "loaded round trip");
                    assert!(
                        loaded <= bound,
                        "loaded round trip {i} with {bulk_streams} bulk streams took {loaded:?}, \
                         more than the idle {idle:?} plus {BATCHES_AHEAD} batches ({bound:?})"
                    );
                    sleep(LOADED_INTERVAL).await;
                }

                let finished = readers_done.load(Ordering::Relaxed);
                assert!(
                    finished < bulk_streams,
                    "all {bulk_streams} bulk streams finished before the last loaded round trip \
                     ended, so the round trips didn't run under load"
                );
            }
            .group("client")
            .primary()
            .spawn();
        }
    });

    (
        writers_done.load(Ordering::Relaxed),
        readers_done.load(Ordering::Relaxed),
    )
}

/// Runs [`run`] and asserts every bulk stream finished.
fn check(bulk_streams: usize) {
    let _no_snap = crate::testing::without_snapshots();
    let _no_trace = crate::testing::without_tracing();

    let (writers, readers) = run(bulk_streams);

    assert_eq!(writers, bulk_streams, "every bulk writer must finish");
    assert_eq!(readers, bulk_streams, "every bulk reader must finish");
}

#[test]
fn highest_round_trip_behind_one_bulk_stream() {
    check(1);
}

#[test]
fn highest_round_trip_behind_eight_bulk_streams() {
    check(8);
}

#[test]
fn highest_round_trip_behind_sixty_four_bulk_streams() {
    check(64);
}
