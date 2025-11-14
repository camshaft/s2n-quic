// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use parking_lot::Mutex;
use std::{
    fmt,
    future::Future,
    ops,
    pin::Pin,
    sync::{Arc, Weak},
    task::Context,
};

pub mod clock;

#[derive(Clone)]
pub struct Pool {
    handles: Arc<[Handle]>,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Pool").finish_non_exhaustive()
    }
}

impl Pool {
    pub fn new(handles: Arc<[Handle]>) -> Self {
        Self { handles }
    }
}

impl<T> From<T> for Pool
where
    Arc<[Handle]>: From<T>,
{
    fn from(handles: T) -> Self {
        Self::new(Arc::from(handles))
    }
}

impl ops::Deref for Pool {
    type Target = [Handle];

    fn deref(&self) -> &Self::Target {
        &self.handles
    }
}

#[derive(Clone)]
pub struct Handle {
    state: Arc<Mutex<State>>,
}

impl Handle {
    pub fn new() -> (Self, Runner) {
        let state = Arc::new(Mutex::new(State {
            spawns: Vec::with_capacity(16),
        }));
        let handle = Self {
            state: state.clone(),
        };
        let runner = Runner(Arc::downgrade(&state));
        (handle, runner)
    }

    pub fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.spawn_with_priority(task, None);
    }

    pub fn spawn_with_priority<F>(&self, task: F, priority: Option<u8>)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let task = Box::pin(task);
        let priority = priority.unwrap_or(128);
        let task = Task { task, priority };
        self.state.lock().spawns.push(task);
    }
}

struct State {
    spawns: Vec<Task>,
}

struct Task {
    task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    priority: u8,
}

#[must_use]
pub struct Runner(Weak<Mutex<State>>);

impl Runner {
    pub fn run(self) {
        let state = self.0;
        let waker = s2n_quic_core::task::waker::noop();
        let mut cx = Context::from_waker(&waker);
        let mut tasks = Tasks::new();
        let mut spawns = Vec::with_capacity(16);

        loop {
            const ITERATIONS: usize = if cfg!(debug_assertions) { 1000 } else { 10_000 };

            for _ in 0..ITERATIONS {
                tasks.poll(&mut cx);
            }

            if let Some(state) = state.upgrade() {
                if let Some(mut guard) = state.try_lock() {
                    core::mem::swap(&mut spawns, &mut guard.spawns);
                }
            } else {
                return;
            }

            if spawns.is_empty() {
                continue;
            }

            for spawn in spawns.drain(..) {
                tasks.spawn(spawn);
            }

            tasks.after_spawn();
        }
    }
}

struct Tasks {
    slots: Vec<Option<Task>>,
    free: Vec<usize>,
}

impl Tasks {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    fn spawn(&mut self, task: Task) {
        if let Some(idx) = self.free.pop() {
            self.slots[idx] = Some(task);
        } else {
            self.slots.push(Some(task));
        }
    }

    fn after_spawn(&mut self) {
        self.free.clear();

        self.slots.sort_by(|a, b| {
            match (a, b) {
                // priority 0 is highest
                (Some(a), Some(b)) => a.priority.cmp(&b.priority),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
        });

        // clear out the free slots
        while self.slots.last().map_or(false, Option::is_none) {
            self.slots.pop();
        }
    }

    fn poll(&mut self, cx: &mut Context) {
        for (idx, slot) in self.slots.iter_mut().enumerate() {
            if let Some(task) = slot {
                if task.task.as_mut().poll(cx).is_ready() {
                    eprintln!("task {idx} done");
                    *slot = None;
                    self.free.push(idx);
                }
            }
        }
    }
}
