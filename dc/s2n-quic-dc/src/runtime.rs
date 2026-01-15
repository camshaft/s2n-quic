use std::{future::Future, pin::Pin};

mod runner;
mod spawn;
mod task;
mod wakers;

use spawn::Sender;

type ExternalTask = Pin<Box<dyn task::Task + Send + 'static>>;
type InternalTask = Pin<Box<dyn task::Task + 'static>>;

#[derive(Clone)]
pub struct Handle {
    external_tasks: Sender<ExternalTask>,
}

impl Handle {
    pub fn spawn<F>(&self, f: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        todo!()
    }
}
