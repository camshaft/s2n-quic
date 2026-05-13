// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the stream3 endpoint packet pipeline.
//!
//! Each test runs inside Bach's deterministic simulation (`testing::sim`) with two fully
//! wired endpoints backed by simulated UDP sockets.  Each endpoint lives in its own Bach
//! group so it is treated as a separate machine from the network perspective.

use crate::stream3::{
    endpoint::testing::sim::{Client, Server},
    Reader, Stream, Writer,
};
use bytes::{Bytes, BytesMut};
use s2n_quic_core::varint::VarInt;

#[derive(Clone, Copy)]
struct TransferPlan {
    chunks: usize,
    chunk_len: usize,
    byte: u8,
    send_delay: core::time::Duration,
}

impl TransferPlan {
    fn new(chunks: usize, chunk_len: usize, byte: u8, send_delay: core::time::Duration) -> Self {
        assert!(chunks > 0, "chunks must be non-zero");
        assert!(chunk_len > 0, "chunk_len must be non-zero");
        Self {
            chunks,
            chunk_len,
            byte,
            send_delay,
        }
    }

    fn total_len(self) -> usize {
        self.chunks
            .checked_mul(self.chunk_len)
            .expect("test transfer size overflow")
    }
}

async fn write_plan(writer: &mut Writer, plan: TransferPlan, role: &str) {
    if !plan.send_delay.is_zero() {
        bach::time::sleep(plan.send_delay).await;
    }

    for idx in 0..plan.chunks {
        let mut chunk = Bytes::from(vec![plan.byte; plan.chunk_len]);
        if idx + 1 == plan.chunks {
            writer
                .write_all_from_fin(&mut chunk)
                .await
                .unwrap_or_else(|err| panic!("{role} write with fin failed: {err}"));
        } else {
            writer
                .write_all_from(&mut chunk)
                .await
                .unwrap_or_else(|err| panic!("{role} write failed: {err}"));
        }
    }
}

async fn read_to_fin(reader: &mut Reader, expected: TransferPlan, role: &str) {
    let mut buf = BytesMut::with_capacity(expected.total_len());
    loop {
        let n = reader
            .read_into(&mut buf)
            .await
            .unwrap_or_else(|err| panic!("{role} read failed: {err}"));
        if n == 0 {
            break;
        }
    }
    assert_eq!(buf.len(), expected.total_len(), "{role} received wrong length");
    assert!(
        buf.iter().all(|b| *b == expected.byte),
        "{role} received unexpected payload bytes"
    );
}

async fn run_stream_exchange(
    stream: Stream,
    send_plan: TransferPlan,
    recv_plan: TransferPlan,
    role: &'static str,
) {
    use core::{future::Future, task::Poll};

    let (mut reader, mut writer) = stream.into_split();

    let mut write_fut = core::pin::pin!(write_plan(&mut writer, send_plan, role));
    let mut read_fut = core::pin::pin!(read_to_fin(&mut reader, recv_plan, role));
    let mut write_done = false;
    let mut read_done = false;

    core::future::poll_fn(|cx| {
        if !write_done && Future::poll(write_fut.as_mut(), cx).is_ready() {
            write_done = true;
        }
        if !read_done && Future::poll(read_fut.as_mut(), cx).is_ready() {
            read_done = true;
        }

        if write_done && read_done {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

fn run_transfer_case(test_name: &'static str, client_send: TransferPlan, server_send: TransferPlan) {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let acceptor_id = VarInt::from_u8(1);

        async move {
            let server = Server::new();
            let acceptor = server
                .register_acceptor_channel(acceptor_id, 8)
                .expect("acceptor registration failed");

            let stream = acceptor.recv_front().await.expect("accept stream");
            run_stream_exchange(stream, server_send, client_send, "server").await;
        }
        .group("server")
        .spawn();

        async move {
            let mut client = Client::new();
            let stream = client
                .connect("server:0", acceptor_id)
                .await
                .expect("connect failed");

            run_stream_exchange(stream, client_send, server_send, "client").await;
            tracing::info!("{test_name} passed");
        }
        .group("client")
        .primary()
        .spawn();
    });
}

/// Ping-pong end-to-end test: the client sends "ping" and the server echoes
/// "pong" back over a real simulated UDP network path.
///
/// Both endpoints run in separate Bach groups (separate simulated machines).
/// [`Server::new`] / [`Client::new`] create their endpoints lazily on
/// first call, and [`Client::connect`] resolves the server address by group
/// name via `bach::net::lookup_host`, automatically inserting fake path-secret
/// entries into both maps.
#[test]
fn ping_pong() {
    crate::testing::sim(|| {
        use crate::testing::ext::*;

        let acceptor_id = VarInt::from_u8(1);

        // ── Server — group "server" ────────────────────────────────────
        async move {
            let server = Server::new();
            let acceptor = server
                .register_acceptor_channel(acceptor_id, 8)
                .expect("acceptor registration failed");

            // Accept one stream.
            while let Ok(stream) = acceptor.recv_front().await {
                async move {
                    let (mut reader, mut writer) = stream.into_split();

                    // Read "ping" (the client sends FIN with the data so we
                    // get EOF after reading all 4 bytes).
                    let mut buf = BytesMut::with_capacity(8);
                    loop {
                        let n = reader.read_into(&mut buf).await.expect("server read");
                        if n == 0 {
                            break;
                        }
                    }
                    assert_eq!(&buf[..], b"ping");

                    // Echo "pong" + FIN back to the client.
                    let mut pong = Bytes::from_static(b"pong");
                    writer
                        .write_all_from_fin(&mut pong)
                        .await
                        .expect("server write");
                }
                .primary()
                .spawn();
            }
        }
        .group("server")
        .spawn();

        // ── Client — group "client" (primary) ─────────────────────────
        async move {
            let mut client = Client::new();
            let stream = client
                .connect("server:0", acceptor_id)
                .await
                .expect("connect failed");

            let (mut reader, mut writer) = stream.into_split();

            // Send "ping" + FIN in the FlowInit packet.
            let mut ping = Bytes::from_static(b"ping");
            writer
                .write_all_from_fin(&mut ping)
                .await
                .expect("client write");

            // Receive "pong" + FIN.
            let mut buf = BytesMut::with_capacity(8);
            loop {
                let n = reader.read_into(&mut buf).await.expect("client read");
                if n == 0 {
                    break;
                }
            }
            assert_eq!(&buf[..], b"pong");

            tracing::info!("ping_pong passed");
        }
        .group("client")
        .primary()
        .spawn();
    });
}

#[test]
fn client_heavy_transfer() {
    run_transfer_case(
        "client_heavy_transfer",
        TransferPlan::new(32, 128, b'c', core::time::Duration::ZERO),
        TransferPlan::new(2, 16, b's', core::time::Duration::ZERO),
    );
}

#[test]
fn server_heavy_transfer() {
    run_transfer_case(
        "server_heavy_transfer",
        TransferPlan::new(2, 16, b'c', core::time::Duration::ZERO),
        TransferPlan::new(32, 128, b's', core::time::Duration::ZERO),
    );
}

#[test]
fn bidirectional_bulk_transfer() {
    run_transfer_case(
        "bidirectional_bulk_transfer",
        TransferPlan::new(24, 128, b'c', core::time::Duration::from_millis(1)),
        TransferPlan::new(24, 128, b's', core::time::Duration::from_millis(2)),
    );
}
