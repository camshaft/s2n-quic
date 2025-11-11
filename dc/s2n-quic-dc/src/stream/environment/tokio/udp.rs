// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    event,
    stream::{
        environment::{tokio::Environment, Peer, SetupResult},
        recv::{buffer, dispatch::Control},
        socket::{application::Single, Tracing, Wheel},
        TransportFeatures,
    },
};
use s2n_quic_core::inet::SocketAddress;
use std::{net::UdpSocket, sync::Arc};

pub(super) type ArcSocket = Arc<UdpSocket>;
pub(super) type WorkerSendSocket = Arc<Tracing<Wheel>>;
pub(super) type ApplicationSendSocket = Arc<Single<Tracing<Wheel>>>;

#[derive(Debug)]
pub struct Pooled(pub SocketAddress);

impl<Sub> Peer<Environment<Sub>> for Pooled
where
    Sub: event::Subscriber + Clone,
{
    type ReadWorkerSocket = WorkerSendSocket;
    type WriteWorkerSocket = (WorkerSendSocket, buffer::Channel<Control>);

    #[inline]
    fn features(&self) -> TransportFeatures {
        TransportFeatures::UDP
    }

    #[inline]
    fn setup(
        self,
        env: &Environment<Sub>,
    ) -> SetupResult<Self::ReadWorkerSocket, Self::WriteWorkerSocket> {
        let peer_addr = self.0;
        let recv_pool = env.recv_pool.as_ref().expect("pool not configured");
        // the client doesn't need to associate credentials since it's already chosen a queue_id
        let credentials = None;
        let (control, stream, application_socket, worker_socket, transmission_pool) =
            recv_pool.alloc(credentials);
        crate::stream::environment::udp::Pooled {
            peer_addr,
            control,
            stream,
            application_socket,
            transmission_pool,
            worker_socket,
        }
        .setup(env)
    }
}
