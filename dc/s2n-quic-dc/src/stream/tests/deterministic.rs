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
                    // the first stream is thrown away to get the server to drop its
                    // state. We must send at least one packet so the server actually
                    // accepts the stream and calls drop_state().
                    let mut stream = stream;
                    stream.write_all(b"hello").await.unwrap();
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
                tracing::info!(packets = ?self, loss = format!("{:.02}%", self.loss_percent()), "starting test");
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

                    crate::testing::timeout(TRANSFER_TIMEOUT, async {
                        stream.write_all(&body).await.unwrap();
                        stream.shutdown().await.unwrap();

                        let mut response = Vec::with_capacity(body_len);
                        stream.read_to_end(&mut response).await.unwrap();
                        assert_eq!(response, body);
                    })
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
        .with_shrink_time(core::time::Duration::from_secs(0))
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
            let elapsed = packets.sim(1 << 18);

            // Duration bound that scales quadratically with loss rate:
            //   0% loss  → 1 s max
            //   50% loss → ~8 s max
            //   100% loss → 30 s max (also enforced by the transfer timeout)
            let max_secs = 1.0_f64 + (loss / 100.0).powi(2) * 29.0;
            let max_secs = max_secs.min(30.0);
            let max_allowed = Duration::from_secs_f64(max_secs);
            assert!(
                elapsed <= max_allowed,
                "transfer took too long for {loss:.1}% loss: {elapsed:?} (max allowed: {max_allowed:?})"
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

/// Reproduces a scenario where streams get stuck in a perpetual retransmission
/// loop under sustained bidirectional packet loss.
///
/// The root cause: `make_stream_packets_as_pto_probes` only checked
/// `sent_stream_packets` when generating PTO probes.  Once all original stream
/// packets were ACKed — but some recovery packets carrying still-unacked data
/// remained in flight — PTO could only produce empty, payload-less probes.  The
/// recovery packets that got lost could not be re-retransmitted through the
/// normal loss-detection path (because their descriptors hadn't arrived from the
/// completion queue yet at the time of the loss event), and PTO couldn't help
/// because it never looked at `sent_recovery_packets`.  This caused the sender
/// to loop endlessly sending empty probes while the receiver waited for data
/// that would never arrive.
///
/// The fix extends `make_stream_packets_as_pto_probes` to fall back to
/// `sent_recovery_packets` when no stream packets are available, so PTO can
/// produce data-carrying probes from in-flight recovery packets.
#[test]
fn retransmission_loop_under_loss() {
    let end_time: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let end_time_inner = end_time.clone();

    sim(|| {
        // Apply sustained ~33% BIDIRECTIONAL packet loss.
        // Unlike the DroppedPackets infrastructure which only drops server→client
        // packets (ACKs), this drops packets in BOTH directions. This is critical
        // because:
        // - Client→server loss means the receiver gets gaps → triggers retransmissions
        // - Server→client loss means ACKs get lost → sender can't confirm delivery
        // - Recovery packets (retransmissions) ALSO get lost
        // This creates the conditions for the retransmission feedback loop.
        {
            let mut count = 0u64;
            bach::net::monitor::on_packet_sent(move |_packet| {
                count += 1;
                // Drop every 3rd packet in both directions
                if count % 3 == 0 {
                    bach::net::monitor::Command::Drop
                } else {
                    bach::net::monitor::Command::Pass
                }
            });
        }

        {
            let end_time = end_time_inner.clone();
            async move {
                let client = Client::builder().build();
                let mut stream = client.connect_sim("server:443").await.unwrap();

                let body = vec![42u8; 1 << 18]; // 256 KiB

                let result = crate::testing::timeout(Duration::from_secs(300), async {
                    stream.write_all(&body).await.unwrap();
                    stream.shutdown().await.unwrap();

                    let mut response = Vec::with_capacity(body.len());
                    stream.read_to_end(&mut response).await.unwrap();
                    assert_eq!(response, body);
                })
                .await;

                *end_time.lock().unwrap() = Some(Instant::now());
                result.expect(
                    "transfer timed out under 33% bidirectional loss — \
                     likely stuck in retransmission loop",
                );
            }
            .group("client")
            .primary()
            .spawn();
        }

        async move {
            let server = Server::udp().port(443).build();

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
    });

    let elapsed = end_time.lock().unwrap().unwrap().elapsed_since_start();

    // With ~33% bidirectional loss and 256 KiB data, the transfer should still
    // complete well within 30s. Without the fix, it times out.
    let max_allowed = Duration::from_secs(15);
    assert!(
        elapsed <= max_allowed,
        "transfer took too long with ~33% bidirectional loss: {elapsed:?} \
         (max allowed: {max_allowed:?}); this suggests a retransmission feedback loop"
    );
}

/// Validates that dropping the receiver after reading a small header promptly stops
/// the sender from transmitting more data.
///
/// This simulates a production pattern where a client races reads against multiple
/// replicas: it reads a small header from each response, picks one winner, and drops
/// the rest. Dropped streams must stop the sender promptly to avoid wasting bandwidth.
///
/// The test measures the total stream-packet bytes sent over the wire from the server
/// after the client drops its reader. The sender should observe the cancellation
/// (via the receiver's connection-close control packet) within a small number of RTTs
/// and stop transmitting.
#[test]
fn recv_cancel_stops_sender() {
    /// Total bytes the server attempts to write — intentionally large so we can
    /// detect if the sender keeps going after cancellation.
    const TOTAL_PAYLOAD: usize = 1 << 20; // 1 MiB

    /// Number of header bytes the client reads before dropping.
    const HEADER_LEN: usize = 64;

    let stream_bytes_from_server = Arc::new(AtomicU64::new(0));
    let monitor_bytes = stream_bytes_from_server.clone();

    sim(|| {
        {
            let bytes = monitor_bytes.clone();
            bach::net::monitor::on_packet_sent(move |packet| {
                // Only count stream packets from the server (port 443)
                if packet.source().port() == 443 {
                    let payload = packet.transport.payload();
                    if !payload.is_empty() && classify_packet(payload) == "stream" {
                        bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);
                    }
                }
                bach::net::monitor::Command::Pass
            });
        }

        async move {
            let client = Client::builder().build();
            let stream = client.connect_sim("server:443").await.unwrap();

            let (mut reader, writer) = stream.into_split();
            // Drop write half — we only care about the read direction
            drop(writer);

            // Read just the header
            let mut header = vec![0u8; HEADER_LEN];
            let mut total_read = 0;
            while total_read < HEADER_LEN {
                match reader.read(&mut header[total_read..]).await {
                    Ok(0) => break,
                    Ok(n) => total_read += n,
                    Err(_) => break,
                }
            }

            assert!(
                total_read > 0,
                "client should have read at least some header bytes"
            );

            // Drop the reader — this triggers stop_sending / connection-close back
            // to the sender, which should stop it from transmitting more data.
            drop(reader);

            // Give the simulation time for the cancellation to propagate and the
            // sender to observe it. A few seconds of simulated time is generous.
            5.s().sleep().await;
        }
        .group("client")
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, _addr)) = server.accept().await {
                spawn(async move {
                    let body = vec![0xABu8; TOTAL_PAYLOAD];
                    // The sender will write until it gets an error from the cancelled receiver
                    let _ = stream.write_all(&body).await;
                });
            }
        }
        .group("server")
        .spawn();
    });

    let total_wire_bytes = stream_bytes_from_server.load(Ordering::Relaxed);

    eprintln!(
        "recv_cancel_stops_sender: server sent {total_wire_bytes} stream bytes on the wire \
         (payload was {TOTAL_PAYLOAD} bytes, client read {HEADER_LEN} header bytes)"
    );

    // The sender should have stopped well before transmitting the entire payload.
    // With a 1ms RTT, the cancellation should propagate within a few RTTs. We allow
    // a generous budget but it must be far less than the full payload.
    //
    // Use a snapshot so any regression in cancellation responsiveness is immediately
    // visible as a diff.
    insta::assert_snapshot!(format!(
        "server_stream_bytes={total_wire_bytes}, payload={TOTAL_PAYLOAD}, header={HEADER_LEN}"
    ));
}

/// Same as `recv_cancel_stops_sender` but races 3 concurrent streams — one is kept
/// alive while the other two are cancelled after reading a header. Validates that
/// cancelled senders stop promptly while the winner continues normally.
#[test]
fn racing_receivers_cancel() {
    const TOTAL_PAYLOAD: usize = 1 << 20; // 1 MiB
    const HEADER_LEN: usize = 128;

    // Track stream bytes per server port. Servers use ports 443, 444, 445.
    let bytes_per_port: [Arc<AtomicU64>; 3] = [
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
    ];
    let monitor_bytes = bytes_per_port.clone();

    sim(|| {
        {
            let bytes = monitor_bytes.clone();
            bach::net::monitor::on_packet_sent(move |packet| {
                let port = packet.source().port();
                let idx = match port {
                    443 => Some(0),
                    444 => Some(1),
                    445 => Some(2),
                    _ => None,
                };
                if let Some(idx) = idx {
                    let payload = packet.transport.payload();
                    if !payload.is_empty() && classify_packet(payload) == "stream" {
                        bytes[idx].fetch_add(payload.len() as u64, Ordering::Relaxed);
                    }
                }
                bach::net::monitor::Command::Pass
            });
        }

        async move {
            let client = Client::builder().build();

            let mut readers = Vec::new();
            let mut writers = Vec::new();

            for port in [443u16, 444, 445] {
                let addr = format!("server:{port}");
                let stream = client.connect_sim(&addr).await.unwrap();
                let (reader, writer) = stream.into_split();
                readers.push(reader);
                writers.push(writer);
            }

            // Drop all write halves
            drop(writers);

            // Read a header from each stream
            for reader in &mut readers {
                let mut header = vec![0u8; HEADER_LEN];
                let mut total_read = 0;
                while total_read < HEADER_LEN {
                    match reader.read(&mut header[total_read..]).await {
                        Ok(0) => break,
                        Ok(n) => total_read += n,
                        Err(_) => break,
                    }
                }
                assert!(total_read > 0, "should read at least some header bytes");
            }

            // "Pick" the first reader (winner), drop the other two (losers)
            let mut winner = readers.remove(0);
            for loser in readers {
                drop(loser);
            }

            // Let cancellation propagate
            2.s().sleep().await;

            // The winner should still be able to read the remaining payload
            // (we already consumed HEADER_LEN bytes above)
            let mut response = vec![];
            winner.read_to_end(&mut response).await.unwrap();
            assert_eq!(response.len() + HEADER_LEN, TOTAL_PAYLOAD);
            drop(winner);

            // Let everything settle
            2.s().sleep().await;
        }
        .group("client")
        .primary()
        .spawn();

        for port in [443u16, 444, 445] {
            async move {
                let server = Server::udp().port(port).build();

                while let Ok((mut stream, _addr)) = server.accept().await {
                    spawn(async move {
                        let body = vec![0xCDu8; TOTAL_PAYLOAD];
                        let _ = stream.write_all(&body).await;
                        let _ = stream.shutdown().await;
                    });
                }
            }
            .group("server")
            .spawn();
        }
    });

    let winner_bytes = bytes_per_port[0].load(Ordering::Relaxed);
    let loser1_bytes = bytes_per_port[1].load(Ordering::Relaxed);
    let loser2_bytes = bytes_per_port[2].load(Ordering::Relaxed);

    eprintln!(
        "racing_receivers_cancel: winner={winner_bytes}, loser1={loser1_bytes}, \
         loser2={loser2_bytes} stream bytes (payload={TOTAL_PAYLOAD})"
    );

    // The winner must have sent the full payload (plus overhead)
    assert!(
        winner_bytes >= TOTAL_PAYLOAD as u64,
        "winner should have sent at least the full payload: {winner_bytes} < {TOTAL_PAYLOAD}"
    );

    insta::assert_snapshot!(format!(
        "winner_bytes={winner_bytes}, loser1_bytes={loser1_bytes}, loser2_bytes={loser2_bytes}, \
         payload={TOTAL_PAYLOAD}, header={HEADER_LEN}"
    ));
}

/// Reproduces a scenario where a stream gets stuck (StreamStuck error) because
/// the receiver's application panics mid-transfer.
///
/// **The stuck scenario in production:**
///
/// 1. Client sends a large request to the server.
/// 2. Server application panics while processing the stream.
/// 3. Server's Reader and Writer drop abruptly (no FIN, no graceful shutdown):
///    - Writer drops with `is_panicking: true` → FIN is skipped
///    - `ShutdownKind::Errored` sends an `ApplicationError` CONNECTION_CLOSE
///    - Recv worker enters **draining state**, throttling ACKs
/// 4. The draining recv worker still periodically ACKs the client's stream data,
///    which counts as **peer activity** and keeps the client's **idle timer** alive.
/// 5. However, if the CONNECTION_CLOSE packet is lost (due to packet loss), the
///    client sender never learns the server errored out.
/// 6. The client's sender keeps retransmitting data. The draining ACKs keep the
///    idle timer refreshed, but the `min_unacked` offset may not advance if the
///    draining-state ACKs don't carry new acknowledgments for the lowest offsets.
/// 7. After `idle_timeout * 2` (60s), the progress watchdog fires **StreamStuck**.
///
/// This test models the scenario: the server reads a partial request then abruptly
/// drops the stream (simulating a panic), while selective packet loss ensures the
/// error notification doesn't reach the client reliably. The client should detect
/// the failure and terminate cleanly rather than getting stuck.
#[test]
fn stream_stuck_on_receiver_panic() {
    const BODY_LEN: usize = 1 << 18; // 256 KiB
                                     // The progress watchdog fires at idle_timeout * 2 = 60s. Use a timeout
                                     // that's long enough to observe it.
    const TRANSFER_TIMEOUT: Duration = Duration::from_secs(90);

    let end_time: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let end_time_inner = end_time.clone();

    let stuck_detected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stuck_flag = stuck_detected.clone();

    sim(|| {
        // Drop only the first few control packets from the server. These
        // carry the CONNECTION_CLOSE (error notification). Later control
        // packets are draining-state ACKs which keep flowing to the client.
        //
        // This creates the exact production scenario:
        // - CONNECTION_CLOSE is lost → client sender doesn't know the server errored
        // - Draining ACKs keep arriving → peer activity keeps idle timer alive
        // - But if draining ACKs don't advance min_unacked (e.g., server only
        //   received partial data before panic), the progress watchdog fires
        {
            let mut server_control_count = 0u64;
            bach::net::monitor::on_packet_sent(move |packet| {
                let payload = packet.transport.payload();
                let is_from_server = packet.source().port() == 443;

                if is_from_server && !payload.is_empty() {
                    if classify_packet(payload) == "control" {
                        server_control_count += 1;
                        // Drop first 2 control packets — these carry the
                        // CONNECTION_CLOSE. Let later draining ACKs through.
                        if server_control_count <= 2 {
                            return bach::net::monitor::Command::Drop;
                        }
                    }
                }

                bach::net::monitor::Command::Pass
            });
        }

        {
            let end_time = end_time_inner.clone();
            let stuck_flag = stuck_flag.clone();
            async move {
                let client = Client::builder().build();
                let mut stream = client.connect_sim("server:443").await.unwrap();

                let body = vec![42u8; BODY_LEN];

                let result = crate::testing::timeout(TRANSFER_TIMEOUT, async {
                    // The write and shutdown may or may not succeed depending on
                    // timing — the server may drop before we finish writing.
                    let _ = stream.write_all(&body).await;
                    let _ = stream.shutdown().await;

                    let mut response = Vec::with_capacity(BODY_LEN);
                    // This will error because the server panicked and dropped
                    // the stream. The important thing is that it errors within
                    // a reasonable time rather than hanging forever.
                    let _ = stream.read_to_end(&mut response).await;
                })
                .await;

                *end_time.lock().unwrap() = Some(Instant::now());

                if result.is_err() {
                    // Transfer timed out — likely hit StreamStuck
                    stuck_flag.store(true, Ordering::Relaxed);
                }
            }
            .group("client")
            .primary()
            .spawn();
        }

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((stream, _addr)) = server.accept().await {
                spawn(async move {
                    let (mut recv, send) = stream.into_split();

                    // Read only a small portion of the request, then "panic" by
                    // dropping both halves abruptly without shutdown.
                    let mut partial = vec![0u8; 1024];
                    let _ = recv.read(&mut partial).await;

                    // Simulate panic: drop both halves without calling shutdown
                    // or writing a FIN. This triggers ShutdownKind::Errored on
                    // both the recv and send workers.
                    drop(send);
                    drop(recv);
                });
            }
        }
        .group("server")
        .spawn();
    });

    let elapsed = end_time
        .lock()
        .unwrap()
        .map(|t| t.elapsed_since_start())
        .unwrap_or(Duration::from_secs(0));

    let was_stuck = stuck_detected.load(Ordering::Relaxed);

    eprintln!("stream_stuck_on_receiver_panic: elapsed={elapsed:?}, stuck={was_stuck}");

    // When the server panics and drops the stream, it sends a CONNECTION_CLOSE.
    // If those first packets are lost, the server's send worker currently exits
    // immediately via on_error → clear_inflight_state → Finished, without
    // retransmitting the CONNECTION_CLOSE. The client then hangs until IdleTimeout
    // at 30s — far too long.
    //
    // The server should retransmit the CONNECTION_CLOSE a few times before giving
    // up, so the client learns about the failure within a few RTTs (< 1s).
    let max_allowed = Duration::from_secs(1);
    assert!(
        !was_stuck && elapsed <= max_allowed,
        "stream took too long to detect receiver panic: elapsed={elapsed:?}, \
         was_stuck={was_stuck}; the server's CONNECTION_CLOSE was likely lost \
         and not retransmitted, causing the client to wait for IdleTimeout"
    );
}

/// Same as `stream_stuck_on_receiver_panic` but with the panic happening on the
/// sender side (server panics while writing the response). This tests the
/// complementary scenario where the sender panics and the receiver gets stuck.
#[test]
fn stream_stuck_on_sender_panic() {
    const BODY_LEN: usize = 1 << 18; // 256 KiB
    const TRANSFER_TIMEOUT: Duration = Duration::from_secs(90);

    let end_time: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let end_time_inner = end_time.clone();

    let stuck_detected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stuck_flag = stuck_detected.clone();

    sim(|| {
        // Drop control packets from the server in a pattern that loses the
        // initial CONNECTION_CLOSE but allows later retransmissions through.
        {
            let mut server_control_count = 0u64;
            let mut total_count = 0u64;
            bach::net::monitor::on_packet_sent(move |packet| {
                total_count += 1;
                let payload = packet.transport.payload();
                let is_from_server = packet.source().port() == 443;

                if is_from_server && !payload.is_empty() {
                    if classify_packet(payload) == "control" {
                        server_control_count += 1;
                        // Drop first 8 control packets — this includes the
                        // CONNECTION_CLOSE and early retransmissions.
                        if server_control_count <= 8 {
                            return bach::net::monitor::Command::Drop;
                        }
                    }
                }

                // Light uniform loss
                if total_count % 13 == 0 {
                    return bach::net::monitor::Command::Drop;
                }

                bach::net::monitor::Command::Pass
            });
        }

        {
            let end_time = end_time_inner.clone();
            let stuck_flag = stuck_flag.clone();
            async move {
                let client = Client::builder().build();
                let stream = client.connect_sim("server:443").await.unwrap();
                let (mut recv, mut send) = stream.into_split();

                let body = vec![42u8; BODY_LEN];

                let result = crate::testing::timeout(TRANSFER_TIMEOUT, async {
                    // Send the request
                    send.write_all(&body).await.unwrap();
                    send.shutdown().unwrap();
                    drop(send);

                    // Try to read the response — server will panic mid-write
                    let mut response = vec![];
                    let _ = recv.read_to_end(&mut response).await;
                })
                .await;

                *end_time.lock().unwrap() = Some(Instant::now());

                match result {
                    Ok(()) => {}
                    Err(_) => {
                        stuck_flag.store(true, Ordering::Relaxed);
                    }
                }
            }
            .group("client")
            .primary()
            .spawn();
        }

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((stream, _addr)) = server.accept().await {
                spawn(async move {
                    let (mut recv, mut send) = stream.into_split();

                    // Read the full request
                    let mut request = vec![];
                    let _ = recv.read_to_end(&mut request).await;
                    drop(recv);

                    // Start writing the response, then "panic" partway through
                    let response = vec![0xABu8; BODY_LEN];
                    let half = BODY_LEN / 2;
                    let _ = send.write_all(&response[..half]).await;

                    // Simulate panic: drop the writer without shutdown/FIN
                    drop(send);
                });
            }
        }
        .group("server")
        .spawn();
    });

    let elapsed = end_time
        .lock()
        .unwrap()
        .map(|t| t.elapsed_since_start())
        .unwrap_or(Duration::from_secs(0));

    let was_stuck = stuck_detected.load(Ordering::Relaxed);

    eprintln!("stream_stuck_on_sender_panic: elapsed={elapsed:?}, stuck={was_stuck}");

    // The client should detect the server's failure and terminate cleanly.
    let max_allowed = Duration::from_secs(10);
    assert!(
        !was_stuck && elapsed <= max_allowed,
        "stream got stuck after sender panic: elapsed={elapsed:?}, was_stuck={was_stuck}; \
         the client was likely kept alive by draining-state activity while \
         the error notification was lost"
    );
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

/// Regression test for InflightCounters bug under control packet starvation.
///
/// This test reproduces a counter tracking bug that occurred when:
/// - Stream enters terminal state via error → `clear_inflight_state()` clears counters/maps
/// - Transmission completions arrive after clearing
/// - `load_completion_queue()` processes them, inserting packets into empty maps
/// - Result: maps contain packets but counters are zero → invariant violation
///
/// The bug was fixed by adding a terminal state check in `load_completion_queue()`
/// (send/state.rs:1026) to prevent processing completions after the stream has ended.
///
/// Test scenario: 95% control packet loss + 40% stream packet loss creates conditions
/// where the stream errors out while many transmission completions are still pending.
#[test]
fn inflight_counters_control_packet_starvation() {
    const BODY_LEN: usize = 1 << 18; // 256 KiB
    const TRANSFER_TIMEOUT: Duration = Duration::from_secs(400);

    let packet_counts = PacketCounts::default();
    let monitor_counts = packet_counts.clone();

    sim(|| {
        // Selective loss: mostly drop control packets, allow most stream packets
        {
            let counts = monitor_counts.clone();
            let mut stream_count = 0u64;
            let mut control_count = 0u64;
            bach::net::monitor::on_packet_sent(move |packet| {
                let payload = packet.transport.payload();

                if !payload.is_empty() {
                    counts.record(payload);

                    let packet_type = classify_packet(payload);

                    match packet_type {
                        "control" => {
                            control_count += 1;
                            // Drop 95% of control packets
                            if control_count % 20 != 1 {
                                return bach::net::monitor::Command::Drop;
                            }
                        }
                        "stream" => {
                            stream_count += 1;
                            // Drop 40% of stream packets
                            if stream_count % 5 == 0 || stream_count % 5 == 1 {
                                return bach::net::monitor::Command::Drop;
                            }
                        }
                        _ => {}
                    }
                }

                bach::net::monitor::Command::Pass
            });
        }

        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            let body = vec![42u8; BODY_LEN];

            let result = crate::testing::timeout(TRANSFER_TIMEOUT, async {
                stream.write_all(&body).await.unwrap();
                stream.shutdown().await.unwrap();

                let mut response = vec![];
                stream.read_to_end(&mut response).await.unwrap();
                assert_eq!(response, body);
            })
            .await;

            result.expect("transfer should complete despite control packet starvation");
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

    let ratio = packet_counts.control_to_stream_ratio();
    eprintln!(
        "inflight_counters_control_packet_starvation: {}, ratio={:.3}",
        packet_counts, ratio
    );
}
