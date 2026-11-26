//! Runtime values supported by the reference interpreter.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::{cell::RefCell, rc::Rc};

use crate::BigInt;

/// A Star value supported by the current reference-execution slice.
#[derive(Clone, Debug, PartialEq)]
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
        self.entries == other.entries
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
#[derive(Clone, Debug, PartialEq)]
pub struct IteratorState {
    pub(crate) values: Vec<Value>,
    pub(crate) next_index: usize,
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
        match self {
            Self::Int(value) => write!(formatter, "{value}"),
            Self::Float(value) => formatter.write_str(&format_float(*value)),
            Self::Bool(value) => write!(formatter, "{value}"),
            Self::String(value) => formatter.write_str(value),
            Self::Char(value) => write!(formatter, "{value}"),
            Self::Byte(value) => write!(formatter, "{value}"),
            Self::Bytes(value) => {
                formatter.write_str("b\"")?;
                for byte in value {
                    write!(formatter, "{byte:02x}")?;
                }
                formatter.write_str("\"")
            }
            Self::Array(values) => {
                let values = values.borrow();
                formatter.write_str("[")?;
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{value}")?;
                }
                formatter.write_str("]")
            }
            Self::Tuple(values) => {
                formatter.write_str("(")?;
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{value}")?;
                }
                if values.len() == 1 {
                    formatter.write_str(",")?;
                }
                formatter.write_str(")")
            }
            Self::Map(map) => {
                let map = map.borrow();
                formatter.write_str("Map{")?;
                for (index, (key_id, key)) in map.order.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{key}: {}", map.entries[key_id])?;
                }
                formatter.write_str("}")
            }
            Self::Set(set) => {
                let set = set.borrow();
                formatter.write_str("Set{")?;
                for (index, (_, value)) in set.order.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{value}")?;
                }
                formatter.write_str("}")
            }
            Self::Iterator(_) => formatter.write_str("<iterator>"),
            Self::Null => formatter.write_str("null"),
        }
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
