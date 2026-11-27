//! Reference execution for literals, primitive operations, variables, and I/O.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::mem::size_of;
use std::time::{Duration, Instant};

use num_bigint::Sign;
use num_traits::Zero;
use star_common::SourcePosition;
use star_parser::Span;
use star_parser::{
    DataLiteral, ForBlock, FunctionBlock, FunctionKind, IfBlock, Instruction, InstructionOpcode,
    LoopBlock, Program, ScopeBlock, Statement, TryBlock,
};

use crate::{
    gc::{GcError, GcHeap, GcStats},
    semantic::{self, ResolvedFunction},
    value::{
        binding_cell, BindingCell, ClosureState, FunctionRef, InterfaceDefinition, IteratorState,
        MapState, ObjectState, RuntimeKey, SetState, StructDefinition, Value,
    },
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
    /// A configured runtime memory, operand-stack, or wall-clock ceiling was exceeded.
    ResourceLimit(&'static str),
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
            Self::ResourceLimit(resource) => write!(formatter, "{resource} limit exceeded"),
            Self::UnsupportedInstruction(opcode) => {
                write!(formatter, "unsupported instruction: {opcode:?}")
            }
            Self::Unsupported(construct) => write!(formatter, "unsupported execution: {construct}"),
        }
    }
}

impl RuntimeErrorKind {
    fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::ExecutionLimit
                | Self::ResourceLimit(_)
                | Self::CompileError(_)
                | Self::UnsupportedInstruction(_)
                | Self::Unsupported(_)
        )
    }
}

/// Host-configurable ceilings enforced during one interpreter or VM execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionLimits {
    /// Estimated guest runtime storage, including managed values and containers.
    pub max_memory_bytes: usize,
    /// Maximum total operand values across active call frames.
    pub max_stack_values: usize,
    /// Maximum nested function calls.
    pub max_call_depth: usize,
    /// Maximum executed source statements or bytecode instructions.
    pub max_instructions: u64,
    /// Cooperative wall-clock deadline, checked between statements/instructions.
    pub max_execution_time: Duration,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: 256 * 1024 * 1024,
            max_stack_values: 65_536,
            max_call_depth: 512,
            max_instructions: 10_000_000,
            max_execution_time: Duration::from_secs(30),
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
    /// Active function frames from outermost to innermost at the failure site.
    pub stack_trace: Vec<StackTraceFrame>,
}

/// A function frame recorded while propagating a runtime error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackTraceFrame {
    /// Stable function ID of the active frame.
    pub function_id: BigInt,
    /// Source location of the call that entered this frame.
    pub call_site: Span,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at line {}, column {} (byte offset {})",
            self.kind, self.span.start.line, self.span.start.column, self.span.start.byte_offset
        )?;
        for frame in self.stack_trace.iter().rev() {
            write!(
                formatter,
                "\n  at function {} (called at line {}, column {}, byte offset {})",
                frame.function_id,
                frame.call_site.start.line,
                frame.call_site.start.column,
                frame.call_site.start.byte_offset
            )?;
        }
        Ok(())
    }
}

struct LimitedString {
    value: String,
    limit: usize,
}

impl fmt::Write for LimitedString {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let required = self.value.len().saturating_add(text.len());
        if required > self.limit {
            return Err(fmt::Error);
        }
        self.value
            .try_reserve_exact(text.len())
            .map_err(|_| fmt::Error)?;
        if self.value.capacity() > self.limit {
            return Err(fmt::Error);
        }
        self.value.push_str(text);
        Ok(())
    }
}

impl std::error::Error for RuntimeError {}

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
    scopes: Vec<HashMap<BigInt, BindingCell>>,
    scope_floors: Vec<usize>,
    frame_stack_bases: Vec<usize>,
    functions: HashMap<BigInt, ResolvedFunction>,
    structs: HashMap<BigInt, StructDefinition>,
    interfaces: HashMap<BigInt, InterfaceDefinition>,
    capture_vectors: Vec<Vec<(BigInt, BindingCell)>>,
    gc: GcHeap,
    call_depth: usize,
    call_trace: Vec<StackTraceFrame>,
    limits: ExecutionLimits,
    execution_started: Option<Instant>,
    executed_steps: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum ControlFlow {
    Normal,
    Break,
    Continue,
    Return(Value),
}

#[derive(Clone, Debug)]
struct ExecutionSnapshot {
    stack: Vec<Value>,
    scope_depth: usize,
    scope_floor_depth: usize,
    frame_stack_depth: usize,
    capture_lengths: Vec<usize>,
    call_depth: usize,
    call_trace_depth: usize,
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
            structs: HashMap::new(),
            interfaces: HashMap::new(),
            capture_vectors: vec![Vec::new()],
            gc: GcHeap::default(),
            call_depth: 0,
            call_trace: Vec::new(),
            limits: ExecutionLimits::default(),
            execution_started: None,
            executed_steps: 0,
        }
    }

    /// Sets all runtime resource ceilings for subsequent executions.
    pub fn with_execution_limits(mut self, limits: ExecutionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets the maximum number of nested direct calls accepted by this instance.
    pub fn with_call_depth_limit(mut self, limit: usize) -> Self {
        self.limits.max_call_depth = limit.max(1);
        self
    }

    /// Sets the maximum number of executed statements accepted by this instance.
    pub fn with_execution_limit(mut self, limit: u64) -> Self {
        self.limits.max_instructions = limit.max(1);
        self
    }

    /// Sets the number of newly allocated managed objects that triggers collection.
    pub fn with_gc_threshold(mut self, threshold: usize) -> Self {
        self.gc.set_threshold(threshold);
        self
    }

    /// Performs a tracing collection from interpreter roots and external handles.
    /// Returns `GcError::BorrowConflict` without modifying the heap if the host
    /// currently borrows a runtime value.
    pub fn collect_garbage(&mut self) -> Result<GcStats, GcError> {
        self.gc
            .collect(&self.stack, &self.scopes, &self.capture_vectors)
    }

    fn collect_if_needed(&mut self) {
        if self.gc.should_collect() {
            let _ = self.collect_garbage();
        }
    }

    fn begin_execution(&mut self) {
        self.executed_steps = 0;
        self.execution_started = Some(Instant::now());
    }

    fn check_resource_limits(&mut self, span: Span) -> Result<(), RuntimeError> {
        if self.stack.len() > self.limits.max_stack_values {
            return Err(self.error(RuntimeErrorKind::ResourceLimit("operand stack"), span));
        }
        if self
            .execution_started
            .is_some_and(|started| started.elapsed() >= self.limits.max_execution_time)
        {
            return Err(self.error(RuntimeErrorKind::ResourceLimit("execution time"), span));
        }
        if self.executed_steps != 0 && self.executed_steps % 32 != 0 {
            return Ok(());
        }

        self.ensure_memory_budget(0, span)?;
        Ok(())
    }

    fn ensure_memory_budget(&mut self, additional: usize, span: Span) -> Result<(), RuntimeError> {
        let mut estimated = self
            .gc
            .estimated_memory_bytes(
                &self.stack,
                &self.scopes,
                &self.capture_vectors,
                self.limits.max_memory_bytes,
            )
            .map_err(|_| self.error(RuntimeErrorKind::ResourceLimit("memory accounting"), span))?;
        if estimated.saturating_add(additional) > self.limits.max_memory_bytes {
            self.collect_garbage().map_err(|_| {
                self.error(RuntimeErrorKind::ResourceLimit("memory accounting"), span)
            })?;
            estimated = self
                .gc
                .estimated_memory_bytes(
                    &self.stack,
                    &self.scopes,
                    &self.capture_vectors,
                    self.limits.max_memory_bytes,
                )
                .map_err(|_| {
                    self.error(RuntimeErrorKind::ResourceLimit("memory accounting"), span)
                })?;
        }
        if estimated.saturating_add(additional) > self.limits.max_memory_bytes {
            return Err(self.error(RuntimeErrorKind::ResourceLimit("memory"), span));
        }
        Ok(())
    }

    fn available_memory_bytes(&mut self, span: Span) -> Result<usize, RuntimeError> {
        self.ensure_memory_budget(0, span)?;
        self.gc
            .estimated_memory_bytes(
                &self.stack,
                &self.scopes,
                &self.capture_vectors,
                self.limits.max_memory_bytes,
            )
            .map(|estimated| self.limits.max_memory_bytes.saturating_sub(estimated))
            .map_err(|_| self.error(RuntimeErrorKind::ResourceLimit("memory accounting"), span))
    }

    pub(crate) fn format_value(
        &mut self,
        value: &Value,
        span: Span,
    ) -> Result<String, RuntimeError> {
        let input_bytes = estimated_unmanaged_payload_bytes(value, 0);
        self.ensure_memory_budget(input_bytes, span)?;
        let output_limit = self
            .available_memory_bytes(span)?
            .saturating_sub(input_bytes);
        let mut output = LimitedString {
            value: String::new(),
            limit: output_limit,
        };
        if output.write_fmt(format_args!("{value}")).is_err() {
            return Err(self.error(RuntimeErrorKind::ResourceLimit("memory"), span));
        }
        Ok(output.value)
    }

    fn allocate_array(&mut self, values: Vec<Value>) -> Value {
        self.gc.allocate_array(values)
    }

    fn allocate_tuple(&mut self, values: Vec<Value>) -> Value {
        self.gc.allocate_tuple(values)
    }

    fn allocate_map(&mut self, value: MapState) -> Value {
        self.gc.allocate_map(value)
    }

    fn allocate_set(&mut self, value: SetState) -> Value {
        self.gc.allocate_set(value)
    }

    fn allocate_iterator(&mut self, value: IteratorState) -> Value {
        self.gc.allocate_iterator(value)
    }

    fn allocate_closure(&mut self, value: ClosureState) -> Value {
        self.gc.allocate_closure(value)
    }

    fn allocate_object(&mut self, value: ObjectState) -> Value {
        self.gc.allocate_object(value)
    }

    /// Executes supported statements in source order.
    pub fn execute(&mut self, program: &Program) -> Result<(), RuntimeError> {
        self.begin_execution();
        let stack_base = self.stack.len();
        let scope_base = self.scopes.len();
        let scope_floor_base = self.scope_floors.len();
        let frame_base = self.frame_stack_bases.len();
        let capture_base = self.capture_vectors.len();
        let trace_base = self.call_trace.len();
        let call_depth_base = self.call_depth;
        let result = self.execute_program(program);
        if result.is_err() {
            self.stack.truncate(stack_base);
            self.scopes.truncate(scope_base);
            self.scope_floors.truncate(scope_floor_base);
            self.frame_stack_bases.truncate(frame_base);
            self.capture_vectors.truncate(capture_base);
            self.call_trace.truncate(trace_base);
            self.call_depth = call_depth_base;
        }
        result
    }

    fn execute_program(&mut self, program: &Program) -> Result<(), RuntimeError> {
        self.structs.clear();
        self.interfaces.clear();
        self.capture_vectors = vec![Vec::new()];
        self.functions = semantic::resolve_functions(program)
            .map_err(|error| {
                self.error(
                    RuntimeErrorKind::CompileError(error.kind.to_string()),
                    error.span,
                )
            })?
            .into_iter()
            .map(|function| (function.id.clone(), function))
            .collect();
        self.check_resource_limits(
            program
                .statements
                .first()
                .map(statement_span)
                .unwrap_or_else(empty_span),
        )?;
        let final_span = program
            .statements
            .last()
            .map(statement_span)
            .unwrap_or_else(empty_span);
        match self.execute_statements(&program.statements)? {
            ControlFlow::Normal => self.check_resource_limits(final_span),
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
    pub fn global(&self, id: &BigInt) -> Option<Value> {
        self.scopes[0]
            .get(id)
            .map(|binding| binding.borrow().value.clone())
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
                Statement::Try(block) => {
                    let flow = self.execute_try(block)?;
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
            self.check_resource_limits(statement_span(statement))?;
        }
        Ok(ControlFlow::Normal)
    }

    fn execute_scope(&mut self, scope: &ScopeBlock) -> Result<ControlFlow, RuntimeError> {
        self.scopes.push(HashMap::new());
        let result = self.execute_statements(&scope.body);
        self.scopes.pop();
        result
    }

    fn execute_try(&mut self, block: &TryBlock) -> Result<ControlFlow, RuntimeError> {
        let snapshot = self.execution_snapshot();
        let mut outcome = match self.execute_statements(&block.try_body) {
            Ok(flow) => Ok(flow),
            Err(error) => {
                self.restore_execution_snapshot(&snapshot);
                if error.kind.is_fatal() {
                    return Err(error);
                }
                match &block.catch_body {
                    Some(body) => self.execute_statements(body),
                    None => Err(error),
                }
            }
        };

        if let Some(finally_body) = &block.finally_body {
            if let Err(error) = &outcome {
                if error.kind.is_fatal() {
                    return Err(error.clone());
                }
                self.restore_execution_snapshot(&snapshot);
            }
            let prior_outcome = outcome;
            match self.execute_statements(finally_body) {
                Ok(ControlFlow::Normal) => outcome = prior_outcome,
                Ok(flow) => outcome = Ok(flow),
                Err(error) => {
                    self.restore_execution_snapshot(&snapshot);
                    outcome = Err(error);
                }
            }
        }

        outcome
    }

    fn execution_snapshot(&self) -> ExecutionSnapshot {
        ExecutionSnapshot {
            stack: self.stack.clone(),
            scope_depth: self.scopes.len(),
            scope_floor_depth: self.scope_floors.len(),
            frame_stack_depth: self.frame_stack_bases.len(),
            capture_lengths: self.capture_vectors.iter().map(Vec::len).collect(),
            call_depth: self.call_depth,
            call_trace_depth: self.call_trace.len(),
        }
    }

    fn restore_execution_snapshot(&mut self, snapshot: &ExecutionSnapshot) {
        self.stack.clone_from(&snapshot.stack);
        self.scopes.truncate(snapshot.scope_depth);
        self.scope_floors.truncate(snapshot.scope_floor_depth);
        self.frame_stack_bases.truncate(snapshot.frame_stack_depth);
        self.capture_vectors
            .truncate(snapshot.capture_lengths.len());
        for (captures, length) in self
            .capture_vectors
            .iter_mut()
            .zip(&snapshot.capture_lengths)
        {
            captures.truncate(*length);
        }
        self.call_depth = snapshot.call_depth;
        self.call_trace.truncate(snapshot.call_trace_depth);
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
            binding_cell(Value::Null, true),
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
                .borrow_mut()
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
        let value = self.invoke_function(function, arguments, Vec::new(), span)?;
        self.stack.push(value);
        Ok(())
    }

    fn invoke_function(
        &mut self,
        function: ResolvedFunction,
        arguments: Vec<Value>,
        captures: Vec<(BigInt, BindingCell)>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let argument_count = arguments.len();
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
        if self.call_depth >= self.limits.max_call_depth {
            return Err(self.error(RuntimeErrorKind::StackOverflow, span));
        }

        let stack_base = self.stack.len();
        let scope_base = self.scopes.len();
        if !captures.is_empty() {
            self.scopes.push(
                captures
                    .into_iter()
                    .collect::<HashMap<BigInt, BindingCell>>(),
            );
        }
        let mut local_bindings = HashMap::new();
        if function.variadic {
            let fixed_count = parameter_count.saturating_sub(1);
            for (id, value) in function
                .parameter_ids
                .iter()
                .take(fixed_count)
                .zip(arguments.iter().take(fixed_count))
            {
                local_bindings.insert(id.clone(), binding_cell(value.clone(), true));
            }
            let rest = arguments.into_iter().skip(fixed_count).collect();
            local_bindings.insert(
                function.parameter_ids[fixed_count].clone(),
                binding_cell(self.allocate_array(rest), true),
            );
        } else {
            for (id, value) in function.parameter_ids.iter().zip(arguments) {
                local_bindings.insert(id.clone(), binding_cell(value, true));
            }
        }
        self.scopes.push(local_bindings);
        self.scope_floors.push(scope_base);
        self.frame_stack_bases.push(stack_base);
        self.capture_vectors.push(Vec::new());
        self.call_depth += 1;
        self.call_trace.push(StackTraceFrame {
            function_id: function.id.clone(),
            call_site: span,
        });
        let result = self.execute_statements(&function.body);
        self.call_trace.pop();
        self.call_depth -= 1;
        self.capture_vectors.pop();
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
        Ok(value)
    }

    fn capture(&mut self, span: Span) -> Result<(), RuntimeError> {
        let id = self.pop_identifier(span)?;
        let floor = *self.scope_floors.last().unwrap_or(&0);
        let cell = self
            .scopes
            .iter()
            .enumerate()
            .skip(floor)
            .rev()
            .find_map(|(_, scope)| scope.get(&id).cloned())
            .or_else(|| {
                (floor > 0)
                    .then(|| self.scopes[0].get(&id).cloned())
                    .flatten()
            });
        let Some(cell) = cell else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("captured binding does not exist"),
                span,
            ));
        };
        let Some(captures) = self.capture_vectors.last_mut() else {
            return Err(self.error(
                RuntimeErrorKind::CompileError("CAPTURE outside an execution frame".into()),
                span,
            ));
        };
        if captures.iter().any(|(captured_id, _)| captured_id == &id) {
            return Err(self.error(
                RuntimeErrorKind::CompileError("duplicate closure capture ID".into()),
                span,
            ));
        }
        captures.push((id, cell));
        Ok(())
    }

    fn lambda(&mut self, span: Span) -> Result<(), RuntimeError> {
        let parameter_count = self.pop_count(span, "nonnegative parameter count")?;
        let body_id = self.pop_identifier(span)?;
        let Some(function) = self.functions.get(&body_id) else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("lambda body function does not exist"),
                span,
            ));
        };
        if function.parameter_ids.len() != parameter_count {
            return Err(self.error(
                RuntimeErrorKind::TypeError {
                    expected: "parameter count matching lambda body",
                    found: "wrong parameter count",
                },
                span,
            ));
        }
        let Some(capture_vector) = self.capture_vectors.last_mut() else {
            return Err(self.error(
                RuntimeErrorKind::CompileError("LAMBDA outside an execution frame".into()),
                span,
            ));
        };
        let captures = std::mem::take(capture_vector);
        let function = FunctionRef {
            module_index: 0,
            id: body_id,
        };
        if captures.is_empty() {
            self.stack.push(Value::Function(function));
        } else {
            let closure = self.allocate_closure(ClosureState {
                function,
                captures: RefCell::new(captures),
            });
            self.stack.push(closure);
        }
        Ok(())
    }

    fn call_value(&mut self, span: Span) -> Result<(), RuntimeError> {
        let callable = self.pop(span)?;
        let argument_count = self.pop_count(span, "representable nonnegative argument count")?;
        if argument_count > self.stack.len() {
            return Err(self.error(RuntimeErrorKind::StackUnderflow, span));
        }
        let mut arguments = Vec::with_capacity(argument_count);
        for _ in 0..argument_count {
            arguments.push(self.pop(span)?);
        }
        arguments.reverse();
        let (function_ref, captures) = match callable {
            Value::Function(function) => (function, Vec::new()),
            Value::Closure(closure) => {
                (closure.function.clone(), closure.captures.borrow().clone())
            }
            other => return Err(self.type_error("Function or Closure", &other, span)),
        };
        if function_ref.module_index != 0 {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("function belongs to another module"),
                span,
            ));
        }
        let Some(function) = self.functions.get(&function_ref.id).cloned() else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("function does not exist"),
                span,
            ));
        };
        let result = self.invoke_function(function, arguments, captures, span)?;
        self.stack.push(result);
        Ok(())
    }

    fn define_struct(&mut self, span: Span) -> Result<(), RuntimeError> {
        let field_count = self.pop_count(span, "nonnegative field count")?;
        let mut field_ids = Vec::with_capacity(field_count);
        for _ in 0..field_count {
            field_ids.push(self.pop_identifier(span)?);
        }
        field_ids.reverse();
        let struct_id = self.pop_identifier(span)?;
        let unique: HashSet<_> = field_ids.iter().cloned().collect();
        if unique.len() != field_ids.len()
            || self.structs.contains_key(&struct_id)
            || self.interfaces.contains_key(&struct_id)
        {
            return Err(self.error(
                RuntimeErrorKind::CompileError("duplicate struct or field ID".into()),
                span,
            ));
        }
        self.structs.insert(
            struct_id,
            StructDefinition {
                field_ids,
                methods: HashMap::new(),
                interfaces: HashSet::new(),
            },
        );
        Ok(())
    }

    fn define_method(
        &mut self,
        span: Span,
        function_ids: &HashSet<BigInt>,
    ) -> Result<(), RuntimeError> {
        let function_id = self.pop_identifier(span)?;
        let method_id = self.pop_identifier(span)?;
        let struct_id = self.pop_identifier(span)?;
        if !function_ids.contains(&function_id) {
            return Err(self.error(
                RuntimeErrorKind::CompileError("METHOD_DEF references an unknown function".into()),
                span,
            ));
        }
        let Some(descriptor) = self.structs.get_mut(&struct_id) else {
            return Err(self.error(
                RuntimeErrorKind::CompileError("METHOD_DEF references an unknown struct".into()),
                span,
            ));
        };
        if descriptor.methods.contains_key(&method_id) {
            return Err(self.error(
                RuntimeErrorKind::CompileError("duplicate method ID".into()),
                span,
            ));
        }
        descriptor.methods.insert(method_id, function_id);
        Ok(())
    }

    fn define_interface(&mut self, span: Span) -> Result<(), RuntimeError> {
        let method_count = self.pop_count(span, "nonnegative interface method count")?;
        let mut method_ids = HashSet::with_capacity(method_count);
        for _ in 0..method_count {
            if !method_ids.insert(self.pop_identifier(span)?) {
                return Err(self.error(
                    RuntimeErrorKind::CompileError("duplicate interface method ID".into()),
                    span,
                ));
            }
        }
        let interface_id = self.pop_identifier(span)?;
        if self.interfaces.contains_key(&interface_id) || self.structs.contains_key(&interface_id) {
            return Err(self.error(
                RuntimeErrorKind::CompileError("duplicate interface ID".into()),
                span,
            ));
        }
        self.interfaces
            .insert(interface_id, InterfaceDefinition { method_ids });
        Ok(())
    }

    fn implement_interface(&mut self, span: Span) -> Result<(), RuntimeError> {
        let interface_id = self.pop_identifier(span)?;
        let struct_id = self.pop_identifier(span)?;
        let Some(interface) = self.interfaces.get(&interface_id) else {
            return Err(self.error(
                RuntimeErrorKind::CompileError("IMPLEMENT references an unknown interface".into()),
                span,
            ));
        };
        let required_methods = interface.method_ids.clone();
        let Some(descriptor) = self.structs.get_mut(&struct_id) else {
            return Err(self.error(
                RuntimeErrorKind::CompileError("IMPLEMENT references an unknown struct".into()),
                span,
            ));
        };
        if !required_methods
            .iter()
            .all(|method_id| descriptor.methods.contains_key(method_id))
        {
            return Err(self.error(
                RuntimeErrorKind::CompileError("struct is missing an interface method".into()),
                span,
            ));
        }
        descriptor.interfaces.insert(interface_id);
        Ok(())
    }

    fn object_new(&mut self, span: Span) -> Result<(), RuntimeError> {
        let struct_id = self.pop_identifier(span)?;
        let argument_count = self.pop_count(span, "representable nonnegative argument count")?;
        if argument_count > self.stack.len() {
            return Err(self.error(RuntimeErrorKind::StackUnderflow, span));
        }
        let mut arguments = Vec::with_capacity(argument_count);
        for _ in 0..argument_count {
            arguments.push(self.pop(span)?);
        }
        arguments.reverse();
        let Some(descriptor) = self.structs.get(&struct_id).cloned() else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("struct does not exist"),
                span,
            ));
        };
        let object = self.allocate_object(ObjectState {
            struct_id: struct_id.clone(),
            fields: descriptor
                .field_ids
                .iter()
                .cloned()
                .map(|field_id| (field_id, Value::Null))
                .collect(),
        });
        if let Some(constructor_id) = descriptor.methods.get(&BigInt::from(0)) {
            let Some(function) = self.functions.get(constructor_id).cloned() else {
                return Err(self.error(
                    RuntimeErrorKind::ReferenceError("constructor function does not exist"),
                    span,
                ));
            };
            let mut constructor_args = Vec::with_capacity(argument_count + 1);
            constructor_args.push(object.clone());
            constructor_args.extend(arguments);
            self.invoke_function(function, constructor_args, Vec::new(), span)?;
        } else if !arguments.is_empty() {
            return Err(self.error(
                RuntimeErrorKind::TypeError {
                    expected: "zero arguments for a struct without a constructor",
                    found: "nonzero argument count",
                },
                span,
            ));
        }
        self.stack.push(object);
        Ok(())
    }

    fn field_get(&mut self, span: Span) -> Result<(), RuntimeError> {
        let field_id = self.pop_identifier(span)?;
        let value = self.pop(span)?;
        let Value::Object(object) = value else {
            return Err(self.type_error("Struct", &value, span));
        };
        let Some(value) = object.borrow().fields.get(&field_id).cloned() else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("field does not exist"),
                span,
            ));
        };
        self.stack.push(value);
        Ok(())
    }

    fn field_set(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let field_id = self.pop_identifier(span)?;
        let object_value = self.pop(span)?;
        let Value::Object(object) = object_value else {
            return Err(self.type_error("Struct", &object_value, span));
        };
        let mut object = object.borrow_mut();
        let Some(field) = object.fields.get_mut(&field_id) else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("field does not exist"),
                span,
            ));
        };
        *field = value;
        Ok(())
    }

    fn method_call(&mut self, span: Span) -> Result<(), RuntimeError> {
        let method_id = self.pop_identifier(span)?;
        if method_id.is_zero() {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("constructor is not an instance method"),
                span,
            ));
        }
        let receiver = self.pop(span)?;
        let Value::Object(object) = &receiver else {
            return Err(self.type_error("Struct", &receiver, span));
        };
        let argument_count = self.pop_count(span, "representable nonnegative argument count")?;
        if argument_count > self.stack.len() {
            return Err(self.error(RuntimeErrorKind::StackUnderflow, span));
        }
        let mut arguments = Vec::with_capacity(argument_count + 1);
        for _ in 0..argument_count {
            arguments.push(self.pop(span)?);
        }
        arguments.reverse();
        arguments.insert(0, receiver.clone());
        let struct_id = object.borrow().struct_id.clone();
        let function_id = self
            .structs
            .get(&struct_id)
            .and_then(|descriptor| descriptor.methods.get(&method_id))
            .cloned()
            .ok_or_else(|| {
                self.error(
                    RuntimeErrorKind::ReferenceError("method does not exist"),
                    span,
                )
            })?;
        let Some(function) = self.functions.get(&function_id).cloned() else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("method function does not exist"),
                span,
            ));
        };
        let result = self.invoke_function(function, arguments, Vec::new(), span)?;
        self.stack.push(result);
        Ok(())
    }

    fn object_is(&mut self, span: Span) -> Result<(), RuntimeError> {
        let type_id = self.pop_identifier(span)?;
        let value = self.pop(span)?;
        let Value::Object(object) = value else {
            return Err(self.type_error("Struct", &value, span));
        };
        if !self.structs.contains_key(&type_id) && !self.interfaces.contains_key(&type_id) {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("type ID does not exist"),
                span,
            ));
        }
        let object = object.borrow();
        let matches = object.struct_id == type_id
            || self
                .structs
                .get(&object.struct_id)
                .is_some_and(|descriptor| descriptor.interfaces.contains(&type_id));
        self.stack.push(Value::Bool(matches));
        Ok(())
    }

    fn object_clone(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let Value::Object(object) = value else {
            return Err(self.type_error("Struct", &value, span));
        };
        let cloned = object.borrow().clone();
        let object = self.allocate_object(cloned);
        self.stack.push(object);
        Ok(())
    }

    fn execute_instruction(&mut self, instruction: &Instruction) -> Result<(), RuntimeError> {
        let span = instruction.span;
        let result = match instruction.opcode {
            InstructionOpcode::Basic(opcode @ 1..=5) => self.arithmetic(opcode, span),
            InstructionOpcode::Basic(6) => {
                let value = self.pop(span)?;
                let formatted = self.format_value(&value, span)?;
                self.output
                    .write_all(formatted.as_bytes())
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
            InstructionOpcode::Extended(0x12) => self.to_bool(span),
            InstructionOpcode::Extended(0x14 | 0x16) => self.utf8_encode(span),
            InstructionOpcode::Extended(0x15) => self.utf8_decode(span),
            InstructionOpcode::Extended(0x13) => {
                let value = self.pop(span)?;
                let formatted = self.format_value(&value, span)?;
                self.stack.push(Value::String(formatted));
                Ok(())
            }
            InstructionOpcode::Extended(0x1D) => self.to_char(span),
            InstructionOpcode::Extended(0x1E) => self.to_byte(span),
            InstructionOpcode::Extended(0x23) => self.less_than(span),
            InstructionOpcode::Extended(0x41) => self.call_function(span),
            InstructionOpcode::Extended(0x43) => self.lambda(span),
            InstructionOpcode::Extended(0x44) => self.capture(span),
            InstructionOpcode::Extended(0x46) => self.call_value(span),
            InstructionOpcode::Extended(0x60) => self.array_new(span),
            InstructionOpcode::Extended(0x61) => self.map_new(span),
            InstructionOpcode::Extended(0x62) => self.set_new(span),
            InstructionOpcode::Extended(0x63) => self.get_item(span),
            InstructionOpcode::Extended(0x64) => self.set_item(span),
            InstructionOpcode::Extended(0x65) => self.append(span),
            InstructionOpcode::Extended(0x66) => self.remove(span),
            InstructionOpcode::Extended(0x67) => self.length(span),
            InstructionOpcode::Extended(0x68) => self.contains(span),
            InstructionOpcode::Extended(0x69) => self.iter(span),
            InstructionOpcode::Extended(0x6A) => self.next(span),
            InstructionOpcode::Extended(0x6B) => self.tuple_new(span),
            InstructionOpcode::Extended(0x6C) => self.slice(span),
            InstructionOpcode::Extended(0x6D) => self.concat(span),
            InstructionOpcode::Extended(0x70) => self.define_struct(span),
            InstructionOpcode::Extended(0x71) => self.object_new(span),
            InstructionOpcode::Extended(0x72) => self.field_get(span),
            InstructionOpcode::Extended(0x73) => self.field_set(span),
            InstructionOpcode::Extended(0x74) => {
                let function_ids = self.functions.keys().cloned().collect();
                self.define_method(span, &function_ids)
            }
            InstructionOpcode::Extended(0x75) => self.method_call(span),
            InstructionOpcode::Extended(0x76) => self.define_interface(span),
            InstructionOpcode::Extended(0x77) => self.implement_interface(span),
            InstructionOpcode::Extended(0x78) => self.object_is(span),
            InstructionOpcode::Extended(0x79) => self.object_clone(span),
            InstructionOpcode::Extended(0x50) => self.declare(span, true),
            InstructionOpcode::Extended(0x51) => self.declare(span, false),
            InstructionOpcode::Extended(0x52) => self.delete(span),
            InstructionOpcode::Extended(0x53) => self.load(span, true),
            InstructionOpcode::Extended(0x54) => self.store_global(span),
            InstructionOpcode::Basic(_) | InstructionOpcode::Extended(_) => Err(self.error(
                RuntimeErrorKind::UnsupportedInstruction(instruction.opcode),
                span,
            )),
        };
        if result.is_ok() {
            self.collect_if_needed();
        }
        result
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

    fn to_bool(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let truthy = match &value {
            Value::Null => false,
            Value::Bool(value) => *value,
            Value::Int(value) => !value.is_zero(),
            Value::Float(value) => *value != 0.0,
            Value::Byte(value) => *value != 0,
            Value::String(value) => !value.is_empty(),
            Value::Bytes(value) => !value.is_empty(),
            Value::Array(value) => !value.borrow().is_empty(),
            Value::Tuple(value) => !value.values.borrow().is_empty(),
            Value::Map(value) => !value.borrow().entries.is_empty(),
            Value::Set(value) => !value.borrow().members.is_empty(),
            Value::Iterator(_)
            | Value::Char(_)
            | Value::Function(_)
            | Value::Closure(_)
            | Value::Object(_) => true,
        };
        self.stack.push(Value::Bool(truthy));
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
        self.ensure_memory_budget(count.saturating_mul(size_of::<Value>()), span)?;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(self.pop(span)?);
        }
        values.reverse();
        let array = self.allocate_array(values);
        self.stack.push(array);
        Ok(())
    }

    fn tuple_new(&mut self, span: Span) -> Result<(), RuntimeError> {
        let values = self.pop_collection_items(span)?;
        let tuple = self.allocate_tuple(values);
        self.stack.push(tuple);
        Ok(())
    }

    fn map_new(&mut self, span: Span) -> Result<(), RuntimeError> {
        let count = self.pop_count(span, "representable nonnegative Map pair count")?;
        let operand_count = count.checked_mul(2).ok_or_else(|| {
            self.error(
                RuntimeErrorKind::TypeError {
                    expected: "Map pair count within available operands",
                    found: "excessive count",
                },
                span,
            )
        })?;
        if operand_count > self.stack.len() {
            return Err(self.error(
                RuntimeErrorKind::TypeError {
                    expected: "Map pair count within available operands",
                    found: "excessive count",
                },
                span,
            ));
        }
        let first_key_index = self.stack.len() - operand_count;
        let duplicated_key_bytes = (0..count).fold(0usize, |total, pair_index| {
            total.saturating_add(estimated_key_clone_bytes(
                &self.stack[first_key_index + pair_index * 2],
                0,
            ))
        });
        let table_bytes = count.saturating_mul(
            size_of::<(RuntimeKey, Value, Value)>()
                .saturating_add(size_of::<(RuntimeKey, Value)>().saturating_mul(4)),
        );
        self.ensure_memory_budget(
            table_bytes.saturating_add(duplicated_key_bytes.saturating_mul(2)),
            span,
        )?;
        let mut pairs = Vec::with_capacity(count);
        for _ in 0..count {
            let value = self.pop(span)?;
            let key_value = self.pop(span)?;
            let key = RuntimeKey::from_value(&key_value)
                .ok_or_else(|| self.type_error("hashable Map key", &key_value, span))?;
            pairs.push((key, key_value, value));
        }
        pairs.reverse();
        let mut map = MapState {
            entries: HashMap::with_capacity(count),
            order: Vec::with_capacity(count),
        };
        for (key, key_value, value) in pairs {
            if !map.entries.contains_key(&key) {
                map.order.push((key.clone(), key_value));
            }
            map.entries.insert(key, value);
        }
        let map = self.allocate_map(map);
        self.stack.push(map);
        Ok(())
    }

    fn set_new(&mut self, span: Span) -> Result<(), RuntimeError> {
        let values = self.pop_collection_items(span)?;
        let collection_bytes = values.len().saturating_mul(
            size_of::<Value>()
                .saturating_add(size_of::<RuntimeKey>())
                .saturating_mul(3),
        );
        let duplicated_key_bytes = values.iter().fold(0usize, |total, value| {
            total.saturating_add(estimated_key_clone_bytes(value, 0))
        });
        let temporary_payload_bytes = values.iter().fold(0usize, |total, value| {
            total.saturating_add(estimated_unmanaged_payload_bytes(value, 0))
        });
        self.ensure_memory_budget(
            collection_bytes
                .saturating_add(duplicated_key_bytes.saturating_mul(2))
                .saturating_add(temporary_payload_bytes),
            span,
        )?;
        let mut set = SetState {
            members: HashSet::with_capacity(values.len()),
            order: Vec::with_capacity(values.len()),
        };
        for value in values {
            let key = RuntimeKey::from_value(&value)
                .ok_or_else(|| self.type_error("hashable Set member", &value, span))?;
            if set.members.insert(key.clone()) {
                set.order.push((key, value));
            }
        }
        let set = self.allocate_set(set);
        self.stack.push(set);
        Ok(())
    }

    fn pop_collection_items(&mut self, span: Span) -> Result<Vec<Value>, RuntimeError> {
        let count = self.pop_count(span, "representable nonnegative collection length")?;
        if count > self.stack.len() {
            return Err(self.error(
                RuntimeErrorKind::TypeError {
                    expected: "collection count not exceeding available operands",
                    found: "excessive count",
                },
                span,
            ));
        }
        self.ensure_memory_budget(count.saturating_mul(size_of::<Value>()), span)?;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(self.pop(span)?);
        }
        values.reverse();
        Ok(values)
    }

    fn get_item(&mut self, span: Span) -> Result<(), RuntimeError> {
        let key = self.pop(span)?;
        let collection = self.pop(span)?;
        let item = match collection {
            Value::Array(values) => values
                .borrow()
                .get(self.index_from_value(key, span)?)
                .cloned()
                .ok_or_else(|| self.index_error(span))?,
            Value::Tuple(values) => values
                .values
                .borrow()
                .get(self.index_from_value(key, span)?)
                .cloned()
                .ok_or_else(|| self.index_error(span))?,
            Value::String(value) => value
                .chars()
                .nth(self.index_from_value(key, span)?)
                .map(Value::Char)
                .ok_or_else(|| self.index_error(span))?,
            Value::Bytes(value) => value
                .get(self.index_from_value(key, span)?)
                .copied()
                .map(Value::Byte)
                .ok_or_else(|| self.index_error(span))?,
            Value::Map(map) => {
                let key_value = RuntimeKey::from_value(&key)
                    .ok_or_else(|| self.type_error("hashable Map key", &key, span))?;
                map.borrow()
                    .entries
                    .get(&key_value)
                    .cloned()
                    .ok_or_else(|| {
                        self.error(RuntimeErrorKind::ReferenceError("Map key not found"), span)
                    })?
            }
            other => return Err(self.type_error("indexable collection or Map", &other, span)),
        };
        self.stack.push(item);
        Ok(())
    }

    fn set_item(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let key = self.pop(span)?;
        match self.pop(span)? {
            Value::Array(values) => {
                let index = self.index_from_value(key, span)?;
                let mut values = values.borrow_mut();
                let Some(slot) = values.get_mut(index) else {
                    return Err(self.index_error(span));
                };
                *slot = value;
                Ok(())
            }
            Value::Map(map) => {
                let key_value = key;
                let key = RuntimeKey::from_value(&key_value)
                    .ok_or_else(|| self.type_error("hashable Map key", &key_value, span))?;
                let mut map = map.borrow_mut();
                if !map.entries.contains_key(&key) {
                    map.order.push((key.clone(), key_value));
                }
                map.entries.insert(key, value);
                Ok(())
            }
            other => Err(self.type_error("mutable Array or Map", &other, span)),
        }
    }

    fn remove(&mut self, span: Span) -> Result<(), RuntimeError> {
        let key_value = self.pop(span)?;
        let result = match self.pop(span)? {
            Value::Array(values) => {
                let index = self.index_from_value(key_value, span)?;
                let mut values = values.borrow_mut();
                if index >= values.len() {
                    return Err(self.index_error(span));
                }
                values.remove(index)
            }
            Value::Map(map) => {
                let key = RuntimeKey::from_value(&key_value)
                    .ok_or_else(|| self.type_error("hashable Map key", &key_value, span))?;
                let mut map = map.borrow_mut();
                map.order.retain(|(entry, _)| entry != &key);
                map.entries.remove(&key).unwrap_or(Value::Null)
            }
            Value::Set(set) => {
                let key = RuntimeKey::from_value(&key_value)
                    .ok_or_else(|| self.type_error("hashable Set member", &key_value, span))?;
                let mut set = set.borrow_mut();
                let removed_value = set
                    .order
                    .iter()
                    .find(|(entry, _)| entry == &key)
                    .map(|(_, value)| value.clone());
                let removed = set.members.remove(&key);
                if removed {
                    set.order.retain(|(entry, _)| entry != &key);
                    removed_value.unwrap_or(Value::Null)
                } else {
                    Value::Null
                }
            }
            other => return Err(self.type_error("mutable Array, Map, or Set", &other, span)),
        };
        self.stack.push(result);
        Ok(())
    }

    fn contains(&mut self, span: Span) -> Result<(), RuntimeError> {
        let needle = self.pop(span)?;
        let collection = self.pop(span)?;
        let found = match collection {
            Value::Array(values) => values.borrow().contains(&needle),
            Value::Tuple(values) => values.values.borrow().contains(&needle),
            Value::String(value) => match needle {
                Value::String(needle) => value.contains(&needle),
                Value::Char(needle) => value.contains(needle),
                other => return Err(self.type_error("String or Char search value", &other, span)),
            },
            Value::Bytes(value) => match needle {
                Value::Byte(needle) => value.contains(&needle),
                Value::Bytes(needle) => {
                    needle.is_empty() || value.windows(needle.len()).any(|window| window == needle)
                }
                other => return Err(self.type_error("Bytes or Byte search value", &other, span)),
            },
            Value::Map(map) => {
                let key = RuntimeKey::from_value(&needle)
                    .ok_or_else(|| self.type_error("hashable Map key", &needle, span))?;
                map.borrow().entries.contains_key(&key)
            }
            Value::Set(set) => {
                let key = RuntimeKey::from_value(&needle)
                    .ok_or_else(|| self.type_error("hashable Set member", &needle, span))?;
                set.borrow().members.contains(&key)
            }
            other => {
                return Err(self.type_error(
                    "Array, Tuple, String, Bytes, Map, or Set",
                    &other,
                    span,
                ))
            }
        };
        self.stack.push(Value::Bool(found));
        Ok(())
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
            Value::Tuple(values) => values.values.borrow().len(),
            Value::Map(map) => map.borrow().entries.len(),
            Value::Set(set) => set.borrow().members.len(),
            Value::String(value) => value.chars().count(),
            Value::Bytes(value) => value.len(),
            other => {
                return Err(self.type_error(
                    "Array, Tuple, Map, Set, String, or Bytes",
                    &other,
                    span,
                ))
            }
        };
        self.stack.push(Value::Int(BigInt::from(count)));
        Ok(())
    }

    fn iter(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let values = match value {
            Value::Array(values) => values.borrow().clone(),
            Value::Tuple(values) => values.values.borrow().clone(),
            Value::Map(map) => map
                .borrow()
                .order
                .iter()
                .map(|(_, value)| value.clone())
                .collect(),
            Value::Set(set) => set
                .borrow()
                .order
                .iter()
                .map(|(_, value)| value.clone())
                .collect(),
            Value::String(value) => value.chars().map(Value::Char).collect(),
            Value::Bytes(value) => value.into_iter().map(Value::Byte).collect(),
            other => {
                return Err(self.type_error(
                    "Array, Tuple, Map, Set, String, or Bytes",
                    &other,
                    span,
                ))
            }
        };
        let iterator = self.allocate_iterator(IteratorState {
            values,
            next_index: 0,
        });
        self.stack.push(iterator);
        Ok(())
    }

    fn slice(&mut self, span: Span) -> Result<(), RuntimeError> {
        let end_value = self.pop(span)?;
        let start_value = self.pop(span)?;
        let collection = self.pop(span)?;
        let (start, end) = self.slice_bounds(start_value, end_value, span)?;
        let sliced = match collection {
            Value::Array(values) => {
                let values = values.borrow();
                if start > end || end > values.len() {
                    return Err(self.index_error(span));
                }
                self.allocate_array(values[start..end].to_vec())
            }
            Value::Tuple(values) => {
                let values = values.values.borrow();
                if start > end || end > values.len() {
                    return Err(self.index_error(span));
                }
                self.allocate_tuple(values[start..end].to_vec())
            }
            Value::String(value) => {
                let length = value.chars().count();
                if start > end || end > length {
                    return Err(self.index_error(span));
                }
                Value::String(value.chars().skip(start).take(end - start).collect())
            }
            Value::Bytes(value) => {
                if start > end || end > value.len() {
                    return Err(self.index_error(span));
                }
                Value::Bytes(value[start..end].to_vec())
            }
            other => return Err(self.type_error("Array, Tuple, String, or Bytes", &other, span)),
        };
        self.stack.push(sliced);
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
        if self.executed_steps > self.limits.max_instructions {
            return Err(self.error(RuntimeErrorKind::ExecutionLimit, span));
        }
        self.check_resource_limits(span)
    }

    fn concat(&mut self, span: Span) -> Result<(), RuntimeError> {
        let right = self.pop(span)?;
        let left = self.pop(span)?;
        let additional_bytes = match (&left, &right) {
            (Value::String(left), Value::String(right)) => left
                .capacity()
                .saturating_add(right.capacity())
                .saturating_add(left.len())
                .saturating_add(right.len()),
            (Value::Bytes(left), Value::Bytes(right)) => left
                .capacity()
                .saturating_add(right.capacity())
                .saturating_add(left.len())
                .saturating_add(right.len()),
            (Value::Array(left), Value::Array(right)) => {
                let left_values = left.borrow();
                let right_values = right.borrow();
                let result_len = left_values.len().saturating_add(right_values.len());
                let result_capacity_bytes = result_len
                    .saturating_mul(2)
                    .saturating_mul(size_of::<Value>());
                let temporary_clone_bytes =
                    left_values.capacity().saturating_mul(size_of::<Value>());
                let cloned_payload_bytes = estimate_cloned_value_payloads(&left_values)
                    .saturating_add(estimate_cloned_value_payloads(&right_values));
                result_capacity_bytes
                    .saturating_add(temporary_clone_bytes)
                    .saturating_add(cloned_payload_bytes)
            }
            (Value::Tuple(left), Value::Tuple(right)) => {
                let left_values = left.values.borrow();
                let right_values = right.values.borrow();
                let result_len = left_values.len().saturating_add(right_values.len());
                let result_capacity_bytes = result_len
                    .saturating_mul(2)
                    .saturating_mul(size_of::<Value>());
                let temporary_clone_bytes =
                    left_values.capacity().saturating_mul(size_of::<Value>());
                let cloned_payload_bytes = estimate_cloned_value_payloads(&left_values)
                    .saturating_add(estimate_cloned_value_payloads(&right_values));
                result_capacity_bytes
                    .saturating_add(temporary_clone_bytes)
                    .saturating_add(cloned_payload_bytes)
            }
            _ => 0,
        };
        if additional_bytes > 0 {
            self.ensure_memory_budget(additional_bytes, span)?;
        }
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
                self.allocate_array(values)
            }
            (Value::Tuple(left), Value::Tuple(right)) => {
                let mut values = left.values.borrow().clone();
                values.extend(right.values.borrow().iter().cloned());
                self.allocate_tuple(values)
            }
            (left, _) => {
                return Err(self.type_error(
                    "matching String, Bytes, Array, or Tuple values",
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
            let input_bytes =
                estimated_bigint_bytes(left).saturating_add(estimated_bigint_bytes(right));
            let max_operand_bytes =
                usize::try_from(left.bits().max(right.bits()).saturating_add(7) / 8)
                    .unwrap_or(usize::MAX);
            self.ensure_memory_budget(
                input_bytes.saturating_add(max_operand_bytes.saturating_mul(3)),
                span,
            )?;
            let quotient = left / right;
            self.stack.push(Value::Int(left - quotient * right));
            return Ok(());
        }

        match (&left, &right) {
            (Value::Int(a), Value::Int(b)) => {
                if opcode == 4 && b == &BigInt::from(0) {
                    return Err(self.error(RuntimeErrorKind::DivisionByZero, span));
                }
                let input_bytes =
                    estimated_bigint_bytes(a).saturating_add(estimated_bigint_bytes(b));
                let result_bits = match opcode {
                    1 | 2 => a.bits().max(b.bits()).saturating_add(1),
                    3 => a.bits().saturating_add(b.bits()),
                    4 => a.bits(),
                    _ => unreachable!("basic arithmetic opcode was range-checked"),
                };
                let result_bytes =
                    usize::try_from(result_bits.saturating_add(7) / 8).unwrap_or(usize::MAX);
                self.ensure_memory_budget(input_bytes.saturating_add(result_bytes), span)?;
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
                .get(&id)
                .expect("binding index was found above")
                .clone();
            let mut binding = binding.borrow_mut();
            if !binding.mutable {
                return Err(self.error(
                    RuntimeErrorKind::ReferenceError("cannot update an immutable binding"),
                    span,
                ));
            }
            binding.value = value;
        } else {
            self.scopes
                .last_mut()
                .expect("root scope exists")
                .insert(id, binding_cell(value, true));
        }
        Ok(())
    }

    fn store_global(&mut self, span: Span) -> Result<(), RuntimeError> {
        let id = self.pop_identifier(span)?;
        let value = self.pop(span)?;
        let existing = self.scopes[0].get(&id).cloned();
        if let Some(binding) = existing {
            if !binding.borrow().mutable {
                return Err(self.error(
                    RuntimeErrorKind::ReferenceError("cannot update an immutable global"),
                    span,
                ));
            }
            binding.borrow_mut().value = value;
        } else {
            self.scopes[0].insert(id, binding_cell(value, true));
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
        self.stack.push(binding.borrow().value.clone());
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
        scope.insert(id, binding_cell(value, mutable));
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
            Some(binding) if !binding.borrow().mutable => Err(self.error(
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

    fn index_from_value(&self, value: Value, span: Span) -> Result<usize, RuntimeError> {
        let Value::Int(index) = value else {
            return Err(self.type_error("nonnegative Int index", &value, span));
        };
        index
            .to_string()
            .parse::<usize>()
            .map_err(|_| self.index_error(span))
    }

    fn slice_bounds(
        &self,
        start: Value,
        end: Value,
        span: Span,
    ) -> Result<(usize, usize), RuntimeError> {
        Ok((
            self.index_from_value(start, span)?,
            self.index_from_value(end, span)?,
        ))
    }

    fn index_error(&self, span: Span) -> RuntimeError {
        self.error(
            RuntimeErrorKind::IndexError("index or slice is out of bounds"),
            span,
        )
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
        let max_line_bytes = self.available_memory_bytes(span)?;
        let memory_error = self.error(RuntimeErrorKind::ResourceLimit("memory"), span);
        let mut line = Vec::new();
        loop {
            let buffer = match self.input.fill_buf() {
                Ok(buffer) => buffer,
                Err(error) => {
                    return Err(self.error(RuntimeErrorKind::IOError(error.to_string()), span));
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
                if Self::reserve_input_bytes(&mut line, index, max_line_bytes).is_err() {
                    return Err(memory_error);
                }
                line.extend_from_slice(&buffer[..index]);
                self.input.consume(index + 1);
                if terminator == b'\r' {
                    let next_is_lf = match self.input.fill_buf() {
                        Ok(buffer) => buffer.first() == Some(&b'\n'),
                        Err(error) => {
                            return Err(
                                self.error(RuntimeErrorKind::IOError(error.to_string()), span)
                            );
                        }
                    };
                    if next_is_lf {
                        self.input.consume(1);
                    }
                }
                return Ok(Some(line));
            }
            let length = buffer.len();
            if Self::reserve_input_bytes(&mut line, length, max_line_bytes).is_err() {
                return Err(memory_error);
            }
            line.extend_from_slice(buffer);
            self.input.consume(length);
        }
    }

    fn reserve_input_bytes(
        line: &mut Vec<u8>,
        additional: usize,
        max_line_bytes: usize,
    ) -> Result<(), ()> {
        if line.len().saturating_add(additional) > max_line_bytes {
            return Err(());
        }
        line.try_reserve_exact(additional).map_err(|_| ())?;
        if line.capacity() > max_line_bytes {
            return Err(());
        }
        Ok(())
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
        RuntimeError {
            kind,
            span,
            stack_trace: self.call_trace.clone(),
        }
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
        Statement::Try(block) => block.span,
    }
}

fn estimated_key_clone_bytes(value: &Value, depth: usize) -> usize {
    if depth >= 256 {
        return usize::MAX;
    }
    match value {
        Value::Int(value) => {
            usize::try_from(value.bits().saturating_add(7) / 8).unwrap_or(usize::MAX)
        }
        Value::String(value) => value.capacity(),
        Value::Bytes(value) => value.capacity(),
        Value::Tuple(values) => {
            let values = values.values.borrow();
            values
                .len()
                .saturating_mul(size_of::<RuntimeKey>())
                .saturating_add(values.iter().fold(0usize, |total, child| {
                    total.saturating_add(estimated_key_clone_bytes(child, depth + 1))
                }))
        }
        Value::Float(_)
        | Value::Bool(_)
        | Value::Byte(_)
        | Value::Char(_)
        | Value::Null
        | Value::Array(_)
        | Value::Map(_)
        | Value::Set(_)
        | Value::Iterator(_)
        | Value::Function(_)
        | Value::Closure(_)
        | Value::Object(_) => 0,
    }
}

fn estimated_unmanaged_payload_bytes(value: &Value, depth: usize) -> usize {
    if depth >= 256 {
        return usize::MAX;
    }
    match value {
        Value::Int(value) => estimated_bigint_bytes(value),
        Value::String(value) => value.capacity(),
        Value::Bytes(value) => value.capacity(),
        Value::Function(function) => {
            usize::try_from(function.id.bits().saturating_add(7) / 8).unwrap_or(usize::MAX)
        }
        Value::Float(_)
        | Value::Bool(_)
        | Value::Byte(_)
        | Value::Char(_)
        | Value::Null
        | Value::Array(_)
        | Value::Tuple(_)
        | Value::Map(_)
        | Value::Set(_)
        | Value::Iterator(_)
        | Value::Closure(_)
        | Value::Object(_) => 0,
    }
}

fn estimated_bigint_bytes(value: &BigInt) -> usize {
    usize::try_from(value.bits().saturating_add(7) / 8).unwrap_or(usize::MAX)
}

fn estimate_cloned_value_payloads(values: &[Value]) -> usize {
    values.iter().fold(0usize, |total, value| {
        total.saturating_add(estimated_unmanaged_payload_bytes(value, 0))
    })
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
