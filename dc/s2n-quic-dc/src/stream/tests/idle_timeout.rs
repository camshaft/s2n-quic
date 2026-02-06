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
            
            // Wait longer than the default idle timeout (30s) to see if stream times out
            60.s().sleep().await;

            // Check that we received control packets with MAX_DATA frames from the sender
            let control_packets_received = client_subscriber
                .stream_control_packet_received
                .load(Ordering::Relaxed);
            let max_data_received = client_subscriber
                .stream_max_data_received
                .load(Ordering::Relaxed);
            
            tracing::info!(
                control_packets_received,
                max_data_received,
                "Client stats after 60s wait"
            );
            
            // If we're flow control blocked, we should be seeing control packets
            // being sent by the sender to communicate its blocked state
            assert!(
                control_packets_received > 0,
                "Expected to receive control packets from sender when flow control blocked, got {}",
                control_packets_received
            );

            // Try to read - if the stream timed out, this should error
            let mut buf = vec![0u8; 100];
            stream.read(&mut buf).await
                .expect("Stream should not timeout when sender is blocked on flow control");
            
            tracing::info!("Read succeeded - stream stayed alive!");
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

