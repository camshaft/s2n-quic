// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    event,
    stream::{
        application::{Builder as StreamBuilder, Stream},
        server::stats,
    },
    sync::mpmc as channel,
};
use std::{fmt, io, net::SocketAddr};

mod pruner;

pub use pruner::Pruner;

#[derive(Clone, Copy, Debug, Default)]
pub enum Flavor {
    #[default]
    Fifo,
    Lifo,
}

pub type Sender<Sub> = channel::Sender<StreamBuilder<Sub>>;
pub type Receiver<Sub> = channel::Receiver<StreamBuilder<Sub>>;

#[inline]
pub fn channel<Sub>(capacity: usize) -> (Sender<Sub>, Receiver<Sub>)
where
    Sub: event::Subscriber,
{
    channel::new(capacity)
}

/// The inner error of the [`io::Error`] that [`accept`] returns once the stream channel closes
///
/// That error's kind is [`io::ErrorKind::NotConnected`], and every later call to [`accept`]
/// returns it too. The error of a single stream that fails while it's accepted doesn't hold a
/// `Closed`, so a caller tells a closed channel apart from one stream's failure with
/// `err.get_ref().is_some_and(|inner| inner.is::<Closed>())`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Closed;

impl fmt::Display for Closed {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("server acceptor runtime is no longer available")
    }
}

impl std::error::Error for Closed {}

#[inline]
pub async fn accept<Sub>(
    streams: &Receiver<Sub>,
    stats: &stats::Sender,
) -> io::Result<(Stream<Sub>, SocketAddr)>
where
    Sub: event::Subscriber,
{
    let stream = streams.recv_front().await.map_err(|_err| {
        // the inner `Closed` sets this error apart from one stream's failed accept
        io::Error::new(io::ErrorKind::NotConnected, Closed)
    })?;

    // build the stream inside the application context
    let (stream, sojourn_time) = stream.accept()?;
    stats.send(sojourn_time);

    let remote_addr = stream.peer_addr()?;

    Ok((stream, remote_addr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{ext::*, sim, NoopSubscriber};

    #[test]
    fn closed_channel_error_holds_closed() {
        sim(|| {
            let (sender, receiver) = channel::<NoopSubscriber>(1);
            let (stats_sender, _worker, _stats) = stats::channel();

            // dropping the only sender closes the channel
            drop(sender);

            async move {
                // every call after the channel closes returns the same error
                for _ in 0..2 {
                    let err = accept(&receiver, &stats_sender).await.unwrap_err();
                    assert_eq!(err.kind(), io::ErrorKind::NotConnected);
                    assert!(err.get_ref().is_some_and(|inner| inner.is::<Closed>()));
                    assert_eq!(err.to_string(), "server acceptor runtime is no longer available");
                }
            }
            .primary()
            .spawn();
        });
    }
}
