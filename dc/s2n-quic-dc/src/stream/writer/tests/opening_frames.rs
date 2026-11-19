// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Every opening frame of one stream carries the same acceptor and the same credit tier.
//!
//! Until the server confirms a stream, each frame the client's writer sends on it is an opening
//! frame: it carries the acceptor and the credit tier, and the server binds the stream on
//! whichever opening frame arrives first. A `set_priority` call after the first opening frame
//! changes the tier of later credit acquires; these tests check that it doesn't change the tier
//! a later opening frame carries.

use super::PairBuilder;
use crate::{credit::Priority, endpoint::frame::Header, testing::sim};
use bach::ext::*;
use bytes::Bytes;
use s2n_quic_core::varint::VarInt;
use std::time::Duration;

/// The acceptor `PairBuilder` builds each writer with.
fn acceptor_id() -> VarInt {
    VarInt::from_u8(7)
}

/// The acceptor and the credit tier an opening frame carries, or `None` for a frame that isn't
/// an opening frame.
fn opening_fields(header: &Header) -> Option<(VarInt, Priority)> {
    match header {
        Header::QueueData {
            dest_acceptor_id,
            priority,
            ..
        }
        | Header::QueueMsg {
            dest_acceptor_id,
            priority,
            ..
        } => Some(((*dest_acceptor_id)?, *priority)),
        Header::QueueReset { init, .. } => {
            let init = init.as_ref()?;
            Some((init.dest_acceptor_id, init.priority))
        }
        _ => None,
    }
}

/// Before the server confirms the stream, the FIN is an opening frame: it carries the tier of the
/// stream's first opening frame, though `set_priority` changed the writer's tier in between.
//= rendered/0005-endpoint/03-binding.md#DC-0005-018
//= type=test
//# A client MUST carry the same acceptor and the same credit tier on every opening frame of a
//# stream.
#[test]
fn a_fin_before_confirmation_carries_the_tier_of_the_first_opening_frame() {
    sim(|| {
        let builder = PairBuilder::default().with_priority(Priority::Low);
        let (mut writer, mut pusher) = builder.build();

        async move {
            let opened = Some((acceptor_id(), Priority::Low));

            let frames = pusher.recv_frames().await;
            assert_eq!(frames.len(), 1);
            assert_eq!(opening_fields(&frames.front().unwrap().header), opened);

            let fin_frames = pusher
                .recv_frames_timeout(Duration::from_secs(1))
                .await
                .expect("expected the FIN after shutdown");
            assert_eq!(fin_frames.len(), 1);
            let fin = &fin_frames.front().unwrap().header;
            assert!(matches!(fin, Header::QueueData { is_fin: true, .. }));
            assert_eq!(opening_fields(fin), opened);
        }
        .primary()
        .spawn();

        async move {
            let mut payload = Bytes::from_static(b"hello");
            let written = writer.write_from(&mut payload).await.expect("first write");
            assert_eq!(written, 5);
            assert!(writer.0.status.is_init_sent());

            // Yield so the endpoint task takes the first opening frame before the tier changes.
            bach::task::yield_now().await;

            writer.set_priority(Priority::High);
            writer.shutdown().expect("shutdown");

            // Later credit acquires use the new tier.
            assert_eq!(writer.priority(), Priority::High);
        }
        .primary()
        .spawn();
    });
}

/// Before the server confirms the stream, a reset carries the acceptor and the tier of the
/// stream's first opening frame, though `set_priority` changed the writer's tier in between.
//= rendered/0005-endpoint/03-binding.md#DC-0005-018
//= type=test
//# A client MUST carry the same acceptor and the same credit tier on every opening frame of a
//# stream.
#[test]
fn a_reset_before_confirmation_carries_the_tier_of_the_first_opening_frame() {
    sim(|| {
        let builder = PairBuilder::default().with_priority(Priority::Low);
        let (mut writer, mut pusher) = builder.build();

        async move {
            let opened = Some((acceptor_id(), Priority::Low));

            let frames = pusher.recv_frames().await;
            assert_eq!(frames.len(), 1);
            assert_eq!(opening_fields(&frames.front().unwrap().header), opened);

            let reset_frames = pusher
                .recv_frames_timeout(Duration::from_secs(1))
                .await
                .expect("expected the reset after the panic drop");
            assert_eq!(reset_frames.len(), 1);
            let reset = &reset_frames.front().unwrap().header;
            assert!(matches!(reset, Header::QueueReset { .. }));
            assert_eq!(opening_fields(reset), opened);
        }
        .primary()
        .spawn();

        async move {
            let mut payload = Bytes::from_static(b"hello");
            let written = writer.write_from(&mut payload).await.expect("first write");
            assert_eq!(written, 5);
            assert!(writer.0.status.is_init_sent());

            // Yield so the endpoint task takes the first opening frame before the tier changes.
            bach::task::yield_now().await;

            writer.set_priority(Priority::High);
            let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _writer = writer;
                panic!("intentional test panic before the server confirms the stream");
            }));
            assert!(panic_result.is_err());
        }
        .primary()
        .spawn();
    });
}
