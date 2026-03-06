// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    event::{self, Subscriber},
    stream::{
        application::{Builder as StreamBuilder, Stream},
        server::stats,
    },
    sync::mpmc as channel,
};
use std::{io, net::SocketAddr, time::Duration};

mod pruner;

pub use pruner::Pruner;
use s2n_quic_core::time::Timestamp;

#[derive(Clone, Copy, Debug, Default)]
pub enum Flavor {
    #[default]
    Fifo,
    Lifo,
}

pub struct Entry<Sub: Subscriber>(Option<StreamBuilder<Sub>>);

impl<Sub: Subscriber> Entry<Sub> {
    pub fn new(builder: StreamBuilder<Sub>) -> Self {
        Self(Some(builder))
    }

    pub fn accept(mut self) -> io::Result<(Stream<Sub>, Duration)> {
        let builder = self.0.take().unwrap();
        builder.accept()
    }

    #[track_caller]
    pub fn prune(mut self, reason: event::builder::AcceptorStreamPruneReason) {
        let builder = self.0.take().unwrap();
        builder.prune(reason)
    }

    pub fn queue_time(&self) -> Timestamp {
        self.0.as_ref().unwrap().queue_time
    }

    /// Returns `true` if the stream already has an error set on its shared state.
    ///
    /// This can happen when a client closes the stream while it's still waiting
    /// in the accept queue.
    pub fn has_error(&self) -> bool {
        self.0.as_ref().unwrap().shared.get_error().is_some()
    }
}

impl<Sub: Subscriber> From<StreamBuilder<Sub>> for Entry<Sub> {
    fn from(builder: StreamBuilder<Sub>) -> Self {
        Self::new(builder)
    }
}

impl<Sub: Subscriber> Drop for Entry<Sub> {
    fn drop(&mut self) {
        if let Some(builder) = self.0.take() {
            builder.prune(event::builder::AcceptorStreamPruneReason::ServerClosed)
        }
    }
}

pub type Sender<Sub> = channel::Sender<Entry<Sub>>;
pub type Receiver<Sub> = channel::Receiver<Entry<Sub>>;

#[inline]
pub fn channel<Sub>(capacity: usize) -> (Sender<Sub>, Receiver<Sub>)
where
    Sub: event::Subscriber,
{
    channel::new(capacity)
}

#[inline]
pub async fn accept<Sub>(
    streams: &Receiver<Sub>,
    stats: &stats::Sender,
) -> io::Result<(Stream<Sub>, SocketAddr)>
where
    Sub: event::Subscriber,
{
    loop {
        let entry = streams.recv_front().await.map_err(|_err| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "server acceptor runtime is no longer available",
            )
        })?;

        // Skip entries where the client has already closed/errored the stream
        // while it was sitting in the accept queue. Dropping the entry will
        // trigger its prune cleanup.
        if entry.has_error() {
            // TODO ideally this would be pushed to a background task so we don't block accepting
            drop(entry);
            continue;
        }

        // build the stream inside the application context
        let (stream, sojourn_time) = entry.accept()?;
        stats.send(sojourn_time);

        let remote_addr = stream.peer_addr()?;

        return Ok((stream, remote_addr));
    }
}
