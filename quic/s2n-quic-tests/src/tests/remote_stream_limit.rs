// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests for changing the number of streams the peer may have open while a connection runs

use super::*;
use s2n_quic::stream::Type;
use s2n_quic_core::connection::limits::Limits;
use std::sync::atomic::{AtomicUsize, Ordering};

//= https://www.rfc-editor.org/rfc/rfc9000#section-4.6
//= type=test
//# An endpoint MUST NOT wait
//# to receive this signal before advertising additional credit, since
//# doing so will mean that the peer will be blocked for at least an
//# entire round trip
#[test]
fn raising_the_limit_lets_the_peer_open_more_streams() {
    let model = Model::default();
    let opened = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(vec![]));

    test(model.clone(), |handle| {
        let limits = Limits::new()
            .with_max_open_remote_bidirectional_streams(1)
            .unwrap();
        let mut server = Server::builder()
            .with_io(handle.builder().build()?)?
            .with_tls(SERVER_CERTS)?
            .with_event(tracing_events(true, model.clone()))?
            .with_random(Random::with_seed(123))?
            .with_limits(limits)?
            .start()?;
        let addr = server.local_addr()?;

        let server_opened = opened.clone();
        let server_observed = observed.clone();
        spawn(async move {
            let mut connection = server.accept().await.unwrap();

            // the client has credit for one stream
            let mut streams = vec![];
            let stream = connection.accept_bidirectional_stream().await.unwrap();
            streams.push(stream.unwrap());

            // the limit holds back the client's second stream
            delay(Duration::from_secs(1)).await;
            let count = server_opened.load(Ordering::SeqCst);
            server_observed.lock().unwrap().push(count);

            // raising the limit lets the client open three more streams
            connection
                .set_max_open_remote_streams(Type::Bidirectional, 4)
                .unwrap();

            for _ in 0..3 {
                let stream = connection.accept_bidirectional_stream().await.unwrap();
                streams.push(stream.unwrap());
            }

            let count = server_opened.load(Ordering::SeqCst);
            server_observed.lock().unwrap().push(count);

            connection.close(0u32.into());

            // keep the endpoint open until the test ends
            let _ = server.accept().await;
        });

        let client = build_client(handle, model.clone(), true)?;
        let client_opened = opened.clone();
        primary::spawn(async move {
            let connect = Connect::new(addr).with_server_name("localhost");
            let mut connection = client.connect(connect).await.unwrap();

            let mut streams = vec![];
            for _ in 0..4 {
                let mut stream = connection.open_bidirectional_stream().await.unwrap();
                stream.send(Bytes::from_static(b"a")).await.unwrap();
                client_opened.fetch_add(1, Ordering::SeqCst);
                streams.push(stream);
            }

            // hold the streams open until the server closes the connection
            let _ = connection.accept().await;
        });

        Ok(addr)
    })
    .unwrap();

    // the client opened one stream before the server raised the limit, and four after it
    assert_eq!(*observed.lock().unwrap(), [1, 4]);
}

//= https://www.rfc-editor.org/rfc/rfc9000#section-4.6
//= type=test
//# Once a receiver advertises a stream limit using the MAX_STREAMS
//# frame, advertising a smaller limit has no effect.
#[test]
fn lowering_the_limit_holds_back_credit_until_streams_close() {
    let model = Model::default();
    let opened = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(vec![]));

    test(model.clone(), |handle| {
        let limits = Limits::new()
            .with_max_open_remote_bidirectional_streams(4)
            .unwrap();
        let mut server = Server::builder()
            .with_io(handle.builder().build()?)?
            .with_tls(SERVER_CERTS)?
            .with_event(tracing_events(true, model.clone()))?
            .with_random(Random::with_seed(123))?
            .with_limits(limits)?
            .start()?;
        let addr = server.local_addr()?;

        let server_opened = opened.clone();
        let server_observed = observed.clone();
        spawn(async move {
            let mut connection = server.accept().await.unwrap();

            // read each of the client's four streams to its end. The server's sending side of
            // each stream stays open, so each stream stays open.
            let mut streams = vec![];
            for _ in 0..4 {
                let stream = connection.accept_bidirectional_stream().await.unwrap();
                let mut stream = stream.unwrap();
                while stream.receive().await.unwrap().is_some() {}
                streams.push(stream);
            }

            // the limit holds back the client's fifth stream
            delay(Duration::from_secs(1)).await;
            let count = server_opened.load(Ordering::SeqCst);
            server_observed.lock().unwrap().push(count);

            // lowering the limit leaves the client's credit as it is
            connection
                .set_max_open_remote_streams(Type::Bidirectional, 2)
                .unwrap();

            // closing two streams brings the open streams down to the new limit, which
            // doesn't give the client new credit
            for stream in &mut streams[..2] {
                stream.finish().unwrap();
            }
            delay(Duration::from_secs(1)).await;
            let count = server_opened.load(Ordering::SeqCst);
            server_observed.lock().unwrap().push(count);

            // closing a third stream lets the client open one more stream
            streams[2].finish().unwrap();
            let stream = connection.accept_bidirectional_stream().await.unwrap();
            let mut stream = stream.unwrap();
            while stream.receive().await.unwrap().is_some() {}
            streams.push(stream);
            delay(Duration::from_secs(1)).await;
            let count = server_opened.load(Ordering::SeqCst);
            server_observed.lock().unwrap().push(count);

            connection.close(0u32.into());

            // keep the endpoint open until the test ends
            let _ = server.accept().await;
        });

        let client = build_client(handle, model.clone(), true)?;
        let client_opened = opened.clone();
        primary::spawn(async move {
            let connect = Connect::new(addr).with_server_name("localhost");
            let mut connection = client.connect(connect).await.unwrap();

            // open streams until the server closes the connection
            let mut streams = vec![];
            while let Ok(mut stream) = connection.open_bidirectional_stream().await {
                if stream.send(Bytes::from_static(b"a")).await.is_err() {
                    break;
                }
                if stream.finish().is_err() {
                    break;
                }
                client_opened.fetch_add(1, Ordering::SeqCst);
                streams.push(stream);
            }
        });

        Ok(addr)
    })
    .unwrap();

    // the client kept to the credit it had when the server lowered the limit, and got one more
    // stream once the server closed a third stream
    assert_eq!(*observed.lock().unwrap(), [4, 4, 5]);
}
