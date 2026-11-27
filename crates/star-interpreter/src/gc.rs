//! Non-moving tracing collection for reference-counted runtime objects.
//!
//! Values keep stable `Rc` handles. The collector tracks weak handles, traces
//! interpreter roots and external strong handles, and clears unreachable object
//! edges before dropping its temporary `Rc`s. Clearing edges is what allows
//! cycles to be reclaimed without invalidating any still-rooted handle.

use std::cell::{BorrowError, BorrowMutError, RefCell};
use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::rc::{Rc, Weak};
use std::{error::Error, fmt};

use crate::value::{
    Binding, BindingCell, ClosureState, IteratorState, MapState, ObjectState, RuntimeKey, SetState,
    TupleState, Value,
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

    /// Estimates guest-owned value and managed-heap storage, including capacity.
    /// This is a deterministic accounting estimate, not the host process RSS.
    pub(crate) fn estimated_memory_bytes(
        &self,
        stack: &Vec<Value>,
        scopes: &Vec<HashMap<BigInt, BindingCell>>,
        capture_vectors: &Vec<Vec<(BigInt, BindingCell)>>,
        limit: usize,
    ) -> Result<usize, GcError> {
        let mut meter = MemoryEstimator::with_limit(limit);
        meter.charge(size_of::<Self>());
        meter.charge(self.nodes.capacity().saturating_mul(size_of::<WeakNode>()));
        if meter.exceeded() {
            return Ok(meter.bytes);
        }
        meter.charge(stack.capacity().saturating_mul(size_of::<Value>()));
        for value in stack {
            meter.value_payload(value, 0)?;
            if meter.exceeded() {
                return Ok(meter.bytes);
            }
        }
        meter.charge(
            scopes
                .capacity()
                .saturating_mul(size_of::<HashMap<BigInt, BindingCell>>()),
        );
        for scope in scopes {
            meter.charge(
                scope
                    .capacity()
                    .saturating_mul(size_of::<(BigInt, BindingCell)>()),
            );
            for (id, cell) in scope {
                meter.bigint(id);
                meter.binding(cell, 0)?;
                if meter.exceeded() {
                    return Ok(meter.bytes);
                }
            }
        }
        meter.charge(
            capture_vectors
                .capacity()
                .saturating_mul(size_of::<Vec<(BigInt, BindingCell)>>()),
        );
        for captures in capture_vectors {
            meter.charge(
                captures
                    .capacity()
                    .saturating_mul(size_of::<(BigInt, BindingCell)>()),
            );
            for (id, cell) in captures {
                meter.bigint(id);
                meter.binding(cell, 0)?;
                if meter.exceeded() {
                    return Ok(meter.bytes);
                }
            }
        }
        for node in self.nodes.iter().filter_map(WeakNode::upgrade) {
            let value = match node {
                StrongNode::Array(value) => Value::Array(value),
                StrongNode::Tuple(value) => Value::Tuple(value),
                StrongNode::Map(value) => Value::Map(value),
                StrongNode::Set(value) => Value::Set(value),
                StrongNode::Iterator(value) => Value::Iterator(value),
                StrongNode::Closure(value) => Value::Closure(value),
                StrongNode::Object(value) => Value::Object(value),
            };
            meter.value_payload(&value, 0)?;
            if meter.exceeded() {
                break;
            }
        }
        Ok(meter.bytes)
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

const MAX_MEMORY_WALK_DEPTH: usize = 256;
const RC_CONTROL_BLOCK_BYTES: usize = 2 * size_of::<usize>();

#[derive(Default)]
struct MemoryEstimator {
    bytes: usize,
    limit: usize,
    seen_nodes: HashSet<NodeKey>,
    seen_cells: HashSet<usize>,
}

impl MemoryEstimator {
    fn with_limit(limit: usize) -> Self {
        Self {
            bytes: 0,
            limit,
            seen_nodes: HashSet::new(),
            seen_cells: HashSet::new(),
        }
    }

    fn exceeded(&self) -> bool {
        self.bytes > self.limit
    }

    fn charge(&mut self, bytes: usize) {
        self.bytes = self.bytes.saturating_add(bytes);
    }

    fn bigint(&mut self, value: &BigInt) {
        let payload = usize::try_from(value.bits().saturating_add(7) / 8).unwrap_or(usize::MAX);
        self.charge(payload);
    }

    fn binding(&mut self, cell: &BindingCell, depth: usize) -> Result<(), GcError> {
        if self.exceeded() {
            return Ok(());
        }
        if !self.seen_cells.insert(Rc::as_ptr(cell) as usize) {
            return Ok(());
        }
        self.charge(size_of::<RefCell<Binding>>().saturating_add(RC_CONTROL_BLOCK_BYTES));
        self.value_payload(&cell.try_borrow()?.value, depth + 1)
    }

    fn key(&mut self, key: &RuntimeKey, depth: usize) {
        if self.exceeded() {
            return;
        }
        if depth >= MAX_MEMORY_WALK_DEPTH {
            self.charge(usize::MAX);
            return;
        }
        match key {
            RuntimeKey::Int(value) => self.bigint(value),
            RuntimeKey::String(value) => self.charge(value.capacity()),
            RuntimeKey::Bytes(value) => self.charge(value.capacity()),
            RuntimeKey::Tuple(values) => {
                self.charge(values.capacity().saturating_mul(size_of::<RuntimeKey>()));
                for value in values {
                    if self.exceeded() {
                        break;
                    }
                    self.key(value, depth + 1);
                }
            }
            RuntimeKey::Null
            | RuntimeKey::Bool(_)
            | RuntimeKey::Float(_)
            | RuntimeKey::Char(_)
            | RuntimeKey::Byte(_) => {}
        }
    }

    fn value_payload(&mut self, value: &Value, depth: usize) -> Result<(), GcError> {
        if self.exceeded() {
            return Ok(());
        }
        if depth >= MAX_MEMORY_WALK_DEPTH {
            self.charge(usize::MAX);
            return Ok(());
        }
        match value {
            Value::Int(value) => self.bigint(value),
            Value::String(value) => self.charge(value.capacity()),
            Value::Bytes(value) => self.charge(value.capacity()),
            Value::Function(function) => self.bigint(&function.id),
            Value::Array(value) => {
                let key = NodeKey {
                    kind: 0,
                    pointer: Rc::as_ptr(value) as usize,
                };
                if self.seen_nodes.insert(key) {
                    let values = value.try_borrow()?;
                    self.charge(size_of::<RefCell<Vec<Value>>>() + RC_CONTROL_BLOCK_BYTES);
                    self.charge(values.capacity().saturating_mul(size_of::<Value>()));
                    for child in values.iter() {
                        self.value_payload(child, depth + 1)?;
                        if self.exceeded() {
                            break;
                        }
                    }
                }
            }
            Value::Tuple(value) => {
                let key = NodeKey {
                    kind: 1,
                    pointer: Rc::as_ptr(value) as usize,
                };
                if self.seen_nodes.insert(key) {
                    let values = value.values.try_borrow()?;
                    self.charge(size_of::<TupleState>() + RC_CONTROL_BLOCK_BYTES);
                    self.charge(values.capacity().saturating_mul(size_of::<Value>()));
                    for child in values.iter() {
                        self.value_payload(child, depth + 1)?;
                        if self.exceeded() {
                            break;
                        }
                    }
                }
            }
            Value::Map(value) => {
                let key = NodeKey {
                    kind: 2,
                    pointer: Rc::as_ptr(value) as usize,
                };
                if self.seen_nodes.insert(key) {
                    let map = value.try_borrow()?;
                    self.charge(size_of::<RefCell<MapState>>() + RC_CONTROL_BLOCK_BYTES);
                    self.charge(
                        map.entries
                            .capacity()
                            .saturating_mul(size_of::<(RuntimeKey, Value)>()),
                    );
                    self.charge(
                        map.order
                            .capacity()
                            .saturating_mul(size_of::<(RuntimeKey, Value)>()),
                    );
                    for (key, child) in &map.entries {
                        self.key(key, depth + 1);
                        self.value_payload(child, depth + 1)?;
                        if self.exceeded() {
                            break;
                        }
                    }
                    for (key, child) in &map.order {
                        if self.exceeded() {
                            break;
                        }
                        self.key(key, depth + 1);
                        self.value_payload(child, depth + 1)?;
                    }
                }
            }
            Value::Set(value) => {
                let key = NodeKey {
                    kind: 3,
                    pointer: Rc::as_ptr(value) as usize,
                };
                if self.seen_nodes.insert(key) {
                    let set = value.try_borrow()?;
                    self.charge(size_of::<RefCell<SetState>>() + RC_CONTROL_BLOCK_BYTES);
                    self.charge(
                        set.members
                            .capacity()
                            .saturating_mul(size_of::<RuntimeKey>()),
                    );
                    self.charge(
                        set.order
                            .capacity()
                            .saturating_mul(size_of::<(RuntimeKey, Value)>()),
                    );
                    for key in &set.members {
                        self.key(key, depth + 1);
                        if self.exceeded() {
                            break;
                        }
                    }
                    for (key, child) in &set.order {
                        if self.exceeded() {
                            break;
                        }
                        self.key(key, depth + 1);
                        self.value_payload(child, depth + 1)?;
                    }
                }
            }
            Value::Iterator(value) => {
                let key = NodeKey {
                    kind: 4,
                    pointer: Rc::as_ptr(value) as usize,
                };
                if self.seen_nodes.insert(key) {
                    let iterator = value.try_borrow()?;
                    self.charge(size_of::<RefCell<IteratorState>>() + RC_CONTROL_BLOCK_BYTES);
                    self.charge(
                        iterator
                            .values
                            .capacity()
                            .saturating_mul(size_of::<Value>()),
                    );
                    for child in &iterator.values {
                        self.value_payload(child, depth + 1)?;
                        if self.exceeded() {
                            break;
                        }
                    }
                }
            }
            Value::Closure(value) => {
                let key = NodeKey {
                    kind: 5,
                    pointer: Rc::as_ptr(value) as usize,
                };
                if self.seen_nodes.insert(key) {
                    self.charge(size_of::<ClosureState>() + RC_CONTROL_BLOCK_BYTES);
                    self.bigint(&value.function.id);
                    let captures = value.captures.try_borrow()?;
                    self.charge(
                        captures
                            .capacity()
                            .saturating_mul(size_of::<(BigInt, BindingCell)>()),
                    );
                    for (id, cell) in captures.iter() {
                        self.bigint(id);
                        self.binding(cell, depth + 1)?;
                        if self.exceeded() {
                            break;
                        }
                    }
                }
            }
            Value::Object(value) => {
                let key = NodeKey {
                    kind: 6,
                    pointer: Rc::as_ptr(value) as usize,
                };
                if self.seen_nodes.insert(key) {
                    let object = value.try_borrow()?;
                    self.charge(size_of::<RefCell<ObjectState>>() + RC_CONTROL_BLOCK_BYTES);
                    self.bigint(&object.struct_id);
                    self.charge(
                        object
                            .fields
                            .capacity()
                            .saturating_mul(size_of::<(BigInt, Value)>()),
                    );
                    for (id, child) in &object.fields {
                        self.bigint(id);
                        self.value_payload(child, depth + 1)?;
                        if self.exceeded() {
                            break;
                        }
                    }
                }
            }
            Value::Float(_)
            | Value::Bool(_)
            | Value::Char(_)
            | Value::Byte(_)
            | Value::Null
            | Value::Handle(_)
            | Value::Task(_)
            | Value::Channel(_)
            | Value::Mutex(_) => {}
        }
        Ok(())
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
