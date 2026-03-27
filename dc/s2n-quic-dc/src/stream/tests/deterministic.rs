// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    stream::testing::{Client, Server},
    testing::{ext::*, sim, spawn},
};
use bach::time::Instant;
use bytes::Bytes;
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

    /// Runs a simulation with the given packet drop pattern.
    ///
    /// Returns the total elapsed simulated time from t=0 to transfer completion.
    /// The transfer is bounded by a 30-second simulated timeout; if it exceeds
    /// that, the method panics so bolero can shrink to the minimal failing case.
    fn sim(self, body_len: usize) -> Duration {
        const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

        let end_time: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let end_time_inner = end_time.clone();

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

            {
                let end_time = end_time_inner.clone();
                async move {
                    let client = Client::builder().build();
                    let mut stream = client.connect_sim("server:443").await.unwrap();

                    let body = vec![42u8; body_len];

                    crate::testing::timeout(
                        TRANSFER_TIMEOUT,
                        async {
                            stream.write_all(&body).await.unwrap();
                            stream.shutdown().await.unwrap();

                            let mut response = Vec::with_capacity(body_len);
                            stream.read_to_end(&mut response).await.unwrap();
                            assert_eq!(response, body);
                        },
                    )
                    .await
                    .expect("transfer timed out");

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
                        let mut request = Vec::with_capacity(body_len);
                        let _ = stream.read_to_end(&mut request).await;
                        let _ = stream.write_all(&request).await;
                    });
                }
            }
            .group("server")
            .spawn();
        });

        let elapsed = { *end_time.lock().unwrap() };
        elapsed.unwrap().elapsed_since_start()
    }

    /// Runs the same packet-drop pattern against a plain s2n-quic (non-DC) connection
    /// inside the same `sim()` environment, using [`bach::net::monitor`] to apply the
    /// drop sequence.
    ///
    /// The drop pattern is applied to packets transmitted from the server (port 443),
    /// matching the semantics of [`Self::sim`].  Both simulations therefore see
    /// identical loss conditions, making their elapsed times directly comparable.
    fn sim_quic(self, body_len: usize) -> Duration {
        use s2n_quic::{
            client::Connect,
            provider::{io::bach_net::Io as BachIo, tls::default as tls},
            Client, Server,
        };
        use s2n_quic_core::crypto::tls::testing::certificates;

        const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);
        const SERVER_PORT: u16 = 443;

        let end_time: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let end_time_inner = end_time.clone();

        sim(|| {
            {
                tracing::info!(packets = ?self, loss = format!("{:.02}%", self.loss_percent()), "dropped packets (quic)");
                let mut enabled = self.enabled_iter().enumerate();
                bach::net::monitor::on_packet_sent(move |packet| {
                    if packet.source().port() == SERVER_PORT {
                        if let Some((idx, enabled)) = enabled.next() {
                            if !enabled {
                                tracing::info!(
                                    idx,
                                    len = packet.transport.payload().len(),
                                    "dropping quic packet"
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
                    // Wait until the server group has registered its virtual IP so that DNS
                    // resolution of "server" succeeds.
                    let server_addr = loop {
                        match bach::net::lookup_host(("server", SERVER_PORT)).await {
                            Ok(mut addrs) => {
                                if let Some(addr) = addrs.next() {
                                    break addr;
                                }
                            }
                            Err(_) => {}
                        }
                        bach::time::sleep(Duration::from_millis(1)).await;
                    };

                    let client_tls = tls::Client::builder()
                        .with_certificate(certificates::CERT_PEM)
                        .unwrap()
                        .build()
                        .unwrap();

                    let client = Client::builder()
                        .with_io(BachIo::new("0.0.0.0:0".parse().unwrap()))
                        .unwrap()
                        .with_tls(client_tls)
                        .unwrap()
                        .start()
                        .unwrap();

                    let connect = Connect::new(server_addr).with_server_name("localhost");
                    let mut conn = client.connect(connect).await.unwrap();
                    let mut stream = conn.open_bidirectional_stream().await.unwrap();

                    let body = Bytes::from(vec![42u8; body_len]);

                    crate::testing::timeout(
                        TRANSFER_TIMEOUT,
                        async {
                            stream.send(body.clone()).await.unwrap();
                            stream.finish().unwrap();

                            let mut received = 0usize;
                            while let Some(chunk) = stream.receive().await.unwrap() {
                                received += chunk.len();
                            }
                            assert_eq!(received, body_len);
                        },
                    )
                    .await
                    .expect("quic transfer timed out");

                    *end_time.lock().unwrap() = Some(Instant::now());
                }
                .group("client")
                .primary()
                .spawn();
            }

            async move {
                let server_tls = tls::Server::builder()
                    .with_certificate(certificates::CERT_PEM, certificates::KEY_PEM)
                    .unwrap()
                    .build()
                    .unwrap();

                let server_addr: std::net::SocketAddr =
                    ([0u8, 0, 0, 0], SERVER_PORT).into();
                let mut server = Server::builder()
                    .with_io(BachIo::new(server_addr))
                    .unwrap()
                    .with_tls(server_tls)
                    .unwrap()
                    .start()
                    .unwrap();

                while let Some(mut conn) = server.accept().await {
                    spawn(async move {
                        while let Ok(Some(mut stream)) =
                            conn.accept_bidirectional_stream().await
                        {
                            spawn(async move {
                                // Accumulate the full request then echo it back.
                                let mut buf = Vec::with_capacity(body_len);
                                while let Some(chunk) = stream.receive().await.unwrap() {
                                    buf.extend_from_slice(&chunk);
                                }
                                stream.send(buf.into()).await.unwrap();
                                let _ = stream.finish();
                            });
                        }
                    });
                }
            }
            .group("server")
            .spawn();
        });

        let elapsed = { *end_time.lock().unwrap() };
        elapsed.unwrap().elapsed_since_start()
    }
}

impl core::fmt::Debug for DroppedPackets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.ranges()).finish()
    }
}

#[test]
fn initial_loss() {
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
    .sim(1 << 18);
    insta::assert_snapshot!(format!("{elapsed:?}"));
}

#[test]
fn sporadic_loss() {
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
    .sim(1 << 18);
    insta::assert_snapshot!(format!("{elapsed:?}"));
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
            let _ = packets.sim(1 << 18);
        });
}

/// Fuzzes all packet-loss patterns and asserts that the transfer duration scales
/// proportionally with the loss rate.
///
/// For each generated [`DroppedPackets`] value bolero produces, a 256 KiB echo
/// transfer is run in the deterministic simulator. The elapsed time is then
/// compared against a bound derived from the loss percentage: at 0% loss the cap
/// is ~1 s; at 100% loss it scales up to the 30 s hard timeout. This means bolero
/// will shrink any pattern where the end-to-end time grows orders of magnitude
/// beyond what the loss rate would predict.
#[test]
fn transmission_rate_fuzz() {
    bolero::check!()
        .with_type::<DroppedPackets>()
        .with_test_time(core::time::Duration::from_secs(30))
        .with_shrink_time(core::time::Duration::from_secs(10))
        .cloned()
        .for_each(|packets| {
            let loss = packets.loss_percent();
            let elapsed_dc = packets.clone().sim(1 << 18);
            let elapsed_quic = packets.sim_quic(1 << 18);

            // Duration bound that scales quadratically with loss rate:
            //   0% loss  → 1 s max
            //   50% loss → ~8 s max
            //   100% loss → 30 s max (also enforced by the transfer timeout)
            let max_secs = 1.0_f64 + (loss / 100.0).powi(2) * 29.0;
            let max_allowed = Duration::from_secs_f64(max_secs);
            assert!(
                elapsed_dc <= max_allowed,
                "dc transfer took too long for {loss:.1}% loss: {elapsed_dc:?} (max allowed: {max_allowed:?})"
            );
            assert!(
                elapsed_quic <= max_allowed,
                "quic transfer took too long for {loss:.1}% loss: {elapsed_quic:?} (max allowed: {max_allowed:?})"
            );
        });
}

/// Measures total simulated time for a clean 256 KiB round-trip echo with no packet loss.
///
/// This is the baseline scenario. The snapshot locks in the expected transfer
/// duration so that any regression is immediately visible as a snapshot diff.
#[test]
fn no_loss() {
    let elapsed = DroppedPackets { counts: vec![] }.sim(1 << 18);
    insta::assert_snapshot!(format!("{elapsed:?}"));
}

/// Baseline no-loss transfer using plain s2n-quic inside the same bach `sim()` environment.
#[test]
fn no_loss_quic() {
    let elapsed = DroppedPackets { counts: vec![] }.sim_quic(1 << 18);
    insta::assert_snapshot!(format!("{elapsed:?}"));
}

/// Compares initial-loss transfer time: plain s2n-quic vs DC-QUIC under the same packet drops.
#[test]
fn initial_loss_quic() {
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
    .sim_quic(1 << 18);
    insta::assert_snapshot!(format!("{elapsed:?}"));
}

/// Compares sporadic-loss transfer time: plain s2n-quic vs DC-QUIC under the same packet drops.
#[test]
fn sporadic_loss_quic() {
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
    .sim_quic(1 << 18);
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
