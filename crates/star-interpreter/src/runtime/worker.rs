//! Isolated bytecode workers with JSON-only message and result transfer.

use std::fmt;
use std::io::{self, Cursor};
use std::thread::{self, JoinHandle};

use serde_json::{Map, Number, Value as JsonValue};
use star_compiler::VerificationPolicy;

use crate::value::{RuntimeKey, Value};

use super::{json, BytecodeVm};

/// A request to execute one bytecode image in a new worker VM.
pub struct WorkerRequest {
    /// Verified `.starc` image supplied to the worker.
    pub bytecode: Vec<u8>,
    /// JSON message serialized as the worker's UTF-8 standard input.
    pub message: JsonValue,
    /// Capabilities and verification limits for the isolated VM.
    pub policy: VerificationPolicy,
    /// Maximum serialized message and result size.
    pub max_message_bytes: usize,
}

/// Worker output and the serializable entry-function result.
pub struct WorkerResult {
    /// Entry-function result converted to JSON.
    pub value: JsonValue,
    /// Bytes written to the worker's standard output.
    pub stdout: Vec<u8>,
}

/// A running isolated worker thread.
pub struct WorkerHandle {
    join: JoinHandle<Result<WorkerResult, WorkerError>>,
}

/// Worker creation, execution, or serialization failure.
#[derive(Debug)]
pub enum WorkerError {
    /// The input or result could not be represented as bounded JSON.
    Json(String),
    /// The bytecode VM returned an execution or output error.
    Execution(String),
    /// A runtime value is not part of the worker's serializable value set.
    NonSerializable(&'static str),
    /// The operating system could not start a worker thread.
    Spawn(io::Error),
    /// The worker thread terminated without returning its result.
    Panicked,
}

impl fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(formatter, "worker JSON transfer failed: {error}"),
            Self::Execution(error) => write!(formatter, "worker execution failed: {error}"),
            Self::NonSerializable(name) => {
                write!(formatter, "worker result type {name} is not serializable")
            }
            Self::Spawn(error) => write!(formatter, "could not start worker: {error}"),
            Self::Panicked => formatter.write_str("worker thread panicked"),
        }
    }
}

impl std::error::Error for WorkerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            _ => None,
        }
    }
}

impl WorkerHandle {
    /// Waits for the worker to finish and returns its JSON result and stdout.
    pub fn join(self) -> Result<WorkerResult, WorkerError> {
        self.join.join().map_err(|_| WorkerError::Panicked)?
    }
}

/// Starts a separate VM thread. The worker receives its request as compact JSON
/// on standard input and can return only JSON-compatible Star values.
pub fn spawn(request: WorkerRequest) -> Result<WorkerHandle, WorkerError> {
    let mut message = json::stringify(&request.message, request.max_message_bytes)
        .map_err(|error| WorkerError::Json(error.to_string()))?;
    message.push(b'\n');
    let join = thread::Builder::new()
        .name("star-worker".to_owned())
        .spawn(move || {
            let WorkerRequest {
                bytecode,
                policy,
                max_message_bytes,
                ..
            } = request;
            let mut vm = BytecodeVm::new(Cursor::new(message), Vec::new(), policy);
            let result = vm
                .execute(&bytecode)
                .map_err(|error| WorkerError::Execution(error.to_string()))?;
            let value = value_to_json(&result, 0, &mut Vec::new())?;
            drop(result);
            let (_, stdout) = vm
                .try_into_io()
                .map_err(|(_, _, error)| WorkerError::Execution(error.to_string()))?;
            if stdout.len() > max_message_bytes {
                return Err(WorkerError::Json(format!(
                    "worker output exceeds the {max_message_bytes}-byte limit"
                )));
            }
            Ok(WorkerResult { value, stdout })
        })
        .map_err(WorkerError::Spawn)?;
    Ok(WorkerHandle { join })
}

fn value_to_json(
    value: &Value,
    depth: usize,
    active: &mut Vec<usize>,
) -> Result<JsonValue, WorkerError> {
    if depth > 256 {
        return Err(WorkerError::Json(
            "worker result exceeds the 256-level nesting limit".to_owned(),
        ));
    }
    match value {
        Value::Null => Ok(JsonValue::Null),
        Value::Bool(value) => Ok(JsonValue::Bool(*value)),
        Value::Int(value) => serde_json::from_str(&value.to_string())
            .map_err(|error| WorkerError::Json(error.to_string())),
        Value::Float(value) => Number::from_f64(*value)
            .map(JsonValue::Number)
            .ok_or_else(|| WorkerError::Json("non-finite Float is not JSON".to_owned())),
        Value::String(value) => Ok(JsonValue::String(value.clone())),
        Value::Char(value) => Ok(JsonValue::String(value.to_string())),
        Value::Byte(value) => Ok(JsonValue::Number(Number::from(*value))),
        Value::Bytes(values) => Ok(JsonValue::Array(
            values
                .iter()
                .copied()
                .map(|value| JsonValue::Number(Number::from(value)))
                .collect(),
        )),
        Value::Array(values) => {
            let identity = std::rc::Rc::as_ptr(values) as usize;
            enter(identity, active)?;
            let values = values
                .try_borrow()
                .map_err(|_| WorkerError::Json("worker result is currently borrowed".to_owned()))?;
            let converted = values
                .iter()
                .map(|value| value_to_json(value, depth + 1, active))
                .collect::<Result<Vec<_>, _>>();
            active.pop();
            Ok(JsonValue::Array(converted?))
        }
        Value::Tuple(tuple) => {
            let identity = std::rc::Rc::as_ptr(tuple) as usize;
            enter(identity, active)?;
            let values = tuple
                .values
                .try_borrow()
                .map_err(|_| WorkerError::Json("worker result is currently borrowed".to_owned()))?;
            let converted = values
                .iter()
                .map(|value| value_to_json(value, depth + 1, active))
                .collect::<Result<Vec<_>, _>>();
            active.pop();
            Ok(JsonValue::Array(converted?))
        }
        Value::Map(map) => {
            let identity = std::rc::Rc::as_ptr(map) as usize;
            enter(identity, active)?;
            let map = map
                .try_borrow()
                .map_err(|_| WorkerError::Json("worker result is currently borrowed".to_owned()))?;
            let mut object = Map::new();
            for (key, value) in &map.order {
                let RuntimeKey::String(key) = key else {
                    active.pop();
                    return Err(WorkerError::Json(
                        "worker JSON object keys must be strings".to_owned(),
                    ));
                };
                object.insert(key.clone(), value_to_json(value, depth + 1, active)?);
            }
            active.pop();
            Ok(JsonValue::Object(object))
        }
        Value::Set(set) => {
            let identity = std::rc::Rc::as_ptr(set) as usize;
            enter(identity, active)?;
            let set = set
                .try_borrow()
                .map_err(|_| WorkerError::Json("worker result is currently borrowed".to_owned()))?;
            let converted = set
                .order
                .iter()
                .map(|(_, value)| value_to_json(value, depth + 1, active))
                .collect::<Result<Vec<_>, _>>();
            active.pop();
            Ok(JsonValue::Array(converted?))
        }
        Value::Iterator(_) | Value::Function(_) | Value::Closure(_) | Value::Object(_) => {
            Err(WorkerError::NonSerializable(value.type_name()))
        }
    }
}

fn enter(identity: usize, active: &mut Vec<usize>) -> Result<(), WorkerError> {
    if active.contains(&identity) {
        return Err(WorkerError::Json(
            "cyclic worker results are not serializable".to_owned(),
        ));
    }
    active.push(identity);
    Ok(())
}
