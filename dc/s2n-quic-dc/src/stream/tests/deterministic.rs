// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    stream::testing::{Client, Server},
    testing::{ext::*, sim, spawn},
};
use bach::time::Instant;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    join,
};

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
            assert_eq!(elapsed, count * 1.ms(), "streams should fail within 1RTT");
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
                dbg!(packet.source(), packet.transport.payload().len());
                if packet.source().port() == 443 {
                    count += 1;
                    if count < 10 {
                        return bach::net::monitor::Command::Drop;
                    }
                }

                bach::net::monitor::Command::Pass
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
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();
            while let Ok((mut stream, _addr)) = server.accept().await {
                spawn(async move {
                    let mut request = Vec::with_capacity(2000);
                    let _ = stream.read_to_end(&mut request).await;
                    let _ = stream.write_all(&request).await;
                });
            }
        }
        .group("server")
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

    fn sim(self) {
        sim(|| {
            {
                tracing::info!(packets = ?self, "dropped packets");
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
            assert_eq!(
                original,
                from_iter_enabled,
                "original: {:?}, original ranges: {:?}, from_iter: {:?}",
                packets.counts,
                packets.ranges().collect::<Vec<_>>(),
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
