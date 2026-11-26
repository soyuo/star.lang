//! Runtime values supported by the reference interpreter.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::{cell::RefCell, rc::Rc};

use crate::BigInt;

/// A Star value supported by the current reference-execution slice.
#[derive(Clone, Debug)]
pub enum Value {
    /// Signed arbitrary-precision integer.
    Int(BigInt),
    /// IEEE 754 binary64 value.
    Float(f64),
    /// Boolean value.
    Bool(bool),
    /// UTF-8 string.
    String(String),
    /// Unicode scalar value.
    Char(char),
    /// Unsigned byte.
    Byte(u8),
    /// Immutable byte sequence.
    Bytes(Vec<u8>),
    /// Mutable, reference-counted array.
    Array(Rc<RefCell<Vec<Value>>>),
    /// Immutable tuple.
    Tuple(Rc<Vec<Value>>),
    /// Mutable insertion-ordered hash map.
    Map(Rc<RefCell<MapState>>),
    /// Mutable insertion-ordered hash set.
    Set(Rc<RefCell<SetState>>),
    /// Stateful iterator over a snapshot of values.
    Iterator(Rc<RefCell<IteratorState>>),
    /// Absence of a value.
    Null,
}

/// Internal storage for a map with average constant-time lookup and stable iteration.
#[derive(Clone, Debug)]
pub struct MapState {
    pub(crate) entries: HashMap<RuntimeKey, Value>,
    pub(crate) order: Vec<(RuntimeKey, Value)>,
}

impl PartialEq for MapState {
    fn eq(&self, other: &Self) -> bool {
        let mut active = HashSet::new();
        map_entries_equal(&self.entries, &other.entries, &mut active)
    }
}

/// Internal storage for a set with average constant-time membership and stable iteration.
#[derive(Clone, Debug)]
pub struct SetState {
    pub(crate) members: HashSet<RuntimeKey>,
    pub(crate) order: Vec<(RuntimeKey, Value)>,
}

impl PartialEq for SetState {
    fn eq(&self, other: &Self) -> bool {
        self.members == other.members
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        values_equal(self, other, &mut HashSet::new())
    }
}

/// Hashable, immutable representation of a Star value used as a Map key or Set member.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RuntimeKey {
    Null,
    Bool(bool),
    Int(BigInt),
    Float(u64),
    String(String),
    Char(char),
    Byte(u8),
    Bytes(Vec<u8>),
    Tuple(Vec<Self>),
}

impl RuntimeKey {
    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        Some(match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(*value),
            Value::Int(value) => Self::Int(value.clone()),
            Value::Float(value) if value.is_nan() => return None,
            Value::Float(value) if *value == 0.0 => Self::Float(0),
            Value::Float(value) => Self::Float(value.to_bits()),
            Value::String(value) => Self::String(value.clone()),
            Value::Char(value) => Self::Char(*value),
            Value::Byte(value) => Self::Byte(*value),
            Value::Bytes(value) => Self::Bytes(value.clone()),
            Value::Tuple(values) => Self::Tuple(
                values
                    .iter()
                    .map(Self::from_value)
                    .collect::<Option<Vec<_>>>()?,
            ),
            Value::Array(_) | Value::Map(_) | Value::Set(_) | Value::Iterator(_) => return None,
        })
    }
}

/// Internal state carried by a runtime iterator value.
#[derive(Clone, Debug)]
pub struct IteratorState {
    pub(crate) values: Vec<Value>,
    pub(crate) next_index: usize,
}

impl PartialEq for IteratorState {
    fn eq(&self, other: &Self) -> bool {
        self.next_index == other.next_index
            && values_slice_equal(&self.values, &other.values, &mut HashSet::new())
    }
}

type ActiveValuePairs = HashSet<(u8, usize, usize)>;

fn values_equal(left: &Value, right: &Value, active: &mut ActiveValuePairs) -> bool {
    match (left, right) {
        (Value::Int(left), Value::Int(right)) => left == right,
        (Value::Float(left), Value::Float(right)) => left == right,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Char(left), Value::Char(right)) => left == right,
        (Value::Byte(left), Value::Byte(right)) => left == right,
        (Value::Bytes(left), Value::Bytes(right)) => left == right,
        (Value::Null, Value::Null) => true,
        (Value::Array(left), Value::Array(right)) => {
            let pair = (0, Rc::as_ptr(left) as usize, Rc::as_ptr(right) as usize);
            if !active.insert(pair) {
                return true;
            }
            let left = left.borrow();
            let right = right.borrow();
            let equal = values_slice_equal(&left, &right, active);
            active.remove(&pair);
            equal
        }
        (Value::Tuple(left), Value::Tuple(right)) => {
            let pair = (1, Rc::as_ptr(left) as usize, Rc::as_ptr(right) as usize);
            if !active.insert(pair) {
                return true;
            }
            let equal = values_slice_equal(left, right, active);
            active.remove(&pair);
            equal
        }
        (Value::Map(left), Value::Map(right)) => {
            let pair = (2, Rc::as_ptr(left) as usize, Rc::as_ptr(right) as usize);
            if !active.insert(pair) {
                return true;
            }
            let left = left.borrow();
            let right = right.borrow();
            let equal = map_entries_equal(&left.entries, &right.entries, active);
            active.remove(&pair);
            equal
        }
        (Value::Set(left), Value::Set(right)) => {
            let pair = (3, Rc::as_ptr(left) as usize, Rc::as_ptr(right) as usize);
            if !active.insert(pair) {
                return true;
            }
            let equal = left.borrow().members == right.borrow().members;
            active.remove(&pair);
            equal
        }
        (Value::Iterator(left), Value::Iterator(right)) => {
            let pair = (4, Rc::as_ptr(left) as usize, Rc::as_ptr(right) as usize);
            if !active.insert(pair) {
                return true;
            }
            let left = left.borrow();
            let right = right.borrow();
            let equal = left.next_index == right.next_index
                && values_slice_equal(&left.values, &right.values, active);
            active.remove(&pair);
            equal
        }
        _ => false,
    }
}

fn values_slice_equal(left: &[Value], right: &[Value], active: &mut ActiveValuePairs) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| values_equal(left, right, active))
}

fn map_entries_equal(
    left: &HashMap<RuntimeKey, Value>,
    right: &HashMap<RuntimeKey, Value>,
    active: &mut ActiveValuePairs,
) -> bool {
    left.len() == right.len()
        && left.iter().all(|(key, left_value)| {
            right
                .get(key)
                .is_some_and(|right_value| values_equal(left_value, right_value, active))
        })
}

impl Value {
    /// Returns the canonical Star type name for this value.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Int(_) => "Int",
            Self::Float(_) => "Float",
            Self::Bool(_) => "Bool",
            Self::String(_) => "String",
            Self::Char(_) => "Char",
            Self::Byte(_) => "Byte",
            Self::Bytes(_) => "Bytes",
            Self::Array(_) => "Array",
            Self::Tuple(_) => "Tuple",
            Self::Map(_) => "Map",
            Self::Set(_) => "Set",
            Self::Iterator(_) => "Iterator",
            Self::Null => "Null",
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_value(self, formatter, &mut HashSet::new())
    }
}

fn fmt_value(
    value: &Value,
    formatter: &mut fmt::Formatter<'_>,
    active: &mut HashSet<(u8, usize)>,
) -> fmt::Result {
    match value {
        Value::Int(value) => write!(formatter, "{value}"),
        Value::Float(value) => formatter.write_str(&format_float(*value)),
        Value::Bool(value) => write!(formatter, "{value}"),
        Value::String(value) => formatter.write_str(value),
        Value::Char(value) => write!(formatter, "{value}"),
        Value::Byte(value) => write!(formatter, "{value}"),
        Value::Bytes(value) => {
            formatter.write_str("0x")?;
            for byte in value {
                write!(formatter, "{byte:02x}")?;
            }
            Ok(())
        }
        Value::Array(values) => {
            let identity = (0, Rc::as_ptr(values) as usize);
            if !active.insert(identity) {
                return formatter.write_str("<cycle>");
            }
            let values = values.borrow();
            let result = (|| {
                formatter.write_str("[")?;
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    fmt_value(value, formatter, active)?;
                }
                formatter.write_str("]")
            })();
            active.remove(&identity);
            result
        }
        Value::Tuple(values) => {
            let identity = (1, Rc::as_ptr(values) as usize);
            if !active.insert(identity) {
                return formatter.write_str("<cycle>");
            }
            let result = (|| {
                formatter.write_str("(")?;
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    fmt_value(value, formatter, active)?;
                }
                if values.len() == 1 {
                    formatter.write_str(",")?;
                }
                formatter.write_str(")")
            })();
            active.remove(&identity);
            result
        }
        Value::Map(map) => write!(formatter, "Map(len={})", map.borrow().entries.len()),
        Value::Set(set) => write!(formatter, "Set(len={})", set.borrow().members.len()),
        Value::Iterator(_) => formatter.write_str("Iterator"),
        Value::Null => formatter.write_str("null"),
    }
}

fn format_float(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value == f64::INFINITY {
        return "Infinity".to_owned();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".to_owned();
    }
    if value == 0.0 && value.is_sign_negative() {
        return "-0.0".to_owned();
    }

    format!("{value:?}")
}
