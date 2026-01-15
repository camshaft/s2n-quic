use std::{
    pin::Pin,
    task::{Poll, Waker},
};

pub trait Task {
    fn set_waker(self: Pin<&mut Self>, waker: Waker);
    fn poll(self: Pin<&mut Self>) -> Poll<()>;
}
