// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use core::fmt;
use std::{
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

/// An entry in the intrusive queue
///
/// Contains the value and links to the previous and next entries.
pub struct Entry<T>(Box<Inner<T>>);

struct Inner<T> {
    value: T,
    prev: Option<NonNull<Inner<T>>>,
    next: Option<Entry<T>>,
}

unsafe impl<T: Send> Send for Entry<T> {}
unsafe impl<T: Sync> Sync for Entry<T> {}

impl<T: fmt::Debug> fmt::Debug for Entry<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.value.fmt(f)
    }
}

impl<T: Clone> Clone for Entry<T> {
    fn clone(&self) -> Self {
        debug_assert!(self.0.prev.is_none());
        debug_assert!(self.0.next.is_none());
        Self::new(self.0.value.clone())
    }
}

impl<T: Default> Default for Entry<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> Entry<T> {
    /// Create a new entry with the given value
    pub fn new(value: T) -> Self {
        Self(Box::new(Inner {
            value,
            prev: None,
            next: None,
        }))
    }

    /// Consume the entry and return the value
    pub fn into_inner(self) -> T {
        self.0.value
    }
}

impl<T> Deref for Entry<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0.value
    }
}

impl<T> DerefMut for Entry<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0.value
    }
}

/// An intrusive FIFO queue
///
/// This is a doubly-linked list where elements are pushed to the back
/// and popped from the front. The queue owns all entries through Box pointers.
pub struct Queue<T> {
    head: Option<Entry<T>>,
    tail: Option<NonNull<Inner<T>>>,
}

unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Sync> Sync for Queue<T> {}

impl<T> Queue<T> {
    /// Create a new empty queue
    pub const fn new() -> Self {
        Self {
            head: None,
            tail: None,
        }
    }

    /// Returns true if the queue is empty
    pub fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    /// Push an entry to the back of the queue
    pub fn push_back(&mut self, mut entry: Entry<T>) {
        debug_assert!(entry.0.prev.is_none());
        debug_assert!(entry.0.next.is_none());

        entry.0.prev = self.tail;

        let new_tail = NonNull::from(entry.0.as_ref());

        // If there's a tail, link it to the new entry
        if let Some(tail) = self.tail {
            unsafe {
                (*tail.as_ptr()).next = Some(entry);
            }
        } else {
            // Queue was empty, this is the new head
            self.head = Some(entry);
        }

        self.tail = Some(new_tail);
    }

    /// Pop an entry from the front of the queue
    ///
    /// Returns None if the queue is empty.
    pub fn pop_front(&mut self) -> Option<Entry<T>> {
        let mut head = self.head.take()?;

        // Take ownership of the next entry
        self.head = head.0.next.take();

        // Update the new head's prev pointer
        if let Some(ref mut new_head) = self.head {
            new_head.0.prev = None;
        } else {
            // Queue is now empty, clear tail
            self.tail = None;
        }

        // Clear the popped entry's pointers
        head.0.prev = None;
        head.0.next = None;

        Some(head)
    }

    /// Peek at the front entry without removing it
    pub fn peek_front(&self) -> Option<&T> {
        self.head.as_deref()
    }

    /// Peek at the front entry mutably without removing it
    pub fn peek_front_mut(&mut self) -> Option<&mut T> {
        self.head.as_deref_mut()
    }

    /// Peek at the back entry without removing it
    pub fn peek_back(&self) -> Option<&T> {
        self.tail.map(|tail| &unsafe { &*tail.as_ptr() }.value)
    }

    /// Peek at the back entry mutably without removing it
    pub fn peek_back_mut(&mut self) -> Option<&mut T> {
        self.tail
            .map(|mut tail| &mut unsafe { tail.as_mut() }.value)
    }

    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            next: self.head.as_ref(),
        }
    }
}

impl<T> Default for Queue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        // Pop all entries to ensure proper cleanup
        while self.pop_front().is_some() {}
    }
}

pub struct Iter<'a, T> {
    next: Option<&'a Entry<T>>,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        let current = self.next.take()?;
        self.next = current.0.next.as_ref();
        Some(&current.0.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bolero::{check, TypeGenerator};
    use std::collections::VecDeque;

    #[test]
    fn test_push_pop() {
        let mut queue = Queue::new();

        assert!(queue.is_empty());
        assert!(queue.pop_front().is_none());

        queue.push_back(Entry::new(1));
        queue.push_back(Entry::new(2));
        queue.push_back(Entry::new(3));

        assert!(!queue.is_empty());

        let entry1 = queue.pop_front().unwrap();
        assert_eq!(*entry1, 1);

        let entry2 = queue.pop_front().unwrap();
        assert_eq!(*entry2, 2);

        let entry3 = queue.pop_front().unwrap();
        assert_eq!(*entry3, 3);

        assert!(queue.is_empty());
        assert!(queue.pop_front().is_none());
    }

    #[test]
    fn test_peek() {
        let mut queue = Queue::new();

        assert!(queue.peek_front().is_none());
        assert!(queue.peek_back().is_none());

        queue.push_back(Entry::new(1));
        assert_eq!(*queue.peek_front().unwrap(), 1);
        assert_eq!(*queue.peek_back().unwrap(), 1);

        queue.push_back(Entry::new(2));
        assert_eq!(*queue.peek_front().unwrap(), 1);
        assert_eq!(*queue.peek_back().unwrap(), 2);

        queue.push_back(Entry::new(3));
        assert_eq!(*queue.peek_front().unwrap(), 1);
        assert_eq!(*queue.peek_back().unwrap(), 3);
    }

    #[test]
    fn test_peek_mut() {
        let mut queue = Queue::new();

        queue.push_back(Entry::new(1));
        queue.push_back(Entry::new(2));

        *queue.peek_front_mut().unwrap() = 10;
        *queue.peek_back_mut().unwrap() = 20;

        assert_eq!(*queue.pop_front().unwrap(), 10);
        assert_eq!(*queue.pop_front().unwrap(), 20);
    }

    #[test]
    fn test_into_value() {
        let mut queue = Queue::new();

        queue.push_back(Entry::new(42));
        let entry = queue.pop_front().unwrap();
        let value = entry.into_inner();

        assert_eq!(value, 42);
    }

    #[test]
    fn test_single_element() {
        let mut queue = Queue::new();

        queue.push_back(Entry::new(100));
        assert!(!queue.is_empty());

        let entry = queue.pop_front().unwrap();
        assert_eq!(*entry, 100);

        assert!(queue.is_empty());
    }

    #[test]
    fn test_push_pop_interleaved() {
        let mut queue = Queue::new();

        queue.push_back(Entry::new(1));
        queue.push_back(Entry::new(2));

        assert_eq!(*queue.pop_front().unwrap(), 1);

        queue.push_back(Entry::new(3));
        queue.push_back(Entry::new(4));

        assert_eq!(*queue.pop_front().unwrap(), 2);
        assert_eq!(*queue.pop_front().unwrap(), 3);

        queue.push_back(Entry::new(5));

        assert_eq!(*queue.pop_front().unwrap(), 4);
        assert_eq!(*queue.pop_front().unwrap(), 5);

        assert!(queue.is_empty());
    }

    #[derive(Clone, Copy, Debug, TypeGenerator)]
    enum Operation {
        Push,
        Pop,
    }

    #[test]
    fn differential_test() {
        check!().with_type::<Vec<Operation>>().for_each(|ops| {
            let mut values = 0u64..;
            let mut oracle = VecDeque::new();
            let mut subject = Queue::new();

            for op in ops {
                match op {
                    Operation::Push => {
                        let value = values.next().unwrap();
                        oracle.push_back(value);
                        subject.push_back(Entry::new(value));
                    }
                    Operation::Pop => {
                        assert_eq!(oracle.pop_front(), subject.pop_front().map(|entry| *entry));
                    }
                }
            }

            while let Some(expected) = oracle.pop_front() {
                let actual = *subject.pop_front().unwrap();
                assert_eq!(expected, actual);
            }
            assert!(subject.pop_front().is_none());
        })
    }
}
