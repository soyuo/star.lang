//! Bounded JSON parsing and serialization with arbitrary-precision numbers.

use std::fmt;
use std::io::{self, Write};

/// A JSON value whose integer and decimal tokens are preserved without an
/// intermediate fixed-width or binary floating-point conversion.
pub type JsonValue = serde_json::Value;

/// A JSON parsing or serialization failure.
#[derive(Debug)]
pub enum JsonError {
    /// The source document exceeded the caller-supplied byte limit.
    InputTooLarge { limit: usize },
    /// The JSON input was malformed or contained an invalid value.
    Parse(serde_json::Error),
    /// Serialized output exceeded the caller-supplied byte limit.
    OutputTooLarge { limit: usize },
    /// The value could not be serialized.
    Serialize(serde_json::Error),
}

impl fmt::Display for JsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge { limit } => {
                write!(formatter, "JSON input exceeds the {limit}-byte limit")
            }
            Self::Parse(error) => write!(formatter, "invalid JSON: {error}"),
            Self::OutputTooLarge { limit } => {
                write!(formatter, "JSON output exceeds the {limit}-byte limit")
            }
            Self::Serialize(error) => write!(formatter, "could not serialize JSON: {error}"),
        }
    }
}

impl std::error::Error for JsonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(error) | Self::Serialize(error) => Some(error),
            _ => None,
        }
    }
}

/// Parses UTF-8 JSON bytes, rejecting inputs larger than `max_bytes`.
pub fn parse(input: &[u8], max_bytes: usize) -> Result<JsonValue, JsonError> {
    if input.len() > max_bytes {
        return Err(JsonError::InputTooLarge { limit: max_bytes });
    }
    serde_json::from_slice(input).map_err(JsonError::Parse)
}

/// Serializes a JSON value compactly without exceeding `max_bytes`.
pub fn stringify(value: &JsonValue, max_bytes: usize) -> Result<Vec<u8>, JsonError> {
    serialize_with_limit(value, max_bytes, false)
}

/// Serializes a JSON value with indentation without exceeding `max_bytes`.
pub fn stringify_pretty(value: &JsonValue, max_bytes: usize) -> Result<Vec<u8>, JsonError> {
    serialize_with_limit(value, max_bytes, true)
}

fn serialize_with_limit(
    value: &JsonValue,
    max_bytes: usize,
    pretty: bool,
) -> Result<Vec<u8>, JsonError> {
    let mut writer = LimitedWriter {
        bytes: Vec::new(),
        limit: max_bytes,
        exceeded: false,
    };
    let result = if pretty {
        serde_json::to_writer_pretty(&mut writer, value)
    } else {
        serde_json::to_writer(&mut writer, value)
    };
    if writer.exceeded {
        return Err(JsonError::OutputTooLarge { limit: max_bytes });
    }
    result.map_err(JsonError::Serialize)?;
    Ok(writer.bytes)
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl Write for LimitedWriter {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let Some(length) = self.bytes.len().checked_add(input.len()) else {
            self.exceeded = true;
            return Err(io::Error::other("JSON output length overflow"));
        };
        if length > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("JSON output exceeds configured limit"));
        }
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
