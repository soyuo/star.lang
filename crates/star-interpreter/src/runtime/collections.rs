//! Deterministic, fallible collection primitives for the core standard library.
//!
//! Star's guest-visible Array, Map, and Set values are implemented by the
//! interpreter value/runtime operations. These stack, queue, and deque types
//! provide the additional standard-library structures without introducing a
//! second guest-value representation. Module registration can expose them as
//! `std.collections`; Star 1.0 does not register these primitives as a guest module.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, TryReserveError, VecDeque};

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

#[derive(Clone, Debug)]
struct MinEntry<T> {
    value: T,
    sequence: u128,
}

impl<T: PartialEq> PartialEq for MinEntry<T> {
    fn eq(&self, other: &Self) -> bool {
        self.sequence == other.sequence && self.value == other.value
    }
}

impl<T: Eq> Eq for MinEntry<T> {}

impl<T: Ord> PartialOrd for MinEntry<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: Ord> Ord for MinEntry<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .value
            .cmp(&self.value)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

#[derive(Clone, Debug)]
struct MaxEntry<T> {
    value: T,
    sequence: u128,
}

impl<T: PartialEq> PartialEq for MaxEntry<T> {
    fn eq(&self, other: &Self) -> bool {
        self.sequence == other.sequence && self.value == other.value
    }
}

impl<T: Eq> Eq for MaxEntry<T> {}

impl<T: Ord> PartialOrd for MaxEntry<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: Ord> Ord for MaxEntry<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.value
            .cmp(&other.value)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

/// A min-priority queue. Equal priorities are removed in insertion order.
#[derive(Clone, Debug)]
pub struct MinPriorityQueue<T> {
    values: BinaryHeap<MinEntry<T>>,
    next_sequence: u128,
}

impl<T: Ord> MinPriorityQueue<T> {
    /// Creates an empty min-priority queue without allocating.
    pub fn new() -> Self {
        Self {
            values: BinaryHeap::new(),
            next_sequence: 0,
        }
    }

    /// Returns the number of queued values.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns whether the queue has no values.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Adds a value, returning allocation failure before changing the queue.
    pub fn try_push(&mut self, value: T) -> Result<(), TryReserveError> {
        self.values.try_reserve(1)?;
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.values.push(MinEntry { value, sequence });
        Ok(())
    }

    /// Removes the smallest value, or the earliest inserted equal value.
    pub fn pop(&mut self) -> Option<T> {
        self.values.pop().map(|entry| entry.value)
    }

    /// Borrows the smallest value, or the earliest inserted equal value.
    pub fn peek(&self) -> Option<&T> {
        self.values.peek().map(|entry| &entry.value)
    }
}

impl<T: Ord> Default for MinPriorityQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// A max-priority queue. Equal priorities are removed in insertion order.
#[derive(Clone, Debug)]
pub struct MaxPriorityQueue<T> {
    values: BinaryHeap<MaxEntry<T>>,
    next_sequence: u128,
}

impl<T: Ord> MaxPriorityQueue<T> {
    /// Creates an empty max-priority queue without allocating.
    pub fn new() -> Self {
        Self {
            values: BinaryHeap::new(),
            next_sequence: 0,
        }
    }

    /// Returns the number of queued values.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns whether the queue has no values.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Adds a value, returning allocation failure before changing the queue.
    pub fn try_push(&mut self, value: T) -> Result<(), TryReserveError> {
        self.values.try_reserve(1)?;
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.values.push(MaxEntry { value, sequence });
        Ok(())
    }

    /// Removes the largest value, or the earliest inserted equal value.
    pub fn pop(&mut self) -> Option<T> {
        self.values.pop().map(|entry| entry.value)
    }

    /// Borrows the largest value, or the earliest inserted equal value.
    pub fn peek(&self) -> Option<&T> {
        self.values.peek().map(|entry| &entry.value)
    }
}

impl<T: Ord> Default for MaxPriorityQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Sorts in ascending order without preserving the order of equal elements.
pub fn sort<T: Ord>(values: &mut [T]) {
    values.sort_unstable();
}

/// Sorts in ascending order while preserving the order of equal elements.
pub fn stable_sort<T: Ord>(values: &mut [T]) {
    values.sort();
}

/// Finds the first equal value, returning its index or the insertion point.
pub fn binary_search<T: Ord>(values: &[T], target: &T) -> Result<usize, usize> {
    let index = lower_bound(values, target);
    if values.get(index).is_some_and(|value| value == target) {
        Ok(index)
    } else {
        Err(index)
    }
}

/// Returns the first index whose value is not less than `target`.
pub fn lower_bound<T: Ord>(values: &[T], target: &T) -> usize {
    let mut left = 0;
    let mut right = values.len();
    while left < right {
        let middle = left + (right - left) / 2;
        if values[middle] < *target {
            left = middle + 1;
        } else {
            right = middle;
        }
    }
    left
}

/// Returns the first index whose value is greater than `target`.
pub fn upper_bound<T: Ord>(values: &[T], target: &T) -> usize {
    let mut left = 0;
    let mut right = values.len();
    while left < right {
        let middle = left + (right - left) / 2;
        if values[middle] <= *target {
            left = middle + 1;
        } else {
            right = middle;
        }
    }
    left
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
