use super::{spawn::Receiver, wakers::Wakers, ExternalTask, InternalTask};
use std::{ops::ControlFlow, task::Waker};

pub struct Runner {
    external_tasks: Receiver<ExternalTask>,
    active: Vec<Option<InternalTask>>,
    free: Vec<usize>,
    wakers: Wakers,
}

impl Runner {
    fn poll(&mut self) {
        // TODO set the local scope

        for index in self.wakers.drain() {
            let slot = unsafe { self.active.get_unchecked_mut(index) };
            let Some(task) = slot.as_mut() else {
                continue;
            };
            let task = task.as_mut();
            if task.poll().is_ready() {
                *slot = None;
                self.free.push(index);
            }
        }
    }

    fn handle_spawns(&mut self) -> ControlFlow<()> {
        loop {
            match self.external_tasks.pop() {
                Ok(Some(mut task)) => {
                    let (index, waker) = self.allocate();
                    waker.wake_by_ref();
                    task.as_mut().set_waker(waker);
                    unsafe {
                        *self.active.get_unchecked_mut(index) = Some(task);
                    }
                }
                Ok(None) => return ControlFlow::Continue(()),
                Err(_) => return ControlFlow::Break(()),
            }
        }
    }

    fn allocate(&mut self) -> (usize, Waker) {
        if let Some(index) = self.free.pop() {
            let waker = self.wakers.waker(index);
            return (index, waker);
        }
        let index = self.active.len();
        let waker = self.wakers.waker(index);
        self.active.push(None);
        (index, waker)
    }
}
