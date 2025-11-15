// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    credentials::Credentials,
    event,
    socket::pool,
    stream::{
        environment::{tokio::Environment, Peer, SetupResult, SocketSet},
        recv::shared::RecvBuffer,
        server::tokio::tcp::LazyBoundStream,
        TransportFeatures,
    },
};
use s2n_quic_core::inet::SocketAddress;
use tokio::net::TcpStream;

/// A socket that is already registered with the application runtime
pub struct Registered {
    pub socket: TcpStream,
    pub peer_addr: SocketAddress,
    pub local_port: u16,
    pub recv_buffer: RecvBuffer,
    pub transmission_pool: pool::Sharded,
}

impl<Sub> Peer<Environment<Sub>> for Registered
where
    Sub: event::Subscriber + Clone,
{
    type ReadWorkerSocket = ();
    type WriteWorkerSocket = ();

    fn features(&self) -> TransportFeatures {
        TransportFeatures::TCP
    }

    #[inline]
    fn setup(
        self,
        _env: &Environment<Sub>,
        _credentials: Option<&Credentials>,
    ) -> SetupResult<Self::ReadWorkerSocket, Self::WriteWorkerSocket> {
        let remote_addr = self.peer_addr;
        let application = Box::new(self.socket);
        let transmission_pool = self.transmission_pool;
        let socket = SocketSet {
            application,
            read_worker: None,
            write_worker: None,
            remote_addr,
            transmission_pool,
            source_queue_id: None,
        };
        Ok((socket, self.recv_buffer))
    }
}

/// A socket that should be reregistered with the application runtime
pub struct Reregistered {
    pub socket: LazyBoundStream,
    pub peer_addr: SocketAddress,
    pub local_port: u16,
    pub recv_buffer: RecvBuffer,
    pub transmission_pool: pool::Sharded,
}

impl<Sub> Peer<Environment<Sub>> for Reregistered
where
    Sub: event::Subscriber + Clone,
{
    type ReadWorkerSocket = ();
    type WriteWorkerSocket = ();

    fn features(&self) -> TransportFeatures {
        TransportFeatures::TCP
    }

    #[inline]
    fn setup(
        self,
        _env: &Environment<Sub>,
        _credentials: Option<&Credentials>,
    ) -> SetupResult<Self::ReadWorkerSocket, Self::WriteWorkerSocket> {
        let remote_addr = self.peer_addr;
        let application = Box::new(self.socket.into_std()?);
        let transmission_pool = self.transmission_pool;
        let socket = SocketSet {
            application,
            read_worker: None,
            write_worker: None,
            remote_addr,
            transmission_pool,
            source_queue_id: None,
        };
        Ok((socket, self.recv_buffer))
    }
}
