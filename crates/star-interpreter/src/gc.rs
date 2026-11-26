//! Non-moving tracing collection for reference-counted runtime objects.
//!
//! Values keep stable `Rc` handles. The collector tracks weak handles, traces
//! interpreter roots and external strong handles, and clears unreachable object
//! edges before dropping its temporary `Rc`s. Clearing edges is what allows
//! cycles to be reclaimed without invalidating any still-rooted handle.

use std::cell::{BorrowError, BorrowMutError, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::{error::Error, fmt};

use crate::value::{
    BindingCell, ClosureState, IteratorState, MapState, ObjectState, SetState, TupleState, Value,
};
use crate::BigInt;

const DEFAULT_THRESHOLD: usize = 64;

/// Summary of one tracing collection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GcStats {
    /// Number of tracked nodes present when collection began.
    pub objects_before: usize,
    /// Number of nodes reachable after collection.
    pub live_objects: usize,
    /// Number of unreachable nodes cleared and reclaimed.
    pub reclaimed_objects: usize,
    /// Number of collections performed by this heap.
    pub collections: u64,
    /// Total number of tracked runtime nodes allocated by this heap.
    pub total_allocations: u64,
    /// Number of tracked nodes that will trigger the next automatic collection.
    pub next_threshold: usize,
}

/// A collection could not inspect a value because the host currently borrows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcError {
    BorrowConflict,
}

impl fmt::Display for GcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BorrowConflict => {
                formatter.write_str("garbage collection deferred while a runtime value is borrowed")
            }
        }
    }
}

impl Error for GcError {}

impl From<BorrowError> for GcError {
    fn from(_: BorrowError) -> Self {
        Self::BorrowConflict
    }
}

impl From<BorrowMutError> for GcError {
    fn from(_: BorrowMutError) -> Self {
        Self::BorrowConflict
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct NodeKey {
    kind: u8,
    pointer: usize,
}

enum WeakNode {
    Array(Weak<RefCell<Vec<Value>>>),
    Tuple(Weak<TupleState>),
    Map(Weak<RefCell<MapState>>),
    Set(Weak<RefCell<SetState>>),
    Iterator(Weak<RefCell<IteratorState>>),
    Closure(Weak<ClosureState>),
    Object(Weak<RefCell<ObjectState>>),
}

enum StrongNode {
    Array(Rc<RefCell<Vec<Value>>>),
    Tuple(Rc<TupleState>),
    Map(Rc<RefCell<MapState>>),
    Set(Rc<RefCell<SetState>>),
    Iterator(Rc<RefCell<IteratorState>>),
    Closure(Rc<ClosureState>),
    Object(Rc<RefCell<ObjectState>>),
}

impl WeakNode {
    fn upgrade(&self) -> Option<StrongNode> {
        match self {
            Self::Array(value) => value.upgrade().map(StrongNode::Array),
            Self::Tuple(value) => value.upgrade().map(StrongNode::Tuple),
            Self::Map(value) => value.upgrade().map(StrongNode::Map),
            Self::Set(value) => value.upgrade().map(StrongNode::Set),
            Self::Iterator(value) => value.upgrade().map(StrongNode::Iterator),
            Self::Closure(value) => value.upgrade().map(StrongNode::Closure),
            Self::Object(value) => value.upgrade().map(StrongNode::Object),
        }
    }

    fn is_alive(&self) -> bool {
        match self {
            Self::Array(value) => value.strong_count() != 0,
            Self::Tuple(value) => value.strong_count() != 0,
            Self::Map(value) => value.strong_count() != 0,
            Self::Set(value) => value.strong_count() != 0,
            Self::Iterator(value) => value.strong_count() != 0,
            Self::Closure(value) => value.strong_count() != 0,
            Self::Object(value) => value.strong_count() != 0,
        }
    }
}

impl StrongNode {
    fn key(&self) -> NodeKey {
        match self {
            Self::Array(value) => NodeKey {
                kind: 0,
                pointer: Rc::as_ptr(value) as usize,
            },
            Self::Tuple(value) => NodeKey {
                kind: 1,
                pointer: Rc::as_ptr(value) as usize,
            },
            Self::Map(value) => NodeKey {
                kind: 2,
                pointer: Rc::as_ptr(value) as usize,
            },
            Self::Set(value) => NodeKey {
                kind: 3,
                pointer: Rc::as_ptr(value) as usize,
            },
            Self::Iterator(value) => NodeKey {
                kind: 4,
                pointer: Rc::as_ptr(value) as usize,
            },
            Self::Closure(value) => NodeKey {
                kind: 5,
                pointer: Rc::as_ptr(value) as usize,
            },
            Self::Object(value) => NodeKey {
                kind: 6,
                pointer: Rc::as_ptr(value) as usize,
            },
        }
    }

    fn strong_count(&self) -> usize {
        match self {
            Self::Array(value) => Rc::strong_count(value),
            Self::Tuple(value) => Rc::strong_count(value),
            Self::Map(value) => Rc::strong_count(value),
            Self::Set(value) => Rc::strong_count(value),
            Self::Iterator(value) => Rc::strong_count(value),
            Self::Closure(value) => Rc::strong_count(value),
            Self::Object(value) => Rc::strong_count(value),
        }
    }

    fn count_children(
        &self,
        incoming: &mut HashMap<NodeKey, usize>,
        counted_cells: &mut HashSet<usize>,
    ) -> Result<(), GcError> {
        match self {
            Self::Array(value) => {
                for child in value.try_borrow()?.iter() {
                    count_value_handle(child, incoming);
                }
            }
            Self::Tuple(value) => {
                for child in value.values.try_borrow()?.iter() {
                    count_value_handle(child, incoming);
                }
            }
            Self::Map(value) => {
                let value = value.try_borrow()?;
                for child in value.entries.values() {
                    count_value_handle(child, incoming);
                }
                for (_, child) in &value.order {
                    count_value_handle(child, incoming);
                }
            }
            Self::Set(value) => {
                for (_, child) in &value.try_borrow()?.order {
                    count_value_handle(child, incoming);
                }
            }
            Self::Iterator(value) => {
                for child in &value.try_borrow()?.values {
                    count_value_handle(child, incoming);
                }
            }
            Self::Closure(value) => {
                for (_, cell) in value.captures.try_borrow()?.iter() {
                    count_binding_cell(cell, incoming, counted_cells)?;
                }
            }
            Self::Object(value) => {
                for child in value.try_borrow()?.fields.values() {
                    count_value_handle(child, incoming);
                }
            }
        }
        Ok(())
    }

    fn trace_children(
        &self,
        marked: &mut HashSet<NodeKey>,
        seen_cells: &mut HashSet<usize>,
    ) -> Result<(), GcError> {
        match self {
            Self::Array(value) => {
                for child in value.try_borrow()?.iter() {
                    trace_value(child, marked, seen_cells)?;
                }
            }
            Self::Tuple(value) => {
                for child in value.values.try_borrow()?.iter() {
                    trace_value(child, marked, seen_cells)?;
                }
            }
            Self::Map(value) => {
                let value = value.try_borrow()?;
                for child in value.entries.values() {
                    trace_value(child, marked, seen_cells)?;
                }
                for (_, child) in &value.order {
                    trace_value(child, marked, seen_cells)?;
                }
            }
            Self::Set(value) => {
                for (_, child) in &value.try_borrow()?.order {
                    trace_value(child, marked, seen_cells)?;
                }
            }
            Self::Iterator(value) => {
                for child in &value.try_borrow()?.values {
                    trace_value(child, marked, seen_cells)?;
                }
            }
            Self::Closure(value) => {
                for (_, cell) in value.captures.try_borrow()?.iter() {
                    trace_binding_cell(cell, marked, seen_cells)?;
                }
            }
            Self::Object(value) => {
                for child in value.try_borrow()?.fields.values() {
                    trace_value(child, marked, seen_cells)?;
                }
            }
        }
        Ok(())
    }

    fn can_clear_edges(&self) -> Result<(), GcError> {
        match self {
            Self::Array(value) => drop(value.try_borrow_mut()?),
            Self::Tuple(value) => drop(value.values.try_borrow_mut()?),
            Self::Map(value) => drop(value.try_borrow_mut()?),
            Self::Set(value) => drop(value.try_borrow_mut()?),
            Self::Iterator(value) => drop(value.try_borrow_mut()?),
            Self::Closure(value) => drop(value.captures.try_borrow_mut()?),
            Self::Object(value) => drop(value.try_borrow_mut()?),
        }
        Ok(())
    }

    fn clear_edges(&self) {
        match self {
            Self::Array(value) => value.borrow_mut().clear(),
            Self::Tuple(value) => value.values.borrow_mut().clear(),
            Self::Map(value) => {
                let mut value = value.borrow_mut();
                value.entries.clear();
                value.order.clear();
            }
            Self::Set(value) => {
                let mut value = value.borrow_mut();
                value.members.clear();
                value.order.clear();
            }
            Self::Iterator(value) => {
                let mut value = value.borrow_mut();
                value.values.clear();
                value.next_index = 0;
            }
            Self::Closure(value) => value.captures.borrow_mut().clear(),
            Self::Object(value) => value.borrow_mut().fields.clear(),
        }
    }
}

/// Weak registry and tracing policy for one interpreter instance.
pub(crate) struct GcHeap {
    nodes: Vec<WeakNode>,
    allocations_since_collection: usize,
    total_allocations: u64,
    collections: u64,
    threshold: usize,
}

impl Default for GcHeap {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            allocations_since_collection: 0,
            total_allocations: 0,
            collections: 0,
            threshold: DEFAULT_THRESHOLD,
        }
    }
}

impl GcHeap {
    pub(crate) fn should_collect(&self) -> bool {
        self.allocations_since_collection >= self.threshold
    }

    pub(crate) fn set_threshold(&mut self, threshold: usize) {
        self.threshold = threshold.max(1);
    }

    pub(crate) fn allocate_array(&mut self, values: Vec<Value>) -> Value {
        let value = Rc::new(RefCell::new(values));
        self.register(WeakNode::Array(Rc::downgrade(&value)));
        Value::Array(value)
    }

    pub(crate) fn allocate_tuple(&mut self, values: Vec<Value>) -> Value {
        let value = Rc::new(TupleState::new(values));
        self.register(WeakNode::Tuple(Rc::downgrade(&value)));
        Value::Tuple(value)
    }

    pub(crate) fn allocate_map(&mut self, value: MapState) -> Value {
        let value = Rc::new(RefCell::new(value));
        self.register(WeakNode::Map(Rc::downgrade(&value)));
        Value::Map(value)
    }

    pub(crate) fn allocate_set(&mut self, value: SetState) -> Value {
        let value = Rc::new(RefCell::new(value));
        self.register(WeakNode::Set(Rc::downgrade(&value)));
        Value::Set(value)
    }

    pub(crate) fn allocate_iterator(&mut self, value: IteratorState) -> Value {
        let value = Rc::new(RefCell::new(value));
        self.register(WeakNode::Iterator(Rc::downgrade(&value)));
        Value::Iterator(value)
    }

    pub(crate) fn allocate_closure(&mut self, value: ClosureState) -> Value {
        let value = Rc::new(value);
        self.register(WeakNode::Closure(Rc::downgrade(&value)));
        Value::Closure(value)
    }

    pub(crate) fn allocate_object(&mut self, value: ObjectState) -> Value {
        let value = Rc::new(RefCell::new(value));
        self.register(WeakNode::Object(Rc::downgrade(&value)));
        Value::Object(value)
    }

    fn register(&mut self, node: WeakNode) {
        self.nodes.push(node);
        self.allocations_since_collection = self.allocations_since_collection.saturating_add(1);
        self.total_allocations = self.total_allocations.saturating_add(1);
    }

    pub(crate) fn collect(
        &mut self,
        stack: &[Value],
        scopes: &[HashMap<BigInt, BindingCell>],
        capture_vectors: &[Vec<(BigInt, BindingCell)>],
    ) -> Result<GcStats, GcError> {
        let live_nodes = self
            .nodes
            .iter()
            .filter_map(WeakNode::upgrade)
            .collect::<Vec<_>>();
        let mut incoming = HashMap::<NodeKey, usize>::new();
        for value in stack {
            count_value_handle(value, &mut incoming);
        }
        let mut counted_cells = HashSet::new();
        for scope in scopes {
            for cell in scope.values() {
                count_binding_cell(cell, &mut incoming, &mut counted_cells)?;
            }
        }
        for capture_vector in capture_vectors {
            for (_, cell) in capture_vector {
                count_binding_cell(cell, &mut incoming, &mut counted_cells)?;
            }
        }
        for node in &live_nodes {
            node.count_children(&mut incoming, &mut counted_cells)?;
        }

        let mut marked = HashSet::new();
        let mut seen_cells = HashSet::new();
        for value in stack {
            trace_value(value, &mut marked, &mut seen_cells)?;
        }
        for scope in scopes {
            for cell in scope.values() {
                trace_binding_cell(cell, &mut marked, &mut seen_cells)?;
            }
        }
        for capture_vector in capture_vectors {
            for (_, cell) in capture_vector {
                trace_binding_cell(cell, &mut marked, &mut seen_cells)?;
            }
        }
        for node in &live_nodes {
            // `live_nodes` contributes one temporary strong handle per node.
            let internal = incoming.get(&node.key()).copied().unwrap_or_default();
            if node.strong_count() > internal.saturating_add(1) {
                trace_node(node, &mut marked, &mut seen_cells)?;
            }
        }

        let objects_before = live_nodes.len();
        for node in &live_nodes {
            if !marked.contains(&node.key()) {
                node.can_clear_edges()?;
            }
        }
        let mut reclaimed_objects = 0;
        for node in &live_nodes {
            if !marked.contains(&node.key()) {
                node.clear_edges();
                reclaimed_objects += 1;
            }
        }
        drop(live_nodes);
        self.nodes.retain(WeakNode::is_alive);
        self.allocations_since_collection = 0;
        self.collections = self.collections.saturating_add(1);
        self.threshold = objects_before
            .saturating_sub(reclaimed_objects)
            .saturating_mul(2)
            .max(DEFAULT_THRESHOLD);

        Ok(GcStats {
            objects_before,
            live_objects: objects_before.saturating_sub(reclaimed_objects),
            reclaimed_objects,
            collections: self.collections,
            total_allocations: self.total_allocations,
            next_threshold: self.threshold,
        })
    }
}

fn count_value_handle(value: &Value, incoming: &mut HashMap<NodeKey, usize>) {
    let key = match value {
        Value::Array(value) => Some(NodeKey {
            kind: 0,
            pointer: Rc::as_ptr(value) as usize,
        }),
        Value::Tuple(value) => Some(NodeKey {
            kind: 1,
            pointer: Rc::as_ptr(value) as usize,
        }),
        Value::Map(value) => Some(NodeKey {
            kind: 2,
            pointer: Rc::as_ptr(value) as usize,
        }),
        Value::Set(value) => Some(NodeKey {
            kind: 3,
            pointer: Rc::as_ptr(value) as usize,
        }),
        Value::Iterator(value) => Some(NodeKey {
            kind: 4,
            pointer: Rc::as_ptr(value) as usize,
        }),
        Value::Closure(value) => Some(NodeKey {
            kind: 5,
            pointer: Rc::as_ptr(value) as usize,
        }),
        Value::Object(value) => Some(NodeKey {
            kind: 6,
            pointer: Rc::as_ptr(value) as usize,
        }),
        _ => None,
    };
    if let Some(key) = key {
        *incoming.entry(key).or_default() += 1;
    }
}

fn count_binding_cell(
    cell: &BindingCell,
    incoming: &mut HashMap<NodeKey, usize>,
    counted_cells: &mut HashSet<usize>,
) -> Result<(), GcError> {
    if counted_cells.insert(Rc::as_ptr(cell) as usize) {
        count_value_handle(&cell.try_borrow()?.value, incoming);
    }
    Ok(())
}

fn trace_binding_cell(
    cell: &BindingCell,
    marked: &mut HashSet<NodeKey>,
    seen_cells: &mut HashSet<usize>,
) -> Result<(), GcError> {
    if seen_cells.insert(Rc::as_ptr(cell) as usize) {
        trace_value(&cell.try_borrow()?.value, marked, seen_cells)?;
    }
    Ok(())
}

fn trace_value(
    value: &Value,
    marked: &mut HashSet<NodeKey>,
    seen_cells: &mut HashSet<usize>,
) -> Result<(), GcError> {
    match value {
        Value::Array(node) => {
            let key = NodeKey {
                kind: 0,
                pointer: Rc::as_ptr(node) as usize,
            };
            if marked.insert(key) {
                for child in node.try_borrow()?.iter() {
                    trace_value(child, marked, seen_cells)?;
                }
            }
        }
        Value::Tuple(node) => {
            let key = NodeKey {
                kind: 1,
                pointer: Rc::as_ptr(node) as usize,
            };
            if marked.insert(key) {
                for child in node.values.try_borrow()?.iter() {
                    trace_value(child, marked, seen_cells)?;
                }
            }
        }
        Value::Map(node) => {
            let key = NodeKey {
                kind: 2,
                pointer: Rc::as_ptr(node) as usize,
            };
            if marked.insert(key) {
                let node = node.try_borrow()?;
                for child in node.entries.values() {
                    trace_value(child, marked, seen_cells)?;
                }
                for (_, child) in &node.order {
                    trace_value(child, marked, seen_cells)?;
                }
            }
        }
        Value::Set(node) => {
            let key = NodeKey {
                kind: 3,
                pointer: Rc::as_ptr(node) as usize,
            };
            if marked.insert(key) {
                for (_, child) in &node.try_borrow()?.order {
                    trace_value(child, marked, seen_cells)?;
                }
            }
        }
        Value::Iterator(node) => {
            let key = NodeKey {
                kind: 4,
                pointer: Rc::as_ptr(node) as usize,
            };
            if marked.insert(key) {
                for child in &node.try_borrow()?.values {
                    trace_value(child, marked, seen_cells)?;
                }
            }
        }
        Value::Closure(node) => {
            let key = NodeKey {
                kind: 5,
                pointer: Rc::as_ptr(node) as usize,
            };
            if marked.insert(key) {
                for (_, cell) in node.captures.try_borrow()?.iter() {
                    trace_binding_cell(cell, marked, seen_cells)?;
                }
            }
        }
        Value::Object(node) => {
            let key = NodeKey {
                kind: 6,
                pointer: Rc::as_ptr(node) as usize,
            };
            if marked.insert(key) {
                for child in node.try_borrow()?.fields.values() {
                    trace_value(child, marked, seen_cells)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn trace_node(
    node: &StrongNode,
    marked: &mut HashSet<NodeKey>,
    seen_cells: &mut HashSet<usize>,
) -> Result<(), GcError> {
    if marked.insert(node.key()) {
        node.trace_children(marked, seen_cells)?;
    }
    Ok(())
}
