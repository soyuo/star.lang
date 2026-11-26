//! Runtime values supported by the reference interpreter.

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
    /// Stateful iterator over a snapshot of values.
    Iterator(Rc<RefCell<IteratorState>>),
    /// Absence of a value.
    Null,
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
