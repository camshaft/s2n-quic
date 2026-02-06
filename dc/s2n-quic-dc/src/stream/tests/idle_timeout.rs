// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    stream::testing::{Client, Server},
    testing::{ext::*, sim},
};
use s2n_quic_core::{buffer::reader::Storage, stream::testing::Data};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info_span, Instrument};

#[test]
fn other_half_keep_alive() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            for _ in 0..120 {
                stream.write_all(b"ping").await.unwrap();
                1.s().sleep().await;
            }
            stream.shutdown().await.unwrap();

            let mut response = vec![];
            stream.read_to_end(&mut response).await.unwrap();

            assert_eq!(response, b"pong!"[..]);
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, peer_addr)) = server.accept().await {
                async move {
                    let mut request = vec![];
                    stream.read_to_end(&mut request).await.unwrap();

                    stream.write_from_fin(&mut &b"pong!"[..]).await.unwrap();
                }
                .instrument(info_span!("stream", ?peer_addr))
                .primary()
                .spawn();
            }
        }
        .group("server")
        .instrument(info_span!("server"))
        .spawn();
    });
}

#[test]
fn server_no_response() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            stream.write_all(b"ping").await.unwrap();
            stream.shutdown().await.unwrap();

            let mut response = vec![];
            stream.read_to_end(&mut response).await.unwrap_err();
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, peer_addr)) = server.accept().await {
                async move {
                    let mut request = vec![];
                    stream.read_to_end(&mut request).await.unwrap();

                    // sleep long enough to trigger the idle timer
                    120.s().sleep().await;
                }
                .instrument(info_span!("stream", ?peer_addr))
                .primary()
                .spawn();
            }
        }
        .group("server")
        .instrument(info_span!("server"))
        .spawn();
    });
}

/// Test to reproduce the idle timeout issue when sender is blocked on flow control.
///
/// This test verifies that when:
/// 1. The server sends enough data to exceed the receiver's flow control window
/// 2. The receiver doesn't read from the stream
/// 3. The sender gets blocked and should send DATA_BLOCKED frames
///
/// The stream should NOT timeout as long as the peer is responding (sending ACKs for DATA_BLOCKED).
#[test]
fn flow_control_blocked_sender_idle_timeout() {
    use std::sync::atomic::Ordering;

    sim(|| {
        async move {
            let client = Client::builder().build();
            let client_subscriber = client.subscriber();
            
            let mut stream = client.connect_sim("server:443").await.unwrap();

            // Send a small request to simulate a request+response pattern
            stream.write_all(b"request").await.unwrap();
            stream.shutdown().await.unwrap();
            
            tracing::info!("Client sent request, now waiting without reading to test flow control block...");
            
            // Wait 10 minutes to observe MAX_DATA frame behavior over time
            // Should see one MAX_DATA frame every 15s (half the 30s idle timeout)
            600.s().sleep().await;

            // Read the entire stream and validate correctness
            let mut received_data = Data::new(1_000_000);
            stream.read_into(&mut received_data).await
                .expect("Stream should not timeout when sender is blocked on flow control");
            
            tracing::info!("Read succeeded - stream stayed alive and data is correct!");

            // Check that we received control packets with MAX_DATA frames from the sender
            let control_packets_received = client_subscriber
                .stream_control_packet_received
                .load(Ordering::Relaxed);
            let max_data_received = client_subscriber
                .stream_max_data_received
                .load(Ordering::Relaxed);
            let max_data_transmitted = client_subscriber
                .stream_max_data_transmitted
                .load(Ordering::Relaxed);
            
            tracing::info!(
                control_packets_received,
                max_data_received,
                max_data_transmitted,
                "Client stats after 10 minute wait"
            );
            
            // We should be seeing MAX_DATA frames every 15s (half idle timeout)
            // In 10 minutes (600s), that's 600/15 = 40 frames
            // The receiver (client) transmits MAX_DATA frames, so check max_data_transmitted
            let expected_min_frames = 35; // Allow some margin
            assert!(
                max_data_transmitted >= expected_min_frames,
                "Expected at least {} MAX_DATA frames transmitted (one every 15s), got {}. This is a BUG!",
                expected_min_frames,
                max_data_transmitted
            );
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();
            let server_subscriber = server.subscriber();

            while let Ok((mut stream, peer_addr)) = server.accept().await {
                let server_subscriber = server_subscriber.clone();
                async move {
                    // Read the request first
                    let mut request = vec![];
                    stream.read_to_end(&mut request).await.unwrap();
                    tracing::info!("Server received request, now sending large response");

                    // Send a large amount of data to exceed flow control window
                    // Use Data type to avoid allocating everything up front
                    // Send 1MB to ensure we hit flow control limits
                    let mut data = Data::new(1_000_000);
                    
                    tracing::info!("Server attempting to send {} bytes...", data.buffered_len());
                    
                    stream.write_from_fin(&mut data).await
                        .expect("Server write should succeed");
                    
                    let control_packets_transmitted = server_subscriber
                        .stream_control_packet_transmitted
                        .load(Ordering::Relaxed);
                    
                    tracing::info!(
                        control_packets_transmitted,
                        "Server transmitted {} control packets",
                        control_packets_transmitted
                    );
                    
                    tracing::info!("Server write completed");
                }
                .instrument(info_span!("stream", ?peer_addr))
                .primary()
                .spawn();
            }
        }
        .group("server")
        .instrument(info_span!("server"))
        .spawn();
    });
}


/// Test to verify MAX_DATA frames are retransmitted when lost.
///
/// This test uses bach network monitor to drop packets containing MAX_DATA frames.
/// MAX_DATA is sent by the receiver (client) to the sender (server) to grant flow control credits.
/// If MAX_DATA frames are not reliably retransmitted, the sender will remain blocked
/// and eventually timeout.
#[test]
fn flow_control_max_data_packet_loss() {
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::atomic::Ordering as StdOrdering;
    
    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
    DROP_COUNT.store(0, Ordering::Relaxed);

    sim(|| {
        // Drop packets from client containing MAX_DATA to test retransmission
        // Client sends MAX_DATA to server (port 443) to grant flow control credits
        ::bach::net::monitor::on_packet_sent(move |packet| {
            use ::bach::net::monitor::Command;
            
            // Only look at packets FROM client TO server (destination port 443)
            if packet.destination().port() == 443 {
                let payload = packet.transport.payload();
                if payload.len() > 0 {
                    let tag = payload[0];
                    
                    // Check for Control packets (0x50-0x5F) which contain MAX_DATA frames
                    let is_control = (tag & 0xF0) == 0x50;
                    
                    // Also check for Stream packets (0x00-0x3F) with control data flag (0x08)
                    let is_stream = (tag & 0xC0) == 0;
                    let has_control_data = (tag & 0x08) != 0;
                    
                    // Drop every other packet containing MAX_DATA to simulate packet loss
                    // while still allowing some through for progress
                    if is_control || (is_stream && has_control_data) {
                        let count = DROP_COUNT.fetch_add(1, Ordering::Relaxed);
                        // Drop every other packet
                        if count % 2 == 0 {
                            tracing::info!(count, tag, is_control, has_control_data, "Dropping packet with MAX_DATA");
                            return Command::Drop;
                        }
                    }
                }
            }
            
            Command::Pass
        });
        
        async move {
            let client = Client::builder().build();
            let client_subscriber = client.subscriber();
            
            let mut stream = client.connect_sim("server:443").await.unwrap();

            // Send a small request
            stream.write_all(b"request").await.unwrap();
            stream.shutdown().await.unwrap();
            
            tracing::info!("Client sent request, waiting to see if MAX_DATA is retransmitted...");
            
            // Wait 2 minutes - if MAX_DATA isn't retransmitted, stream will timeout
            120.s().sleep().await;

            let max_data_transmitted = client_subscriber
                .stream_max_data_transmitted
                .load(StdOrdering::Relaxed);
            let dropped_packets = DROP_COUNT.load(Ordering::Relaxed);
            
            tracing::info!(
                max_data_transmitted,
                dropped_packets,
                "Stats after 2 minutes with packet loss"
            );

            // Try to read - will fail if MAX_DATA frames weren't retransmitted
            let mut received_data = Data::new(1_000_000);
            stream.read_into(&mut received_data).await
                .expect("Stream should not timeout - MAX_DATA frames should be retransmitted when lost");
            
            tracing::info!("SUCCESS: Stream stayed alive despite packet loss!");
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, peer_addr)) = server.accept().await {
                async move {
                    let mut request = vec![];
                    stream.read_to_end(&mut request).await.unwrap();
                    tracing::info!("Server sending 1MB response");

                    let mut data = Data::new(1_000_000);
                    stream.write_from_fin(&mut data).await
                        .expect("Server write should succeed");
                }
                .instrument(info_span!("stream", ?peer_addr))
                .primary()
                .spawn();
            }
        }
        .group("server")
        .instrument(info_span!("server"))
        .spawn();
    });
    
    let total_encountered = DROP_COUNT.load(Ordering::Relaxed);
    tracing::info!(total_encountered, "Total packets with MAX_DATA encountered");
    
    // We should have encountered some packets containing MAX_DATA
    assert!(total_encountered > 0, "Test should have seen at least one packet containing MAX_DATA");
}
