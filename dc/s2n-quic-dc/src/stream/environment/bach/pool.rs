// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::udp::{ApplicationSendSocket, ArcSocket, WorkerSendSocket};
use crate::{
    clock::bach::Clock,
    credentials::Credentials,
    event,
    socket::{
        pool::Pool as Packets,
        recv::{router::Router, udp},
        send,
    },
    stream::{
        self,
        environment::{bach::Environment, udp::Config, Environment as _},
        recv::dispatch::{Allocator as Queues, Control, Stream},
        server::{accept, udp::Acceptor},
        socket::{application::Single, Tracing},
    },
};
use bach::{
    net::{socket, SocketAddr, UdpSocket},
    rand::Any,
};
use s2n_quic_platform::socket::options::{Options, ReusePort};
use std::{
    io::Result,
    sync::{Arc, Mutex},
};
use tracing::Instrument;

pub(super) struct Pool {
    sockets: Box<[PoolSocket]>,
    local_addr: SocketAddr,
}

struct PoolSocket {
    socket: ArcSocket,
    worker: WorkerSendSocket,
    application: Box<[ApplicationSendSocket]>,
    recv_queue: Mutex<Queues>,
    descriptor_pool: Arc<Packets>,
}

impl PoolSocket {
    fn new(
        socket: Arc<UdpSocket>,
        recv_queue: Mutex<Queues>,
        config: &Config,
        clock: &Clock,
    ) -> Self {
        let local_addr = socket.local_addr().unwrap();

        let create_socket = || {
            let wheel = send::wheel::Wheel::new(config.send_wheel_horizon, clock);
            stream::socket::Wheel::new(wheel, local_addr)
        };

        let worker = Arc::new(Tracing(create_socket()));

        let application = (0..config.priority_levels)
            .map(|_| Arc::new(Single(Tracing(create_socket()))))
            .collect::<Vec<_>>()
            .into_boxed_slice();

        let descriptor_pool = Arc::new(Packets::new(config.max_packet_size, config.packet_count));

        Self {
            worker,
            application,
            socket,
            recv_queue,
            descriptor_pool,
        }
    }

    fn spawn_send_worker(&self, config: &Config, clock: Clock) {
        let socket = self.socket.clone();

        let mut wheels = vec![(***self.worker).clone()];

        for application in &self.application {
            wheels.push((*****application).clone());
        }

        let token_bucket = config.token_bucket();

        let span = tracing::trace_span!("send_socket_worker");
        let task = async move {
            send::udp::non_blocking(socket, wheels, clock, token_bucket).await;
        };

        if span.is_disabled() {
            bach::spawn(task);
        } else {
            bach::spawn(task.instrument(span));
        }
    }
}

impl Pool {
    pub fn new<Sub>(
        env: &Environment<Sub>,
        workers: usize,
        mut config: Config,
        acceptor: Option<accept::Sender<Sub>>,
    ) -> Result<Self>
    where
        Sub: event::Subscriber + Clone,
    {
        debug_assert_ne!(workers, 0);

        let options = env.socket_options.clone();

        if config.workers.is_none() {
            config.workers = Some(workers);
        }

        let sockets = Self::create_workers(&env.clock(), options, &config)?;

        let local_addr = sockets[0].socket.local_addr()?;

        if cfg!(debug_assertions) && config.reuse_port {
            for socket in sockets.iter().skip(1) {
                debug_assert_eq!(local_addr, socket.socket.local_addr()?);
            }
        }

        let max_packet_size = config.max_packet_size;
        let packet_count = config.packet_count;
        let create_packets = || Packets::new(max_packet_size, packet_count);

        let unroutable_packets = {
            let socket = sockets[0].worker.clone();
            let (tx, task) = config.unroutable_packets(socket);

            bach::spawn(task);

            tx
        };

        macro_rules! spawn {
            ($create_router:expr) => {
                Self::spawn_non_blocking(env, &config, &sockets, create_packets, $create_router)?;
            };
        }

        if let Some(sender) = acceptor {
            spawn!(|_packets: &Packets, socket: &PoolSocket| {
                let queues = socket.recv_queue.lock().unwrap();
                let app_socket = socket.application[0].clone();
                let worker_socket = socket.worker.clone();
                let descriptor_pool = socket.descriptor_pool.clone();

                let acceptor = Acceptor::new(
                    env.clone(),
                    sender.clone(),
                    config.map.clone(),
                    config.accept_flavor,
                    queues.clone(),
                    app_socket,
                    worker_socket,
                    unroutable_packets.clone(),
                    descriptor_pool,
                );

                let router = queues
                    .dispatcher(unroutable_packets.clone())
                    .with_map(config.map.clone());
                router.with_zero_router(acceptor)
            });
        } else {
            spawn!(|_packets: &Packets, socket: &PoolSocket| {
                let dispatch = socket
                    .recv_queue
                    .lock()
                    .unwrap()
                    .dispatcher(unroutable_packets.clone());
                dispatch.with_map(config.map.clone())
            });
        }

        Ok(Self {
            sockets: sockets.into(),
            local_addr,
        })
    }

    pub fn alloc(
        &self,
        credentials: Option<&Credentials>,
    ) -> (Control, Stream, ApplicationSendSocket, WorkerSendSocket, Arc<Packets>) {
        // "Pick 2" worker selection
        let idx = self.pick_worker();

        let socket = &self.sockets[idx];

        let (control, stream) = socket.recv_queue.lock().unwrap().alloc_or_grow(credentials);

        // Application sockets currently only have 1 priority
        // TODO take this in as a parameter
        let priority = 0;

        let worker_socket = socket.worker.clone();
        let app_socket = socket.application[priority].clone();
        let descriptor_pool = socket.descriptor_pool.clone();

        (control, stream, app_socket, worker_socket, descriptor_pool)
    }

    /// Implements "pick 2" load balancing: select two random workers and choose the one with lower queue length
    fn pick_worker(&self) -> usize {
        if self.sockets.len() == 1 {
            return 0;
        }

        let idx1: usize = (0..self.sockets.len()).any();
        let mut idx2: usize = (0..self.sockets.len() - 1).any();
        // shift the second index up so they're non-overlapping
        if idx2 >= idx1 {
            idx2 += 1;
        }

        // Choose the worker with lower queue length
        let len1 = self.sockets[idx1].application[0].len();
        let len2 = self.sockets[idx2].application[0].len();

        if len1 <= len2 {
            idx1
        } else {
            idx2
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn spawn_non_blocking<Sub, R>(
        env: &Environment<Sub>,
        config: &Config,
        sockets: &[PoolSocket],
        create_packets: impl Fn() -> Packets,
        create_router: impl Fn(&Packets, &PoolSocket) -> R,
    ) -> Result<()>
    where
        Sub: event::Subscriber + Clone,
        R: 'static + Send + Router,
    {
        for (udp_socket_worker, socket) in sockets.iter().enumerate() {
            let packets = create_packets();
            let router = create_router(&packets, socket);
            let recv_socket = socket.socket.clone();
            let span = tracing::trace_span!("udp_socket_worker", udp_socket_worker);
            let task = async move {
                udp::non_blocking(recv_socket, packets, router).await;
            };
            if span.is_disabled() {
                bach::spawn(task);
            } else {
                bach::spawn(task.instrument(span));
            }

            socket.spawn_send_worker(config, env.clock())
        }
        Ok(())
    }

    fn create_workers(
        clock: &Clock,
        mut options: Options,
        config: &Config,
    ) -> Result<Vec<PoolSocket>> {
        let mut sockets = vec![];

        let stream_cap = config.stream_recv_queue;
        let control_cap = config.control_recv_queue;

        let shared_queue = if config.reuse_port {
            // if we are reusing the port, we need to share the queue_ids
            Some(Queues::new_non_zero(stream_cap, control_cap))
        } else {
            // otherwise, each worker can get its own queue to reduce thread contention
            None
        };

        let workers = config.workers.unwrap_or(1).max(1);

        for i in 0..workers {
            let socket = if i == 0 && workers > 1 {
                if config.reuse_port {
                    // set reuse port after we bind for the first socket
                    options.reuse_port = ReusePort::AfterBind;
                }
                let socket = build_udp(&options)?;

                if config.reuse_port {
                    // for any additional sockets, set reuse port before bind
                    options.reuse_port = ReusePort::BeforeBind;

                    // in case the application bound to a wildcard, resolve the local address
                    options.addr = socket.local_addr()?;
                }

                socket
            } else {
                build_udp(&options)?
            };

            let queue = if let Some(shared_queue) = &shared_queue {
                shared_queue.clone()
            } else {
                Queues::new(stream_cap, control_cap)
            };

            let socket = Arc::new(socket);
            let queue = Mutex::new(queue);
            let socket = PoolSocket::new(socket, queue, config, clock);

            sockets.push(socket);
        }

        Ok(sockets)
    }
}

fn build_udp(options: &Options) -> Result<UdpSocket> {
    let mut opts = socket::Options::default();
    opts.local_addr = options.addr;
    opts.reuse_port = !matches!(options.reuse_port, ReusePort::Disabled);

    // TODO send buffer, recv buffer

    let socket = opts.build_udp()?;
    Ok(socket)
}
