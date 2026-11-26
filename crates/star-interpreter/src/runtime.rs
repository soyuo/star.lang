//! Reference execution for literals, primitive operations, variables, and I/O.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, Write};
use std::rc::Rc;

use num_bigint::Sign;
use star_common::SourcePosition;
use star_parser::Span;
use star_parser::{
    DataLiteral, ForBlock, FunctionBlock, FunctionKind, IfBlock, Instruction, InstructionOpcode,
    LoopBlock, Program, ScopeBlock, Statement,
};

use crate::{
    semantic::{self, ResolvedFunction},
    value::{IteratorState, Value},
    BigInt,
};

mod bytecode_vm;
pub use bytecode_vm::BytecodeVm;

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
    /// An indexed collection access used an invalid position.
    IndexError(&'static str),
    /// Standard input or output failed or contained invalid UTF-8.
    IOError(String),
    /// A resource-sensitive operation lacked a host-granted capability.
    PermissionError(&'static str),
    /// Static semantic analysis rejected the program before execution.
    CompileError(String),
    /// The configured function call-depth limit was exceeded.
    StackOverflow,
    /// The configured statement execution limit was exceeded.
    ExecutionLimit,
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
            Self::IndexError(reason) => write!(formatter, "index error: {reason}"),
            Self::IOError(reason) => write!(formatter, "I/O error: {reason}"),
            Self::PermissionError(capability) => {
                write!(
                    formatter,
                    "permission error: missing {capability} capability"
                )
            }
            Self::CompileError(reason) => write!(formatter, "compile error: {reason}"),
            Self::StackOverflow => formatter.write_str("function call-depth limit exceeded"),
            Self::ExecutionLimit => formatter.write_str("execution step limit exceeded"),
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
/// This execution slice runs synchronous named functions. Calling an
/// `ASYNC_FUNC` returns `Unsupported` until task scheduling is implemented.
pub struct Interpreter<R: BufRead, W: Write> {
    input: R,
    output: W,
    stack: Vec<Value>,
    scopes: Vec<HashMap<BigInt, Binding>>,
    scope_floors: Vec<usize>,
    frame_stack_bases: Vec<usize>,
    functions: HashMap<BigInt, ResolvedFunction>,
    call_depth: usize,
    call_depth_limit: usize,
    executed_steps: u64,
    execution_limit: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum ControlFlow {
    Normal,
    Break,
    Continue,
    Return(Value),
}

impl<R: BufRead, W: Write> Interpreter<R, W> {
    /// Creates an interpreter with empty operand and module-global state.
    pub fn new(input: R, output: W) -> Self {
        Self {
            input,
            output,
            stack: Vec::new(),
            scopes: vec![HashMap::new()],
            scope_floors: vec![0],
            frame_stack_bases: Vec::new(),
            functions: HashMap::new(),
            call_depth: 0,
            call_depth_limit: 512,
            executed_steps: 0,
            execution_limit: 10_000_000,
        }
    }

    /// Sets the maximum number of nested direct calls accepted by this instance.
    pub fn with_call_depth_limit(mut self, limit: usize) -> Self {
        self.call_depth_limit = limit.max(1);
        self
    }

    /// Sets the maximum number of executed statements accepted by this instance.
    pub fn with_execution_limit(mut self, limit: u64) -> Self {
        self.execution_limit = limit.max(1);
        self
    }

    /// Executes supported statements in source order.
    pub fn execute(&mut self, program: &Program) -> Result<(), RuntimeError> {
        self.executed_steps = 0;
        self.functions = semantic::resolve_functions(program)
            .map_err(|error| RuntimeError {
                kind: RuntimeErrorKind::CompileError(error.kind.to_string()),
                span: error.span,
            })?
            .into_iter()
            .map(|function| (function.id.clone(), function))
            .collect();
        match self.execute_statements(&program.statements)? {
            ControlFlow::Normal => Ok(()),
            ControlFlow::Break => Err(self.error(
                RuntimeErrorKind::CompileError("BREAK outside a loop".to_owned()),
                program
                    .statements
                    .last()
                    .map(statement_span)
                    .unwrap_or_else(empty_span),
            )),
            ControlFlow::Continue => Err(self.error(
                RuntimeErrorKind::CompileError("CONTINUE outside a loop".to_owned()),
                program
                    .statements
                    .last()
                    .map(statement_span)
                    .unwrap_or_else(empty_span),
            )),
            ControlFlow::Return(_) => Err(self.error(
                RuntimeErrorKind::CompileError("RETURN outside a function".to_owned()),
                program
                    .statements
                    .last()
                    .map(statement_span)
                    .unwrap_or_else(empty_span),
            )),
        }
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

    fn execute_statements(
        &mut self,
        statements: &[Statement],
    ) -> Result<ControlFlow, RuntimeError> {
        for statement in statements {
            self.advance_step(statement_span(statement))?;
            match statement {
                Statement::Instruction(instruction) => match instruction.opcode {
                    InstructionOpcode::Extended(0x35) => return Ok(ControlFlow::Break),
                    InstructionOpcode::Extended(0x36) => return Ok(ControlFlow::Continue),
                    InstructionOpcode::Extended(0x42) => {
                        let frame_base =
                            self.frame_stack_bases.last().copied().ok_or_else(|| {
                                self.error(
                                    RuntimeErrorKind::CompileError(
                                        "RETURN outside a function".to_owned(),
                                    ),
                                    instruction.span,
                                )
                            })?;
                        let value = if self.stack.len() > frame_base {
                            self.pop(instruction.span)?
                        } else {
                            Value::Null
                        };
                        return Ok(ControlFlow::Return(value));
                    }
                    _ => self.execute_instruction(instruction)?,
                },
                Statement::DataLiteral(literal) => self.push_literal(literal),
                Statement::Scope(scope) => {
                    let flow = self.execute_scope(scope)?;
                    if flow != ControlFlow::Normal {
                        return Ok(flow);
                    }
                }
                Statement::If(block) => {
                    let flow = self.execute_if(block)?;
                    if flow != ControlFlow::Normal {
                        return Ok(flow);
                    }
                }
                Statement::Loop(block) => {
                    let flow = self.execute_loop(block)?;
                    if flow != ControlFlow::Normal {
                        return Ok(flow);
                    }
                }
                Statement::For(block) => {
                    let flow = self.execute_for(block)?;
                    if flow != ControlFlow::Normal {
                        return Ok(flow);
                    }
                }
                Statement::Function(block) => self.consume_function_header(block)?,
            }
        }
        Ok(ControlFlow::Normal)
    }

    fn execute_scope(&mut self, scope: &ScopeBlock) -> Result<ControlFlow, RuntimeError> {
        self.scopes.push(HashMap::new());
        let result = self.execute_statements(&scope.body);
        self.scopes.pop();
        result
    }

    fn execute_if(&mut self, block: &IfBlock) -> Result<ControlFlow, RuntimeError> {
        match self.pop(block.span)? {
            Value::Bool(true) => self.execute_statements(&block.then_body),
            Value::Bool(false) => match &block.else_body {
                Some(body) => self.execute_statements(body),
                None => Ok(ControlFlow::Normal),
            },
            other => Err(self.type_error("Bool", &other, block.span)),
        }
    }

    fn execute_loop(&mut self, block: &LoopBlock) -> Result<ControlFlow, RuntimeError> {
        loop {
            self.advance_step(block.span)?;
            if let Some(condition) = &block.condition {
                match self.execute_statements(condition)? {
                    ControlFlow::Normal => {}
                    ControlFlow::Break => return Ok(ControlFlow::Normal),
                    ControlFlow::Continue => continue,
                    flow @ ControlFlow::Return(_) => return Ok(flow),
                }
                match self.pop(block.span)? {
                    Value::Bool(false) => return Ok(ControlFlow::Normal),
                    Value::Bool(true) => {}
                    other => return Err(self.type_error("Bool", &other, block.span)),
                }
            }
            match self.execute_statements(&block.body)? {
                ControlFlow::Normal | ControlFlow::Continue => {}
                ControlFlow::Break => return Ok(ControlFlow::Normal),
                flow @ ControlFlow::Return(_) => return Ok(flow),
            }
        }
    }

    fn execute_for(&mut self, block: &ForBlock) -> Result<ControlFlow, RuntimeError> {
        let variable_id = self.pop_identifier(block.span)?;
        let iterator = match self.pop(block.span)? {
            Value::Iterator(iterator) => iterator,
            other => return Err(self.type_error("Iterator", &other, block.span)),
        };
        self.scopes.push(HashMap::from([(
            variable_id.clone(),
            Binding {
                value: Value::Null,
                mutable: true,
            },
        )]));

        let result = (|| loop {
            self.advance_step(block.span)?;
            let next = {
                let mut iterator = iterator.borrow_mut();
                let value = iterator.values.get(iterator.next_index).cloned();
                if value.is_some() {
                    iterator.next_index += 1;
                }
                value
            };
            let Some(value) = next else {
                return Ok(ControlFlow::Normal);
            };
            self.scopes
                .last_mut()
                .expect("FOR scope exists")
                .get_mut(&variable_id)
                .expect("FOR binding exists")
                .value = value;
            match self.execute_statements(&block.body)? {
                ControlFlow::Normal | ControlFlow::Continue => {}
                ControlFlow::Break => return Ok(ControlFlow::Normal),
                flow @ ControlFlow::Return(_) => return Ok(flow),
            }
        })();
        self.scopes.pop();
        result
    }

    fn consume_function_header(&mut self, block: &FunctionBlock) -> Result<(), RuntimeError> {
        let function = self
            .functions
            .values()
            .find(|function| function.span == block.span)
            .cloned()
            .ok_or_else(|| {
                self.error(
                    RuntimeErrorKind::CompileError(
                        "function header was not resolved by semantic analysis".to_owned(),
                    ),
                    block.span,
                )
            })?;
        let rest_code = self.pop_identifier(block.span)?;
        let parameter_count = self.pop_identifier(block.span)?;
        if parameter_count != BigInt::from(function.parameter_ids.len())
            || rest_code
                != if function.variadic {
                    parameter_count.clone()
                } else {
                    BigInt::from(0)
                }
            || function.kind != block.kind
        {
            return Err(self.error(
                RuntimeErrorKind::CompileError("runtime function header mismatch".to_owned()),
                block.span,
            ));
        }
        for expected in function.parameter_ids.iter().rev() {
            if self.pop_identifier(block.span)? != *expected {
                return Err(self.error(
                    RuntimeErrorKind::CompileError("runtime parameter header mismatch".to_owned()),
                    block.span,
                ));
            }
        }
        if self.pop_identifier(block.span)? != function.id {
            return Err(self.error(
                RuntimeErrorKind::CompileError("runtime function ID mismatch".to_owned()),
                block.span,
            ));
        }
        Ok(())
    }

    fn call_function(&mut self, span: Span) -> Result<(), RuntimeError> {
        let function_id = self.pop_identifier(span)?;
        let argument_count = self.pop_count(span, "representable nonnegative argument count")?;
        if argument_count > self.stack.len() {
            return Err(self.error(RuntimeErrorKind::StackUnderflow, span));
        }
        let mut arguments = Vec::with_capacity(argument_count);
        for _ in 0..argument_count {
            arguments.push(self.pop(span)?);
        }
        arguments.reverse();
        let function = self.functions.get(&function_id).cloned().ok_or_else(|| {
            self.error(
                RuntimeErrorKind::ReferenceError("function does not exist"),
                span,
            )
        })?;
        if function.kind == FunctionKind::Asynchronous {
            return Err(self.error(
                RuntimeErrorKind::Unsupported("asynchronous function scheduling"),
                span,
            ));
        }
        let parameter_count = function.parameter_ids.len();
        let arity_matches = if function.variadic {
            argument_count >= parameter_count.saturating_sub(1)
        } else {
            argument_count == parameter_count
        };
        if !arity_matches {
            return Err(self.error(
                RuntimeErrorKind::TypeError {
                    expected: "argument count matching function signature",
                    found: "wrong argument count",
                },
                span,
            ));
        }
        if self.call_depth >= self.call_depth_limit {
            return Err(self.error(RuntimeErrorKind::StackOverflow, span));
        }

        let stack_base = self.stack.len();
        let scope_base = self.scopes.len();
        let mut local_bindings = HashMap::new();
        if function.variadic {
            let fixed_count = parameter_count.saturating_sub(1);
            for (id, value) in function
                .parameter_ids
                .iter()
                .take(fixed_count)
                .zip(arguments.iter().take(fixed_count))
            {
                local_bindings.insert(
                    id.clone(),
                    Binding {
                        value: value.clone(),
                        mutable: true,
                    },
                );
            }
            let rest = arguments.into_iter().skip(fixed_count).collect();
            local_bindings.insert(
                function.parameter_ids[fixed_count].clone(),
                Binding {
                    value: Value::Array(Rc::new(RefCell::new(rest))),
                    mutable: true,
                },
            );
        } else {
            for (id, value) in function.parameter_ids.iter().zip(arguments) {
                local_bindings.insert(
                    id.clone(),
                    Binding {
                        value,
                        mutable: true,
                    },
                );
            }
        }
        self.scopes.push(local_bindings);
        self.scope_floors.push(scope_base);
        self.frame_stack_bases.push(stack_base);
        self.call_depth += 1;
        let result = self.execute_statements(&function.body);
        self.call_depth -= 1;
        self.frame_stack_bases.pop();
        self.scope_floors.pop();
        self.scopes.truncate(scope_base);
        self.stack.truncate(stack_base);
        let value = match result? {
            ControlFlow::Normal => Value::Null,
            ControlFlow::Return(value) => value,
            ControlFlow::Break | ControlFlow::Continue => {
                return Err(self.error(
                    RuntimeErrorKind::CompileError(
                        "loop control cannot leave a function frame".to_owned(),
                    ),
                    function.span,
                ));
            }
        };
        self.stack.push(value);
        Ok(())
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
            InstructionOpcode::Extended(0x10) => self.to_int(span),
            InstructionOpcode::Extended(0x14 | 0x16) => self.utf8_encode(span),
            InstructionOpcode::Extended(0x15) => self.utf8_decode(span),
            InstructionOpcode::Extended(0x13) => {
                let value = self.pop(span)?;
                self.stack.push(Value::String(value.to_string()));
                Ok(())
            }
            InstructionOpcode::Extended(0x1D) => self.to_char(span),
            InstructionOpcode::Extended(0x1E) => self.to_byte(span),
            InstructionOpcode::Extended(0x23) => self.less_than(span),
            InstructionOpcode::Extended(0x41) => self.call_function(span),
            InstructionOpcode::Extended(0x60) => self.array_new(span),
            InstructionOpcode::Extended(0x63) => self.get_item(span),
            InstructionOpcode::Extended(0x64) => self.set_item(span),
            InstructionOpcode::Extended(0x65) => self.append(span),
            InstructionOpcode::Extended(0x67) => self.length(span),
            InstructionOpcode::Extended(0x69) => self.iter(span),
            InstructionOpcode::Extended(0x6A) => self.next(span),
            InstructionOpcode::Extended(0x6D) => self.concat(span),
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

    fn to_int(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let integer = match value {
            Value::Int(value) => value,
            Value::Byte(value) => BigInt::from(value),
            Value::Bool(value) => BigInt::from(u8::from(value)),
            Value::Float(value) if value.is_finite() => {
                format!("{:.0}", value.trunc()).parse().map_err(|_| {
                    self.error(
                        RuntimeErrorKind::TypeError {
                            expected: "finite value representable as Int",
                            found: "Float",
                        },
                        span,
                    )
                })?
            }
            Value::String(value) => value.parse().map_err(|_| {
                self.error(
                    RuntimeErrorKind::TypeError {
                        expected: "ASCII decimal String",
                        found: "String",
                    },
                    span,
                )
            })?,
            other => {
                return Err(self.type_error(
                    "Int, Byte, finite Float, Bool, or decimal String",
                    &other,
                    span,
                ))
            }
        };
        self.stack.push(Value::Int(integer));
        Ok(())
    }

    fn to_char(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let Value::Int(codepoint) = value else {
            return Err(self.type_error("Int Unicode scalar value", &value, span));
        };
        let codepoint = codepoint.to_string().parse::<u32>().ok();
        let Some(character) = codepoint.and_then(char::from_u32) else {
            return Err(self.error(
                RuntimeErrorKind::TypeError {
                    expected: "valid Unicode scalar value",
                    found: "Int",
                },
                span,
            ));
        };
        self.stack.push(Value::Char(character));
        Ok(())
    }

    fn to_byte(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let Value::Int(integer) = value else {
            return Err(self.type_error("Int in 0..=255", &value, span));
        };
        let Some(byte) = integer.to_string().parse::<u8>().ok() else {
            return Err(self.error(
                RuntimeErrorKind::TypeError {
                    expected: "Int in 0..=255",
                    found: "Int",
                },
                span,
            ));
        };
        self.stack.push(Value::Byte(byte));
        Ok(())
    }

    fn utf8_encode(&mut self, span: Span) -> Result<(), RuntimeError> {
        match self.pop(span)? {
            Value::String(value) => {
                self.stack.push(Value::Bytes(value.into_bytes()));
                Ok(())
            }
            other => Err(self.type_error("String", &other, span)),
        }
    }

    fn utf8_decode(&mut self, span: Span) -> Result<(), RuntimeError> {
        match self.pop(span)? {
            Value::Bytes(value) => match String::from_utf8(value) {
                Ok(value) => {
                    self.stack.push(Value::String(value));
                    Ok(())
                }
                Err(_) => Err(self.error(
                    RuntimeErrorKind::TypeError {
                        expected: "valid UTF-8 Bytes",
                        found: "Bytes",
                    },
                    span,
                )),
            },
            other => Err(self.type_error("Bytes", &other, span)),
        }
    }

    fn less_than(&mut self, span: Span) -> Result<(), RuntimeError> {
        let right = self.pop(span)?;
        let left = self.pop(span)?;
        let result = match (&left, &right) {
            (Value::Int(left), Value::Int(right)) => left < right,
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                to_float(&left) < to_float(&right)
            }
            _ => return Err(self.type_error("numeric operands", &left, span)),
        };
        self.stack.push(Value::Bool(result));
        Ok(())
    }

    fn array_new(&mut self, span: Span) -> Result<(), RuntimeError> {
        let count = self.pop_count(span, "representable nonnegative Array length")?;
        if count > self.stack.len() {
            return Err(self.error(
                RuntimeErrorKind::TypeError {
                    expected: "Array count not exceeding available operands",
                    found: "excessive count",
                },
                span,
            ));
        }
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(self.pop(span)?);
        }
        values.reverse();
        self.stack.push(Value::Array(Rc::new(RefCell::new(values))));
        Ok(())
    }

    fn get_item(&mut self, span: Span) -> Result<(), RuntimeError> {
        let index = self.pop_index(span)?;
        let collection = self.pop(span)?;
        let item = match collection {
            Value::Array(values) => values.borrow().get(index).cloned(),
            Value::String(value) => value.chars().nth(index).map(Value::Char),
            Value::Bytes(value) => value.get(index).copied().map(Value::Byte),
            other => return Err(self.type_error("Array, String, or Bytes", &other, span)),
        }
        .ok_or_else(|| self.error(RuntimeErrorKind::IndexError("index is out of bounds"), span))?;
        self.stack.push(item);
        Ok(())
    }

    fn set_item(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let index = self.pop_index(span)?;
        match self.pop(span)? {
            Value::Array(values) => {
                let mut values = values.borrow_mut();
                let Some(slot) = values.get_mut(index) else {
                    return Err(
                        self.error(RuntimeErrorKind::IndexError("index is out of bounds"), span)
                    );
                };
                *slot = value;
                Ok(())
            }
            other => Err(self.type_error("mutable Array", &other, span)),
        }
    }

    fn append(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        match self.pop(span)? {
            Value::Array(values) => {
                values.borrow_mut().push(value);
                Ok(())
            }
            other => Err(self.type_error("mutable Array", &other, span)),
        }
    }

    fn length(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let count = match value {
            Value::Array(values) => values.borrow().len(),
            Value::String(value) => value.chars().count(),
            Value::Bytes(value) => value.len(),
            other => return Err(self.type_error("Array, String, or Bytes", &other, span)),
        };
        self.stack.push(Value::Int(BigInt::from(count)));
        Ok(())
    }

    fn iter(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let values = match value {
            Value::Array(values) => values.borrow().clone(),
            Value::String(value) => value.chars().map(Value::Char).collect(),
            Value::Bytes(value) => value.into_iter().map(Value::Byte).collect(),
            other => return Err(self.type_error("Array, String, or Bytes", &other, span)),
        };
        self.stack
            .push(Value::Iterator(Rc::new(RefCell::new(IteratorState {
                values,
                next_index: 0,
            }))));
        Ok(())
    }

    fn next(&mut self, span: Span) -> Result<(), RuntimeError> {
        let iterator = match self.pop(span)? {
            Value::Iterator(iterator) => iterator,
            other => return Err(self.type_error("Iterator", &other, span)),
        };
        let value = {
            let mut iterator = iterator.borrow_mut();
            let value = iterator.values.get(iterator.next_index).cloned();
            if value.is_some() {
                iterator.next_index += 1;
            }
            value
        };
        self.stack.push(value.clone().unwrap_or(Value::Null));
        self.stack.push(Value::Bool(value.is_some()));
        Ok(())
    }

    fn advance_step(&mut self, span: Span) -> Result<(), RuntimeError> {
        self.executed_steps = self.executed_steps.saturating_add(1);
        if self.executed_steps > self.execution_limit {
            return Err(self.error(RuntimeErrorKind::ExecutionLimit, span));
        }
        Ok(())
    }

    fn concat(&mut self, span: Span) -> Result<(), RuntimeError> {
        let right = self.pop(span)?;
        let left = self.pop(span)?;
        let result = match (left, right) {
            (Value::String(mut left), Value::String(right)) => {
                left.push_str(&right);
                Value::String(left)
            }
            (Value::Bytes(mut left), Value::Bytes(right)) => {
                left.extend(right);
                Value::Bytes(left)
            }
            (Value::Array(left), Value::Array(right)) => {
                let mut values = left.borrow().clone();
                values.extend(right.borrow().iter().cloned());
                Value::Array(Rc::new(RefCell::new(values)))
            }
            (left, _) => {
                return Err(self.type_error(
                    "matching String, Bytes, or Array values",
                    &left,
                    span,
                ));
            }
        };
        self.stack.push(result);
        Ok(())
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
        let floor = *self.scope_floors.last().unwrap_or(&0);
        let index = self
            .scopes
            .iter()
            .enumerate()
            .skip(floor)
            .rev()
            .find_map(|(index, scope)| scope.contains_key(&id).then_some(index))
            .or_else(|| (floor > 0 && self.scopes[0].contains_key(&id)).then_some(0));
        if let Some(index) = index {
            let binding = self.scopes[index]
                .get_mut(&id)
                .expect("binding index was found above");
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
        let floor = *self.scope_floors.last().unwrap_or(&0);
        let binding = if global {
            self.scopes.first().and_then(|scope| scope.get(&id))
        } else {
            self.scopes
                .iter()
                .enumerate()
                .skip(floor)
                .rev()
                .find_map(|(_, scope)| scope.get(&id))
                .or_else(|| (floor > 0).then(|| self.scopes[0].get(&id)).flatten())
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

    fn pop_count(&mut self, span: Span, expected: &'static str) -> Result<usize, RuntimeError> {
        let value = self.pop(span)?;
        let Value::Int(integer) = value else {
            return Err(self.type_error(expected, &value, span));
        };
        integer.to_string().parse::<usize>().map_err(|_| {
            self.error(
                RuntimeErrorKind::TypeError {
                    expected,
                    found: "invalid Int count",
                },
                span,
            )
        })
    }

    fn pop_index(&mut self, span: Span) -> Result<usize, RuntimeError> {
        let value = self.pop(span)?;
        let Value::Int(index) = value else {
            return Err(self.type_error("nonnegative Int index", &value, span));
        };
        index.to_string().parse::<usize>().map_err(|_| {
            self.error(
                RuntimeErrorKind::IndexError("index is negative or too large"),
                span,
            )
        })
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

fn empty_span() -> Span {
    let position = SourcePosition {
        byte_offset: 0,
        line: 1,
        column: 1,
    };
    Span {
        start: position,
        end: position,
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
