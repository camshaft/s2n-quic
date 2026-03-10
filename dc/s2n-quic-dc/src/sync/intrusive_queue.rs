// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub use crate::intrusive_queue::{self as queue, Entry};
use parking_lot::Mutex;

pub struct Queue<T>(Mutex<queue::Queue<T>>);

impl<T> Default for Queue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Queue<T> {
    pub fn new() -> Self {
        Self(Mutex::new(queue::Queue::new()))
    }

    /// Returns true if the queue is empty
    pub fn is_empty(&self) -> bool {
        self.0.lock().is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.lock().len()
    }

    /// Push an entry to the back of the queue
    pub fn push_back(&self, entry: Entry<T>) {
        self.0.lock().push_back(entry);
    }

    /// Pop an entry from the front of the queue
    ///
    /// Returns None if the queue is empty.
    pub fn pop_front(&self) -> Option<Entry<T>> {
        self.0.lock().pop_front()
    }

    /// Append an entire queue in one lock acquisition.
    pub fn append(&self, other: &mut queue::Queue<T>) {
        self.0.lock().append(other);
    }

    /// Takes the inner queue from the shared queue
    pub fn take(&self) -> queue::Queue<T> {
        core::mem::take(&mut *self.0.lock())
    }
}
