//! Reference execution for literals, primitive operations, variables, and I/O.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::mem::size_of;
use std::time::{Duration, Instant};

use num_bigint::Sign;
use num_traits::{ToPrimitive, Zero};
use star_common::SourcePosition;
use star_compiler::Capability;
use star_parser::Span;
use star_parser::{
    DataLiteral, ForBlock, FunctionBlock, FunctionKind, IfBlock, Instruction, InstructionOpcode,
    LoopBlock, Program, ScopeBlock, Statement, TryBlock,
};

use crate::{
    gc::{GcError, GcHeap, GcStats},
    runtime::host::HostServices,
    runtime::stdio::{BufferedOutput, InputError, StandardInput},
    semantic::{self, ResolvedFunction},
    value::{
        binding_cell, BindingCell, ClosureState, FunctionRef, InterfaceDefinition, IteratorState,
        MapState, ObjectState, RuntimeKey, SetState, StructDefinition, Value,
    },
    BigInt,
};

pub mod collections;
pub mod concurrency;
pub mod ffi;
pub mod host;
pub mod json;
pub mod math;
pub mod network;
pub mod path;
pub mod stdio;
pub mod strings;
pub mod worker;

mod bytecode_vm;
mod guest_concurrency;
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
    /// A task was awaited after cancellation.
    TaskCancelled,
    /// A cooperative channel or mutex has no runnable task that can unblock it.
    Deadlock,
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
            Self::TaskCancelled => formatter.write_str("task was cancelled"),
            Self::Deadlock => formatter.write_str("cooperative scheduler deadlock"),
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
    /// Whether the trace omits outer frames to remain within the memory limit.
    pub stack_trace_truncated: bool,
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
        if self.stack_trace_truncated {
            write!(formatter, "\n  <stack trace truncated by memory limit>")?;
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
/// Source `ASYNC_FUNC` calls create cooperative tasks that run when scheduled
/// or awaited; bytecode task scheduling belongs to `BytecodeVm`.
pub struct Interpreter<R: BufRead, W: Write> {
    input: StandardInput<R>,
    output: BufferedOutput<W>,
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
    host_services: HostServices,
    limits: ExecutionLimits,
    external_memory_bytes: usize,
    execution_started: Option<Instant>,
    executed_steps: u64,
    tasks: HashMap<u64, GuestTask>,
    ready_tasks: VecDeque<u64>,
    next_task_id: u64,
    current_task_id: Option<u64>,
    channels: HashMap<u64, GuestChannel>,
    next_channel_id: u64,
    next_channel_waiter_id: u64,
    mutexes: HashMap<u64, GuestMutex>,
    next_mutex_id: u64,
    pending_channel_message_bytes: usize,
}

pub(crate) struct NativeCallPlan {
    function: ffi::NativeFunctionHandle,
    arguments: Vec<ffi::NativeValue>,
    callback_roots: Vec<Value>,
    capabilities: BTreeSet<Capability>,
}

impl NativeCallPlan {
    pub(crate) fn call(
        &self,
        callback_context: &mut ffi::NativeCallbackContext,
    ) -> Result<ffi::NativeValue, ffi::NativeCallError> {
        self.function
            .call_scoped(&self.arguments, &self.capabilities, Some(callback_context))
    }

    fn take_callback_roots(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.callback_roots)
    }
}

#[derive(Debug)]
struct GuestChannel {
    capacity: usize,
    queue: VecDeque<Value>,
    waiting_receivers: VecDeque<GuestChannelReceiver>,
}

#[derive(Debug)]
struct GuestChannelReceiver {
    waiter_id: u64,
    task_id: u64,
    transfer: Option<Value>,
}

#[derive(Debug, Default)]
struct GuestMutex {
    owner: Option<u64>,
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

#[derive(Clone, Debug)]
enum GuestTaskStatus {
    Pending,
    Running,
    Completed(Value),
    Failed(RuntimeError),
    Cancelled,
}

#[derive(Clone, Copy, Debug)]
enum GuestTaskStatusKind {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug)]
struct GuestTask {
    function: ResolvedFunction,
    arguments: Vec<Value>,
    captures: Vec<(BigInt, BindingCell)>,
    status: GuestTaskStatus,
}

impl<R: BufRead, W: Write> Interpreter<R, W> {
    /// Creates an interpreter with empty operand and module-global state.
    pub fn new(input: R, output: W) -> Self {
        Self {
            input: StandardInput::new(input),
            output: BufferedOutput::new(output),
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
            host_services: HostServices::new(BTreeSet::new()),
            limits: ExecutionLimits::default(),
            external_memory_bytes: 0,
            execution_started: None,
            executed_steps: 0,
            tasks: HashMap::new(),
            ready_tasks: VecDeque::new(),
            next_task_id: 1,
            current_task_id: None,
            channels: HashMap::new(),
            next_channel_id: 1,
            next_channel_waiter_id: 1,
            mutexes: HashMap::new(),
            next_mutex_id: 1,
            pending_channel_message_bytes: 0,
        }
    }

    /// Sets all runtime resource ceilings for subsequent executions.
    pub fn with_execution_limits(mut self, limits: ExecutionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets the host capabilities available through this interpreter's host services.
    pub fn with_capabilities(mut self, capabilities: BTreeSet<Capability>) -> Self {
        self.host_services = HostServices::new(capabilities);
        self
    }

    /// Returns the configured host services for trusted host setup or use.
    pub fn host_services(&self) -> &HostServices {
        &self.host_services
    }

    /// Returns mutable host services for trusted host setup or use.
    pub fn host_services_mut(&mut self) -> &mut HostServices {
        &mut self.host_services
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
        estimated = estimated
            .saturating_add(self.external_memory_bytes)
            .saturating_add(self.call_trace_memory_bytes().saturating_mul(2))
            .saturating_add(self.call_frame_bookkeeping_memory_bytes())
            .saturating_add(self.task_memory_bytes())
            .saturating_add(self.concurrent_primitive_memory_bytes())
            .saturating_add(self.pending_channel_message_bytes);
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
            estimated = estimated
                .saturating_add(self.external_memory_bytes)
                .saturating_add(self.call_trace_memory_bytes().saturating_mul(2))
                .saturating_add(self.call_frame_bookkeeping_memory_bytes())
                .saturating_add(self.task_memory_bytes())
                .saturating_add(self.concurrent_primitive_memory_bytes())
                .saturating_add(self.pending_channel_message_bytes);
        }
        if estimated.saturating_add(additional) > self.limits.max_memory_bytes {
            return Err(self.error(RuntimeErrorKind::ResourceLimit("memory"), span));
        }
        Ok(())
    }

    pub(crate) fn set_external_memory_bytes(&mut self, bytes: usize) {
        self.external_memory_bytes = bytes;
    }

    fn call_trace_memory_bytes(&self) -> usize {
        self.call_trace
            .capacity()
            .saturating_mul(size_of::<StackTraceFrame>())
            .saturating_add(
                self.call_trace
                    .iter()
                    .map(|frame| estimated_bigint_bytes(&frame.function_id))
                    .fold(0usize, usize::saturating_add),
            )
    }

    fn call_frame_bookkeeping_memory_bytes(&self) -> usize {
        self.scope_floors
            .capacity()
            .saturating_mul(size_of::<usize>())
            .saturating_add(
                self.frame_stack_bases
                    .capacity()
                    .saturating_mul(size_of::<usize>()),
            )
    }

    fn task_memory_bytes(&self) -> usize {
        self.tasks
            .capacity()
            .saturating_mul(size_of::<(u64, GuestTask)>())
            .saturating_add(self.ready_tasks.capacity().saturating_mul(size_of::<u64>()))
            .saturating_add(self.tasks.values().fold(0usize, |total, task| {
                total
                    .saturating_add(estimated_function_clone_bytes(&task.function))
                    .saturating_add(task.arguments.capacity().saturating_mul(size_of::<Value>()))
                    .saturating_add(estimate_cloned_value_payloads(&task.arguments))
                    .saturating_add(estimated_capture_clone_bytes(&task.captures))
                    .saturating_add(match &task.status {
                        GuestTaskStatus::Completed(value) => size_of::<Value>()
                            .saturating_add(estimated_unmanaged_payload_bytes(value, 0)),
                        GuestTaskStatus::Failed(error) => estimated_runtime_error_bytes(error),
                        GuestTaskStatus::Pending
                        | GuestTaskStatus::Running
                        | GuestTaskStatus::Cancelled => 0,
                    })
            }))
    }

    fn concurrent_primitive_memory_bytes(&self) -> usize {
        self.channels
            .capacity()
            .saturating_mul(size_of::<(u64, GuestChannel)>())
            .saturating_add(self.channels.values().fold(0usize, |total, channel| {
                total
                    .saturating_add(channel.queue.capacity().saturating_mul(size_of::<Value>()))
                    .saturating_add(
                        channel
                            .waiting_receivers
                            .capacity()
                            .saturating_mul(size_of::<GuestChannelReceiver>()),
                    )
                    .saturating_add(
                        channel
                            .queue
                            .iter()
                            .map(|value| estimated_unmanaged_payload_bytes(value, 0))
                            .fold(0usize, usize::saturating_add),
                    )
                    .saturating_add(
                        channel
                            .waiting_receivers
                            .iter()
                            .filter_map(|receiver| receiver.transfer.as_ref())
                            .map(|value| estimated_unmanaged_payload_bytes(value, 0))
                            .fold(0usize, usize::saturating_add),
                    )
            }))
            .saturating_add(
                self.mutexes
                    .capacity()
                    .saturating_mul(size_of::<(u64, GuestMutex)>()),
            )
    }

    fn task_status_kind_and_copy_bytes(&self, id: u64) -> Option<(GuestTaskStatusKind, usize)> {
        self.tasks.get(&id).map(|task| match &task.status {
            GuestTaskStatus::Pending => (GuestTaskStatusKind::Pending, 0),
            GuestTaskStatus::Running => (GuestTaskStatusKind::Running, 0),
            GuestTaskStatus::Completed(value) => (
                GuestTaskStatusKind::Completed,
                size_of::<Value>().saturating_add(estimated_unmanaged_payload_bytes(value, 0)),
            ),
            GuestTaskStatus::Failed(error) => (
                GuestTaskStatusKind::Failed,
                estimated_runtime_error_bytes(error),
            ),
            GuestTaskStatus::Cancelled => (GuestTaskStatusKind::Cancelled, 0),
        })
    }

    pub(crate) fn call_frame_bookkeeping_growth_bytes(&self) -> usize {
        let scope_capacity =
            projected_capacity_after_push(self.scope_floors.len(), self.scope_floors.capacity());
        let frame_capacity = projected_capacity_after_push(
            self.frame_stack_bases.len(),
            self.frame_stack_bases.capacity(),
        );
        let capture_capacity = projected_capacity_after_push(
            self.capture_vectors.len(),
            self.capture_vectors.capacity(),
        );
        scope_capacity
            .saturating_sub(self.scope_floors.capacity())
            .saturating_add(frame_capacity.saturating_sub(self.frame_stack_bases.capacity()))
            .saturating_mul(size_of::<usize>())
            .saturating_add(
                capture_capacity
                    .saturating_sub(self.capture_vectors.capacity())
                    .saturating_mul(size_of::<Vec<(BigInt, BindingCell)>>()),
            )
    }

    pub(crate) fn reserve_call_frame_bookkeeping(
        &mut self,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let scope_capacity =
            projected_capacity_after_push(self.scope_floors.len(), self.scope_floors.capacity());
        let frame_capacity = projected_capacity_after_push(
            self.frame_stack_bases.len(),
            self.frame_stack_bases.capacity(),
        );
        let capture_capacity = projected_capacity_after_push(
            self.capture_vectors.len(),
            self.capture_vectors.capacity(),
        );
        if scope_capacity > self.scope_floors.capacity()
            && self
                .scope_floors
                .try_reserve_exact(scope_capacity.saturating_sub(self.scope_floors.len()))
                .is_err()
        {
            return Err(self.error(
                RuntimeErrorKind::ResourceLimit("call frame bookkeeping"),
                span,
            ));
        }
        if frame_capacity > self.frame_stack_bases.capacity()
            && self
                .frame_stack_bases
                .try_reserve_exact(frame_capacity.saturating_sub(self.frame_stack_bases.len()))
                .is_err()
        {
            return Err(self.error(
                RuntimeErrorKind::ResourceLimit("call frame bookkeeping"),
                span,
            ));
        }
        if capture_capacity > self.capture_vectors.capacity()
            && self
                .capture_vectors
                .try_reserve_exact(capture_capacity.saturating_sub(self.capture_vectors.len()))
                .is_err()
        {
            return Err(self.error(
                RuntimeErrorKind::ResourceLimit("call frame bookkeeping"),
                span,
            ));
        }
        self.ensure_memory_budget(0, span)
    }

    pub(crate) fn push_value(&mut self, value: Value, span: Span) -> Result<(), RuntimeError> {
        let next_len =
            self.stack.len().checked_add(1).ok_or_else(|| {
                self.error(RuntimeErrorKind::ResourceLimit("operand stack"), span)
            })?;
        if next_len > self.limits.max_stack_values {
            return Err(self.error(RuntimeErrorKind::ResourceLimit("operand stack"), span));
        }
        let current_capacity = self.stack.capacity();
        let target_capacity = if next_len > current_capacity {
            if current_capacity == 0 {
                next_len
            } else {
                current_capacity
                    .saturating_mul(2)
                    .max(next_len)
                    .min(self.limits.max_stack_values)
            }
        } else {
            current_capacity
        };
        let growth_bytes = target_capacity
            .saturating_sub(current_capacity)
            .saturating_mul(size_of::<Value>());
        let payload_bytes = estimated_unmanaged_payload_bytes(&value, 0);
        self.ensure_memory_budget(growth_bytes.saturating_add(payload_bytes), span)?;
        if target_capacity > current_capacity {
            let reserve_result = self
                .stack
                .try_reserve_exact(target_capacity.saturating_sub(self.stack.len()));
            if reserve_result.is_err() {
                return Err(self.error(RuntimeErrorKind::ResourceLimit("operand stack"), span));
            }
        }
        self.stack.push(value);
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
        match (result, self.output.flush()) {
            (Err(error), _) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(error)) => {
                Err(self.error(RuntimeErrorKind::IOError(error.to_string()), empty_span()))
            }
        }
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

    /// Returns the injected I/O objects after successfully flushing output.
    /// On failure, returns the input, buffered output, and error so callers can retry.
    pub fn try_into_io(self) -> Result<(R, W), (R, BufferedOutput<W>, std::io::Error)> {
        let Self { input, output, .. } = self;
        match output.into_inner() {
            Ok(output) => Ok((input.into_inner(), output)),
            Err((output, error)) => Err((input.into_inner(), output, error)),
        }
    }

    /// Returns the injected I/O objects after execution.
    ///
    /// This compatibility method discards any bytes still buffered if flushing
    /// fails. Use [`Self::try_into_io`] when output delivery must be checked.
    pub fn into_io(self) -> (R, W) {
        match self.try_into_io() {
            Ok(io) => io,
            Err((input, output, _error)) => (input, output.into_inner_lossy()),
        }
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
                Statement::DataLiteral(literal) => self.push_literal(literal)?,
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
            let next_payload_bytes = {
                let iterator = iterator.borrow();
                iterator
                    .values
                    .get(iterator.next_index)
                    .map(|value| estimated_unmanaged_payload_bytes(value, 0))
            };
            if let Some(payload_bytes) = next_payload_bytes {
                self.ensure_memory_budget(payload_bytes, block.span)?;
            }
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
        self.ensure_memory_budget(argument_count.saturating_mul(size_of::<Value>()), span)?;
        let mut arguments = Vec::with_capacity(argument_count);
        for _ in 0..argument_count {
            arguments.push(self.pop(span)?);
        }
        arguments.reverse();
        let function_copy_bytes = self.functions.get(&function_id).map_or_else(
            || {
                Err(self.error(
                    RuntimeErrorKind::ReferenceError("function does not exist"),
                    span,
                ))
            },
            |function| Ok(estimated_function_clone_bytes(function)),
        )?;
        self.ensure_memory_budget(
            function_copy_bytes
                .saturating_add(estimated_bigint_bytes(&function_id))
                .saturating_add(arguments.capacity().saturating_mul(size_of::<Value>()))
                .saturating_add(estimate_cloned_value_payloads(&arguments)),
            span,
        )?;
        let function = self.functions.get(&function_id).cloned().ok_or_else(|| {
            self.error(
                RuntimeErrorKind::ReferenceError("function does not exist"),
                span,
            )
        })?;
        let value = self.invoke_function(function, arguments, Vec::new(), span)?;
        self.push_value(value, span)?;
        Ok(())
    }

    fn create_task(
        &mut self,
        function: ResolvedFunction,
        arguments: Vec<Value>,
        captures: Vec<(BigInt, BindingCell)>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let parameter_count = function.parameter_ids.len();
        let arity_matches = if function.variadic {
            arguments.len() >= parameter_count.saturating_sub(1)
        } else {
            arguments.len() == parameter_count
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
        let Some(next_id) = self.next_task_id.checked_add(1) else {
            return Err(self.error(RuntimeErrorKind::ResourceLimit("task identifiers"), span));
        };
        let task_bytes = estimated_function_clone_bytes(&function)
            .saturating_add(arguments.capacity().saturating_mul(size_of::<Value>()))
            .saturating_add(estimate_cloned_value_payloads(&arguments))
            .saturating_add(estimated_capture_clone_bytes(&captures))
            .saturating_add(size_of::<GuestTask>())
            .saturating_add(size_of::<u64>() * 2)
            .saturating_add(
                projected_capacity_after_push(self.tasks.len(), self.tasks.capacity())
                    .saturating_sub(self.tasks.capacity())
                    .saturating_mul(size_of::<(u64, GuestTask)>()),
            )
            .saturating_add(
                projected_capacity_after_push(self.ready_tasks.len(), self.ready_tasks.capacity())
                    .saturating_sub(self.ready_tasks.capacity())
                    .saturating_mul(size_of::<u64>()),
            );
        self.ensure_memory_budget(task_bytes, span)?;
        self.tasks
            .try_reserve(1)
            .map_err(|_| self.error(RuntimeErrorKind::ResourceLimit("task registry"), span))?;
        self.ready_tasks
            .try_reserve(1)
            .map_err(|_| self.error(RuntimeErrorKind::ResourceLimit("task queue"), span))?;
        let id = self.next_task_id;
        self.next_task_id = next_id;
        self.tasks.insert(
            id,
            GuestTask {
                function,
                arguments,
                captures,
                status: GuestTaskStatus::Pending,
            },
        );
        self.ready_tasks.push_back(id);
        Ok(Value::Task(id))
    }

    fn run_task(&mut self, id: u64, span: Span) -> Result<Value, RuntimeError> {
        let Some((status, clone_bytes)) = self.task_status_kind_and_copy_bytes(id) else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("task does not exist"),
                span,
            ));
        };
        match status {
            GuestTaskStatusKind::Completed => {
                self.ensure_memory_budget(clone_bytes, span)?;
                let Some(GuestTaskStatus::Completed(value)) =
                    self.tasks.get(&id).map(|task| &task.status)
                else {
                    return Err(
                        self.error(RuntimeErrorKind::ReferenceError("task state changed"), span)
                    );
                };
                return Ok(value.clone());
            }
            GuestTaskStatusKind::Failed => {
                self.ensure_memory_budget(clone_bytes, span)?;
                let Some(GuestTaskStatus::Failed(error)) =
                    self.tasks.get(&id).map(|task| &task.status)
                else {
                    return Err(
                        self.error(RuntimeErrorKind::ReferenceError("task state changed"), span)
                    );
                };
                return Err(error.clone());
            }
            GuestTaskStatusKind::Cancelled => {
                return Err(self.error(RuntimeErrorKind::TaskCancelled, span))
            }
            GuestTaskStatusKind::Running => {
                return Err(self.error(
                    RuntimeErrorKind::ReferenceError("task cannot await itself while running"),
                    span,
                ))
            }
            GuestTaskStatusKind::Pending => {}
        }
        let Some(clone_bytes) = self.tasks.get(&id).map(|task| {
            let clone_bytes = estimated_function_clone_bytes(&task.function)
                .saturating_add(task.arguments.capacity().saturating_mul(size_of::<Value>()))
                .saturating_add(estimate_cloned_value_payloads(&task.arguments))
                .saturating_add(estimated_capture_clone_bytes(&task.captures));
            clone_bytes
        }) else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("task does not exist"),
                span,
            ));
        };
        self.ensure_memory_budget(clone_bytes, span)?;
        let task = self.tasks.get_mut(&id).expect("task checked above");
        task.status = GuestTaskStatus::Running;
        let function = task.function.clone();
        let arguments = task.arguments.clone();
        let captures = task.captures.clone();
        let previous_task = self.current_task_id.replace(id);
        let result = self.invoke_function_body(function, arguments, captures, span);
        self.current_task_id = previous_task;
        self.cleanup_task_concurrency(id);
        let status_copy_bytes = match &result {
            Ok(value) => {
                size_of::<Value>().saturating_add(estimated_unmanaged_payload_bytes(value, 0))
            }
            Err(error) => estimated_runtime_error_bytes(error),
        };
        if self.ensure_memory_budget(status_copy_bytes, span).is_err() {
            let error = RuntimeError {
                kind: RuntimeErrorKind::ResourceLimit("memory"),
                span,
                stack_trace: Vec::new(),
                stack_trace_truncated: true,
            };
            if let Some(task) = self.tasks.get_mut(&id) {
                task.status = GuestTaskStatus::Failed(error.clone());
            }
            return Err(error);
        }
        if let Some(task) = self.tasks.get_mut(&id) {
            task.status = match &result {
                Ok(value) => GuestTaskStatus::Completed(value.clone()),
                Err(error) => GuestTaskStatus::Failed(error.clone()),
            };
        }
        result
    }

    fn run_next_task(&mut self, span: Span) -> Result<bool, RuntimeError> {
        while let Some(id) = self.ready_tasks.pop_front() {
            let pending = self
                .tasks
                .get(&id)
                .is_some_and(|task| matches!(task.status, GuestTaskStatus::Pending));
            if pending {
                let _ = self.run_task(id, span);
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn await_task(&mut self, id: u64, span: Span) -> Result<Value, RuntimeError> {
        loop {
            let Some((status, _)) = self.task_status_kind_and_copy_bytes(id) else {
                return Err(self.error(
                    RuntimeErrorKind::ReferenceError("task does not exist"),
                    span,
                ));
            };
            match status {
                GuestTaskStatusKind::Completed
                | GuestTaskStatusKind::Failed
                | GuestTaskStatusKind::Cancelled
                | GuestTaskStatusKind::Running => return self.run_task(id, span),
                GuestTaskStatusKind::Pending => {
                    if !self.run_next_task(span)? {
                        return self.run_task(id, span);
                    }
                }
            }
        }
    }

    fn spawn_task(&mut self, span: Span) -> Result<(), RuntimeError> {
        if !self.host_services.task_creation_enabled() {
            return Err(self.error(RuntimeErrorKind::PermissionError("task.spawn"), span));
        }
        let callable = self.pop(span)?;
        let (module_index, function_id, capture_bytes) = match &callable {
            Value::Function(function_ref) => (function_ref.module_index, &function_ref.id, 0usize),
            Value::Closure(closure) => {
                let capture_bytes = {
                    let captures = closure.captures.borrow();
                    estimated_capture_clone_bytes(&captures)
                };
                (
                    closure.function.module_index,
                    &closure.function.id,
                    capture_bytes,
                )
            }
            other => return Err(self.type_error("Function or Closure", other, span)),
        };
        if module_index != 0 {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("function belongs to another module"),
                span,
            ));
        }
        let function_clone_bytes = self
            .functions
            .get(function_id)
            .map(estimated_function_clone_bytes)
            .ok_or_else(|| {
                self.error(
                    RuntimeErrorKind::ReferenceError("function does not exist"),
                    span,
                )
            })?;
        self.ensure_memory_budget(function_clone_bytes.saturating_add(capture_bytes), span)?;
        let function = self.functions.get(function_id).cloned().ok_or_else(|| {
            self.error(
                RuntimeErrorKind::ReferenceError("function does not exist"),
                span,
            )
        })?;
        let captures = match &callable {
            Value::Closure(closure) => closure.captures.borrow().clone(),
            _ => Vec::new(),
        };
        let task = self.create_task(function, Vec::new(), captures, span)?;
        self.push_value(task, span)
    }

    fn await_instruction(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let Value::Task(id) = value else {
            return Err(self.type_error("Task", &value, span));
        };
        let result = self.await_task(id, span)?;
        self.push_value(result, span)
    }

    fn cancel_task(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let Value::Task(id) = value else {
            return Err(self.type_error("Task", &value, span));
        };
        let Some(task) = self.tasks.get_mut(&id) else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("task does not exist"),
                span,
            ));
        };
        let accepted = if matches!(task.status, GuestTaskStatus::Pending) {
            task.status = GuestTaskStatus::Cancelled;
            true
        } else {
            false
        };
        self.push_value(Value::Bool(accepted), span)
    }

    fn invoke_function(
        &mut self,
        function: ResolvedFunction,
        arguments: Vec<Value>,
        captures: Vec<(BigInt, BindingCell)>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if function.kind == FunctionKind::Asynchronous {
            return self.create_task(function, arguments, captures, span);
        }
        self.invoke_function_body(function, arguments, captures, span)
    }

    fn invoke_function_body(
        &mut self,
        function: ResolvedFunction,
        arguments: Vec<Value>,
        captures: Vec<(BigInt, BindingCell)>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let argument_count = arguments.len();
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

        let fixed_count = if function.variadic {
            parameter_count.saturating_sub(1)
        } else {
            parameter_count
        };
        let rest_count = argument_count.saturating_sub(fixed_count);
        let argument_storage_bytes = arguments
            .capacity()
            .saturating_mul(size_of::<Value>())
            .saturating_add(estimate_cloned_value_payloads(&arguments));
        let capture_storage_bytes = captures
            .capacity()
            .saturating_mul(size_of::<(BigInt, BindingCell)>())
            .saturating_add(
                captures
                    .iter()
                    .map(|(id, _)| estimated_bigint_bytes(id))
                    .fold(0usize, usize::saturating_add),
            );
        let capture_scope_bytes = if captures.is_empty() {
            0
        } else {
            captures
                .len()
                .saturating_mul(size_of::<(BigInt, BindingCell)>() + 2 * size_of::<usize>())
                .saturating_add(
                    captures
                        .iter()
                        .map(|(id, _)| estimated_bigint_bytes(id))
                        .fold(0usize, usize::saturating_add),
                )
        };
        let parameter_storage_bytes = parameter_count
            .saturating_mul(size_of::<(BigInt, BindingCell)>() + 2 * size_of::<usize>())
            .saturating_add(
                function
                    .parameter_ids
                    .iter()
                    .map(estimated_bigint_bytes)
                    .fold(0usize, usize::saturating_add),
            )
            .saturating_add(
                parameter_count.saturating_mul(size_of::<Value>() + 4 * size_of::<usize>()),
            )
            .saturating_add(estimate_cloned_value_payloads(&arguments[..fixed_count]));
        let rest_vector_bytes = if function.variadic {
            rest_count.saturating_mul(size_of::<Value>())
        } else {
            0
        };
        let next_trace_len = self.call_trace.len().saturating_add(1);
        let trace_capacity = if next_trace_len <= self.call_trace.capacity() {
            self.call_trace.capacity()
        } else {
            self.call_trace
                .capacity()
                .saturating_mul(2)
                .max(next_trace_len)
                .min(self.limits.max_call_depth.max(1))
        };
        let trace_capacity_growth = trace_capacity
            .saturating_sub(self.call_trace.capacity())
            .saturating_mul(size_of::<StackTraceFrame>());
        self.ensure_memory_budget(
            argument_storage_bytes
                .saturating_add(capture_storage_bytes)
                .saturating_add(capture_scope_bytes)
                .saturating_add(parameter_storage_bytes)
                .saturating_add(rest_vector_bytes)
                .saturating_add(self.call_frame_bookkeeping_growth_bytes())
                .saturating_add(trace_capacity_growth.saturating_mul(2))
                .saturating_add(estimated_bigint_bytes(&function.id).saturating_mul(2)),
            span,
        )?;
        self.reserve_call_frame_bookkeeping(span)?;
        if trace_capacity > self.call_trace.capacity() {
            self.call_trace
                .try_reserve_exact(trace_capacity.saturating_sub(self.call_trace.len()))
                .map_err(|_| self.error(RuntimeErrorKind::ResourceLimit("call trace"), span))?;
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
            self.push_value(Value::Function(function), span)?;
        } else {
            let closure = self.allocate_closure(ClosureState {
                function,
                captures: RefCell::new(captures),
            });
            self.push_value(closure, span)?;
        }
        Ok(())
    }

    fn call_value(&mut self, span: Span) -> Result<(), RuntimeError> {
        let callable = self.pop(span)?;
        let argument_count = self.pop_count(span, "representable nonnegative argument count")?;
        if argument_count > self.stack.len() {
            return Err(self.error(RuntimeErrorKind::StackUnderflow, span));
        }
        self.ensure_memory_budget(argument_count.saturating_mul(size_of::<Value>()), span)?;
        let mut arguments = Vec::with_capacity(argument_count);
        for _ in 0..argument_count {
            arguments.push(self.pop(span)?);
        }
        arguments.reverse();
        let (function_ref, captures) = match callable {
            Value::Function(function) => (function, Vec::new()),
            Value::Closure(closure) => {
                let captures = closure.captures.borrow();
                self.ensure_memory_budget(
                    estimated_capture_clone_bytes(&captures)
                        .saturating_add(size_of::<FunctionRef>())
                        .saturating_add(estimated_bigint_bytes(&closure.function.id))
                        .saturating_add(arguments.capacity().saturating_mul(size_of::<Value>()))
                        .saturating_add(estimate_cloned_value_payloads(&arguments)),
                    span,
                )?;
                (closure.function.clone(), captures.clone())
            }
            other => return Err(self.type_error("Function or Closure", &other, span)),
        };
        if function_ref.module_index != 0 {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("function belongs to another module"),
                span,
            ));
        }
        let Some(function_copy_bytes) = self
            .functions
            .get(&function_ref.id)
            .map(estimated_function_clone_bytes)
        else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("function does not exist"),
                span,
            ));
        };
        self.ensure_memory_budget(
            function_copy_bytes
                .saturating_add(size_of::<FunctionRef>())
                .saturating_add(estimated_bigint_bytes(&function_ref.id))
                .saturating_add(estimated_capture_clone_bytes(&captures))
                .saturating_add(arguments.capacity().saturating_mul(size_of::<Value>()))
                .saturating_add(estimate_cloned_value_payloads(&arguments)),
            span,
        )?;
        let Some(function) = self.functions.get(&function_ref.id).cloned() else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("function does not exist"),
                span,
            ));
        };
        let result = self.invoke_function(function, arguments, captures, span)?;
        self.push_value(result, span)?;
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
            let Some(function_copy_bytes) = self
                .functions
                .get(constructor_id)
                .map(estimated_function_clone_bytes)
            else {
                return Err(self.error(
                    RuntimeErrorKind::ReferenceError("constructor function does not exist"),
                    span,
                ));
            };
            self.ensure_memory_budget(
                function_copy_bytes
                    .saturating_add(arguments.capacity().saturating_mul(size_of::<Value>()))
                    .saturating_add(estimate_cloned_value_payloads(&arguments)),
                span,
            )?;
            let Some(function) = self.functions.get(constructor_id).cloned() else {
                return Err(self.error(
                    RuntimeErrorKind::ReferenceError("constructor function does not exist"),
                    span,
                ));
            };
            self.ensure_memory_budget(
                arguments
                    .capacity()
                    .saturating_mul(size_of::<Value>())
                    .saturating_add(estimate_cloned_value_payloads(&arguments))
                    .saturating_add(
                        argument_count
                            .saturating_add(1)
                            .saturating_mul(size_of::<Value>()),
                    ),
                span,
            )?;
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
        self.push_value(object, span)?;
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
        self.push_value(value, span)?;
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
        self.ensure_memory_budget(
            argument_count
                .saturating_add(1)
                .saturating_mul(size_of::<Value>())
                .saturating_add(estimated_unmanaged_payload_bytes(&receiver, 0)),
            span,
        )?;
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
        let Some(function_copy_bytes) = self
            .functions
            .get(&function_id)
            .map(estimated_function_clone_bytes)
        else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("method function does not exist"),
                span,
            ));
        };
        self.ensure_memory_budget(
            function_copy_bytes
                .saturating_add(arguments.capacity().saturating_mul(size_of::<Value>()))
                .saturating_add(estimate_cloned_value_payloads(&arguments)),
            span,
        )?;
        let Some(function) = self.functions.get(&function_id).cloned() else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("method function does not exist"),
                span,
            ));
        };
        let result = self.invoke_function(function, arguments, Vec::new(), span)?;
        self.push_value(result, span)?;
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
        self.push_value(Value::Bool(matches), span)?;
        Ok(())
    }

    fn object_clone(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let Value::Object(object) = value else {
            return Err(self.type_error("Struct", &value, span));
        };
        let clone_bytes = {
            let object = object.borrow();
            let field_bytes = object
                .fields
                .capacity()
                .saturating_mul(size_of::<(BigInt, Value)>());
            let owned_payload_bytes = object.fields.iter().fold(
                estimated_bigint_bytes(&object.struct_id),
                |total, (id, value)| {
                    total
                        .saturating_add(estimated_bigint_bytes(id))
                        .saturating_add(estimated_unmanaged_payload_bytes(value, 0))
                },
            );
            size_of::<crate::value::ObjectState>()
                .saturating_add(2 * size_of::<usize>())
                .saturating_add(field_bytes)
                .saturating_add(owned_payload_bytes)
        };
        self.ensure_memory_budget(clone_bytes, span)?;
        let cloned = object.borrow().clone();
        let object = self.allocate_object(cloned);
        self.push_value(object, span)?;
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
                self.push_value(
                    match line {
                        Some(bytes) => Value::String(String::from_utf8(bytes).map_err(|_| {
                            self.error(
                                RuntimeErrorKind::IOError(
                                    "standard input is not valid UTF-8".to_owned(),
                                ),
                                span,
                            )
                        })?),
                        None => Value::Null,
                    },
                    span,
                )?;
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
                        self.push_value(Value::Bool(!value), span)?;
                        Ok(())
                    }
                    other => Err(self.type_error("Bool", &other, span)),
                }
            }
            InstructionOpcode::Extended(0x18) => {
                self.push_value(Value::Null, span)?;
                Ok(())
            }
            InstructionOpcode::Extended(0x19) => {
                self.push_value(Value::Bool(true), span)?;
                Ok(())
            }
            InstructionOpcode::Extended(0x1A) => {
                self.push_value(Value::Bool(false), span)?;
                Ok(())
            }
            InstructionOpcode::Extended(0x10) => self.to_int(span),
            InstructionOpcode::Extended(0x12) => self.to_bool(span),
            InstructionOpcode::Extended(0x14 | 0x16) => self.utf8_encode(span),
            InstructionOpcode::Extended(0x15) => self.utf8_decode(span),
            InstructionOpcode::Extended(0x13) => {
                let value = self.pop(span)?;
                let formatted = self.format_value(&value, span)?;
                self.push_value(Value::String(formatted), span)?;
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
            InstructionOpcode::Extended(0xA1 | 0xA5) => self.await_instruction(span),
            InstructionOpcode::Extended(0xA2) => self.spawn_task(span),
            InstructionOpcode::Extended(0xA3) => {
                if self.current_task_id.is_none() {
                    Err(self.error(
                        RuntimeErrorKind::TypeError {
                            expected: "async task context",
                            found: "main execution context",
                        },
                        span,
                    ))
                } else {
                    self.run_next_task(span).map(|_| ())
                }
            }
            InstructionOpcode::Extended(0xA4) => {
                if self.current_task_id.is_none() {
                    Err(self.error(
                        RuntimeErrorKind::TypeError {
                            expected: "async task context",
                            found: "main execution context",
                        },
                        span,
                    ))
                } else {
                    let value = self.pop(span)?;
                    let Value::Int(milliseconds) = value else {
                        return Err(self.type_error("nonnegative Int milliseconds", &value, span));
                    };
                    let milliseconds = milliseconds.to_u64().ok_or_else(|| {
                        self.error(
                            RuntimeErrorKind::TypeError {
                                expected: "nonnegative representable Int milliseconds",
                                found: "Int",
                            },
                            span,
                        )
                    })?;
                    let duration = Duration::from_millis(milliseconds);
                    if self.execution_started.is_some_and(|started| {
                        started.elapsed().saturating_add(duration) >= self.limits.max_execution_time
                    }) {
                        return Err(
                            self.error(RuntimeErrorKind::ResourceLimit("execution time"), span)
                        );
                    }
                    let Some(deadline) = Instant::now().checked_add(duration) else {
                        return Err(
                            self.error(RuntimeErrorKind::ResourceLimit("execution time"), span)
                        );
                    };
                    while Instant::now() < deadline {
                        if !self.run_next_task(span)? {
                            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                            break;
                        }
                        self.check_resource_limits(span)?;
                    }
                    self.push_value(Value::Null, span)
                }
            }
            InstructionOpcode::Extended(0xA6) => self.cancel_task(span),
            InstructionOpcode::Extended(0xA7) => self.create_channel(span),
            InstructionOpcode::Extended(0xA8) => self.send_channel(span),
            InstructionOpcode::Extended(0xA9) => self.receive_channel(span),
            InstructionOpcode::Extended(0xAA) => self.create_mutex(span),
            InstructionOpcode::Extended(0xAB) => self.acquire_mutex(span),
            InstructionOpcode::Extended(0xAC) => self.release_mutex(span),
            InstructionOpcode::Extended(opcode @ 0xB0..=0xB5) => {
                self.execute_native_instruction(opcode, span, None)
            }
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

    pub(crate) fn execute_native_instruction(
        &mut self,
        opcode: u8,
        span: Span,
        callback_context: Option<&mut ffi::NativeCallbackContext>,
    ) -> Result<(), RuntimeError> {
        use ffi::{NativeBuffer, NativeHandle, NativeLibraryHandle, NativeSignature};

        let capabilities = self.host_services.capabilities().clone();
        if !capabilities.contains(&Capability::Ffi) {
            return Err(self.error(RuntimeErrorKind::PermissionError("ffi"), span));
        }
        let error = |this: &Self, error: ffi::FfiError, span| match error {
            ffi::FfiError::PermissionDenied => {
                this.error(RuntimeErrorKind::PermissionError("ffi"), span)
            }
            ffi::FfiError::Load(message) => this.error(RuntimeErrorKind::IOError(message), span),
            ffi::FfiError::Symbol(_) => this.error(
                RuntimeErrorKind::ReferenceError("native symbol not found"),
                span,
            ),
            ffi::FfiError::LibraryClosed => this.error(
                RuntimeErrorKind::ReferenceError("native library is closed"),
                span,
            ),
            _ => this.error(
                RuntimeErrorKind::TypeError {
                    expected: "valid native handle, signature, and arguments",
                    found: "invalid native call",
                },
                span,
            ),
        };

        match opcode {
            0xB0 => {
                let path = self.pop(span)?;
                let Value::String(path) = path else {
                    return Err(self.type_error("String native library path", &path, span));
                };
                let library = NativeLibraryHandle::load(path, &capabilities)
                    .map_err(|error_value| error(self, error_value, span))?;
                self.push_value(Value::Handle(NativeHandle::Library(library)), span)
            }
            0xB1 => {
                let signature = self.pop(span)?;
                let name = self.pop(span)?;
                let library = self.pop(span)?;
                let Value::String(signature) = signature else {
                    return Err(self.type_error(
                        "String native function signature",
                        &signature,
                        span,
                    ));
                };
                let Value::String(name) = name else {
                    return Err(self.type_error("String native symbol name", &name, span));
                };
                let Value::Handle(NativeHandle::Library(library)) = library else {
                    return Err(self.type_error("native library Handle", &library, span));
                };
                let signature = NativeSignature::parse(&signature)
                    .map_err(|error_value| error(self, error_value, span))?;
                let function = library
                    .symbol(&name, signature, &capabilities)
                    .map_err(|error_value| error(self, error_value, span))?;
                self.push_value(Value::Handle(NativeHandle::Function(function)), span)
            }
            0xB2 => {
                let mut plan = self.prepare_native_call(span)?;
                let stack_base = self.push_native_callback_roots(&mut plan, span)?;
                let mut source_context;
                let context = if let Some(context) = callback_context {
                    context
                } else {
                    source_context = ffi::NativeCallbackContext::new(
                        self as *mut Self as *mut std::ffi::c_void,
                        invoke_interpreter_callback::<R, W>,
                        preflight_interpreter_callback::<R, W>,
                        span,
                    );
                    &mut source_context
                };
                let call_result = plan.call(context);
                self.stack.truncate(stack_base);
                self.finish_native_call(call_result, span)
            }
            0xB3 => {
                let library = self.pop(span)?;
                let Value::Handle(NativeHandle::Library(library)) = library else {
                    return Err(self.type_error("native library Handle", &library, span));
                };
                library
                    .close(&capabilities)
                    .map_err(|error_value| error(self, error_value, span))
            }
            0xB4 => {
                let signature = self.pop(span)?;
                let callable = self.pop(span)?;
                let Value::String(signature) = signature else {
                    return Err(self.type_error(
                        "String native callback signature",
                        &signature,
                        span,
                    ));
                };
                let signature = NativeSignature::parse(&signature)
                    .map_err(|error_value| error(self, error_value, span))?;
                let callback = ffi::NativeCallbackHandle::new(callable, signature, &capabilities)
                    .map_err(|error_value| error(self, error_value, span))?;
                self.ensure_memory_budget(callback.estimated_bytes(), span)?;
                self.push_value(Value::Handle(NativeHandle::Callback(callback)), span)
            }
            0xB5 => {
                let length = self.pop(span)?;
                let Value::Int(length) = length else {
                    return Err(self.type_error("nonnegative Int buffer length", &length, span));
                };
                let length = length.to_string().parse::<usize>().map_err(|_| {
                    self.error(
                        RuntimeErrorKind::TypeError {
                            expected: "nonnegative Int buffer length",
                            found: "Int",
                        },
                        span,
                    )
                })?;
                self.ensure_memory_budget(length, span)?;
                let buffer = NativeBuffer::new(length, self.limits.max_memory_bytes)
                    .map_err(|error_value| error(self, error_value, span))?;
                self.push_value(Value::Handle(NativeHandle::Buffer(buffer)), span)
            }
            _ => Err(self.error(
                RuntimeErrorKind::UnsupportedInstruction(InstructionOpcode::Extended(opcode)),
                span,
            )),
        }
    }

    pub(crate) fn prepare_native_call(
        &mut self,
        span: Span,
    ) -> Result<NativeCallPlan, RuntimeError> {
        use ffi::{NativeHandle, NativeValue};

        let function = self.pop(span)?;
        let count = self.pop(span)?;
        let Value::Handle(NativeHandle::Function(function)) = function else {
            return Err(self.type_error("native function Handle", &function, span));
        };
        let Value::Int(count) = count else {
            return Err(self.type_error("Int native argument count", &count, span));
        };
        let count = count.to_string().parse::<usize>().map_err(|_| {
            self.error(
                RuntimeErrorKind::TypeError {
                    expected: "nonnegative native argument count",
                    found: "Int",
                },
                span,
            )
        })?;
        if count != function.signature().arguments().len() {
            return Err(self.error(
                RuntimeErrorKind::TypeError {
                    expected: "argument count matching native signature",
                    found: "Int",
                },
                span,
            ));
        }
        self.ensure_memory_budget(
            count
                .saturating_mul(size_of::<NativeValue>())
                .saturating_add(count.saturating_mul(size_of::<Value>())),
            span,
        )?;
        let mut arguments = Vec::new();
        arguments
            .try_reserve_exact(count)
            .map_err(|_| self.error(RuntimeErrorKind::ResourceLimit("native arguments"), span))?;
        let mut callback_roots = Vec::new();
        callback_roots.try_reserve(count).map_err(|_| {
            self.error(
                RuntimeErrorKind::ResourceLimit("native callback roots"),
                span,
            )
        })?;
        for _ in 0..count {
            let value = self.pop(span)?;
            arguments.push(match value {
                Value::Int(value) => NativeValue::Int(value),
                Value::Float(value) => NativeValue::Float(value),
                Value::Bool(value) => NativeValue::Bool(value),
                Value::String(value) => NativeValue::String(value),
                Value::Handle(NativeHandle::Buffer(buffer)) => NativeValue::Buffer(buffer),
                Value::Handle(NativeHandle::Callback(callback)) => {
                    let callable_ref = callback.callable();
                    let callable_bytes = match callable_ref {
                        Value::Function(function) => size_of::<FunctionRef>()
                            .saturating_add(estimated_bigint_bytes(&function.id)),
                        Value::Closure(closure) => size_of::<FunctionRef>()
                            .saturating_add(estimated_bigint_bytes(&closure.function.id))
                            .saturating_add(
                                closure
                                    .captures
                                    .try_borrow()
                                    .map(|captures| estimated_capture_clone_bytes(&captures))
                                    .unwrap_or(0),
                            ),
                        _ => 0,
                    };
                    self.ensure_memory_budget(callable_bytes, span)?;
                    let callable = callable_ref.clone();
                    callback_roots.push(callable);
                    NativeValue::Callback(callback)
                }
                value => {
                    return Err(self.type_error("C-compatible native argument", &value, span));
                }
            });
        }
        arguments.reverse();
        callback_roots.reverse();
        Ok(NativeCallPlan {
            function,
            arguments,
            callback_roots,
            capabilities: self.host_services.capabilities().clone(),
        })
    }

    pub(crate) fn push_native_callback_roots(
        &mut self,
        plan: &mut NativeCallPlan,
        span: Span,
    ) -> Result<usize, RuntimeError> {
        let stack_base = self.stack.len();
        for root in plan.take_callback_roots() {
            if let Err(error) = self.push_value(root, span) {
                self.stack.truncate(stack_base);
                return Err(error);
            }
        }
        Ok(stack_base)
    }

    pub(crate) fn finish_native_call(
        &mut self,
        call_result: Result<ffi::NativeValue, ffi::NativeCallError>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        use ffi::{NativeHandle, NativeValue};

        let result = match call_result {
            Ok(value) => value,
            Err(ffi::NativeCallError::Runtime(error)) => return Err(error),
            Err(ffi::NativeCallError::Ffi(error)) => match error {
                ffi::FfiError::PermissionDenied => {
                    return Err(self.error(RuntimeErrorKind::PermissionError("ffi"), span));
                }
                ffi::FfiError::Load(message) => {
                    return Err(self.error(RuntimeErrorKind::IOError(message), span));
                }
                ffi::FfiError::Symbol(_) => {
                    return Err(self.error(
                        RuntimeErrorKind::ReferenceError("native symbol not found"),
                        span,
                    ));
                }
                ffi::FfiError::LibraryClosed => {
                    return Err(self.error(
                        RuntimeErrorKind::ReferenceError("native library is closed"),
                        span,
                    ));
                }
                _ => {
                    return Err(self.error(
                        RuntimeErrorKind::TypeError {
                            expected: "valid native handle, signature, and arguments",
                            found: "invalid native call",
                        },
                        span,
                    ));
                }
            },
        };
        let value = match result {
            NativeValue::Void => Value::Null,
            NativeValue::Int(value) => Value::Int(value),
            NativeValue::Float(value) => Value::Float(value),
            NativeValue::Bool(value) => Value::Bool(value),
            NativeValue::String(value) => Value::String(value),
            NativeValue::Buffer(value) => Value::Handle(NativeHandle::Buffer(value)),
            NativeValue::Callback(value) => Value::Handle(NativeHandle::Callback(value)),
        };
        self.push_value(value, span)
    }

    fn invoke_native_callback(
        &mut self,
        callable: &Value,
        arguments: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let (function_ref_ref, capture_bytes) = match callable {
            Value::Function(function) => (function, 0),
            Value::Closure(closure) => {
                let captures = closure.captures.try_borrow().map_err(|_| {
                    self.error(
                        RuntimeErrorKind::TypeError {
                            expected: "available callback captures",
                            found: "borrowed Closure",
                        },
                        span,
                    )
                })?;
                (&closure.function, estimated_capture_clone_bytes(&captures))
            }
            _ => {
                return Err(self.type_error("Function or Closure callback", callable, span));
            }
        };
        if function_ref_ref.module_index != 0 {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("callback function does not exist"),
                span,
            ));
        }
        let Some((function_bytes, asynchronous)) =
            self.functions.get(&function_ref_ref.id).map(|function| {
                (
                    estimated_function_clone_bytes(function),
                    function.kind == FunctionKind::Asynchronous,
                )
            })
        else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("callback function does not exist"),
                span,
            ));
        };
        if asynchronous {
            return Err(self.error(
                RuntimeErrorKind::TypeError {
                    expected: "synchronous native callback",
                    found: "async Function",
                },
                span,
            ));
        }
        let argument_bytes = arguments
            .len()
            .saturating_mul(size_of::<Value>())
            .saturating_add(estimate_cloned_value_payloads(arguments));
        self.ensure_memory_budget(
            size_of::<FunctionRef>()
                .saturating_add(estimated_bigint_bytes(&function_ref_ref.id))
                .saturating_add(function_bytes)
                .saturating_add(capture_bytes)
                .saturating_add(argument_bytes),
            span,
        )?;
        let function_ref = function_ref_ref.clone();
        let function = self
            .functions
            .get(&function_ref.id)
            .cloned()
            .ok_or_else(|| {
                self.error(
                    RuntimeErrorKind::ReferenceError("callback function does not exist"),
                    span,
                )
            })?;
        let captures = match callable {
            Value::Closure(closure) => closure
                .captures
                .try_borrow()
                .map_err(|_| {
                    self.error(
                        RuntimeErrorKind::TypeError {
                            expected: "available callback captures",
                            found: "borrowed Closure",
                        },
                        span,
                    )
                })?
                .clone(),
            _ => Vec::new(),
        };
        let mut owned_arguments = Vec::new();
        owned_arguments
            .try_reserve_exact(arguments.len())
            .map_err(|_| self.error(RuntimeErrorKind::ResourceLimit("callback arguments"), span))?;
        owned_arguments.extend_from_slice(arguments);
        let stack_base = self.stack.len();
        let scope_count = self.scopes.len();
        let scope_floor_count = self.scope_floors.len();
        let frame_stack_base_count = self.frame_stack_bases.len();
        let capture_vector_count = self.capture_vectors.len();
        let call_depth = self.call_depth;
        let trace_count = self.call_trace.len();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.invoke_function(function, owned_arguments, captures, span)
        })) {
            Ok(result) => result,
            Err(_) => {
                self.stack.truncate(stack_base);
                self.scopes.truncate(scope_count);
                self.scope_floors.truncate(scope_floor_count);
                self.frame_stack_bases.truncate(frame_stack_base_count);
                self.capture_vectors.truncate(capture_vector_count);
                self.call_depth = call_depth;
                self.call_trace.truncate(trace_count);
                Err(self.error(
                    RuntimeErrorKind::Unsupported("native callback panicked"),
                    span,
                ))
            }
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
        self.push_value(Value::Int(integer), span)?;
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
            | Value::Object(_)
            | Value::Handle(_)
            | Value::Task(_)
            | Value::Channel(_)
            | Value::Mutex(_) => true,
        };
        self.push_value(Value::Bool(truthy), span)?;
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
        self.push_value(Value::Char(character), span)?;
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
        self.push_value(Value::Byte(byte), span)?;
        Ok(())
    }

    fn utf8_encode(&mut self, span: Span) -> Result<(), RuntimeError> {
        match self.pop(span)? {
            Value::String(value) => {
                self.push_value(Value::Bytes(value.into_bytes()), span)?;
                Ok(())
            }
            other => Err(self.type_error("String", &other, span)),
        }
    }

    fn utf8_decode(&mut self, span: Span) -> Result<(), RuntimeError> {
        match self.pop(span)? {
            Value::Bytes(value) => match String::from_utf8(value) {
                Ok(value) => {
                    self.push_value(Value::String(value), span)?;
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
        self.push_value(Value::Bool(result), span)?;
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
        self.push_value(array, span)?;
        Ok(())
    }

    fn tuple_new(&mut self, span: Span) -> Result<(), RuntimeError> {
        let values = self.pop_collection_items(span)?;
        let tuple = self.allocate_tuple(values);
        self.push_value(tuple, span)?;
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
        self.push_value(map, span)?;
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
        self.push_value(set, span)?;
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
            Value::Array(values) => {
                let index = self.index_from_value(key, span)?;
                let payload_bytes = values
                    .borrow()
                    .get(index)
                    .map(|value| estimated_unmanaged_payload_bytes(value, 0))
                    .ok_or_else(|| self.index_error(span))?;
                self.ensure_memory_budget(payload_bytes, span)?;
                values
                    .borrow()
                    .get(index)
                    .cloned()
                    .ok_or_else(|| self.index_error(span))?
            }
            Value::Tuple(values) => {
                let index = self.index_from_value(key, span)?;
                let payload_bytes = values
                    .values
                    .borrow()
                    .get(index)
                    .map(|value| estimated_unmanaged_payload_bytes(value, 0))
                    .ok_or_else(|| self.index_error(span))?;
                self.ensure_memory_budget(payload_bytes, span)?;
                values
                    .values
                    .borrow()
                    .get(index)
                    .cloned()
                    .ok_or_else(|| self.index_error(span))?
            }
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
                let key_payload_bytes = estimated_key_clone_bytes(&key, 0);
                self.ensure_memory_budget(key_payload_bytes, span)?;
                let key_value = RuntimeKey::from_value(&key)
                    .ok_or_else(|| self.type_error("hashable Map key", &key, span))?;
                let value_payload_bytes = map
                    .borrow()
                    .entries
                    .get(&key_value)
                    .map(|value| estimated_unmanaged_payload_bytes(value, 0))
                    .ok_or_else(|| {
                        self.error(RuntimeErrorKind::ReferenceError("Map key not found"), span)
                    })?;
                self.ensure_memory_budget(
                    key_payload_bytes.saturating_add(value_payload_bytes),
                    span,
                )?;
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
        self.push_value(item, span)?;
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
                let key_clone_bytes = estimated_key_clone_bytes(&key_value, 0);
                let key_value_bytes = estimated_unmanaged_payload_bytes(&key_value, 0);
                let value_bytes = estimated_unmanaged_payload_bytes(&value, 0);
                self.ensure_memory_budget(
                    key_clone_bytes
                        .saturating_add(key_value_bytes)
                        .saturating_add(value_bytes),
                    span,
                )?;
                let key = RuntimeKey::from_value(&key_value)
                    .ok_or_else(|| self.type_error("hashable Map key", &key_value, span))?;
                let (is_new_key, growth_bytes) = {
                    let map = map.borrow();
                    let is_new_key = !map.entries.contains_key(&key);
                    let order_growth = if is_new_key && map.order.len() == map.order.capacity() {
                        map.order
                            .capacity()
                            .max(1)
                            .saturating_mul(2)
                            .max(4)
                            .saturating_sub(map.order.capacity())
                            .saturating_mul(size_of::<(RuntimeKey, Value)>())
                    } else {
                        0
                    };
                    let entries_growth =
                        if is_new_key && map.entries.len() == map.entries.capacity() {
                            map.entries
                                .capacity()
                                .max(1)
                                .saturating_mul(2)
                                .max(4)
                                .saturating_sub(map.entries.capacity())
                                .saturating_mul(size_of::<(RuntimeKey, Value)>())
                        } else {
                            0
                        };
                    (is_new_key, order_growth.saturating_add(entries_growth))
                };
                if is_new_key {
                    self.ensure_memory_budget(
                        key_clone_bytes
                            .saturating_mul(2)
                            .saturating_add(key_value_bytes)
                            .saturating_add(value_bytes)
                            .saturating_add(growth_bytes),
                        span,
                    )?;
                }
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
        self.push_value(result, span)?;
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
        self.push_value(Value::Bool(found), span)?;
        Ok(())
    }

    fn append(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        match self.pop(span)? {
            Value::Array(values) => {
                let growth_bytes = {
                    let values = values.borrow();
                    if values.len() == values.capacity() {
                        values
                            .capacity()
                            .max(1)
                            .saturating_mul(2)
                            .max(4)
                            .saturating_sub(values.capacity())
                            .saturating_mul(size_of::<Value>())
                    } else {
                        0
                    }
                };
                self.ensure_memory_budget(
                    growth_bytes.saturating_add(estimated_unmanaged_payload_bytes(&value, 0)),
                    span,
                )?;
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
        self.push_value(Value::Int(BigInt::from(count)), span)?;
        Ok(())
    }

    fn iter(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        self.ensure_memory_budget(estimated_iterator_storage(&value), span)?;
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
        self.push_value(iterator, span)?;
        Ok(())
    }

    fn slice(&mut self, span: Span) -> Result<(), RuntimeError> {
        let end_value = self.pop(span)?;
        let start_value = self.pop(span)?;
        let collection = self.pop(span)?;
        let (start, end) = self.slice_bounds(start_value, end_value, span)?;
        let sliced = match collection {
            Value::Array(values) => {
                let allocation_bytes = {
                    let values = values.borrow();
                    if start > end || end > values.len() {
                        return Err(self.index_error(span));
                    }
                    end.saturating_sub(start)
                        .saturating_mul(size_of::<Value>())
                        .saturating_add(estimate_cloned_value_payloads(&values[start..end]))
                };
                self.ensure_memory_budget(allocation_bytes, span)?;
                self.allocate_array(values.borrow()[start..end].to_vec())
            }
            Value::Tuple(values) => {
                let allocation_bytes = {
                    let values = values.values.borrow();
                    if start > end || end > values.len() {
                        return Err(self.index_error(span));
                    }
                    end.saturating_sub(start)
                        .saturating_mul(size_of::<Value>())
                        .saturating_add(estimate_cloned_value_payloads(&values[start..end]))
                };
                self.ensure_memory_budget(allocation_bytes, span)?;
                self.allocate_tuple(values.values.borrow()[start..end].to_vec())
            }
            Value::String(value) => {
                let length = value.chars().count();
                if start > end || end > length {
                    return Err(self.index_error(span));
                }
                let result_bytes = value
                    .chars()
                    .skip(start)
                    .take(end - start)
                    .map(char::len_utf8)
                    .sum::<usize>();
                self.ensure_memory_budget(value.capacity().saturating_add(result_bytes), span)?;
                Value::String(value.chars().skip(start).take(end - start).collect())
            }
            Value::Bytes(value) => {
                if start > end || end > value.len() {
                    return Err(self.index_error(span));
                }
                self.ensure_memory_budget(
                    value.capacity().saturating_add(end.saturating_sub(start)),
                    span,
                )?;
                Value::Bytes(value[start..end].to_vec())
            }
            other => return Err(self.type_error("Array, Tuple, String, or Bytes", &other, span)),
        };
        self.push_value(sliced, span)?;
        Ok(())
    }

    fn next(&mut self, span: Span) -> Result<(), RuntimeError> {
        let iterator = match self.pop(span)? {
            Value::Iterator(iterator) => iterator,
            other => return Err(self.type_error("Iterator", &other, span)),
        };
        let next_payload_bytes = {
            let iterator = iterator.borrow();
            iterator
                .values
                .get(iterator.next_index)
                .map(|value| estimated_unmanaged_payload_bytes(value, 0))
        };
        if let Some(payload_bytes) = next_payload_bytes {
            self.ensure_memory_budget(payload_bytes, span)?;
        }
        let value = {
            let mut iterator = iterator.borrow_mut();
            let value = iterator.values.get(iterator.next_index).cloned();
            if value.is_some() {
                iterator.next_index += 1;
            }
            value
        };
        let found = value.is_some();
        self.push_value(value.unwrap_or(Value::Null), span)?;
        self.push_value(Value::Bool(found), span)?;
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
        self.push_value(result, span)?;
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
            self.push_value(Value::Int(left - quotient * right), span)?;
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
                self.push_value(Value::Int(result), span)?;
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
                self.push_value(Value::Float(result), span)?;
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
        self.push_value(Value::Bool(left == right), span)?;
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
        self.push_value(Value::Bool(result), span)?;
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
        let binding = binding.clone();
        let payload_bytes = {
            let binding = binding.borrow();
            estimated_unmanaged_payload_bytes(&binding.value, 0)
        };
        self.ensure_memory_budget(payload_bytes, span)?;
        self.push_value(binding.borrow().value.clone(), span)?;
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

    fn push_literal(&mut self, literal: &DataLiteral) -> Result<(), RuntimeError> {
        let significant_bits = literal
            .bits
            .iter()
            .position(|bit| *bit == 1)
            .map_or(0, |first_one| literal.bits.len() - first_one);
        let payload_bytes = significant_bits
            .saturating_add(63)
            .checked_div(64)
            .unwrap_or(usize::MAX)
            .saturating_mul(size_of::<u64>());
        self.ensure_memory_budget(payload_bytes, literal.span)?;
        let mut value = BigInt::from(0);
        for bit in &literal.bits {
            value <<= 1usize;
            if *bit == 1 {
                value += 1;
            }
        }
        self.push_value(Value::Int(value), literal.span)?;
        Ok(())
    }

    fn read_input_line(&mut self, span: Span) -> Result<Option<Vec<u8>>, RuntimeError> {
        let max_line_bytes = self.available_memory_bytes(span)?;
        match self.input.read_line_bytes(max_line_bytes) {
            Ok(line) => Ok(line),
            Err(InputError::LimitExceeded) => {
                Err(self.error(RuntimeErrorKind::ResourceLimit("memory"), span))
            }
            Err(error) => Err(self.error(RuntimeErrorKind::IOError(error.to_string()), span)),
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

    fn bounded_error_stack_trace(&self) -> (Vec<StackTraceFrame>, bool) {
        let current_memory = self
            .gc
            .estimated_memory_bytes(
                &self.stack,
                &self.scopes,
                &self.capture_vectors,
                self.limits.max_memory_bytes,
            )
            .map(|bytes| {
                bytes
                    .saturating_add(self.external_memory_bytes)
                    .saturating_add(self.call_trace_memory_bytes())
                    .saturating_add(self.call_frame_bookkeeping_memory_bytes())
            })
            .unwrap_or(usize::MAX);
        let mut available = self.limits.max_memory_bytes.saturating_sub(current_memory);
        let mut first = self.call_trace.len();
        for (index, frame) in self.call_trace.iter().enumerate().rev() {
            let frame_bytes = size_of::<StackTraceFrame>()
                .saturating_add(estimated_bigint_bytes(&frame.function_id));
            if frame_bytes > available {
                break;
            }
            available = available.saturating_sub(frame_bytes);
            first = index;
        }
        let mut stack_trace = Vec::new();
        if stack_trace
            .try_reserve_exact(self.call_trace.len().saturating_sub(first))
            .is_err()
        {
            return (stack_trace, !self.call_trace.is_empty());
        }
        stack_trace.extend(self.call_trace[first..].iter().cloned());
        (stack_trace, first > 0)
    }

    fn error(&self, kind: RuntimeErrorKind, span: Span) -> RuntimeError {
        let (stack_trace, stack_trace_truncated) = self.bounded_error_stack_trace();
        RuntimeError {
            kind,
            span,
            stack_trace,
            stack_trace_truncated,
        }
    }
}

fn projected_capacity_after_push(length: usize, capacity: usize) -> usize {
    let required = length.saturating_add(1);
    if required <= capacity {
        capacity
    } else {
        capacity.saturating_mul(2).max(required)
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
        Value::Handle(_) | Value::Task(_) | Value::Channel(_) | Value::Mutex(_) => 0,
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
        | Value::Object(_)
        | Value::Task(_)
        | Value::Channel(_)
        | Value::Mutex(_) => 0,
        Value::Handle(handle) => handle.estimated_bytes(),
    }
}

fn estimated_bigint_bytes(value: &BigInt) -> usize {
    usize::try_from(value.bits().saturating_add(7) / 8).unwrap_or(usize::MAX)
}

fn estimated_runtime_error_bytes(error: &RuntimeError) -> usize {
    let message_bytes = match &error.kind {
        RuntimeErrorKind::IOError(message) | RuntimeErrorKind::CompileError(message) => {
            message.capacity()
        }
        RuntimeErrorKind::StackUnderflow
        | RuntimeErrorKind::TypeError { .. }
        | RuntimeErrorKind::DivisionByZero
        | RuntimeErrorKind::ReferenceError(_)
        | RuntimeErrorKind::IndexError(_)
        | RuntimeErrorKind::PermissionError(_)
        | RuntimeErrorKind::StackOverflow
        | RuntimeErrorKind::TaskCancelled
        | RuntimeErrorKind::Deadlock
        | RuntimeErrorKind::ExecutionLimit
        | RuntimeErrorKind::ResourceLimit(_)
        | RuntimeErrorKind::UnsupportedInstruction(_)
        | RuntimeErrorKind::Unsupported(_) => 0,
    };
    size_of::<RuntimeError>()
        .saturating_add(message_bytes)
        .saturating_add(
            error
                .stack_trace
                .capacity()
                .saturating_mul(size_of::<StackTraceFrame>()),
        )
        .saturating_add(
            error
                .stack_trace
                .iter()
                .map(|frame| estimated_bigint_bytes(&frame.function_id))
                .fold(0usize, usize::saturating_add),
        )
}

fn estimated_capture_clone_bytes(captures: &[(BigInt, BindingCell)]) -> usize {
    captures
        .len()
        .saturating_mul(size_of::<(BigInt, BindingCell)>())
        .saturating_add(
            captures
                .iter()
                .map(|(id, _)| estimated_bigint_bytes(id))
                .fold(0usize, usize::saturating_add),
        )
}

fn estimated_function_clone_bytes(function: &ResolvedFunction) -> usize {
    size_of::<ResolvedFunction>()
        .saturating_add(
            function
                .parameter_ids
                .capacity()
                .saturating_mul(size_of::<BigInt>()),
        )
        .saturating_add(estimated_bigint_bytes(&function.id))
        .saturating_add(
            function
                .parameter_ids
                .iter()
                .map(estimated_bigint_bytes)
                .fold(0usize, usize::saturating_add),
        )
}

fn estimate_cloned_value_payloads(values: &[Value]) -> usize {
    values.iter().fold(0usize, |total, value| {
        total.saturating_add(estimated_unmanaged_payload_bytes(value, 0))
    })
}

fn estimated_iterator_storage(value: &Value) -> usize {
    let (values_bytes, payload_bytes) = match value {
        Value::Array(values) => {
            let values = values.borrow();
            (
                values.capacity().saturating_mul(size_of::<Value>()),
                estimate_cloned_value_payloads(&values),
            )
        }
        Value::Tuple(values) => {
            let values = values.values.borrow();
            (
                values.capacity().saturating_mul(size_of::<Value>()),
                estimate_cloned_value_payloads(&values),
            )
        }
        Value::Map(map) => {
            let map = map.borrow();
            let payload_bytes = map.order.iter().fold(0usize, |total, (_, value)| {
                total.saturating_add(estimated_unmanaged_payload_bytes(value, 0))
            });
            (
                map.order.len().saturating_mul(size_of::<Value>()),
                payload_bytes,
            )
        }
        Value::Set(set) => {
            let set = set.borrow();
            let payload_bytes = set.order.iter().fold(0usize, |total, (_, value)| {
                total.saturating_add(estimated_unmanaged_payload_bytes(value, 0))
            });
            (
                set.order.len().saturating_mul(size_of::<Value>()),
                payload_bytes,
            )
        }
        Value::String(value) => (
            value.len().saturating_mul(size_of::<Value>()),
            value.capacity(),
        ),
        Value::Bytes(value) => (
            value.len().saturating_mul(size_of::<Value>()),
            value.capacity(),
        ),
        _ => return 0,
    };
    values_bytes
        .saturating_add(payload_bytes)
        .saturating_add(size_of::<IteratorState>())
        .saturating_add(2 * size_of::<usize>())
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

unsafe fn invoke_interpreter_callback<R: BufRead, W: Write>(
    data: *mut std::ffi::c_void,
    callable: &Value,
    arguments: &[Value],
    span: Span,
) -> Result<Value, RuntimeError> {
    let interpreter = unsafe { &mut *data.cast::<Interpreter<R, W>>() };
    interpreter.invoke_native_callback(callable, arguments, span)
}

unsafe fn preflight_interpreter_callback<R: BufRead, W: Write>(
    data: *mut std::ffi::c_void,
    bytes: usize,
    span: Span,
) -> Result<(), RuntimeError> {
    let interpreter = unsafe { &mut *data.cast::<Interpreter<R, W>>() };
    interpreter.ensure_memory_budget(bytes, span)
}
