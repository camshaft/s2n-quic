use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    sync::{Arc, Weak},
};

pub struct Sender<T>(Arc<Mutex<VecDeque<T>>>);

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Default for Sender<T> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(VecDeque::new())))
    }
}

impl<T> Sender<T> {
    pub fn push(&self, value: T) {
        self.0.lock().push_back(value);
    }
}

pub struct Closed;

pub struct Receiver<T> {
    queue: Weak<Mutex<VecDeque<T>>>,
    local: VecDeque<T>,
}

impl<T> Receiver<T> {
    pub fn pop(&mut self) -> Result<Option<T>, Closed> {
        loop {
            if let Some(value) = self.local.pop_front() {
                return Ok(Some(value));
            }

            let Some(queue) = self.queue.upgrade() else {
                return Err(Closed);
            };

            let mut queue = queue.lock();
            core::mem::swap(&mut self.local, &mut *queue);
            if queue.is_empty() {
                return Ok(None);
            }
        }
    }
}
