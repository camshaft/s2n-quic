// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    event,
    stream::{
        runtime,
        send::{
            application::{Inner, Writer},
            queue,
        },
        shared::ArcShared,
        socket,
    },
};
use s2n_quic_core::time::Timestamp;

pub struct Builder<Sub>
where
    Sub: event::Subscriber,
{
    runtime: runtime::ArcHandle<Sub>,
    start_time: Option<Timestamp>,
}

impl<Sub> Builder<Sub>
where
    Sub: event::Subscriber,
{
    #[inline]
    pub fn new(runtime: runtime::ArcHandle<Sub>, start_time: Option<Timestamp>) -> Self {
        Self {
            runtime,
            start_time,
        }
    }

    #[inline]
    pub fn build(self, shared: ArcShared<Sub>, sockets: socket::ArcApplication) -> Writer<Sub> {
        let Self {
            runtime,
            start_time,
        } = self;

        let timer = shared.clock.timer();

        let mut queue = Default::default();

        // If we have a rate-limited start time, initialize the queue's transmission
        // time so the first packet is delayed until that time.
        if let Some(start_time) = start_time {
            queue::Queue::set_initial_transmission_time(&mut queue, start_time.into());
        }

        Writer(Box::new(Inner {
            shared,
            sockets,
            queue,
            timer,
            status: Default::default(),
            runtime,
        }))
    }
}
