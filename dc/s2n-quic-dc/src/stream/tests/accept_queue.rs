// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    stream::testing::{Client, Server},
    testing::{ext::*, init_tracing, sim},
};
use bach::time::Instant;
use s2n_quic_core::stream::testing::Data;
use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
    vec,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, info_span, Instrument};

async fn check_stream(client: &Client, server: &Server) -> io::Result<()> {
    tokio::try_join!(
        async {
            let mut a = client.connect_to(server).await?;
            let _ = a.write_all(b"testing").await;

            // wait some time before calling shutdown in case the server reset the connection so we
            // can observe it in `shutdown`
            tokio::time::sleep(Duration::from_millis(10)).await;

            let _ = a.shutdown().await;

            let mut buffer = vec![];
            a.read_to_end(&mut buffer).await?;
            assert_eq!(buffer, b"testing");
            Ok(())
        }
        .instrument(info_span!("client")),
        async {
            let (mut b, _) = server.accept().await.expect("accept");
            let mut buffer = vec![];
            b.read_to_end(&mut buffer).await.unwrap();
            assert_eq!(buffer, b"testing");

            b.write_all(&buffer).await.unwrap();
            b.shutdown().await.unwrap();

            Ok(())
        }
        .instrument(info_span!("server"))
    )
    .map(|_| ())
}

#[test]
fn backlog_rejection() {
    let backlog = 2;
    let timed_out = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(AtomicUsize::new(0));
    sim(|| {
        for idx in 0..(backlog * 2) {
            let timed_out = timed_out.clone();
            let refused = refused.clone();
            async move {
                let client = Client::builder().build();
                let mut stream = client.connect_sim("server:443").await.unwrap();

                // Alternate between small and large payloads to try and trigger different failure modes
                let small_len = 100;
                let large_len = u32::MAX as u64;
                let payload_len = if idx % 2 == 0 { small_len } else { large_len };
                let is_small = payload_len == small_len;
                let mut payload = Data::new(payload_len);

                let start = Instant::now();

                let write_res = stream.write_all_from_fin(&mut payload).await;

                info!(res = ?write_res, payload_len, "write");

                let write_err = if is_small {
                    // The small payloads shouldn't block on the stream getting accepted
                    write_res.unwrap();
                    None
                } else {
                    Some(write_res.unwrap_err())
                };

                let mut response = vec![];
                let read_res = stream.read_to_end(&mut response).await;

                info!(res = ?read_res, payload_len, "read");
                let read_err = read_res.unwrap_err();

                if let Some(write_err) = write_err {
                    assert_eq!(
                        read_err.kind(),
                        write_err.kind(),
                        "read error ({:?}) should match write error ({:?}); payload_len: {payload_len}",
                        read_err.kind(),
                        write_err.kind(),
                    );
                }

                let elapsed = start.elapsed();

                match read_err.kind() {
                    io::ErrorKind::ConnectionRefused => {
                        assert!(elapsed < 1.s(), "connection refused should be fast");
                        refused.fetch_add(1, Ordering::Relaxed);
                    }
                    io::ErrorKind::TimedOut => {
                        timed_out.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {
                        panic!("unexpected error kind: {read_err:?}");
                    }
                }
            }
            .group(format!("client-{idx}"))
            .instrument(info_span!("client", client = idx))
            .primary()
            .spawn();
        }

        async move {
            let server = Server::udp().port(443).backlog(backlog).build();

            60.s().sleep().await;

            drop(server);
        }
        .group("server")
        .instrument(info_span!("server"))
        .spawn();
    });

    let timed_out = timed_out.load(Ordering::Relaxed);
    let refused = refused.load(Ordering::Relaxed);
    assert_eq!(
        timed_out, refused,
        "timed_out: {timed_out} == refused: {refused}"
    );
}
