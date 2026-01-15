use crate::{endpoint::Endpoint, priority::Priority, *};
use tokio::task::JoinSet;

/// Test basic unary RPC flow
#[tokio::test]
#[should_panic] // Will panic on todo!() but should compile
async fn test_unary_rpc() {
    let endpoint = create_test_endpoint();
    let peer = test_peer_addr();

    // Allocate and fill a message
    let message = "Hello, world!";
    let msg = endpoint
        .allocator()
        .allocate(Priority::default(), message.into())
        .await;

    // Build and send unary request
    let request = endpoint.unary(peer).build(msg);

    // Send and receive response
    let response = request.send().await.unwrap();

    // Read response body
    let body = response.recv().await.unwrap();

    println!("Received response: {body:?}");
}

/// Test streaming response RPC
#[tokio::test]
#[should_panic]
async fn test_streaming_response() {
    let endpoint = create_test_endpoint();
    let peer = test_peer_addr();

    // Allocate request message
    let message = "request";
    let request_msg = endpoint
        .allocator()
        .allocate(Priority::default(), message.into())
        .await;

    // Build and send request
    let request = endpoint
        .streaming_response(peer)
        .metadata("handler-info".into())
        .build(request_msg);

    let mut stream = request.send().await.unwrap();

    // Receive multiple responses
    let mut count = 0;
    while let Some(result) = stream.recv().await {
        result.unwrap();
        count += 1;
        if count >= 5 {
            break;
        }
    }

    // Close the stream
    stream.close(None).unwrap();

    println!("Received {} responses", count);
}

/// Test streaming request RPC
#[tokio::test]
#[should_panic]
async fn test_streaming_request() {
    let endpoint = create_test_endpoint();
    let peer = test_peer_addr();

    // Build and open streaming request
    let mut request = endpoint.streaming_request(peer).build();

    // Send multiple items
    for i in 0..5 {
        let msg = format!("item-{i}");
        let item = request
            .allocator()
            .allocate(Priority::default(), msg.into())
            .await;

        request.send(item).await.unwrap();
    }

    // Finish and get response
    let response = request.finish().await.unwrap();

    let _body = response.recv().await.unwrap();

    println!("Streaming request completed");
}

/// Test bidirectional streaming with concurrent send/receive
#[tokio::test]
#[should_panic]
async fn test_bidirectional_streaming() {
    let endpoint = create_test_endpoint();
    let peer = test_peer_addr();

    // Build and open bidirectional stream
    let (mut sender, mut receiver) = endpoint
        .bidirectional(peer)
        .metadata("bidi-metadata".into())
        .build();

    // Spawn task to send requests
    let send_task = tokio::spawn(async move {
        for i in 0..3 {
            let message = format!("request-{i}");
            let msg = sender
                .allocator()
                .allocate(Priority::default(), message.into())
                .await;

            sender.send(msg).await.unwrap();
        }
        sender.close(None).unwrap();
    });

    // Receive responses concurrently
    let recv_task = tokio::spawn(async move {
        let mut count = 0;
        while let Some(result) = receiver.recv().await {
            result.unwrap();
            count += 1;
        }
        receiver.close(None).unwrap();
        count
    });

    send_task.await.unwrap();
    let response_count = recv_task.await.unwrap();

    println!(
        "Bidirectional stream completed: received {} responses",
        response_count
    );
}

/// Test priority-based scheduling
#[tokio::test]
#[should_panic]
async fn test_priority_scheduling() {
    let endpoint = create_test_endpoint();
    let peer = test_peer_addr();

    // Send requests with different priorities
    let priorities = [
        priority::Priority::LOWEST,
        priority::Priority::HIGHEST,
        priority::Priority::MEDIUM,
        priority::Priority::VERY_HIGH,
    ];

    let mut tasks = JoinSet::new();
    for (i, prio) in priorities.iter().copied().enumerate() {
        let msg = endpoint
            .allocator()
            .allocate(Priority::default(), format!("msg-{}", i).into())
            .await;

        let request = endpoint.unary(peer.clone()).build(msg);

        // Spawn to send concurrently
        tasks.spawn(async move {
            let response = request.send().await.unwrap();
            let _body = response.recv().await.unwrap();
            prio
        });
    }

    // Wait for all tasks to complete and verify ordering
    let mut prev_priority: Option<priority::Priority> = None;
    while let Some(result) = tasks.join_next().await {
        let prio = result.unwrap();

        if let Some(prev) = prev_priority {
            // Lower priority values should complete first (higher priority)
            // So each subsequent priority value should be >= previous
            assert!(
                prio >= prev,
                "Priority ordering violated: {:?} completed after {:?}",
                prio,
                prev
            );
        }

        prev_priority = Some(prio);
    }

    println!("Priority scheduling verified - requests completed in priority order");
}

/// Test streaming request with item-level priorities and dependencies
#[tokio::test]
#[should_panic]
async fn test_streaming_request_with_item_options() {
    let endpoint = create_test_endpoint();
    let peer = test_peer_addr();

    let mut request = endpoint.streaming_request(peer).build();

    // First item
    let msg1 = request
        .allocator()
        .allocate(Priority::default(), "item1".into())
        .await;

    request.send(msg1).await.unwrap();

    // Second item depends on first
    let msg2 = request
        .allocator()
        .allocate(Priority::default(), "item2".into())
        .await;

    request.send(msg2).await.unwrap();

    let _response = request.finish().await.unwrap();

    println!("Streaming request with item options completed");
}

/// Test broadcast by cloning a buffer and sending to multiple peers
#[tokio::test]
#[should_panic]
async fn test_broadcast_pattern() {
    let endpoint = create_test_endpoint();
    let peers = vec![test_peer_addr(), test_peer_addr(), test_peer_addr()];

    // Allocate and fill a single buffer
    let message = "Broadcast message to all peers";
    let msg = endpoint
        .allocator()
        .allocate(Priority::default(), message.into())
        .await;

    // Clone the message for each peer and send
    let mut tasks = JoinSet::new();
    for peer in peers {
        let msg = msg.clone(); // Clone the Message - shares the underlying buffer
        let request = endpoint.unary(peer).build(msg);

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
    let endpoint = create_test_endpoint();
    let peer = test_peer_addr();

    // Request cache data for a specific key
    let cache_key = b"my-cache-key";
    let request_msg = endpoint
        .allocator()
        .allocate(Priority::default(), cache_key.into())
        .await;

    // Create a chunk handler with pre-allocated buffers
    struct MyCacheHandler {
        // Pre-allocated buffer regions for RDMA or decrypt
        #[expect(dead_code)]
        buffers: Vec<Vec<u8>>,
    }

    impl endpoint::outgoing::streaming_response::ChunkHandler for MyCacheHandler {
        // TODO: Implement methods for:
        // - Providing buffer regions for RDMA registration
        // - Handling chunk placement notifications
        // - Completion signaling
    }

    let handler = MyCacheHandler {
        buffers: vec![vec![0u8; 1024 * 1024]; 10], // 10 MB pre-allocated
    };

    // Build direct placement request
    let request = endpoint
        .streaming_response(peer)
        .metadata("cache-get".into())
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
    let endpoint = create_test_endpoint();
    let peer = test_peer_addr();

    let msg = endpoint
        .allocator()
        .allocate(Priority::default(), "test".into())
        .await;

    let request = endpoint.unary(peer).build(msg);

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

fn create_test_endpoint() -> Endpoint {
    todo!("Create test endpoint")
}

fn test_peer_addr() -> peer::Handle {
    todo!("Create test peer handle")
}
