// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    stream::testing::{Client, Server},
    testing::{ext::*, sim},
};
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
/// 3. The sender gets blocked and should send DATA_BLOCKED frames (in theory)
///
/// The stream should NOT timeout as long as the peer is responding (sending ACKs for DATA_BLOCKED).
///
/// CURRENT STATUS: This test demonstrates that with UDP streams (which don't have flow control
/// enabled by default in dc - TransportFeatures::UDP is empty), the data is sent without flow
/// control enforcement. The test passes because:
/// - No flow control is applied to UDP streams
/// - The write completes successfully even with 100KB of data
/// - The stream stays alive past the idle timeout
///
/// To actually reproduce the flow control idle timeout issue, we would need:
/// 1. Flow control to be enabled for UDP streams, OR
/// 2. Test with TCP (but bach simulator doesn't support TCP), OR  
/// 3. A different test harness that supports flow-controlled streams
///
/// This test serves as documentation of the current behavior and can be used as a starting
/// point once flow control is implemented for dc UDP streams.
#[test]
fn flow_control_blocked_sender_idle_timeout() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            
            // Connect to UDP server (note: UDP streams don't have flow control by default)
            let mut stream = client.connect_sim("server:443").await.unwrap();

            // Important: Don't read from the stream at all
            // If flow control were enabled, this would force the server to hit flow control limits
            tracing::info!("Client connected, waiting without reading to test flow control block...");
            
            // Wait longer than the default idle timeout (30s) to see if stream times out
            60.s().sleep().await;

            // Try to read - if the stream timed out, this should error
            let mut buf = vec![0u8; 100];
            match stream.read(&mut buf).await {
                Ok(n) => {
                    tracing::info!("Read succeeded with {} bytes - stream stayed alive!", n);
                    // Currently passes because no flow control is enforced on UDP streams
                }
                Err(e) => {
                    tracing::error!("Read failed with error: {:?} - stream timed out!", e);
                    // Once flow control is implemented, if this error occurs, it indicates
                    // the bug: stream timing out even though DATA_BLOCKED frames should keep it alive
                    panic!("Stream timed out due to idle timeout: {:?}", e);
                }
            }
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            // Use UDP (TCP not supported in bach simulator)
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, peer_addr)) = server.accept().await {
                async move {
                    tracing::info!("Server accepted connection");

                    // Send a large amount of data all at once to exceed flow control window
                    // The default TEST_APPLICATION_PARAMS has remote_max_data of ~14KB
                    // We send 100KB to ensure we would hit flow control limits (if enforced)
                    let large_data = vec![0xAB; 100_000];
                    
                    tracing::info!("Server attempting to send {} bytes...", large_data.len());
                    
                    match stream.write_all(&large_data).await {
                        Ok(_) => {
                            tracing::info!("Server write_all completed");
                            stream.shutdown().await.ok();
                        }
                        Err(e) => {
                            tracing::error!("Server write failed: {:?}", e);
                        }
                    }
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

/// Test with real TCP to reproduce the flow control idle timeout issue.
///
/// This test uses TCP streams (which have flow control enabled via TransportFeatures::TCP)
/// to verify the behavior when a sender is blocked on flow control.
///
/// Expected behavior:
/// - Server sends large amount of data (exceeding flow control window)
/// - Client doesn't read, causing sender to block
/// - Sender should send DATA_BLOCKED frames
/// - Receiver should ACK those DATA_BLOCKED frames
/// - These ACKs should update the last peer activity timestamp
/// - Stream should NOT timeout as long as DATA_BLOCKED frames are being exchanged
///
/// If this test fails with an idle timeout error, it confirms the bug.
#[tokio::test]
async fn tcp_flow_control_blocked_sender_idle_timeout() {
    use crate::stream::testing::dcquic::tcp;
    use std::time::Duration;

    let ctx = tcp::Context::new().await;
    let (mut client_stream, mut server_stream) = ctx.pair().await;

    // Spawn server task that sends large amount of data
    let server_task = tokio::spawn(async move {
        tracing::info!("Server: starting to send 100KB of data");
        let large_data = vec![0xAB; 100_000];
        
        match server_stream.write_all(&large_data).await {
            Ok(_) => {
                tracing::info!("Server: write_all completed successfully");
                server_stream.shutdown().await.ok();
            }
            Err(e) => {
                tracing::error!("Server: write failed with error: {:?}", e);
            }
        }
    });

    // Client waits without reading to force flow control blocking
    tracing::info!("Client: waiting 60s without reading to test flow control");
    tokio::time::sleep(Duration::from_secs(60)).await;

    // Try to read - if stream timed out, this should error
    let mut buf = vec![0u8; 100];
    match client_stream.read(&mut buf).await {
        Ok(n) => {
            tracing::info!("Client: read succeeded with {} bytes - stream stayed alive!", n);
        }
        Err(e) => {
            tracing::error!("Client: read failed - stream timed out incorrectly: {:?}", e);
            panic!("Stream timed out due to idle timeout even though sender was blocked on flow control: {:?}", e);
        }
    }

    // Wait for server task to complete
    server_task.await.ok();
}

