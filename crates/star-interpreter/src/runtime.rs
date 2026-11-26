//! Reference execution for literals, primitive operations, variables, and I/O.

use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, Write};

use num_bigint::Sign;
use star_parser::Span;
use star_parser::{DataLiteral, Instruction, InstructionOpcode, Program, ScopeBlock, Statement};

use crate::{value::Value, BigInt};

/// The reason execution stopped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeErrorKind {
    /// An instruction needed more values than the operand stack contained.
    StackUnderflow,
    /// An instruction received a value of the wrong runtime type.
    TypeError {
        /// Accepted type description.
        expected: &'static str,
        /// Actual type name.
        found: &'static str,
    },
    /// Integer or floating division had a zero divisor.
    DivisionByZero,
    /// A variable operation could not resolve or mutate its binding.
    ReferenceError(&'static str),
    /// Standard input or output failed or contained invalid UTF-8.
    IOError(String),
    /// The instruction is valid source but not implemented by this execution slice.
    UnsupportedInstruction(InstructionOpcode),
    /// The parsed construct belongs to a later reference-execution slice.
    Unsupported(&'static str),
}

impl fmt::Display for RuntimeErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StackUnderflow => formatter.write_str("operand stack underflow"),
            Self::TypeError { expected, found } => {
                write!(formatter, "expected {expected}, found {found}")
            }
            Self::DivisionByZero => formatter.write_str("division by zero"),
            Self::ReferenceError(reason) => write!(formatter, "reference error: {reason}"),
            Self::IOError(reason) => write!(formatter, "I/O error: {reason}"),
            Self::UnsupportedInstruction(opcode) => {
                write!(formatter, "unsupported instruction: {opcode:?}")
            }
            Self::Unsupported(construct) => write!(formatter, "unsupported execution: {construct}"),
        }
    }
}

/// An execution failure associated with the source construct that caused it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeError {
    /// Error category and explanation.
    pub kind: RuntimeErrorKind,
    /// Source range of the failing instruction or construct.
    pub span: Span,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at line {}, column {} (byte offset {})",
            self.kind, self.span.start.line, self.span.start.column, self.span.start.byte_offset
        )
    }
}

impl std::error::Error for RuntimeError {}

#[derive(Clone, Debug)]
struct Binding {
    value: Value,
    mutable: bool,
}

/// An interpreter instance with injectable standard input and output.
///
/// The first environment is the module-global scope. Nested `SCOPE` nodes add
/// temporary lexical environments; ordinary reads and stores walk outward.
pub struct Interpreter<R: BufRead, W: Write> {
    input: R,
    output: W,
    stack: Vec<Value>,
    scopes: Vec<HashMap<BigInt, Binding>>,
}

impl<R: BufRead, W: Write> Interpreter<R, W> {
    /// Creates an interpreter with empty operand and module-global state.
    pub fn new(input: R, output: W) -> Self {
        Self {
            input,
            output,
            stack: Vec::new(),
            scopes: vec![HashMap::new()],
        }
    }

    /// Executes supported statements in source order.
    pub fn execute(&mut self, program: &Program) -> Result<(), RuntimeError> {
        self.execute_statements(&program.statements)
    }

    /// Returns the current operand stack, from bottom to top.
    pub fn stack(&self) -> &[Value] {
        &self.stack
    }

    /// Returns a module-global binding, if one exists.
    pub fn global(&self, id: &BigInt) -> Option<&Value> {
        self.scopes[0].get(id).map(|binding| &binding.value)
    }

    /// Returns the injected I/O objects after execution.
    pub fn into_io(self) -> (R, W) {
        (self.input, self.output)
    }

    fn execute_statements(&mut self, statements: &[Statement]) -> Result<(), RuntimeError> {
        for statement in statements {
            match statement {
                Statement::Instruction(instruction) => self.execute_instruction(instruction)?,
                Statement::DataLiteral(literal) => self.push_literal(literal),
                Statement::Scope(scope) => self.execute_scope(scope)?,
                Statement::If(_) => {
                    return Err(self.error(
                        RuntimeErrorKind::Unsupported("IF"),
                        statement_span(statement),
                    ))
                }
                Statement::Loop(_) => {
                    return Err(self.error(
                        RuntimeErrorKind::Unsupported("LOOP"),
                        statement_span(statement),
                    ))
                }
                Statement::For(_) => {
                    return Err(self.error(
                        RuntimeErrorKind::Unsupported("FOR"),
                        statement_span(statement),
                    ))
                }
                Statement::Function(_) => {
                    return Err(self.error(
                        RuntimeErrorKind::Unsupported("function definition"),
                        statement_span(statement),
                    ))
                }
            }
        }
        Ok(())
    }

    fn execute_scope(&mut self, scope: &ScopeBlock) -> Result<(), RuntimeError> {
        self.scopes.push(HashMap::new());
        let result = self.execute_statements(&scope.body);
        self.scopes.pop();
        result
    }

    fn execute_instruction(&mut self, instruction: &Instruction) -> Result<(), RuntimeError> {
        let span = instruction.span;
        match instruction.opcode {
            InstructionOpcode::Basic(opcode @ 1..=5) => self.arithmetic(opcode, span),
            InstructionOpcode::Basic(6) => {
                let value = self.pop(span)?;
                self.output
                    .write_all(value.to_string().as_bytes())
                    .map_err(|error| self.error(RuntimeErrorKind::IOError(error.to_string()), span))
            }
            InstructionOpcode::Basic(7) => {
                let line = self.read_input_line(span)?;
                self.stack.push(match line {
                    Some(bytes) => Value::String(String::from_utf8(bytes).map_err(|_| {
                        self.error(
                            RuntimeErrorKind::IOError(
                                "standard input is not valid UTF-8".to_owned(),
                            ),
                            span,
                        )
                    })?),
                    None => Value::Null,
                });
                Ok(())
            }
            InstructionOpcode::Basic(8) => self.store(span),
            InstructionOpcode::Basic(9) => self.load(span, false),
            InstructionOpcode::Basic(10) => self.equal(span),
            InstructionOpcode::Basic(11) => self.greater_than(span),
            InstructionOpcode::Basic(12) => {
                let value = self.pop(span)?;
                match value {
                    Value::Bool(value) => {
                        self.stack.push(Value::Bool(!value));
                        Ok(())
                    }
                    other => Err(self.type_error("Bool", &other, span)),
                }
            }
            InstructionOpcode::Extended(0x18) => {
                self.stack.push(Value::Null);
                Ok(())
            }
            InstructionOpcode::Extended(0x19) => {
                self.stack.push(Value::Bool(true));
                Ok(())
            }
            InstructionOpcode::Extended(0x1A) => {
                self.stack.push(Value::Bool(false));
                Ok(())
            }
            InstructionOpcode::Extended(0x50) => self.declare(span, true),
            InstructionOpcode::Extended(0x51) => self.declare(span, false),
            InstructionOpcode::Extended(0x52) => self.delete(span),
            InstructionOpcode::Extended(0x53) => self.load(span, true),
            InstructionOpcode::Extended(0x54) => self.store_global(span),
            InstructionOpcode::Basic(_) | InstructionOpcode::Extended(_) => Err(self.error(
                RuntimeErrorKind::UnsupportedInstruction(instruction.opcode),
                span,
            )),
        }
    }

    fn arithmetic(&mut self, opcode: u8, span: Span) -> Result<(), RuntimeError> {
        let right = self.pop(span)?;
        let left = self.pop(span)?;

        if opcode == 5 {
            let (Value::Int(left), Value::Int(right)) = (&left, &right) else {
                return Err(self.type_error(
                    "two Int values",
                    if !matches!(left, Value::Int(_)) {
                        &left
                    } else {
                        &right
                    },
                    span,
                ));
            };
            if right == &BigInt::from(0) {
                return Err(self.error(RuntimeErrorKind::DivisionByZero, span));
            }
            let quotient = left / right;
            self.stack.push(Value::Int(left - quotient * right));
            return Ok(());
        }

        match (&left, &right) {
            (Value::Int(a), Value::Int(b)) => {
                if opcode == 4 && b == &BigInt::from(0) {
                    return Err(self.error(RuntimeErrorKind::DivisionByZero, span));
                }
                let result = match opcode {
                    1 => a + b,
                    2 => a - b,
                    3 => a * b,
                    4 => a / b,
                    _ => unreachable!("basic arithmetic opcode was range-checked"),
                };
                self.stack.push(Value::Int(result));
            }
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                let a = to_float(&left);
                let b = to_float(&right);
                if opcode == 4 && b == 0.0 {
                    return Err(self.error(RuntimeErrorKind::DivisionByZero, span));
                }
                let result = match opcode {
                    1 => a + b,
                    2 => a - b,
                    3 => a * b,
                    4 => a / b,
                    _ => unreachable!("basic arithmetic opcode was range-checked"),
                };
                self.stack.push(Value::Float(result));
            }
            (_, _) if !matches!(left, Value::Int(_) | Value::Float(_)) => {
                return Err(self.type_error("Int or Float", &left, span));
            }
            (_, other) => return Err(self.type_error("Int or Float", other, span)),
        }
        Ok(())
    }

    fn equal(&mut self, span: Span) -> Result<(), RuntimeError> {
        let right = self.pop(span)?;
        let left = self.pop(span)?;
        self.stack.push(Value::Bool(left == right));
        Ok(())
    }

    fn greater_than(&mut self, span: Span) -> Result<(), RuntimeError> {
        let right = self.pop(span)?;
        let left = self.pop(span)?;
        let result = match (&left, &right) {
            (Value::Int(a), Value::Int(b)) => a > b,
            (Value::Float(a), Value::Float(b)) => a > b,
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                to_float(&left) > to_float(&right)
            }
            _ => {
                return Err(self.type_error(
                    "Int or Float",
                    if !is_numeric(&left) { &left } else { &right },
                    span,
                ))
            }
        };
        self.stack.push(Value::Bool(result));
        Ok(())
    }

    fn store(&mut self, span: Span) -> Result<(), RuntimeError> {
        let id = self.pop_identifier(span)?;
        let value = self.pop(span)?;
        if let Some(binding) = self
            .scopes
            .iter_mut()
            .rev()
            .find_map(|scope| scope.get_mut(&id))
        {
            if !binding.mutable {
                return Err(self.error(
                    RuntimeErrorKind::ReferenceError("cannot update an immutable binding"),
                    span,
                ));
            }
            binding.value = value;
        } else {
            self.scopes.last_mut().expect("root scope exists").insert(
                id,
                Binding {
                    value,
                    mutable: true,
                },
            );
        }
        Ok(())
    }

    fn store_global(&mut self, span: Span) -> Result<(), RuntimeError> {
        let id = self.pop_identifier(span)?;
        let value = self.pop(span)?;
        let globals = self.scopes.first_mut().expect("root scope exists");
        if let Some(binding) = globals.get_mut(&id) {
            if !binding.mutable {
                return Err(self.error(
                    RuntimeErrorKind::ReferenceError("cannot update an immutable global"),
                    span,
                ));
            }
            binding.value = value;
        } else {
            globals.insert(
                id,
                Binding {
                    value,
                    mutable: true,
                },
            );
        }
        Ok(())
    }

    fn load(&mut self, span: Span, global: bool) -> Result<(), RuntimeError> {
        let id = self.pop_identifier(span)?;
        let binding = if global {
            self.scopes.first().and_then(|scope| scope.get(&id))
        } else {
            self.scopes.iter().rev().find_map(|scope| scope.get(&id))
        };
        let Some(binding) = binding else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("binding does not exist"),
                span,
            ));
        };
        self.stack.push(binding.value.clone());
        Ok(())
    }

    fn declare(&mut self, span: Span, mutable: bool) -> Result<(), RuntimeError> {
        let id = self.pop_identifier(span)?;
        let value = self.pop(span)?;
        let scope = self.scopes.last_mut().expect("root scope exists");
        if scope.contains_key(&id) {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("binding already exists in this scope"),
                span,
            ));
        }
        scope.insert(id, Binding { value, mutable });
        Ok(())
    }

    fn delete(&mut self, span: Span) -> Result<(), RuntimeError> {
        let id = self.pop_identifier(span)?;
        let scope = self.scopes.last_mut().expect("root scope exists");
        match scope.get(&id) {
            None => Err(self.error(
                RuntimeErrorKind::ReferenceError("binding does not exist in this scope"),
                span,
            )),
            Some(binding) if !binding.mutable => Err(self.error(
                RuntimeErrorKind::ReferenceError("cannot delete an immutable binding"),
                span,
            )),
            Some(_) => {
                scope.remove(&id);
                Ok(())
            }
        }
    }

    fn pop_identifier(&mut self, span: Span) -> Result<BigInt, RuntimeError> {
        match self.pop(span)? {
            Value::Int(id) if id.sign() != Sign::Minus => Ok(id),
            Value::Int(_) => Err(self.error(
                RuntimeErrorKind::ReferenceError("identifier must be nonnegative"),
                span,
            )),
            other => Err(self.type_error("nonnegative Int identifier", &other, span)),
        }
    }

    fn pop(&mut self, span: Span) -> Result<Value, RuntimeError> {
        self.stack
            .pop()
            .ok_or_else(|| self.error(RuntimeErrorKind::StackUnderflow, span))
    }

    fn push_literal(&mut self, literal: &DataLiteral) {
        let mut value = BigInt::from(0);
        for bit in &literal.bits {
            value <<= 1usize;
            if *bit == 1 {
                value += 1;
            }
        }
        self.stack.push(Value::Int(value));
    }

    fn read_input_line(&mut self, span: Span) -> Result<Option<Vec<u8>>, RuntimeError> {
        let mut line = Vec::new();
        loop {
            let buffer = match self.input.fill_buf() {
                Ok(buffer) => buffer,
                Err(error) => {
                    return Err(RuntimeError {
                        kind: RuntimeErrorKind::IOError(error.to_string()),
                        span,
                    });
                }
            };
            if buffer.is_empty() {
                return Ok((!line.is_empty()).then_some(line));
            }
            if let Some(index) = buffer
                .iter()
                .position(|byte| *byte == b'\n' || *byte == b'\r')
            {
                let terminator = buffer[index];
                line.extend_from_slice(&buffer[..index]);
                self.input.consume(index + 1);
                if terminator == b'\r' {
                    let next_is_lf = match self.input.fill_buf() {
                        Ok(buffer) => buffer.first() == Some(&b'\n'),
                        Err(error) => {
                            return Err(RuntimeError {
                                kind: RuntimeErrorKind::IOError(error.to_string()),
                                span,
                            });
                        }
                    };
                    if next_is_lf {
                        self.input.consume(1);
                    }
                }
                return Ok(Some(line));
            }
            let length = buffer.len();
            line.extend_from_slice(buffer);
            self.input.consume(length);
        }
    }

    fn type_error(&self, expected: &'static str, found: &Value, span: Span) -> RuntimeError {
        self.error(
            RuntimeErrorKind::TypeError {
                expected,
                found: found.type_name(),
            },
            span,
        )
    }

    fn error(&self, kind: RuntimeErrorKind, span: Span) -> RuntimeError {
        RuntimeError { kind, span }
    }
}

fn statement_span(statement: &Statement) -> Span {
    match statement {
        Statement::Instruction(instruction) => instruction.span,
        Statement::DataLiteral(literal) => literal.span,
        Statement::If(block) => block.span,
        Statement::Loop(block) => block.span,
        Statement::For(block) => block.span,
        Statement::Function(block) => block.span,
        Statement::Scope(block) => block.span,
    }
}

fn is_numeric(value: &Value) -> bool {
    matches!(value, Value::Int(_) | Value::Float(_))
}

fn to_float(value: &Value) -> f64 {
    match value {
        Value::Float(value) => *value,
        Value::Int(value) => value.to_string().parse().unwrap_or_else(|_| {
            if value.sign() == Sign::Minus {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }
        }),
        _ => unreachable!("numeric operands are validated before conversion"),
    }
}
