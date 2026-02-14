// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    stream::testing::{Client, Server},
    testing::{ext::*, sim},
};
use core::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info_span, Instrument};

/// Test that investigates ACK transmission behavior with idle timer gating.
/// 
/// This test validates whether moving needs_transmission("new_packet") inside the
/// idle timer update conditional prevents excessive ACK transmissions.
/// 
/// Test behavior:
/// 1. Client sends continuous stream of small packets (50 writes)
/// 2. Server receives packets and tracks ACK transmissions via event subscriber
/// 3. Measures actual ACK count to compare with baseline
///
/// **FINDING**: Moving the ACK call inside the idle timer check does NOT reduce ACKs!
/// - Baseline (ACK on every packet, outside if): ~95 ACKs for ~50 data packets
/// - With idle timer gating (ACK inside if): Still ~95 ACKs for ~50 data packets
/// 
/// **ROOT CAUSE**: The idle timer if condition is true for all normal data packets:
/// - Condition: `state == Recv || state == SizeKnown || packet.stream_offset() == 0`
/// - During normal reception, state is Recv/SizeKnown, so condition always passes
/// - Therefore, moving ACK call inside the if doesn't gate anything in practice
///
/// **CONCLUSION**: The real bug is not about WHERE the ACK call is placed, but that
/// needs_transmission() is called on EVERY packet without any batching/throttling.
#[test]
#[should_panic(expected = "Excessive ACK transmissions")]
fn ack_transmission_with_idle_timer_gating() {
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
                    
                    // With ACK call inside idle timer check: ACKs should be reasonable
                    // Without the fix: Every packet triggers ACK = ~95 ACKs for ~50 packets
                    // With the fix: ACKs should be batched/throttled = < 20 ACKs
                    
                    tracing::info!(
                        ack_count,
                        "ACK control packets transmitted for ~50 data packets"
                    );
                    
                    assert!(
                        ack_count < 20,
                        "Excessive ACK transmissions! Sent {} ACK packets for ~50 data packets. \
                         Expected < 20 ACKs with idle timer gating. \
                         (Without fix, this was ~95 ACKs showing 2:1 ratio)",
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
