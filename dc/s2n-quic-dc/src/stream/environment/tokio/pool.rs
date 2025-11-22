// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::udp::{ApplicationSendSocket, ArcSocket, WorkerSendSocket};
use crate::{
    clock::{tokio::Clock, Clock as _},
    credentials::Credentials,
    event,
    socket::{
        pool::{self, Pool as Packets},
        recv::{router::Router, udp},
        send,
    },
    stream::{
        self,
        environment::{
            tokio::Environment,
            udp::{Config, Workers},
            Environment as _,
        },
        recv::dispatch::{Allocator as Queues, Control, Stream},
        server::{accept, udp::Acceptor},
        socket::{application::Single, BusyPoll, Gso, SendOnly, Tracing},
    },
};
use s2n_quic_platform::socket::options::{Options, ReusePort};
use std::{
    io::Result,
    net::{SocketAddr, UdpSocket},
    sync::{Arc, Mutex},
};
use tokio::io::unix::AsyncFd;
use tracing::Instrument;

pub(super) struct Pool {
    sockets: Box<[PoolSocket]>,
    local_addr: SocketAddr,
    transmission_pool: pool::Sharded,
}

struct PoolSocket {
    socket: ArcSocket,
    worker: WorkerSendSocket,
    application: Box<[ApplicationSendSocket]>,
    recv_queue: Mutex<Queues>,
}

macro_rules! spawn_span {
    ($span:expr, $task:ident, | $spanned:ident | $spawn:block) => {
        let span = $span;
        if span.is_disabled() {
            let $spanned = $task;
            $spawn
        } else {
            let $spanned = ($task).instrument(span);
            $spawn
        }
    };
}

impl PoolSocket {
    fn new(socket: UdpSocket, recv_queue: Mutex<Queues>, config: &Config, clock: &Clock) -> Self {
        let socket = Arc::new(socket);
        let local_addr = socket.local_addr().unwrap();

        let create_socket = || {
            let wheel = send::wheel::Wheel::new(config.send_wheel_horizon, clock);
            let socket = stream::socket::Wheel::new(wheel, local_addr.into());
            Tracing(socket)
        };

        let worker = Arc::new(create_socket());

        let application = (0..config.priority_levels)
            .map(|_| Arc::new(Single(create_socket())))
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            worker,
            application,
            socket,
            recv_queue,
        }
    }

    fn create_send_socket_worker(
        &self,
        config: &Config,
        env: &Environment<impl event::Subscriber + Clone>,
        timer: impl crate::clock::precision::Timer + Send + 'static,
        wake_mode: impl send::udp::WakeMode + Send + 'static,
    ) -> impl core::future::Future<Output = ()> + Send + 'static {
        let socket = Tracing(Gso(SendOnly(self.socket.clone()), env.gso.clone()));

        let mut wheels = vec![send::wheel::Wheel::clone(&self.worker)];

        for application in &self.application {
            wheels.push(send::wheel::Wheel::clone(application));
        }

        let token_bucket = config.bucket();

        send::udp::non_blocking(socket, wheels, timer, token_bucket, wake_mode)
    }

    fn spawn_non_blocking_send_worker(
        &self,
        config: &Config,
        env: &Environment<impl event::Subscriber + Clone>,
    ) {
        let timer = env.clock().timer();
        let wake_mode = send::udp::WithWaker;
        let task = self.create_send_socket_worker(config, env, timer, wake_mode);
        let span = tracing::trace_span!("send_socket_worker");
        spawn_span!(span, task, |task| {
            env.writer_rt.spawn(task);
        });
    }

    fn spawn_non_blocking_recv_worker(
        &self,
        _config: &Config,
        env: &Environment<impl event::Subscriber + Clone>,
        alloc: pool::Pool,
        router: impl Router + Send + 'static,
    ) {
        let recv_socket = Tracing(AsyncFd::new(self.socket.clone()).unwrap());
        let task = udp::non_blocking(recv_socket, alloc, router);
        let span = tracing::trace_span!("recv_socket_worker");
        spawn_span!(span, task, |task| {
            env.reader_rt.spawn(task);
        });
    }

    fn spawn_busy_poll_send_worker(
        &self,
        config: &Config,
        env: &Environment<impl event::Subscriber + Clone>,
        handle: &crate::busy_poll::Handle,
    ) {
        let timer = crate::busy_poll::clock::Timer::new(env.clock());
        let wake_mode = send::udp::BusyPoll;
        let task = self.create_send_socket_worker(config, env, timer, wake_mode);
        let span = tracing::trace_span!("send_socket_worker");
        spawn_span!(span, task, |task| {
            handle.spawn_with_priority(task, config.flow_priority);
        });
    }

    fn spawn_busy_poll_recv_worker(
        &self,
        config: &Config,
        _env: &Environment<impl event::Subscriber + Clone>,
        alloc: pool::Pool,
        router: impl Router + Send + 'static,
        handle: &crate::busy_poll::Handle,
    ) {
        let recv_socket = BusyPoll(self.socket.clone());
        let task = udp::non_blocking(recv_socket, alloc, router);
        let span = tracing::trace_span!("recv_socket_worker");
        spawn_span!(span, task, |task| {
            handle.spawn_with_priority(task, config.flow_priority);
        });
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

        config.send_workers.set_default(workers);
        config.recv_workers.set_default(workers);

        if acceptor.is_some() && config.socket_count() > 1 {
            config.reuse_port = true;
        }

        let create_queue = || {
            if acceptor.is_some() {
                Queues::new_non_zero(config.stream_recv_queue, config.control_recv_queue)
            } else {
                Queues::new(config.stream_recv_queue, config.control_recv_queue)
            }
        };
        let sockets = Self::create_workers(&env.clock(), options, &config, create_queue)?;

        let local_addr = sockets[0].socket.local_addr()?;
        if cfg!(debug_assertions) && config.reuse_port {
            for socket in sockets.iter().skip(1) {
                debug_assert_eq!(local_addr, socket.socket.local_addr()?);
            }
        }

        let unroutable_packets = {
            // TODO pace these packets
            let socket = Tracing(SendOnly(sockets[0].socket.clone()));
            let (tx, task) = config.unroutable_packets(socket);

            env.reader_rt.spawn(task);

            tx
        };

        let transmission_pool = config.tx_packet_pool(workers);

        macro_rules! spawn {
            ($create_router:expr) => {
                let _rt = env.reader_rt.enter();
                Self::spawn_non_blocking(env, &config, &sockets, $create_router)?;
            };
        }

        if let Some(sender) = acceptor {
            spawn!(|_packets: &Packets, socket: &PoolSocket| {
                let queues = socket.recv_queue.lock().unwrap();
                let app_socket = socket.application[0].clone();
                let worker_socket = socket.worker.clone();

                // TODO pace these packets
                let secret_socket = Tracing(SendOnly(sockets[0].socket.clone()));

                let acceptor = Acceptor::new(
                    env.clone(),
                    sender.clone(),
                    config.map.clone(),
                    config.accept_flavor,
                    queues.clone(),
                    app_socket,
                    worker_socket,
                    secret_socket,
                    transmission_pool.clone(),
                    unroutable_packets.clone(),
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
            transmission_pool,
        })
    }

    pub fn alloc(
        &self,
        credentials: &Credentials,
    ) -> (
        Control,
        Stream,
        ApplicationSendSocket,
        WorkerSendSocket,
        pool::Sharded,
    ) {
        // "Pick 2" worker selection
        let idx = self.pick_worker();

        let socket = &self.sockets[idx];

        let (control, stream) = socket.recv_queue.lock().unwrap().alloc_or_grow(credentials);

        // Application sockets currently only have 1 priority
        // TODO take this in as a parameter
        let priority = 0;

        let worker_socket = socket.worker.clone();
        let app_socket = socket.application[priority].clone();
        let transmission_pool = self.transmission_pool.clone();

        (
            control,
            stream,
            app_socket,
            worker_socket,
            transmission_pool,
        )
    }

    /// Implements "pick 2" load balancing: select two random workers and choose the one with lower queue length
    fn pick_worker(&self) -> usize {
        if self.sockets.len() == 1 {
            return 0;
        }

        let idx1 = rand::random_range(..self.sockets.len());
        let mut idx2 = rand::random_range(..self.sockets.len() - 1);
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

    fn spawn_non_blocking<R>(
        env: &Environment<impl event::Subscriber + Clone>,
        config: &Config,
        sockets: &[PoolSocket],
        create_router: impl Fn(&Packets, &PoolSocket) -> R,
    ) -> Result<()>
    where
        R: 'static + Send + Router,
    {
        for (udp_socket_worker, socket) in sockets.iter().enumerate() {
            let alloc = config.rx_packet_pool();
            let router = create_router(&alloc, socket);

            match &config.send_workers {
                Workers::BusyPoll(pool) => {
                    let idx = udp_socket_worker % pool.len();
                    let handle = &pool[idx];
                    socket.spawn_busy_poll_send_worker(config, env, handle);
                }
                Workers::Environment(_) => {
                    socket.spawn_non_blocking_send_worker(config, env);
                }
            }

            match &config.recv_workers {
                Workers::BusyPoll(pool) => {
                    let idx = udp_socket_worker % pool.len();
                    let handle = &pool[idx];
                    socket.spawn_busy_poll_recv_worker(config, env, alloc, router, handle);
                }
                Workers::Environment(_) => {
                    socket.spawn_non_blocking_recv_worker(config, env, alloc, router);
                }
            }
        }
        Ok(())
    }

    fn create_workers(
        clock: &Clock,
        mut options: Options,
        config: &Config,
        create_queue: impl Fn() -> Queues,
    ) -> Result<Vec<PoolSocket>> {
        let mut sockets = vec![];

        let shared_queue = if config.reuse_port {
            // if we are reusing the port, we need to share the queue_ids
            Some(create_queue())
        } else {
            // otherwise, each worker can get its own queue to reduce thread contention
            None
        };

        let socket_count = config.socket_count();

        for i in 0..socket_count {
            let socket = if i == 0 && socket_count > 1 {
                if config.reuse_port {
                    // set reuse port after we bind for the first socket
                    options.reuse_port = ReusePort::AfterBind;
                }
                let socket = options.build_udp()?;

                if config.reuse_port {
                    // for any additional sockets, set reuse port before bind
                    options.reuse_port = ReusePort::BeforeBind;

                    // in case the application bound to a wildcard, resolve the local address
                    options.addr = socket.local_addr()?;
                }

                socket
            } else {
                options.build_udp()?
            };

            let queue = if let Some(shared_queue) = &shared_queue {
                shared_queue.clone()
            } else {
                create_queue()
            };

            let queue = Mutex::new(queue);
            let socket = PoolSocket::new(socket, queue, config, clock);

            sockets.push(socket);
        }

        Ok(sockets)
    }
}
