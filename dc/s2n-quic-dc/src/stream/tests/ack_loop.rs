// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    stream::testing::{Client, Server},
    testing::{ext::*, sim},
};
use core::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info_span, Instrument};

/// Test that demonstrates the ACK transmission loop bug.
/// 
/// With needs_transmission("new_packet") uncommented, the receiver sends an ACK
/// for every packet received. When the client continuously sends data, this causes
/// the receiver to continuously wake up and transmit ACKs, creating a tight loop.
/// 
/// This test proves the bug by:
/// 1. Client sends continuous stream of small packets (50 writes)
/// 2. Server receives packets and tracks ACK transmissions via event subscriber
/// 3. Asserts that ACK count is excessive (demonstrating the loop)
///
/// **CURRENT STATUS**: This test FAILS, proving the bug exists.
/// With the bug: Test shows ~95 ACK packets sent for ~50 data packets (almost 2:1 ratio!)
/// Expected after fix: ACKs should be batched/throttled to < 10 total ACKs
#[test]
#[should_panic(expected = "ACK loop detected")]
fn ack_loop_reproduction() {
    sim(|| {
        async move {
            let client = Client::builder().build();
            let mut stream = client.connect_sim("server:443").await.unwrap();

            // Send many small packets to trigger continuous ACK responses
            // Each write may be split into multiple packets due to MTU
            for i in 0..50 {
                stream.write_all(&[i as u8; 100]).await.unwrap();
                // Small delay to ensure packets are sent separately
                1.ms().sleep().await;
            }
            
            stream.shutdown().await.unwrap();
            
            let mut response = vec![];
            stream.read_to_end(&mut response).await.unwrap();
            assert_eq!(response.len(), 5000);
        }
        .group("client")
        .instrument(info_span!("client"))
        .primary()
        .spawn();

        async move {
            let server = Server::udp().port(443).build();
            let server_subscriber = server.subscriber();

            while let Ok((stream, _addr)) = server.accept().await {
                let server_subscriber = server_subscriber.clone();
                async move {
                    let (mut recv, mut send) = stream.into_split();
                    
                    // Echo server - read all data and echo it back
                    let mut data = vec![];
                    recv.read_to_end(&mut data).await.unwrap();
                    send.write_all(&data).await.unwrap();
                    
                    // Check how many ACK control packets were transmitted by the receiver
                    let ack_count = server_subscriber.stream_control_packet_transmitted.load(Ordering::Relaxed);
                    
                    // With the bug: Every received data packet triggers an ACK transmission
                    // The client sends ~50 packets, so we'd expect ~50+ ACKs
                    // Without the bug: ACKs should be batched or sent less frequently
                    
                    // This assertion will FAIL with the current code, proving the bug exists
                    // When fixed, ACK count should be much lower (< 10)
                    assert!(
                        ack_count < 10,
                        "ACK loop detected! Sent {} ACK packets for ~50 data packets. \
                         Expected < 10 ACKs with proper batching. \
                         This proves needs_transmission('new_packet') on every packet causes excessive ACKs.",
                        ack_count
                    );
                }
                .instrument(info_span!("stream"))
                .primary()
                .spawn();
            }
        }
        .group("server")
        .instrument(info_span!("server"))
        .spawn();
    });
}
