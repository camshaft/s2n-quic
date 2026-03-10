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

type Link<T> = NonNull<Inner<T>>;

struct Inner<T> {
    value: T,
    prev: Option<Link<T>>,
    next: Option<Link<T>>,
}

unsafe impl<T: Send> Send for Entry<T> {}
unsafe impl<T: Sync> Sync for Entry<T> {}

impl<T> Inner<T> {
    #[inline(always)]
    fn assert_unlinked(&self) {
        if cfg!(debug_assertions) {
            debug_assert!(self.prev.is_none());
            debug_assert!(self.next.is_none());
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Entry<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.value.fmt(f)
    }
}

impl<T: Clone> Clone for Entry<T> {
    fn clone(&self) -> Self {
        self.0.assert_unlinked();
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
        let inner = Inner {
            value,
            prev: None,
            next: None,
        };
        Self(Box::new(inner))
    }

    /// Consume the entry and return the value
    pub fn into_inner(self) -> T {
        let inner = self.0;
        inner.assert_unlinked();
        inner.value
    }

    #[inline(always)]
    fn assert_unlinked(&self) {
        if cfg!(debug_assertions) {
            self.0.assert_unlinked();
        }
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
    head: Option<Link<T>>,
    tail: Option<Link<T>>,
    len: usize,
}

unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Sync> Sync for Queue<T> {}

impl<T> Queue<T> {
    /// Create a new empty queue
    pub const fn new() -> Self {
        Self {
            head: None,
            tail: None,
            len: 0,
        }
    }

    /// Returns true if the queue is empty
    pub fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    /// Returns the number of entries in the queue
    pub fn len(&self) -> usize {
        self.len
    }

    /// Push an entry to the back of the queue
    pub fn push_back(&mut self, entry: Entry<T>) {
        entry.assert_unlinked();

        // Leak the box to get a raw pointer we can store in the queue
        let new_tail = NonNull::from(Box::leak(entry.0));

        unsafe {
            // Set the new entry's prev pointer to the current tail
            (*new_tail.as_ptr()).prev = self.tail;

            // If there's a tail, link it to the new entry
            if let Some(tail) = self.tail {
                (*tail.as_ptr()).next = Some(new_tail);
            } else {
                // Queue was empty, this is the new head
                self.head = Some(new_tail);
            }
        }

        self.tail = Some(new_tail);
        self.len += 1;
    }

    /// Pop an entry from the front of the queue
    ///
    /// Returns None if the queue is empty.
    pub fn pop_front(&mut self) -> Option<Entry<T>> {
        let head_ptr = self.head.take()?;

        unsafe {
            // Get the next pointer from the head
            let next = (*head_ptr.as_ptr()).next;
            self.head = next;

            // Update the new head's prev pointer
            if let Some(new_head) = self.head {
                (*new_head.as_ptr()).prev = None;
            } else {
                // Queue is now empty, clear tail
                self.tail = None;
            }

            // Clear the popped entry's pointers
            (*head_ptr.as_ptr()).prev = None;
            (*head_ptr.as_ptr()).next = None;

            self.len -= 1;

            // Reconstruct the Entry from the leaked box
            Some(Entry(Box::from_raw(head_ptr.as_ptr())))
        }
    }

    /// Peek at the front entry without removing it
    pub fn peek_front(&self) -> Option<&T> {
        self.head.map(|head| unsafe { &(*head.as_ptr()).value })
    }

    /// Peek at the front entry mutably without removing it
    pub fn peek_front_mut(&mut self) -> Option<&mut T> {
        self.head.map(|head| unsafe { &mut (*head.as_ptr()).value })
    }

    /// Peek at the back entry without removing it
    pub fn peek_back(&self) -> Option<&T> {
        self.tail.map(|tail| unsafe { &(*tail.as_ptr()).value })
    }

    /// Peek at the back entry mutably without removing it
    pub fn peek_back_mut(&mut self) -> Option<&mut T> {
        self.tail.map(|tail| unsafe { &mut (*tail.as_ptr()).value })
    }

    /// Append another queue to the back of this queue.
    ///
    /// This is O(1) — just a pointer splice. The `other` queue is left empty.
    pub fn append(&mut self, other: &mut Queue<T>) {
        let Some(other_head) = other.head.take() else {
            // other is empty
            return;
        };
        let other_tail = other.tail.take().unwrap();
        let other_len = other.len;
        other.len = 0;

        if let Some(tail) = self.tail {
            unsafe {
                (*tail.as_ptr()).next = Some(other_head);
                (*other_head.as_ptr()).prev = Some(tail);
            }
            self.tail = Some(other_tail);
        } else {
            // self is empty
            self.head = Some(other_head);
            self.tail = Some(other_tail);
        }

        self.len += other_len;
    }

    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            next: self.head,
            len: self.len,
            _phantom: std::marker::PhantomData,
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
    next: Option<Link<T>>,
    len: usize,
    _phantom: std::marker::PhantomData<&'a T>,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        let current = self.next.take()?;
        unsafe {
            let inner = &*current.as_ptr();
            self.next = inner.next;
            self.len -= 1;
            Some(&inner.value)
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len;
        (len, Some(len))
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

    #[test]
    fn test_append_both_non_empty() {
        let mut a = Queue::new();
        a.push_back(Entry::new(1));
        a.push_back(Entry::new(2));

        let mut b = Queue::new();
        b.push_back(Entry::new(3));
        b.push_back(Entry::new(4));

        a.append(&mut b);

        assert!(b.is_empty());
        assert_eq!(*a.pop_front().unwrap(), 1);
        assert_eq!(*a.pop_front().unwrap(), 2);
        assert_eq!(*a.pop_front().unwrap(), 3);
        assert_eq!(*a.pop_front().unwrap(), 4);
        assert!(a.is_empty());
    }

    #[test]
    fn test_append_to_empty() {
        let mut a = Queue::new();
        let mut b = Queue::new();
        b.push_back(Entry::new(10));
        b.push_back(Entry::new(20));

        a.append(&mut b);

        assert!(b.is_empty());
        assert_eq!(*a.pop_front().unwrap(), 10);
        assert_eq!(*a.pop_front().unwrap(), 20);
        assert!(a.is_empty());
    }

    #[test]
    fn test_append_empty_other() {
        let mut a = Queue::new();
        a.push_back(Entry::new(1));

        let mut b = Queue::new();
        a.append(&mut b);

        assert_eq!(*a.pop_front().unwrap(), 1);
        assert!(a.is_empty());
    }

    #[test]
    fn test_append_both_empty() {
        let mut a: Queue<u64> = Queue::new();
        let mut b: Queue<u64> = Queue::new();
        a.append(&mut b);
        assert!(a.is_empty());
        assert!(b.is_empty());
    }

    #[test]
    fn test_append_peek() {
        let mut a = Queue::new();
        a.push_back(Entry::new(1));

        let mut b = Queue::new();
        b.push_back(Entry::new(2));

        a.append(&mut b);

        assert_eq!(*a.peek_front().unwrap(), 1);
        assert_eq!(*a.peek_back().unwrap(), 2);
    }

    #[derive(Clone, Copy, Debug, TypeGenerator)]
    enum Operation {
        Push,
        Pop,
        Append,
    }

    #[test]
    fn differential_test() {
        check!().with_type::<Vec<Operation>>().for_each(|ops| {
            let mut values = 0u64..;
            let mut oracle = VecDeque::new();
            let mut subject = Queue::new();

            // secondary queue for append operations
            let mut oracle_other = VecDeque::new();
            let mut subject_other = Queue::new();

            for op in ops {
                match op {
                    Operation::Push => {
                        let value = values.next().unwrap();
                        oracle.push_back(value);
                        subject.push_back(Entry::new(value));
                        assert_eq!(oracle.len(), subject.len());

                        // also push to secondary so appends have something to work with
                        let value2 = values.next().unwrap();
                        oracle_other.push_back(value2);
                        subject_other.push_back(Entry::new(value2));
                    }
                    Operation::Pop => {
                        assert_eq!(oracle.pop_front(), subject.pop_front().map(|entry| *entry));
                        assert_eq!(oracle.len(), subject.len());
                    }
                    Operation::Append => {
                        oracle.extend(oracle_other.drain(..));
                        subject.append(&mut subject_other);
                        assert!(subject_other.is_empty());
                        assert_eq!(oracle.len(), subject.len());
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
