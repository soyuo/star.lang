//! Deterministic, fallible collection primitives for the core standard library.
//!
//! Star's guest-visible Array, Map, and Set values are implemented by the
//! interpreter value/runtime operations. These stack, queue, and deque types
//! provide the additional standard-library structures without introducing a
//! second guest-value representation. Module registration can expose them as
//! `std.collections` when module loading is implemented.

use std::collections::TryReserveError;
use std::collections::VecDeque;

/// A last-in, first-out collection backed by a growable vector.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Stack<T> {
    values: Vec<T>,
}

impl<T> Stack<T> {
    /// Creates an empty stack without allocating.
    pub fn new() -> Self {
        Self { values: Vec::new() }
    }

    /// Returns the number of stored values.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns whether the stack has no values.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Adds a value to the top, reporting allocation failure before mutation.
    pub fn try_push(&mut self, value: T) -> Result<(), TryReserveError> {
        self.values.try_reserve(1)?;
        self.values.push(value);
        Ok(())
    }

    /// Removes and returns the top value, or `None` when empty.
    pub fn pop(&mut self) -> Option<T> {
        self.values.pop()
    }

    /// Borrows the top value, or returns `None` when empty.
    pub fn peek(&self) -> Option<&T> {
        self.values.last()
    }

    /// Iterates from the bottom of the stack to the top.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
        self.values.iter()
    }
}

/// A first-in, first-out collection backed by a growable ring buffer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Queue<T> {
    values: VecDeque<T>,
}

impl<T> Queue<T> {
    /// Creates an empty queue without allocating.
    pub fn new() -> Self {
        Self {
            values: VecDeque::new(),
        }
    }

    /// Returns the number of stored values.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns whether the queue has no values.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Adds a value at the back, reporting allocation failure before mutation.
    pub fn try_enqueue(&mut self, value: T) -> Result<(), TryReserveError> {
        self.values.try_reserve(1)?;
        self.values.push_back(value);
        Ok(())
    }

    /// Removes and returns the oldest value, or `None` when empty.
    pub fn dequeue(&mut self) -> Option<T> {
        self.values.pop_front()
    }

    /// Borrows the oldest value, or returns `None` when empty.
    pub fn front(&self) -> Option<&T> {
        self.values.front()
    }

    /// Iterates from the oldest value to the newest.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
        self.values.iter()
    }
}

/// A double-ended queue backed by a growable ring buffer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Deque<T> {
    values: VecDeque<T>,
}

impl<T> Deque<T> {
    /// Creates an empty deque without allocating.
    pub fn new() -> Self {
        Self {
            values: VecDeque::new(),
        }
    }

    /// Returns the number of stored values.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns whether the deque has no values.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Adds a value at the front, reporting allocation failure before mutation.
    pub fn try_push_front(&mut self, value: T) -> Result<(), TryReserveError> {
        self.values.try_reserve(1)?;
        self.values.push_front(value);
        Ok(())
    }

    /// Adds a value at the back, reporting allocation failure before mutation.
    pub fn try_push_back(&mut self, value: T) -> Result<(), TryReserveError> {
        self.values.try_reserve(1)?;
        self.values.push_back(value);
        Ok(())
    }

    /// Removes and returns the front value, or `None` when empty.
    pub fn pop_front(&mut self) -> Option<T> {
        self.values.pop_front()
    }

    /// Removes and returns the back value, or `None` when empty.
    pub fn pop_back(&mut self) -> Option<T> {
        self.values.pop_back()
    }

    /// Borrows the front value, or returns `None` when empty.
    pub fn front(&self) -> Option<&T> {
        self.values.front()
    }

    /// Borrows the back value, or returns `None` when empty.
    pub fn back(&self) -> Option<&T> {
        self.values.back()
    }

    /// Iterates from the front value to the back value.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
        self.values.iter()
    }
}
