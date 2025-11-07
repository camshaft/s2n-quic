// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    stream::testing::{Client, Server},
    testing::{ext::*, sim},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info_span, Instrument};

/// Test that a stream with keep alive enabled on the client side stays alive
/// while the client is idle doing expensive work (simulated by sleep).
#[test]
fn keep_alive_client_only() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            // Enable keep alive on the client stream only
            stream.keep_alive(true);

            // Send initial request
            stream.write_all(b"request").await.unwrap();

            // Client is idle for 3 minutes doing expensive work
            // Keep alive should maintain the connection
            180.s().sleep().await;

            // After idle period, send final message and shutdown
            stream.write_all(b"done").await.unwrap();
            stream.shutdown().await.unwrap();

            let mut response = vec![];
            stream.read_to_end(&mut response).await.unwrap();

            assert_eq!(response, b"got it"[..]);
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, peer_addr)) = server.accept().await {
                async move {
                    // Server does NOT enable keep alive
                    // Read first chunk
                    let mut buf = [0u8; 100];
                    let n = stream.read(&mut buf).await.unwrap();
                    assert_eq!(&buf[..n], b"request");

                    // Server can now wait for more data
                    // The client's keep_alive should maintain connection during its 180s idle
                    let mut request = vec![];
                    stream.read_to_end(&mut request).await.unwrap();
                    assert_eq!(&request, b"done");

                    stream.write_from_fin(&mut &b"got it"[..]).await.unwrap();
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

/// Test that a stream with keep alive enabled on the server side stays alive
/// while the server is idle doing expensive work (simulated by sleep).
#[test]
fn keep_alive_server_only() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            // Client does NOT enable keep alive
            // Send request normally
            stream.write_all(b"request").await.unwrap();
            stream.shutdown().await.unwrap();

            // Wait for server's response
            let mut response = vec![];
            stream.read_to_end(&mut response).await.unwrap();

            assert_eq!(response, b"processed"[..]);
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, peer_addr)) = server.accept().await {
                async move {
                    // Enable keep alive on the server side only
                    stream.keep_alive(true);

                    let mut request = vec![];
                    stream.read_to_end(&mut request).await.unwrap();

                    // Server takes a long time to process (3 minutes)
                    // Keep alive should maintain the connection
                    180.s().sleep().await;

                    stream.write_from_fin(&mut &b"processed"[..]).await.unwrap();
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

/// Test that a stream times out after keep alive is disabled.
/// This test enables keep_alive, verifies it works, then disables it and verifies timeout.
#[test]
fn keep_alive_enable_then_disable_times_out() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            // Enable keep alive initially
            stream.keep_alive(true);

            // Wait for 1 minute while idle to show keep alive is working
            60.s().sleep().await;

            // Write to verify we haven't timed out yet
            stream.write_all(b"still alive").await.unwrap();

            // Disable keep alive
            stream.keep_alive(false);

            // Wait longer than the idle timeout (60s > 30s timeout)
            60.s().sleep().await;

            // Try to send a message - this should fail because the stream timed out
            let result = stream.write_all(b"should fail").await;
            assert!(
                result.is_err(),
                "Stream should have timed out after disabling keep alive"
            );
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, peer_addr)) = server.accept().await {
                async move {
                    // Server reads the first message
                    let mut buf = [0u8; 100];
                    let result = stream.read(&mut buf).await;
                    assert!(result.is_ok(), "Should read first message");

                    // Server waits, expecting the connection to timeout
                    // After client disables keep alive, the stream should timeout
                    let result = stream.read(&mut buf).await;
                    assert!(result.is_err(), "Stream should timeout on server side");
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

/// Test that enabling keep alive close to idle timeout prevents timeout.
#[test]
fn keep_alive_enabled_near_timeout() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            // Send some initial data to establish the stream
            stream.write_all(b"start").await.unwrap();

            // Wait until we're close to the idle timeout (20s out of 30s)
            20.s().sleep().await;

            // Enable keep alive just before timeout (10s before the 30s timeout)
            stream.keep_alive(true);

            // Wait another 2 minutes while idle with keep alive preventing timeout
            120.s().sleep().await;

            // Send a message to verify stream is still alive
            stream.write_all(b"still here").await.unwrap();
            stream.shutdown().await.unwrap();

            let mut response = vec![];
            stream.read_to_end(&mut response).await.unwrap();

            assert!(response.len() > 0, "Should have received response");
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, peer_addr)) = server.accept().await {
                async move {
                    // Server does NOT enable keep alive
                    let mut request = vec![];
                    stream.read_to_end(&mut request).await.unwrap();

                    stream.write_all(&request).await.unwrap();
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

/// Test that keep alive is no longer active after the stream is shutdown.
/// This verifies that after client shutdown, keep_alive becomes inactive and
/// the connection will timeout if the server takes too long.
#[test]
fn keep_alive_inactive_after_shutdown() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            // Enable keep alive
            stream.keep_alive(true);

            // Send data and shutdown immediately
            stream.write_all(b"request").await.unwrap();
            stream.shutdown().await.unwrap();

            // After shutdown, keep alive becomes inactive
            // Server takes too long to respond, so the connection times out
            let mut response = vec![];
            let result = stream.read_to_end(&mut response).await;
            assert!(
                result.is_err(),
                "Client read should timeout after keep alive became inactive"
            );
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

                    // Server sleeps for longer than idle timeout
                    // Because client's keep alive is now inactive after shutdown,
                    // the connection will timeout
                    60.s().sleep().await;

                    let result = stream.write_from_fin(&mut &b"response"[..]).await;
                    assert!(result.is_err(), "Server write should fail due to timeout");
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
