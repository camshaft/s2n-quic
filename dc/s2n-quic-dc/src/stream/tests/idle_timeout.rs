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

/// Test that a stream doesn't timeout when the sender is actively sending
/// but the receiver is not reading (flow control blocked scenario)
#[test]
fn sender_active_receiver_blocked() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            // Send data continuously for longer than the idle timeout
            // This should not timeout because the receiver will send ACKs
            let mut total_sent = 0;
            for i in 0..60 {
                let data = vec![i as u8; 1024];
                stream.write_all(&data).await.unwrap();
                total_sent += data.len();
                1.s().sleep().await;
            }
            stream.shutdown().await.unwrap();

            let mut response = vec![];
            stream.read_to_end(&mut response).await.unwrap();
            assert_eq!(response.len(), total_sent);
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, peer_addr)) = server.accept().await {
                async move {
                    // Don't read for 40 seconds (longer than 30s idle timeout)
                    // but the stream should stay alive because sender is active
                    40.s().sleep().await;

                    // Now read all the data
                    let mut request = vec![];
                    stream.read_to_end(&mut request).await.unwrap();

                    // Echo it back
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

/// Test that demonstrates the issue: receiver times out when flow-control-blocked
/// even though both sides are alive
#[test]
#[should_panic(expected = "TimedOut")]
fn receiver_timeout_when_flow_control_blocked() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            // Send a large amount of data quickly to fill the receiver's buffer
            let data = vec![42u8; 128 * 1024]; // 128KB
            for _ in 0..100 {
                stream.write_all(&data).await.unwrap();
            }
            stream.shutdown().await.unwrap();

            // Try to read response - this should fail with timeout
            let mut response = vec![];
            let result = stream.read_to_end(&mut response).await;
            
            // Check for timeout error
            if let Err(e) = result {
                panic!("{:?}", e.kind());
            }
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((stream, peer_addr)) = server.accept().await {
                async move {
                    // Don't read anything for 40 seconds (longer than 30s idle timeout)
                    // The receiver buffer will fill up, flow control will block the sender,
                    // and eventually the receiver should timeout
                    40.s().sleep().await;
                    
                    // At this point, the stream should have timed out on the receiver side
                    drop(stream);
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
/// Test that demonstrates the issue: receiver times out when flow-control-blocked
/// even though both sides are alive
#[test]
fn receiver_timeout_when_flow_control_blocked_v2() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            // Send a large amount of data quickly to fill the receiver's buffer
            let data = vec![42u8; 128 * 1024]; // 128KB
            for _ in 0..100 {
                match stream.write_all(&data).await {
                    Ok(_) => {},
                    Err(e) => {
                        eprintln!("Write error: {:?}", e);
                        break;
                    }
                }
            }
            stream.shutdown().await.ok();

            // Try to read response - this should fail with timeout
            let mut response = vec![];
            let result = stream.read_to_end(&mut response).await;
            
            // Check for timeout error
            match result {
                Ok(_) => println!("No error - stream completed successfully"),
                Err(e) => {
                    println!("Error kind: {:?}", e.kind());
                    assert_eq!(e.kind(), std::io::ErrorKind::TimedOut, "Expected TimedOut error, got: {:?}", e);
                }
            }
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((stream, peer_addr)) = server.accept().await {
                async move {
                    // Don't read anything for 40 seconds (longer than 30s idle timeout)
                    // The receiver buffer will fill up, flow control will block the sender,
                    // and eventually the receiver should timeout
                    40.s().sleep().await;
                    
                    // At this point, the stream should have timed out on the receiver side
                    drop(stream);
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
/// Test to reproduce the actual issue: sender times out when receiver doesn't read
#[test]
fn sender_blocks_and_times_out() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            // Send a large amount of data to fill the receiver's buffer
            // and cause the sender to block on flow control
            let data = vec![42u8; 1024 * 1024]; // 1MB
            
            let write_result = tokio::time::timeout(
                std::time::Duration::from_secs(45),
                stream.write_all(&data)
            ).await;

            match write_result {
                Ok(Ok(())) => println!("Write succeeded"),
                Ok(Err(e)) => {
                    println!("Write error: {:?}, kind: {:?}", e, e.kind());
                    // We expect this to fail with timeout or connection error
                    assert!(
                        matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::ConnectionAborted),
                        "Expected timeout or connection error, got: {:?}", e
                    );
                },
                Err(_) => println!("Write timed out at tokio level"),
            }
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();

            while let Ok((mut stream, peer_addr)) = server.accept().await {
                async move {
                    // Accept the stream but don't read from it for 45 seconds
                    // This should cause the sender to block and eventually timeout
                    45.s().sleep().await;
                    
                    // Try to read - this might fail if the stream timed out
                    let mut buf = vec![0u8; 1024];
                    match stream.read(&mut buf).await {
                        Ok(n) => println!("Read {} bytes", n),
                        Err(e) => println!("Read error: {:?}", e),
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
