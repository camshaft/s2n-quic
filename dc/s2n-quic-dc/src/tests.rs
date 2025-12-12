use tokio::task::JoinSet;

use crate::*;
use std::net::SocketAddr;

/// Test basic unary RPC flow
#[tokio::test]
#[should_panic] // Will panic on todo!() but should compile
async fn test_unary_rpc() {
    let client = create_test_client();
    let peer = test_peer_addr();

    // Allocate and fill a message
    let message = "Hello, world!";
    let msg = client
        .allocator()
        .allocate(message.len())
        .await
        .fill_plaintext(&mut message.as_bytes());

    // Build and send unary request
    let request = client
        .unary(peer)
        .priority(priority::Priority::HIGH)
        .header(bytes::Bytes::from("request-header"))
        .expect_response_header(true)
        .causal_token(true)
        .build(msg);

    // Get the causality token
    let token = request.causal_token();

    // Send and receive response
    let response = request.send().await.unwrap();

    // Check response header
    assert!(response.header().is_some());

    // Read response body
    let mut body = bytes::BytesMut::new();
    response.read_into(&mut body).await.unwrap();

    println!("Received response with token: {:?}", token);
}

/// Test streaming response RPC
#[tokio::test]
#[should_panic]
async fn test_streaming_response() {
    let client = create_test_client();
    let peer = test_peer_addr();

    // Allocate request message
    let message = "request";
    let request_msg = client
        .allocator()
        .allocate(message.len())
        .await
        .fill_plaintext(&mut message.as_bytes());

    // Build and send request
    let request = client
        .streaming_response(peer)
        .priority(priority::Priority::MEDIUM)
        .metadata(bytes::Bytes::from("handler-info"))
        .expect_response_headers(true)
        .causal_token(true)
        .build(request_msg);

    let token = request.causal_token();
    let mut stream = request.send().await.unwrap();

    // Check header
    let _header = stream.header();

    // Receive multiple responses
    let mut count = 0;
    let mut response = bytes::BytesMut::new();
    while let Some(result) = stream.recv(&mut response).await {
        result.unwrap();
        count += 1;
        if count >= 5 {
            break;
        }
        response.clear();
    }

    // Close the stream
    stream.close(None).unwrap();

    println!("Received {} responses with token: {:?}", count, token);
}

/// Test streaming request RPC
#[tokio::test]
#[should_panic]
async fn test_streaming_request() {
    let client = create_test_client();
    let peer = test_peer_addr();

    // Build and open streaming request
    let mut request = client
        .streaming_request(peer)
        .priority(priority::Priority::HIGH)
        .header(bytes::Bytes::from("stream-header"))
        .expect_response_header(true)
        .causal_token(true)
        .build();

    let stream_token = request.causal_token();

    // Send multiple items
    for i in 0..5 {
        let msg = format!("item-{i}");
        let msg = request
            .allocator()
            .allocate(100)
            .await
            .fill_plaintext(&mut msg.as_bytes());

        let item = client::streaming_request::Item::new(msg).priority(priority::Priority::MEDIUM);

        let item_token = request.send(item).await.unwrap();
        println!("Sent item {i} with token: {item_token:?}");
    }

    // Finish and get response
    let response = request.finish().await.unwrap();

    let mut body = bytes::BytesMut::new();
    response.read_into(&mut body).await.unwrap();

    println!("Streaming request completed with token: {stream_token:?}");
}

/// Test bidirectional streaming with concurrent send/receive
#[tokio::test]
#[should_panic]
async fn test_bidirectional_streaming() {
    let client = create_test_client();
    let peer = test_peer_addr();

    // Build and open bidirectional stream
    let (mut sender, mut receiver) = client
        .bidirectional(peer)
        .priority(priority::Priority::VERY_HIGH)
        .metadata(bytes::Bytes::from("bidi-metadata"))
        .expect_response_headers(true)
        .causal_token(true)
        .build();

    let token = sender.causal_token();

    // Spawn task to send requests
    let send_task = tokio::spawn(async move {
        for i in 0..3 {
            let message = format!("request-{i}");
            let msg = sender
                .allocator()
                .allocate(50)
                .await
                .fill_plaintext(&mut message.as_bytes());

            let item = client::bidirectional::Item::new(msg);
            sender.send(item).await.unwrap();
        }
        sender.close(None).unwrap();
    });

    // Receive responses concurrently
    let recv_task = tokio::spawn(async move {
        let mut count = 0;
        while let Some(result) = receiver.recv(&mut bytes::BytesMut::new()).await {
            result.unwrap();
            count += 1;
        }
        receiver.close(None).unwrap();
        count
    });

    send_task.await.unwrap();
    let response_count = recv_task.await.unwrap();

    println!(
        "Bidirectional stream completed with token: {:?}, received {} responses",
        token, response_count
    );
}

/// Test causality dependencies
#[tokio::test]
#[should_panic]
async fn test_causality_dependencies() {
    let client = create_test_client();
    let peer = test_peer_addr();

    // First request
    let msg1 = client
        .allocator()
        .allocate(50)
        .await
        .fill_plaintext(&mut &b"first"[..]);

    let req1 = client
        .unary(peer)
        .priority(priority::Priority::HIGH)
        .causal_token(true)
        .build(msg1);

    let token1 = req1.causal_token().unwrap();

    // Second request depends on first (wait for response, cascade errors)
    let msg2 = client
        .allocator()
        .allocate(50)
        .await
        .fill_plaintext(&mut &b"second"[..]);

    let req2 = client
        .unary(peer)
        .depends_on(token1.wait_for_response())
        .causal_token(true)
        .build(msg2);

    let token2 = req2.causal_token().unwrap();

    // Third request depends on second (wait for request only, don't cascade)
    let msg3 = client
        .allocator()
        .allocate(50)
        .await
        .fill_plaintext(&mut &b"third"[..]);

    let _req3 = client
        .unary(peer)
        .depends_on(token2.wait_for_request_no_cascade())
        .build(msg3);

    // Send requests - they will be ordered by dependencies
    let _resp1 = req1.send().await.unwrap();
    let _resp2 = req2.send().await.unwrap();

    println!("Causality chain completed");
}

/// Test priority-based scheduling
#[tokio::test]
#[should_panic]
async fn test_priority_scheduling() {
    let client = create_test_client();
    let peer = test_peer_addr();

    // Send requests with different priorities
    let priorities = [
        priority::Priority::LOWEST,
        priority::Priority::HIGHEST,
        priority::Priority::MEDIUM,
        priority::Priority::VERY_HIGH,
    ];

    for (i, prio) in priorities.iter().enumerate() {
        let msg = client
            .allocator()
            .allocate(50)
            .await
            .fill_plaintext(&mut format!("msg-{}", i).as_bytes());

        let request = client.unary(peer).priority(*prio).build(msg);

        // Spawn to send concurrently
        tokio::spawn(async move {
            let _ = request.send().await;
        });
    }

    // Higher priority requests should be processed first
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
}

/// Test buffer allocation with NUMA preference
#[tokio::test]
#[should_panic]
async fn test_buffer_allocation_numa() {
    let client = create_test_client();

    // Allocate buffer with NUMA preference
    let worker_id = worker_id();
    let allocator = client.allocator().clone().prefer_worker(worker_id);

    let msg = allocator
        .allocate(1024)
        .await
        .fill_encrypted(&mut &b"encrypted-data"[..], 0..16);

    assert_eq!(msg.len(), 1024);
    assert!(!msg.is_empty());

    println!(
        "Buffer allocated with NUMA preference for worker {:?}",
        worker_id
    );
}

/// Test streaming request with item-level priorities and dependencies
#[tokio::test]
#[should_panic]
async fn test_streaming_request_with_item_options() {
    let client = create_test_client();
    let peer = test_peer_addr();

    let mut request = client
        .streaming_request(peer)
        .priority(priority::Priority::MEDIUM)
        .build();

    // First item
    let msg1 = request
        .allocator()
        .allocate(50)
        .await
        .fill_plaintext(&mut &b"item1"[..]);

    let item1 = client::streaming_request::Item::new(msg1).priority(priority::Priority::VERY_HIGH);

    let token1 = request.send(item1).await.unwrap();

    // Second item depends on first
    let msg2 = request
        .allocator()
        .allocate(50)
        .await
        .fill_plaintext(&mut &b"item2"[..]);

    let item2 = client::streaming_request::Item::new(msg2)
        .priority(priority::Priority::HIGH)
        .depends_on(token1.wait_for_request());

    let _token2 = request.send(item2).await.unwrap();

    let _response = request.finish().await.unwrap();

    println!("Streaming request with item options completed");
}

/// Test broadcast by cloning a buffer and sending to multiple peers
#[tokio::test]
#[should_panic]
async fn test_broadcast_pattern() {
    let client = create_test_client();
    let peers = vec![
        "127.0.0.1:8081".parse().unwrap(),
        "127.0.0.1:8082".parse().unwrap(),
        "127.0.0.1:8083".parse().unwrap(),
    ];

    // Allocate and fill a single buffer
    let message = "Broadcast message to all peers";
    let shared_msg = client
        .allocator()
        .allocate(message.len())
        .await
        .fill_plaintext(&mut message.as_bytes());

    // Clone the message for each peer and send
    let mut tasks = JoinSet::new();
    for peer in peers {
        let msg = shared_msg.clone(); // Clone the Message - shares the underlying buffer
        let request = client
            .unary(peer)
            .priority(priority::Priority::HIGH)
            .build(msg);

        tasks.spawn(async move { request.send().await });
    }

    // Wait for all to complete and collect results
    while let Some(res) = tasks.join_next().await {
        match res {
            Ok(_response) => println!("Peer succeeded"),
            Err(e) => println!("Peer failed: {:?}", e),
        }
    }

    println!("Broadcast complete - shared buffer sent to {} peers", 3);
}

/// Test bulk transfer with direct placement mode
#[tokio::test]
#[should_panic]
async fn test_bulk_direct_placement() {
    let client = create_test_client();
    let peer = test_peer_addr();

    // Request cache data for a specific key
    let cache_key = b"my-cache-key";
    let request_msg = client
        .allocator()
        .allocate(cache_key.len())
        .await
        .fill_plaintext(&mut &cache_key[..]);

    // Create a chunk handler with pre-allocated buffers
    struct MyCacheHandler {
        // Pre-allocated buffer regions for RDMA or decrypt
        #[expect(dead_code)]
        buffers: Vec<Vec<u8>>,
    }

    impl client::streaming_response::ChunkHandler for MyCacheHandler {
        // TODO: Implement methods for:
        // - Providing buffer regions for RDMA registration
        // - Handling chunk placement notifications
        // - Completion signaling
    }

    let handler = MyCacheHandler {
        buffers: vec![vec![0u8; 1024 * 1024]; 10], // 10 MB pre-allocated
    };

    // Build direct placement request
    let request = client
        .streaming_response(peer)
        .priority(priority::Priority::HIGH)
        .metadata(bytes::Bytes::from("cache-get"))
        .causal_token(false) // Don't need causality for cache reads
        .build_direct_placement(request_msg, handler);

    // Send and receive with zero-copy placement
    // Handler will be invoked by worker for each chunk:
    // - RDMA: Direct into pre-registered regions
    // - UDP: Decrypt from socket buffer into app buffer
    request.send().await.unwrap();

    println!("Bulk transfer with direct placement completed");
}

/// Test error handling
#[tokio::test]
#[should_panic]
async fn test_error_handling() {
    let client = create_test_client();
    let peer = test_peer_addr();

    let msg = client
        .allocator()
        .allocate(50)
        .await
        .fill_plaintext(&mut &b"test"[..]);

    let request = client.unary(peer).build(msg);

    // This will hit todo!() and panic, but demonstrates error handling pattern
    match request.send().await {
        Ok(_response) => println!("Success"),
        Err(stream::Error::PeerUnreachable) => println!("Peer unreachable"),
        Err(stream::Error::DependencyFailed) => println!("Dependency failed"),
        Err(stream::Error::TransferTimeout) => println!("Timeout"),
        Err(e) => println!("Other error: {:?}", e),
    }
}

// Helper functions for tests

fn create_test_client() -> client::Client {
    todo!("Create test client")
}

fn worker_id() -> worker::Id {
    todo!("Get worker ID")
}

fn test_peer_addr() -> SocketAddr {
    "127.0.0.1:8080".parse().unwrap()
}
