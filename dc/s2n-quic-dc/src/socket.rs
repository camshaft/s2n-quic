// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(all(not(loom), target_os = "linux"))]
mod bpf;
// this module is used on platforms other than linux, but we still want to make
// sure it compiles
#[cfg(not(loom))]
#[cfg_attr(target_os = "linux", allow(dead_code))]
mod pair;

#[cfg(all(not(loom), target_os = "linux"))]
pub use bpf::Pair;
#[cfg(all(not(loom), not(target_os = "linux")))]
pub use pair::Pair;

#[cfg(not(loom))]
pub mod channel;
#[cfg(loom)]
pub mod channel {
    use core::{
        mem::MaybeUninit,
        task::{Context, Poll},
    };

    pub trait Receiver<T> {
        fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>>;

        fn on_consumed(&mut self, bytes: u64);
    }

    pub trait Sender<T> {
        fn poll_send(
            &mut self,
            cx: &mut Context<'_>,
            slot: &mut MaybeUninit<T>,
        ) -> Poll<Result<(), ()>>;
    }

    pub trait UnboundedSender<T> {
        fn send(&mut self, value: T) -> Result<(), T>;
    }

    pub mod intrusive_queue {
        #[path = "sharded.rs"]
        pub mod sharded;
    }
}
#[cfg(not(loom))]
pub mod pool;
#[cfg(not(loom))]
pub mod rate;
#[cfg(not(loom))]
pub mod recv;
#[cfg(not(loom))]
pub mod send;

#[cfg(not(loom))]
pub use s2n_quic_platform::socket::options::{Options, ReusePort};
