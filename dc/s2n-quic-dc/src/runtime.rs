// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Runtime abstraction for stream endpoint initialization.
//!
//! This provides a generic interface for spawning tasks and obtaining clocks across
//! different runtimes (busy-poll, tokio, bach) while respecting worker affinity for
//! non-Send types.
//!
//! The key challenge is that busy-poll uses a two-phase spawn pattern:
//! 1. Call `handle.spawn_local(|spawner| { ... })` with a Send closure
//! 2. Inside that closure, use `spawner.spawn(future)` to spawn !Send futures
//!
//! This abstraction needs to support both this pattern and simpler runtimes like tokio.

use crate::{counter, socket::channel};
use crate::time::precision;
use s2n_quic_core::time;
use std::future::Future;

/// Describes a spawned pipeline task for runtime-level introspection.
#[derive(Clone, Debug)]
pub struct TaskRegistration {
    pub name: String,
    pub description: String,
    pub function: String,
    pub budget: Option<usize>,
    pub metrics: Vec<String>,
}

impl TaskRegistration {
    #[inline]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        function: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            function: function.into(),
            budget: None,
            metrics: Vec::new(),
        }
    }

    #[inline]
    pub fn with_budget(mut self, budget: Option<usize>) -> Self {
        self.budget = budget;
        self
    }

    #[inline]
    pub fn with_metric(mut self, metric: impl Into<String>) -> Self {
        self.metrics.push(metric.into());
        self
    }
}

/// Describes a channel/queue edge between tasks for runtime-level introspection.
#[derive(Clone, Debug)]
pub struct ChannelRegistration {
    pub name: String,
    pub description: String,
    pub function: String,
    pub from_task: String,
    pub to_task: String,
    pub metrics: Vec<String>,
}

impl ChannelRegistration {
    #[inline]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        function: impl Into<String>,
        from_task: impl Into<String>,
        to_task: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            function: function.into(),
            from_task: from_task.into(),
            to_task: to_task.into(),
            metrics: Vec::new(),
        }
    }

    #[inline]
    pub fn with_metric(mut self, metric: impl Into<String>) -> Self {
        self.metrics.push(metric.into());
        self
    }
}

/// Abstraction over a task runtime and its associated clock.
///
/// Each runtime implementation bundles both spawning capability and the clock
/// appropriate for that execution model (e.g. busy-poll timers that don't use wakers,
/// tokio timers backed by the tokio runtime, bach simulated time).
pub trait Runtime: Clone + Send + 'static {
    /// The clock type associated with this runtime.
    type Clock: time::Clock + precision::Clock + Clone + Send + 'static;

    /// The worker-local spawner type for spawning !Send futures.
    type Spawner<'a>: Spawner;

    /// Number of workers in this runtime.
    fn worker_count(&self) -> usize;

    /// Returns a clone of the runtime's clock.
    fn clock(&self) -> Self::Clock;

    /// Spawn !Send futures on a specific worker.
    ///
    /// The closure receives a spawner handle that can spawn !Send futures.
    /// This matches the busy-poll pattern where you call spawn_local with a Send closure
    /// that receives a Spawner to spawn !Send futures.
    fn spawn_local<F>(&self, worker_id: usize, f: F)
    where
        F: FnOnce(Self::Spawner<'_>) + Send + 'static;
}

/// Handle for spawning !Send futures within a worker-local context.
pub trait Spawner {
    /// Spawn a !Send future on the current worker.
    fn spawn<F>(&mut self, future: F)
    where
        F: Future<Output = ()> + 'static;

    /// Register metadata describing a task spawned on this worker.
    #[inline]
    fn register_task(&mut self, _task: TaskRegistration) {}

    /// Register metadata describing a queue/channel edge on this worker.
    #[inline]
    fn register_channel(&mut self, _channel: ChannelRegistration) {}

    /// Spawn a !Send future and register metadata for the task.
    #[inline]
    fn spawn_named<F>(&mut self, task: TaskRegistration, future: F)
    where
        F: Future<Output = ()> + 'static,
    {
        self.register_task(task);
        self.spawn(future);
    }

    /// Convenience helper for pipeline receiver tasks with budget and task counters.
    #[inline]
    fn spawn_receiver_task<R>(
        &mut self,
        task: TaskRegistration,
        receiver: R,
        budget: Option<usize>,
        task_counter: counter::Task,
    ) where
        R: channel::Receiver<()> + 'static,
        Self: Sized,
    {
        use crate::socket::channel::ReceiverExt as _;
        let task = task.with_budget(budget);
        self.spawn_named(task, receiver.drain_budgeted_metered(budget, task_counter));
    }
}

// ── BusyPoll Implementation ────────────────────────────────────────────────

/// Implementations for busy_poll runtime
pub mod busy_poll {
    use super::{Runtime, Spawner};
    use crate::busy_poll::clock;
    use std::future::Future;

    /// Busy-poll runtime: a pool of polling workers with a wall-clock timer.
    #[derive(Clone)]
    pub struct Handle {
        pool: crate::busy_poll::Pool,
        clock: clock::Clock,
    }

    impl Handle {
        pub fn new(pool: crate::busy_poll::Pool) -> Self {
            Self {
                pool,
                clock: clock::Clock::new(),
            }
        }
    }

    impl Runtime for Handle {
        type Clock = clock::Timer;
        type Spawner<'a> = crate::busy_poll::Spawner<'a>;

        fn worker_count(&self) -> usize {
            self.pool.len()
        }

        fn clock(&self) -> Self::Clock {
            use crate::time::precision::Clock as _;
            self.clock.timer()
        }

        fn spawn_local<F>(&self, worker_id: usize, f: F)
        where
            F: FnOnce(Self::Spawner<'_>) + Send + 'static,
        {
            self.pool[worker_id].spawn_local(f);
        }
    }

    impl Spawner for crate::busy_poll::Spawner<'_> {
        fn spawn<F>(&mut self, future: F)
        where
            F: Future<Output = ()> + 'static,
        {
            crate::busy_poll::Spawner::spawn(self, future);
        }
    }
}

// ── Bach Implementation ────────────────────────────────────────────────────

/// Bach runtime for deterministic testing
#[cfg(any(test, feature = "testing"))]
pub mod bach {
    use super::{Runtime, Spawner};
    use std::future::Future;

    /// Bach runtime for deterministic testing.
    ///
    /// Bach is single-threaded but we emulate multiple workers for testing worker affinity logic.
    #[derive(Clone)]
    pub struct Handle {
        worker_count: usize,
        clock: crate::time::bach::Clock,
    }

    impl Handle {
        pub fn new(worker_count: usize) -> Self {
            Self {
                worker_count,
                clock: crate::time::bach::Clock::default(),
            }
        }
    }

    /// Bach local spawner
    pub struct Local;

    /// Wrapper to make !Send futures Send for bach's API
    struct SendWrapper<F>(F);

    // SAFETY: Bach is single-threaded and never executes concurrently
    unsafe impl<F> Send for SendWrapper<F> {}
    unsafe impl<F> Sync for SendWrapper<F> {}

    impl<F> Future for SendWrapper<F>
    where
        F: Future,
    {
        type Output = F::Output;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            unsafe {
                std::future::Future::poll(
                    std::pin::Pin::new_unchecked(&mut self.get_unchecked_mut().0),
                    cx,
                )
            }
        }
    }

    impl Spawner for Local {
        fn spawn<F>(&mut self, future: F)
        where
            F: Future<Output = ()> + 'static,
        {
            ::bach::spawn(SendWrapper(future));
        }
    }

    impl Runtime for Handle {
        type Clock = crate::time::bach::Clock;
        type Spawner<'a> = Local;

        fn worker_count(&self) -> usize {
            self.worker_count
        }

        fn clock(&self) -> Self::Clock {
            self.clock.clone()
        }

        fn spawn_local<F>(&self, _worker_id: usize, f: F)
        where
            F: FnOnce(Self::Spawner<'_>) + Send + 'static,
        {
            let local = Local;
            f(local);
        }
    }
}

// ── Tokio Implementation ───────────────────────────────────────────────────

/// Tokio runtime with single-threaded runtimes per worker
pub mod tokio {
    use super::{Runtime, Spawner};
    use std::future::Future;

    /// Tokio runtime with single-threaded runtimes per worker.
    ///
    /// Each worker is a LocalSet that can run !Send futures.
    #[derive(Clone)]
    pub struct Handle {
        workers: std::sync::Arc<Vec<WorkerHandle>>,
        clock: crate::time::tokio::Clock,
    }

    struct WorkerHandle {
        sender: tokio::sync::mpsc::UnboundedSender<WorkItem>,
    }

    type WorkItem = Box<dyn FnOnce(Local) + Send>;

    impl Handle {
        /// Create a new tokio runtime with the specified number of workers.
        pub fn new(worker_count: usize) -> Self {
            let workers: Vec<_> = (0..worker_count)
                .map(|_| {
                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WorkItem>();

                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("failed to build tokio runtime");

                        let local = tokio::task::LocalSet::new();

                        local.block_on(&rt, async move {
                            while let Some(work) = rx.recv().await {
                                let spawner = Local;
                                work(spawner);
                            }
                        });
                    });

                    WorkerHandle { sender: tx }
                })
                .collect();

            Self {
                workers: std::sync::Arc::new(workers),
                clock: crate::time::tokio::Clock::default(),
            }
        }
    }

    /// Tokio local spawner that uses spawn_local within a LocalSet.
    pub struct Local;

    impl Spawner for Local {
        fn spawn<F>(&mut self, future: F)
        where
            F: Future<Output = ()> + 'static,
        {
            tokio::task::spawn_local(future);
        }
    }

    impl Runtime for Handle {
        type Clock = crate::time::tokio::Clock;
        type Spawner<'a> = Local;

        fn worker_count(&self) -> usize {
            self.workers.len()
        }

        fn clock(&self) -> Self::Clock {
            self.clock.clone()
        }

        fn spawn_local<F>(&self, worker_id: usize, f: F)
        where
            F: FnOnce(Self::Spawner<'_>) + Send + 'static,
        {
            let work: WorkItem = Box::new(f);
            self.workers[worker_id]
                .sender
                .send(work)
                .expect("worker thread died");
        }
    }
}

/// Wrapper runtime that records per-worker task/channel registrations and can emit DOT graphs.
pub mod inspector {
    use super::{ChannelRegistration, Runtime, Spawner, TaskRegistration};
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    pub struct Handle<R: Runtime> {
        inner: R,
        state: Arc<Mutex<State>>,
    }

    #[derive(Default)]
    struct State {
        tasks: Vec<RegisteredTask>,
        channels: Vec<RegisteredChannel>,
    }

    #[derive(Clone)]
    struct RegisteredTask {
        worker_id: usize,
        task: TaskRegistration,
    }

    #[derive(Clone)]
    struct RegisteredChannel {
        worker_id: usize,
        channel: ChannelRegistration,
    }

    impl<R: Runtime> Handle<R> {
        pub fn new(inner: R) -> Self {
            Self {
                inner,
                state: Arc::new(Mutex::new(State::default())),
            }
        }

        pub fn to_dot(&self) -> String {
            let state = self.state.lock().expect("inspector lock poisoned");
            let mut out = String::from("digraph pipeline {\n  rankdir=LR;\n  compound=true;\n");

            let mut by_worker: BTreeMap<usize, Vec<_>> = BTreeMap::new();
            for task in &state.tasks {
                by_worker
                    .entry(task.worker_id)
                    .or_default()
                    .push(task.task.clone());
            }

            let mut task_node_ids = std::collections::HashMap::new();
            for worker_id in 0..self.inner.worker_count() {
                out.push_str(&format!("  subgraph cluster_worker_{worker_id} {{\n"));
                out.push_str(&format!("    label=\"worker {worker_id}\";\n"));
                if let Some(tasks) = by_worker.get(&worker_id) {
                    for (idx, task) in tasks.iter().enumerate() {
                        let node_id = format!("w{worker_id}_t{idx}");
                        task_node_ids.insert((worker_id, task.name.clone()), node_id.clone());
                        let budget = task
                            .budget
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "none".to_string());
                        let metrics = if task.metrics.is_empty() {
                            "none".to_string()
                        } else {
                            task.metrics.join(", ")
                        };
                        let label = format!(
                            "{}\\nfn: {}\\nbudget: {}\\nmetrics: {}\\n{}",
                            task.name, task.function, budget, metrics, task.description
                        );
                        out.push_str(&format!(
                            "    {node_id} [shape=box,label=\"{}\"];\n",
                            escape_dot(&label)
                        ));
                    }
                }
                out.push_str("  }\n");
            }

            for channel in &state.channels {
                let worker_id = channel.worker_id;
                let Some(from) = task_node_ids.get(&(worker_id, channel.channel.from_task.clone())) else {
                    continue;
                };
                let Some(to) = task_node_ids.get(&(worker_id, channel.channel.to_task.clone())) else {
                    continue;
                };
                let metrics = if channel.channel.metrics.is_empty() {
                    "none".to_string()
                } else {
                    channel.channel.metrics.join(", ")
                };
                let label = format!(
                    "{}\\nfn: {}\\nmetrics: {}\\n{}",
                    channel.channel.name,
                    channel.channel.function,
                    metrics,
                    channel.channel.description
                );
                out.push_str(&format!(
                    "  {from} -> {to} [label=\"{}\"];\n",
                    escape_dot(&label)
                ));
            }

            out.push_str("}\n");
            out
        }
    }

    fn escape_dot(input: &str) -> String {
        input.replace('\\', "\\\\").replace('"', "\\\"")
    }

    impl<R: Runtime> Runtime for Handle<R> {
        type Clock = R::Clock;
        type Spawner<'a> = Local<R::Spawner<'a>>;

        fn worker_count(&self) -> usize {
            self.inner.worker_count()
        }

        fn clock(&self) -> Self::Clock {
            self.inner.clock()
        }

        fn spawn_local<F>(&self, worker_id: usize, f: F)
        where
            F: FnOnce(Self::Spawner<'_>) + Send + 'static,
        {
            let state = self.state.clone();
            self.inner.spawn_local(worker_id, move |spawner| {
                f(Local {
                    inner: spawner,
                    worker_id,
                    state,
                });
            });
        }
    }

    pub struct Local<S> {
        inner: S,
        worker_id: usize,
        state: Arc<Mutex<State>>,
    }

    impl<S: Spawner> Spawner for Local<S> {
        fn spawn<F>(&mut self, future: F)
        where
            F: std::future::Future<Output = ()> + 'static,
        {
            self.inner.spawn(future);
        }

        fn register_task(&mut self, task: TaskRegistration) {
            self.state
                .lock()
                .expect("inspector lock poisoned")
                .tasks
                .push(RegisteredTask {
                    worker_id: self.worker_id,
                    task: task.clone(),
                });
            self.inner.register_task(task);
        }

        fn register_channel(&mut self, channel: ChannelRegistration) {
            self.state
                .lock()
                .expect("inspector lock poisoned")
                .channels
                .push(RegisteredChannel {
                    worker_id: self.worker_id,
                    channel: channel.clone(),
                });
            self.inner.register_channel(channel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokio_runtime_worker_count() {
        let rt = tokio::Handle::new(4);
        assert_eq!(rt.worker_count(), 4);
    }

    #[test]
    fn bach_runtime_worker_count() {
        let rt = bach::Handle::new(4);
        assert_eq!(rt.worker_count(), 4);
    }

    #[test]
    fn inspector_runtime_emits_dot_with_worker_task_and_channel_metadata() {
        let rt = inspector::Handle::new(bach::Handle::new(2));

        rt.spawn_local(1, |mut local| {
            local.spawn_named(
                TaskRegistration::new(
                    "task.producer",
                    "Produces work for downstream pipeline stages",
                    "tests::producer",
                )
                .with_budget(Some(32))
                .with_metric("task.producer.drained"),
                async {},
            );

            local.spawn_named(
                TaskRegistration::new(
                    "task.consumer",
                    "Consumes work from producer",
                    "tests::consumer",
                )
                .with_metric("task.consumer.drained"),
                async {},
            );

            local.register_channel(
                ChannelRegistration::new(
                    "ch.producer_to_consumer",
                    "Unsync queue carrying producer items",
                    "tests::channel",
                    "task.producer",
                    "task.consumer",
                )
                .with_metric("q.producer_to_consumer.depth"),
            );
        });

        let dot = rt.to_dot();
        assert!(dot.contains("cluster_worker_1"));
        assert!(dot.contains("task.producer"));
        assert!(dot.contains("task.consumer"));
        assert!(dot.contains("ch.producer_to_consumer"));
        assert!(dot.contains("budget: 32"));
    }
}
