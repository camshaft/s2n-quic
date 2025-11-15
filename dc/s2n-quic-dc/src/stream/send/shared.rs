// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    packet::stream::PacketSpace,
    socket::send::completion::{Completer, Completion},
    stream::{
        packet_number,
        send::{flow, path, queue::Queue, state::transmission},
        shared::{CompletionQueue, ShutdownKind},
    },
    task::waker::worker::Waker as WorkerWaker,
};
use core::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};
use crossbeam_queue::SegQueue;
use s2n_quic_core::recovery::bandwidth::Bandwidth;
use std::sync::{Arc, Weak};
use tracing::trace;

#[derive(Debug)]
pub struct Message {
    /// The event being submitted to the worker
    pub event: Event,
}

#[derive(Debug)]
pub enum Event {
    Shutdown { kind: ShutdownKind, queue: Queue },
}

pub struct State {
    pub flow: flow::non_blocking::State,
    pub packet_number: packet_number::Counter,
    pub path: path::State,
    pub worker_waker: WorkerWaker,
    bandwidth: AtomicU64,
    /// A channel sender for pushing transmission information to the worker task
    ///
    /// We use an unbounded sender since we already rely on flow control to apply backpressure
    worker_queue: SegQueue<Message>,
    pub transmission_queue: transmission::Queue,
    pub completion_handle: CompletionQueue<Weak<dyn AsShared>>,
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("send::shared::State")
            .field("flow", &self.flow)
            .field("packet_number", &self.packet_number)
            .field("path", &self.path)
            .finish()
    }
}

pub trait AsShared: 'static + Send + Sync {
    fn as_shared(&self) -> &State;
}

impl Completion<transmission::PacketInfo, transmission::Meta> for Weak<dyn AsShared> {
    type Completer = Arc<dyn AsShared>;

    fn upgrade(&self) -> Option<Self::Completer> {
        Weak::upgrade(self)
    }
}

impl Completer<transmission::PacketInfo, transmission::Meta, Weak<dyn AsShared>>
    for Arc<dyn AsShared>
{
    fn complete(self, transmission: transmission::Entry) {
        let shared = self.as_shared();
        shared
            .transmission_queue
            .complete_transmission(transmission);
        shared.worker_waker.wake();
    }
}

impl State {
    #[inline]
    pub fn new(
        flow: flow::non_blocking::State,
        path: path::Info,
        bandwidth: Option<Bandwidth>,
    ) -> Self {
        let path = path::State::new(path);
        let bandwidth = bandwidth.map(|v| v.serialize()).unwrap_or(u64::MAX).into();
        Self {
            flow,
            packet_number: Default::default(),
            path,
            bandwidth,
            // this will get set once the waker spawns
            worker_waker: Default::default(),
            worker_queue: Default::default(),
            transmission_queue: Default::default(),
            completion_handle: CompletionQueue::uninit(),
        }
    }

    #[inline]
    pub fn bandwidth(&self) -> Bandwidth {
        Bandwidth::deserialize(self.bandwidth.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn set_bandwidth(&self, value: Bandwidth) {
        self.bandwidth.store(value.serialize(), Ordering::Relaxed);
    }

    #[inline]
    pub fn pop_worker_message(&self) -> Option<Message> {
        self.worker_queue.pop()
    }

    pub fn on_prune(&self) {
        self.shutdown(ShutdownKind::Pruned, Queue::default());
    }

    #[inline]
    pub fn shutdown(&self, kind: ShutdownKind, queue: Queue) {
        trace!(event = "shutdown", ?kind);
        let message = Message {
            event: Event::Shutdown { kind, queue },
        };
        self.worker_queue.push(message);
        self.worker_waker.wake();
    }

    pub fn alloc_transmission(
        &self,
        batch_size: usize,
        packet_space: PacketSpace,
    ) -> transmission::Entry {
        let completion_queue = || unsafe { self.completion_handle.load() };
        let mut entry = self
            .transmission_queue
            .alloc_entry(batch_size, completion_queue);
        entry.meta.packet_space = packet_space;
        entry
    }
}
