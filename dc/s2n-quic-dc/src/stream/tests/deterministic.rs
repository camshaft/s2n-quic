// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    stream::testing::{Client, Server},
    testing::{ext::*, sim, spawn},
};
use bach::time::Instant;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    join,
};
use tracing::{info_span, Instrument};

#[test]
fn request_response() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            let request = vec![42; 100_000];
            stream.write_all(&request).await.unwrap();
            stream.shutdown().await.unwrap();

            let mut response = vec![];
            stream.read_to_end(&mut response).await.unwrap();

            assert_eq!(request, response);
        }
        .group("client")
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, _addr)) = server.accept().await {
                spawn(async move {
                    let mut request = vec![];
                    stream.read_to_end(&mut request).await.unwrap();

                    stream.write_all(&request).await.unwrap();
                });
            }
        }
        .group("server")
        .spawn();
    });
}

#[test]
fn split() {
    sim(|| {
        ::bach::net::monitor::on_packet_sent(move |packet| {
            tracing::info!(?packet, "on_packet_sent");
            Default::default()
        });

        async move {
            let client = Client::builder().build();
            let stream = client.connect_sim("server:443").await.unwrap();

            let (mut recv, mut send) = stream.into_split();

            let request = b"hello!";
            send.write_all_from_fin(&mut &request[..]).await.unwrap();
            drop(send);

            1.s().sleep().await;

            let mut response = vec![];
            recv.read_to_end(&mut response).await.unwrap();

            assert_eq!(request, &response[..]);
        }
        .group("client")
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((stream, _addr)) = server.accept().await {
                spawn(async move {
                    let (mut recv, mut send) = stream.into_split();
                    let mut request = vec![];
                    recv.read_to_end(&mut request).await.unwrap();
                    drop(recv);

                    1.s().sleep().await;

                    send.write_all_from_fin(&mut &request[..]).await.unwrap();
                });
            }
        }
        .group("server")
        .spawn();
    });
}

#[test]
fn fail_fast_unknown_path_secret() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            let start = Instant::now();

            let count = 8u32;

            for idx in 0..count {
                // The current simulation code doesn't have a way to rehandshake so this will continue to fail for the peer
                let stream = client.connect_sim("server:443").await.unwrap();

                if idx == 0 {
                    // the first stream is throw away to get the server to drop its state
                    drop(stream);
                    1.ms().sleep().await;
                } else {
                    let (mut recv, mut send) = stream.into_split();

                    let send = async move {
                        if idx % 2 == 0 {
                            // small payloads should succeed from the application's point of view
                            let request = vec![42; 1024];
                            send.write_all(&request).await.unwrap();
                        } else {
                            // write large payloads to get blocked by flow
                            let request = vec![42; 1024 * 1024];
                            send.write_all(&request).await.unwrap_err();
                        }
                    };

                    let recv = async move {
                        let mut response = vec![];
                        recv.read_to_end(&mut response).await.unwrap_err();
                    };

                    join!(send, recv);
                }
            }

            let elapsed = start.elapsed();
            let expected = count * 1.ms();
            // Allow some timing tolerance
            assert!(
                elapsed <= expected + 1.ms(),
                "streams should fail within 1RTT: elapsed={elapsed:?}, expected={expected:?}"
            );
        }
        .group("client")
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, _addr)) = server.accept().await {
                // drop the state after accepting a stream to simulate a restart
                server.map().drop_state();

                spawn(async move {
                    let mut request = vec![];
                    let _ = stream.read_to_end(&mut request).await;
                });
            }
        }
        .group("server")
        .spawn();
    });
}

#[test]
fn lost_initial_ack() {
    sim(|| {
        {
            let mut count = 0;
            bach::net::monitor::on_packet_sent(move |packet| {
                let mut command = bach::net::monitor::Command::Pass;

                if packet.source().port() == 443 {
                    count += 1;
                    if count <= 10 {
                        command = bach::net::monitor::Command::Drop;
                    }
                }

                tracing::info!(source = ?packet.source(), len = packet.transport.payload().len(), ?command);

                command
            });
        }

        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            let body = vec![42; 1000];

            stream.write_all(&mut &body[..]).await.unwrap();
            stream.shutdown().await.unwrap();

            let mut response = Vec::with_capacity(2000);
            stream.read_to_end(&mut response).await.unwrap();
            assert_eq!(response, body);
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();
            while let Ok((mut stream, _addr)) = server.accept().await {
                spawn(
                    async move {
                        let mut request = Vec::with_capacity(2000);
                        let _ = stream.read_to_end(&mut request).await;
                        let _ = stream.write_all(&request).await;
                    }
                    .instrument(info_span!("server:stream")),
                );
            }
        }
        .group("server")
        .instrument(info_span!("server"))
        .spawn();
    })
}

use core::ops::Range;

#[derive(Clone, bolero::TypeGenerator)]
struct DroppedPackets {
    #[generator(produce::<Vec<u8>>().with().values(0..=10))]
    counts: Vec<u8>,
}

impl DroppedPackets {
    fn ranges(&self) -> impl Iterator<Item = Range<usize>> + '_ {
        Self::iter_from_counts(self.counts.iter().copied(), |range, enabled| {
            if !enabled && range.end > range.start {
                Some(range)
            } else {
                None
            }
        })
    }

    fn enabled_iter(self) -> impl Iterator<Item = bool> {
        Self::iter_from_counts(self.counts.into_iter(), |range, enabled| {
            range.map(move |_| enabled)
        })
    }

    fn iter_from_counts<T: IntoIterator>(
        v: impl Iterator<Item = u8>,
        map: impl Fn(Range<usize>, bool) -> T,
    ) -> impl Iterator<Item = T::Item> {
        let mut start = 0;
        let mut enabled = true;
        let mut is_enabled = move || {
            let v = enabled;
            enabled = !enabled;
            v
        };

        v.flat_map(move |mut len| {
            let local_start = start;

            if start > 0 {
                len = len.max(1);
            }

            let end = start + len as usize;

            start = end;

            let v = is_enabled();

            map(local_start..end, v)
        })
    }

    fn from_iter(iter: impl IntoIterator<Item = Range<usize>>) -> Self {
        let mut counts = vec![];
        let mut last = 0;
        for range in iter {
            counts.push((range.start - last) as u8);
            counts.push((range.end - range.start) as u8);
            last = range.end;
        }

        Self { counts }
    }

    fn loss_percent(&self) -> f64 {
        let mut total = 0;
        let mut dropped = 0;
        for range in self.ranges() {
            total = range.end;
            dropped += range.end - range.start;
        }
        (dropped as f64 / total as f64) * 100.0
    }

    fn sim(self) {
        sim(|| {
            {
                tracing::info!(packets = ?self, loss = format!("{:.02}%", self.loss_percent()), "dropped packets");
                let mut enabled = self.enabled_iter().enumerate();
                bach::net::monitor::on_packet_sent(move |packet| {
                    if packet.source().port() == 443 {
                        if let Some((idx, enabled)) = enabled.next() {
                            if !enabled {
                                tracing::info!(
                                    idx,
                                    len = packet.transport.payload().len(),
                                    "dropping packet"
                                );
                                return bach::net::monitor::Command::Drop;
                            } else {
                                tracing::info!(
                                    idx,
                                    len = packet.transport.payload().len(),
                                    "allowing packet"
                                );
                            }
                        }
                    }

                    bach::net::monitor::Command::Pass
                });
            }

            async move {
                let client = Client::builder().build();
                let mut stream = client.connect_sim("server:443").await.unwrap();

                let body = vec![42; 1 << 18];

                stream.write_all(&mut &body[..]).await.unwrap();
                stream.shutdown().await.unwrap();

                let mut response = Vec::with_capacity(2000);
                stream.read_to_end(&mut response).await.unwrap();
                assert_eq!(response, body);
            }
            .group("client")
            .primary()
            .spawn();

            async move {
                let server = Server::udp().port(443).build();

                2.s().sleep().await;

                while let Ok((mut stream, _addr)) = server.accept().await {
                    spawn(async move {
                        let mut request = Vec::with_capacity(1 << 18);
                        let _ = stream.read_to_end(&mut request).await;
                        let _ = stream.write_all(&request).await;
                    });
                }
            }
            .group("server")
            .spawn();
        })
    }

    /// Runs a simulation with the given packet drop pattern and measures the total
    /// elapsed simulated time from t=0 to transfer completion.
    ///
    /// Uses the same timing approach as the `rpc.rs` tests: an `Instant::now()` is
    /// captured inside the async task at the moment the round-trip completes, and
    /// `elapsed_since_start()` is called after `sim()` returns to get the simulated
    /// wall-clock duration from the start of the simulation.
    ///
    /// If the transfer does not complete within 30 simulated seconds the method
    /// panics, letting bolero shrink the packet-loss pattern to the minimal case
    /// that stalls BBRv2.
    fn sim_with_timing(self) -> Duration {
        const BODY_LEN: usize = 1 << 18;
        const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

        let end_time: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let end_time_inner = end_time.clone();

        sim(|| {
            {
                tracing::info!(
                    packets = ?self,
                    loss = format!("{:.02}%", self.loss_percent()),
                    "dropped packets"
                );
                let mut enabled = self.enabled_iter().enumerate();
                bach::net::monitor::on_packet_sent(move |packet| {
                    if packet.source().port() == 443 {
                        if let Some((idx, enabled)) = enabled.next() {
                            if !enabled {
                                tracing::info!(
                                    idx,
                                    len = packet.transport.payload().len(),
                                    "dropping packet"
                                );
                                return bach::net::monitor::Command::Drop;
                            }
                        }
                    }
                    bach::net::monitor::Command::Pass
                });
            }

            {
                let end_time = end_time_inner.clone();
                async move {
                    let client = Client::builder().build();
                    let mut stream = client.connect_sim("server:443").await.unwrap();

                    let body = vec![42u8; BODY_LEN];

                    crate::testing::timeout(
                        TRANSFER_TIMEOUT,
                        async {
                            stream.write_all(&body).await.unwrap();
                            stream.shutdown().await.unwrap();

                            let mut response = Vec::with_capacity(BODY_LEN);
                            stream.read_to_end(&mut response).await.unwrap();
                            assert_eq!(response, body);
                        },
                    )
                    .await
                    .expect(
                        "transfer timed out; BBRv2 may have stalled under this loss pattern",
                    );

                    // Capture the simulated time at completion so we can report
                    // elapsed_since_start() after sim() exits.
                    *end_time.lock().unwrap() = Some(Instant::now());
                }
                .group("client")
                .primary()
                .spawn();
            }

            async move {
                let server = Server::udp().port(443).build();

                while let Ok((mut stream, _addr)) = server.accept().await {
                    spawn(async move {
                        let mut request = Vec::with_capacity(BODY_LEN);
                        let _ = stream.read_to_end(&mut request).await;
                        let _ = stream.write_all(&request).await;
                    });
                }
            }
            .group("server")
            .spawn();
        });

        let elapsed = end_time.lock().unwrap().unwrap().elapsed_since_start();
        elapsed
    }
}

impl core::fmt::Debug for DroppedPackets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.ranges()).finish()
    }
}

#[test]
fn initial_loss() {
    DroppedPackets::from_iter([
        1..10,
        10..20,
        20..29,
        29..34,
        38..45,
        52..54,
        61..66,
        67..71,
        78..86,
        91..97,
        97..100,
        100..102,
        113..117,
        121..124,
        129..130,
        138..141,
    ])
    .sim();
}

#[test]
fn sporadic_loss() {
    DroppedPackets::from_iter([
        10..12,
        22..27,
        28..32,
        40..45,
        54..61,
        63..66,
        75..77,
        81..84,
        89..98,
        108..116,
        121..127,
        129..131,
        136..137,
        143..152,
        153..160,
        163..172,
        181..183,
        185..192,
        199..201,
        202..212,
        213..219,
        227..236,
        238..248,
        275..283,
        284..290,
    ])
    .sim();
}

#[test]
fn dropped_packets_round_trip() {
    bolero::check!()
        .with_type::<DroppedPackets>()
        .with_test_time(core::time::Duration::from_secs(30))
        .with_shrink_time(core::time::Duration::from_secs(10))
        .cloned()
        .for_each(|packets| {
            let from_ranges = packets.ranges().flatten().collect::<Vec<_>>();
            let from_enabled = packets
                .clone()
                .enabled_iter()
                .enumerate()
                .filter_map(|(idx, enabled)| if !enabled { Some(idx) } else { None })
                .collect::<Vec<_>>();
            assert_eq!(from_ranges, from_enabled, "original: {:?}", packets.counts);

            let from_iter = DroppedPackets::from_iter(packets.ranges());
            let from_iter_ranges = from_iter.ranges().flatten().collect::<Vec<_>>();
            assert_eq!(
                from_ranges, from_iter_ranges,
                "original: {:?}, from_iter: {:?}",
                packets.counts, from_iter.counts
            );

            let original = packets.clone().enabled_iter().collect::<Vec<_>>();
            let from_iter_enabled = from_iter.clone().enabled_iter().collect::<Vec<_>>();

            // from_iter can't recover trailing enabled packets, so compare up to from_iter's
            // length and ensure any remaining original items are all enabled (true)
            let common_len = from_iter_enabled.len();
            assert_eq!(
                &original[..common_len],
                &from_iter_enabled[..],
                "original: {:?}, original ranges: {:?}, from_iter: {:?}",
                packets.counts,
                packets.ranges().collect::<Vec<_>>(),
                from_iter.counts
            );
            assert!(
                original[common_len..].iter().all(|&v| v),
                "trailing elements should all be enabled; original: {:?}, from_iter: {:?}",
                packets.counts,
                from_iter.counts
            );
        });
}

#[test]
fn lost_flow_increase() {
    bolero::check!()
        .with_type::<DroppedPackets>()
        .with_test_time(core::time::Duration::from_secs(30))
        .with_shrink_time(core::time::Duration::from_secs(10))
        .cloned()
        .for_each(|packets| {
            packets.sim();
        });
}

/// Fuzzes all packet-loss patterns to find inputs that stall or severely degrade BBRv2.
///
/// For each generated [`DroppedPackets`] value bolero produces, a 256 KiB echo
/// transfer is run in the deterministic simulator. The transfer must complete
/// within the 30-second simulated timeout inside [`DroppedPackets::sim_with_timing`].
/// If bolero discovers a pattern that exceeds that bound it will shrink it to the
/// minimal failing case, pointing directly at the problematic loss sequence.
#[test]
fn transmission_rate_fuzz() {
    bolero::check!()
        .with_type::<DroppedPackets>()
        .with_test_time(core::time::Duration::from_secs(30))
        .with_shrink_time(core::time::Duration::from_secs(10))
        .cloned()
        .for_each(|packets| {
            let _elapsed = packets.sim_with_timing();
        });
}

/// Measures total simulated time for a clean 256 KiB round-trip echo with no packet loss.
///
/// This is the baseline scenario. The snapshot locks in the expected transfer
/// duration so that any regression (e.g., caused by changing ACK thresholds or
/// BBRv2 pacing parameters) is immediately visible as a snapshot diff.
#[test]
fn no_loss_timing() {
    let elapsed = DroppedPackets { counts: vec![] }.sim_with_timing();
    insta::assert_snapshot!(format!("{elapsed:?}"));
}

/// Measures total simulated time for a 256 KiB round-trip echo under heavy initial loss.
///
/// Uses the same loss pattern as the [`initial_loss`] test. The snapshot locks in
/// the expected recovery time so changes to retransmission or BBRv2 startup
/// behaviour are surfaced as a snapshot diff.
#[test]
fn initial_loss_timing() {
    let elapsed = DroppedPackets::from_iter([
        1..10,
        10..20,
        20..29,
        29..34,
        38..45,
        52..54,
        61..66,
        67..71,
        78..86,
        91..97,
        97..100,
        100..102,
        113..117,
        121..124,
        129..130,
        138..141,
    ])
    .sim_with_timing();
    insta::assert_snapshot!(format!("{elapsed:?}"));
}

/// Measures total simulated time for a 256 KiB round-trip echo under sporadic loss.
///
/// Uses the same loss pattern as the [`sporadic_loss`] test (~49% loss rate).
/// The snapshot locks in the expected transfer duration; if ACK thresholds,
/// BBRv2 pacing, or retransmission heuristics are changed the diff will show
/// exactly how the end-to-end time is affected.
#[test]
fn sporadic_loss_timing() {
    let elapsed = DroppedPackets::from_iter([
        10..12,
        22..27,
        28..32,
        40..45,
        54..61,
        63..66,
        75..77,
        81..84,
        89..98,
        108..116,
        121..127,
        129..131,
        136..137,
        143..152,
        153..160,
        163..172,
        181..183,
        185..192,
        199..201,
        202..212,
        213..219,
        227..236,
        238..248,
        275..283,
        284..290,
    ])
    .sim_with_timing();
    insta::assert_snapshot!(format!("{elapsed:?}"));
}

/// Classifies a raw UDP payload by its first byte (tag byte) into a packet category.
fn classify_packet(payload: &[u8]) -> &'static str {
    match payload.first().copied() {
        Some(0x00..=0x3F) => "stream",
        Some(0x40..=0x4F) => "datagram",
        Some(0x50..=0x5F) => "control",
        Some(0x60..=0x67) => "secret_control",
        _ => "other",
    }
}

/// Tracks packet counts by type across the simulation using atomic counters.
#[derive(Clone, Default)]
struct PacketCounts {
    stream: Arc<AtomicU64>,
    control: Arc<AtomicU64>,
    other: Arc<AtomicU64>,
}

impl PacketCounts {
    fn record(&self, payload: &[u8]) {
        match classify_packet(payload) {
            "stream" => self.stream.fetch_add(1, Ordering::Relaxed),
            "control" => self.control.fetch_add(1, Ordering::Relaxed),
            _ => self.other.fetch_add(1, Ordering::Relaxed),
        };
    }

    fn stream(&self) -> u64 {
        self.stream.load(Ordering::Relaxed)
    }

    fn control(&self) -> u64 {
        self.control.load(Ordering::Relaxed)
    }

    fn control_to_stream_ratio(&self) -> f64 {
        let stream = self.stream() as f64;
        let control = self.control() as f64;
        if stream == 0.0 {
            return 0.0;
        }
        control / stream
    }
}

impl core::fmt::Display for PacketCounts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "stream={}, control={}, other={}, control/stream ratio={:.3}",
            self.stream(),
            self.control(),
            self.other.load(Ordering::Relaxed),
            self.control_to_stream_ratio(),
        )
    }
}

/// Asserts that during a bulk unidirectional transfer, the ratio of control packets
/// (ACKs) to stream packets stays within a reasonable bound.
///
/// In production we observed a ~1.5x control-to-stream ratio, which is far too high.
/// Ideally the receiver should ACK much less frequently than one ACK per stream packet,
/// targeting something around 0.5x or lower.
#[test]
fn bulk_transfer_control_packet_ratio() {
    let counts = PacketCounts::default();
    let monitor_counts = counts.clone();

    sim(|| {
        {
            let counts = monitor_counts.clone();
            bach::net::monitor::on_packet_sent(move |packet| {
                let payload = packet.transport.payload();
                if !payload.is_empty() {
                    counts.record(payload);
                }
                bach::net::monitor::Command::Pass
            });
        }

        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            // Send a large bulk transfer (1 MiB) to generate enough packets for
            // meaningful ratio measurement
            let body = vec![42u8; 1 << 20];
            stream.write_all(&body).await.unwrap();
            stream.shutdown().await.unwrap();

            // Wait for all ACKs to flush
            let mut response = vec![];
            stream.read_to_end(&mut response).await.unwrap();
        }
        .group("client")
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, _addr)) = server.accept().await {
                spawn(async move {
                    let mut request = vec![];
                    stream.read_to_end(&mut request).await.unwrap();
                    // echo back empty response to let client's read_to_end complete
                    let _ = stream.shutdown().await;
                });
            }
        }
        .group("server")
        .spawn();
    });

    let ratio = counts.control_to_stream_ratio();
    eprintln!("bulk_transfer_control_packet_ratio: {counts}");

    // We expect the control-to-stream ratio to stay well below 1.0.
    // A ratio of 1.5 means the receiver sends 1.5 ACKs per stream packet, which
    // floods the TX path with control packets. We target <= 0.5 for a healthy ratio.
    assert!(
        counts.stream() > 0,
        "expected stream packets to be sent; {counts}"
    );
    assert!(
        counts.control() > 0,
        "expected control packets to be sent; {counts}"
    );
    assert!(
        ratio <= 0.5,
        "control-to-stream packet ratio is too high: {ratio:.3} (expected <= 0.5); {counts}"
    );
}
