use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, Write};
use std::rc::Rc;

use num_bigint::BigInt;
use num_traits::{One, Signed, ToPrimitive, Zero};
use star_common::{SourcePosition, Span};
use star_compiler::{verify_bytecode, Capability, IrOp, VerificationPolicy, VerifiedProgram};

use crate::gc::{GcError, GcStats};
use crate::value::{
    binding_cell, BindingCell, ClosureState, FunctionRef, IteratorState, ObjectState, Value,
};

use super::{empty_span, Interpreter, RuntimeError, RuntimeErrorKind};

/// Executes verified version 1.0 `.starc` bytecode using the reference runtime.
///
/// This VM implements the emitted control-flow instructions and the current
/// primitive, variable, I/O, collection, and direct-call runtime slice. Features
/// assigned to later runtime roadmap items return an explicit unsupported error.
pub struct BytecodeVm<R: BufRead, W: Write> {
    runtime: Interpreter<R, W>,
    policy: VerificationPolicy,
    frame_storage_estimates: Vec<usize>,
    tasks: HashMap<u64, BytecodeTask>,
    ready_tasks: VecDeque<u64>,
    next_task_id: u64,
    current_task_id: Option<u64>,
    task_memory_bytes: usize,
}

impl<R: BufRead, W: Write> BytecodeVm<R, W> {
    /// Creates a VM with injected standard input/output and verifier policy.
    pub fn new(input: R, output: W, policy: VerificationPolicy) -> Self {
        let capabilities = policy.granted_capabilities.clone();
        Self {
            runtime: Interpreter::new(input, output).with_capabilities(capabilities),
            policy,
            frame_storage_estimates: Vec::new(),
            tasks: HashMap::new(),
            ready_tasks: VecDeque::new(),
            next_task_id: 1,
            current_task_id: None,
            task_memory_bytes: 0,
        }
    }

    /// Returns the host services configured from this VM's verifier policy.
    pub fn host_services(&self) -> &crate::runtime::host::HostServices {
        self.runtime.host_services()
    }

    /// Returns mutable host services for trusted host setup or use.
    pub fn host_services_mut(&mut self) -> &mut crate::runtime::host::HostServices {
        self.runtime.host_services_mut()
    }

    /// Sets the maximum number of executed bytecode instructions.
    pub fn with_execution_limit(mut self, limit: u64) -> Self {
        self.runtime.limits.max_instructions = limit.max(1);
        self
    }

    /// Sets the maximum number of nested bytecode function calls.
    pub fn with_call_depth_limit(mut self, limit: usize) -> Self {
        self.runtime.limits.max_call_depth = limit.max(1);
        self
    }

    /// Sets all runtime resource ceilings and narrows verifier stack/instruction limits.
    pub fn with_execution_limits(mut self, limits: super::ExecutionLimits) -> Self {
        self.policy.limits.max_instructions = self
            .policy
            .limits
            .max_instructions
            .min(limits.max_instructions);
        self.policy.limits.max_stack_depth = self
            .policy
            .limits
            .max_stack_depth
            .min(u32::try_from(limits.max_stack_values).unwrap_or(u32::MAX));
        self.runtime.limits = limits;
        self
    }

    /// Sets the number of newly allocated managed objects that triggers collection.
    pub fn with_gc_threshold(mut self, threshold: usize) -> Self {
        self.runtime.gc.set_threshold(threshold);
        self
    }

    /// Performs a tracing collection of values owned or externally held during execution.
    /// Returns `BorrowConflict` without modifying the heap if the host has an
    /// active borrow; automatic collection is retried at a later instruction.
    pub fn collect_garbage(&mut self) -> Result<GcStats, GcError> {
        self.runtime.collect_garbage()
    }

    /// Verifies and executes a `.starc` program, returning the module result.
    pub fn execute(&mut self, bytes: &[u8]) -> Result<Value, RuntimeError> {
        self.reset_state();
        self.runtime.begin_execution();
        let result = (|| {
            let verified = verify_bytecode(bytes, &self.policy).map_err(|error| {
                self.runtime.error(
                    RuntimeErrorKind::CompileError(error.to_string()),
                    empty_span(),
                )
            })?;
            let image = Image::parse(&verified).map_err(|message| {
                self.runtime
                    .error(RuntimeErrorKind::CompileError(message), empty_span())
            })?;
            self.execute_image(&image)
        })();
        let result = match (result, self.runtime.output.flush()) {
            (Err(error), _) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(error)) => Err(self
                .runtime
                .error(RuntimeErrorKind::IOError(error.to_string()), empty_span())),
        };
        self.runtime.set_external_memory_bytes(0);
        if result.is_err() {
            self.cleanup_after_error();
        }
        result
    }

    /// Returns the current operand stack, from bottom to top.
    pub fn stack(&self) -> &[Value] {
        self.runtime.stack()
    }

    /// Returns a module-global binding, if one exists.
    pub fn global(&self, id: &BigInt) -> Option<Value> {
        self.runtime.global(id)
    }

    /// Returns the injected I/O objects after successfully flushing output.
    /// On failure, returns the input, buffered output, and error so callers can retry.
    pub fn try_into_io(
        self,
    ) -> Result<(R, W), (R, super::stdio::BufferedOutput<W>, std::io::Error)> {
        self.runtime.try_into_io()
    }

    /// Returns the injected I/O objects after execution.
    ///
    /// This compatibility method discards any bytes still buffered if flushing
    /// fails. Use [`Self::try_into_io`] when output delivery must be checked.
    pub fn into_io(self) -> (R, W) {
        self.runtime.into_io()
    }

    fn reset_state(&mut self) {
        self.runtime.stack.clear();
        self.runtime.scopes.truncate(1);
        self.runtime.scopes[0].clear();
        self.runtime.scope_floors.truncate(1);
        if self.runtime.scope_floors.is_empty() {
            self.runtime.scope_floors.push(0);
        }
        self.runtime.frame_stack_bases.clear();
        self.runtime.functions.clear();
        self.runtime.structs.clear();
        self.runtime.interfaces.clear();
        self.runtime.capture_vectors.clear();
        self.runtime.capture_vectors.push(Vec::new());
        self.runtime.call_trace.clear();
        self.runtime.call_depth = 0;
        self.runtime.executed_steps = 0;
        self.runtime.set_external_memory_bytes(0);
        self.frame_storage_estimates.clear();
        self.tasks.clear();
        self.ready_tasks.clear();
        self.next_task_id = 1;
        self.current_task_id = None;
        self.task_memory_bytes = 0;
    }

    fn cleanup_after_error(&mut self) {
        self.runtime.stack.clear();
        self.runtime.scopes.truncate(1);
        self.runtime.scope_floors.truncate(1);
        if self.runtime.scope_floors.is_empty() {
            self.runtime.scope_floors.push(0);
        }
        self.runtime.frame_stack_bases.clear();
        self.runtime.capture_vectors.truncate(1);
        self.runtime.capture_vectors[0].clear();
        self.runtime.call_trace.clear();
        self.runtime.call_depth = 0;
    }

    fn execute_image(&mut self, image: &Image<'_>) -> Result<Value, RuntimeError> {
        let entry = image.modules[image.entry_module as usize].initializer_index as usize;
        let entry_function = &image.functions[entry];
        let entry_span = image.span(entry, entry_function.code_offset);
        let frame_capacity = frame_capacity_after_push(
            0,
            0,
            self.runtime.limits.max_call_depth.saturating_add(1).max(1),
        )
        .ok_or_else(|| {
            self.runtime
                .error(RuntimeErrorKind::StackOverflow, entry_span)
        })?;
        self.runtime.ensure_memory_budget(
            frame_capacity
                .saturating_mul(size_of::<Frame>())
                .saturating_add(function_record_payload_bytes(entry_function)),
            entry_span,
        )?;
        let mut frames = Vec::new();
        frames.try_reserve_exact(frame_capacity).map_err(|_| {
            self.runtime
                .error(RuntimeErrorKind::ResourceLimit("call frames"), entry_span)
        })?;
        frames.push(Frame::initializer(entry, entry_function.clone()));
        self.run_frames(image, &mut frames)
    }

    fn run_frames(
        &mut self,
        image: &Image<'_>,
        frames: &mut Vec<Frame>,
    ) -> Result<Value, RuntimeError> {
        self.frame_storage_estimates.push(0);
        let result = self.run_frames_loop(image, frames);
        self.frame_storage_estimates.pop();
        self.sync_external_memory();
        result
    }

    fn run_frames_loop(
        &mut self,
        image: &Image<'_>,
        frames: &mut Vec<Frame>,
    ) -> Result<Value, RuntimeError> {
        loop {
            self.update_external_frame_memory(&frames);
            let Some(frame) = frames.last() else {
                return Ok(Value::Null);
            };
            let stack_depth = self.runtime.stack.len().saturating_sub(frame.stack_base);
            if self.runtime.stack.len() > self.runtime.limits.max_stack_values {
                return Err(self.runtime.error(
                    RuntimeErrorKind::ResourceLimit("operand stack"),
                    image.span(frame.function_index, frame.function.code_offset + frame.ip),
                ));
            }
            if stack_depth > frame.function.max_stack as usize
                || stack_depth
                    > usize::try_from(self.policy.limits.max_stack_depth).unwrap_or(usize::MAX)
            {
                return Err(self.runtime.error(
                    RuntimeErrorKind::ExecutionLimit,
                    image.span(frame.function_index, frame.function.code_offset + frame.ip),
                ));
            }
            if frame.ip >= frame.function.code_length {
                return Err(self.error_for(
                    image,
                    frame,
                    frame.ip,
                    "instruction pointer escaped function",
                ));
            }
            self.runtime.executed_steps = self.runtime.executed_steps.saturating_add(1);
            if self.runtime.executed_steps > self.runtime.limits.max_instructions {
                return Err(self.runtime.error(
                    RuntimeErrorKind::ExecutionLimit,
                    image.span(frame.function_index, frame.function.code_offset + frame.ip),
                ));
            }
            let instruction_offset = frame.ip;
            let function_index = frame.function_index;
            let absolute_code_offset = frame.function.code_offset + instruction_offset;
            self.expire_pending_exceptions(frames.last_mut().expect("active frame"));
            self.activate_exception_regions(image, frames.last_mut().expect("active frame"));
            let span = image.span(function_index, absolute_code_offset);
            self.runtime.check_resource_limits(span)?;
            let instruction = {
                let frame = frames.last_mut().expect("active frame");
                decode_instruction(image, frame, span, &self.runtime)?
            };
            let instruction_result =
                self.execute_instruction(image, frames, instruction, instruction_offset, span);
            self.update_external_frame_memory(&frames);
            match instruction_result {
                Ok(Some(value)) => {
                    self.runtime.check_resource_limits(span)?;
                    return Ok(value);
                }
                Ok(None) => self.runtime.check_resource_limits(span)?,
                Err(error) => {
                    if self.dispatch_exception(image, frames, instruction_offset, error.clone()) {
                        continue;
                    }
                    return Err(error);
                }
            }
            self.runtime.collect_if_needed();
        }
    }

    fn update_external_frame_memory(&mut self, frames: &Vec<Frame>) {
        if let Some(current) = self.frame_storage_estimates.last_mut() {
            *current = frame_storage_bytes(frames, frames.capacity());
        }
        self.sync_external_memory();
    }

    fn sync_external_memory(&mut self) {
        self.task_memory_bytes = bytecode_task_memory_bytes(
            &self.tasks,
            self.tasks.capacity(),
            self.ready_tasks.capacity(),
        );
        let frame_bytes = self
            .frame_storage_estimates
            .iter()
            .copied()
            .fold(0usize, usize::saturating_add);
        self.runtime
            .set_external_memory_bytes(frame_bytes.saturating_add(self.task_memory_bytes));
    }

    fn enqueue_task(&mut self, request: CallRequest, span: Span) -> Result<u64, RuntimeError> {
        let Some(next_id) = self.next_task_id.checked_add(1) else {
            return Err(self
                .runtime
                .error(RuntimeErrorKind::ResourceLimit("task identifiers"), span));
        };
        let task_capacity =
            super::projected_capacity_after_push(self.tasks.len(), self.tasks.capacity());
        let ready_capacity = super::projected_capacity_after_push(
            self.ready_tasks.len(),
            self.ready_tasks.capacity(),
        );
        let additional = size_of::<BytecodeTask>()
            .saturating_add(call_request_memory_bytes(&request))
            .saturating_add(
                task_capacity
                    .saturating_sub(self.tasks.capacity())
                    .saturating_mul(size_of::<(u64, BytecodeTask)>()),
            )
            .saturating_add(
                ready_capacity
                    .saturating_sub(self.ready_tasks.capacity())
                    .saturating_mul(size_of::<u64>()),
            );
        self.runtime.ensure_memory_budget(additional, span)?;
        self.tasks.try_reserve(1).map_err(|_| {
            self.runtime
                .error(RuntimeErrorKind::ResourceLimit("task registry"), span)
        })?;
        self.ready_tasks.try_reserve(1).map_err(|_| {
            self.runtime
                .error(RuntimeErrorKind::ResourceLimit("task queue"), span)
        })?;
        let id = self.next_task_id;
        self.next_task_id = next_id;
        self.tasks.insert(
            id,
            BytecodeTask {
                request: Some(request),
                status: BytecodeTaskStatus::Pending,
            },
        );
        self.ready_tasks.push_back(id);
        self.sync_external_memory();
        Ok(id)
    }

    fn task_status_kind_and_copy_bytes(&self, id: u64) -> Option<(BytecodeTaskStatusKind, usize)> {
        self.tasks.get(&id).map(|task| match &task.status {
            BytecodeTaskStatus::Pending => (BytecodeTaskStatusKind::Pending, 0),
            BytecodeTaskStatus::Running => (BytecodeTaskStatusKind::Running, 0),
            BytecodeTaskStatus::Completed(value) => (
                BytecodeTaskStatusKind::Completed,
                size_of::<Value>()
                    .saturating_add(super::estimated_unmanaged_payload_bytes(value, 0)),
            ),
            BytecodeTaskStatus::Failed(error) => (
                BytecodeTaskStatusKind::Failed,
                super::estimated_runtime_error_bytes(error),
            ),
            BytecodeTaskStatus::Cancelled => (BytecodeTaskStatusKind::Cancelled, 0),
        })
    }

    fn run_next_task(&mut self, image: &Image<'_>, span: Span) -> Result<bool, RuntimeError> {
        while let Some(id) = self.ready_tasks.pop_front() {
            if matches!(
                self.tasks.get(&id).map(|task| &task.status),
                Some(BytecodeTaskStatus::Pending)
            ) {
                let _ = self.run_task(image, id, span);
                self.sync_external_memory();
                return Ok(true);
            }
        }
        self.sync_external_memory();
        Ok(false)
    }

    fn run_task(&mut self, image: &Image<'_>, id: u64, span: Span) -> Result<Value, RuntimeError> {
        let Some((status, copy_bytes)) = self.task_status_kind_and_copy_bytes(id) else {
            return Err(self.runtime.error(
                RuntimeErrorKind::ReferenceError("task does not exist"),
                span,
            ));
        };
        match status {
            BytecodeTaskStatusKind::Completed => {
                self.runtime.ensure_memory_budget(copy_bytes, span)?;
                let Some(BytecodeTaskStatus::Completed(value)) =
                    self.tasks.get(&id).map(|task| &task.status)
                else {
                    return Err(self
                        .runtime
                        .error(RuntimeErrorKind::ReferenceError("task state changed"), span));
                };
                return Ok(value.clone());
            }
            BytecodeTaskStatusKind::Failed => {
                self.runtime.ensure_memory_budget(copy_bytes, span)?;
                let Some(BytecodeTaskStatus::Failed(error)) =
                    self.tasks.get(&id).map(|task| &task.status)
                else {
                    return Err(self
                        .runtime
                        .error(RuntimeErrorKind::ReferenceError("task state changed"), span));
                };
                return Err(error.clone());
            }
            BytecodeTaskStatusKind::Cancelled => {
                return Err(self.runtime.error(RuntimeErrorKind::TaskCancelled, span))
            }
            BytecodeTaskStatusKind::Running => {
                return Err(self.runtime.error(
                    RuntimeErrorKind::ReferenceError("task cannot await itself while running"),
                    span,
                ))
            }
            BytecodeTaskStatusKind::Pending => {}
        }

        let request = {
            let task = self.tasks.get_mut(&id).expect("task was found above");
            task.status = BytecodeTaskStatus::Running;
            task.request.take().ok_or_else(|| {
                self.runtime.error(
                    RuntimeErrorKind::ReferenceError("task call state is unavailable"),
                    span,
                )
            })?
        };
        let previous_task = self.current_task_id.replace(id);
        let mut frames = Vec::new();
        self.frame_storage_estimates.push(0);
        let execution = self
            .enter_function(request, span, 0, image, &mut frames, true)
            .and_then(|()| self.run_frames_loop(image, &mut frames));
        self.frame_storage_estimates.pop();
        self.current_task_id = previous_task;
        if execution.is_err() {
            for frame in frames.drain(..).rev() {
                self.unwind_exception_frame(&frame);
            }
        }
        self.sync_external_memory();

        let status_copy_bytes = match &execution {
            Ok(value) => size_of::<Value>()
                .saturating_add(super::estimated_unmanaged_payload_bytes(value, 0)),
            Err(error) => super::estimated_runtime_error_bytes(error),
        };
        let execution = if self
            .runtime
            .ensure_memory_budget(status_copy_bytes, span)
            .is_err()
        {
            Err(RuntimeError {
                kind: RuntimeErrorKind::ResourceLimit("memory"),
                span,
                stack_trace: Vec::new(),
                stack_trace_truncated: true,
            })
        } else {
            execution
        };
        let result = match execution {
            Ok(value) => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.status = BytecodeTaskStatus::Completed(value.clone());
                }
                Ok(value)
            }
            Err(error) => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.status = BytecodeTaskStatus::Failed(error.clone());
                }
                Err(error)
            }
        };
        self.sync_external_memory();
        result
    }

    fn await_task(
        &mut self,
        image: &Image<'_>,
        id: u64,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        loop {
            let Some((status, _)) = self.task_status_kind_and_copy_bytes(id) else {
                return Err(self.runtime.error(
                    RuntimeErrorKind::ReferenceError("task does not exist"),
                    span,
                ));
            };
            match status {
                BytecodeTaskStatusKind::Pending => {
                    if !self.run_next_task(image, span)? {
                        return self.run_task(image, id, span);
                    }
                }
                BytecodeTaskStatusKind::Completed
                | BytecodeTaskStatusKind::Failed
                | BytecodeTaskStatusKind::Cancelled
                | BytecodeTaskStatusKind::Running => return self.run_task(image, id, span),
            }
        }
    }

    fn spawn_task(&mut self, span: Span, image: &Image<'_>) -> Result<(), RuntimeError> {
        if !self.runtime.host_services().task_creation_enabled() {
            return Err(self
                .runtime
                .error(RuntimeErrorKind::PermissionError("task.spawn"), span));
        }
        let callable = self.runtime.pop(span)?;
        let (function_module, function_id, capture_bytes) = match &callable {
            Value::Function(function) => (function.module_index, &function.id, 0usize),
            Value::Closure(closure) => {
                let capture_bytes = {
                    let captures = closure.captures.borrow();
                    super::estimated_capture_clone_bytes(&captures)
                };
                (
                    closure.function.module_index,
                    &closure.function.id,
                    capture_bytes,
                )
            }
            other => return Err(self.runtime.type_error("Function or Closure", &other, span)),
        };
        let function_index = image
            .functions
            .iter()
            .position(|function| {
                function.module_index == function_module
                    && function.id.as_ref() == Some(function_id)
            })
            .ok_or_else(|| {
                self.runtime.error(
                    RuntimeErrorKind::ReferenceError("function does not exist"),
                    span,
                )
            })?;
        let function_record = &image.functions[function_index];
        let parameter_count = function_record.parameter_ids.len();
        let required = if function_record.rest_parameter_code == 0 {
            parameter_count
        } else {
            parameter_count.saturating_sub(1)
        };
        if required > 0 || (function_record.rest_parameter_code == 0 && parameter_count != 0) {
            return Err(self.runtime.error(
                RuntimeErrorKind::TypeError {
                    expected: "zero-argument function for SPAWN",
                    found: "function requiring arguments",
                },
                span,
            ));
        }
        let request_memory = size_of::<CallRequest>()
            .saturating_add(super::estimated_bigint_bytes(function_id))
            .saturating_add(capture_bytes);
        self.runtime
            .ensure_memory_budget(self.projected_task_insert_bytes(request_memory), span)?;
        let (owned_module, owned_id, captures) = match callable {
            Value::Function(function) => (function.module_index, function.id, Vec::new()),
            Value::Closure(closure) => (
                closure.function.module_index,
                closure.function.id.clone(),
                closure.captures.borrow().clone(),
            ),
            _ => unreachable!("callable kind was validated"),
        };
        let request = CallRequest {
            module_index: owned_module,
            function_id: owned_id,
            arguments: Vec::new(),
            captures,
            return_action: ReturnAction::PushResult,
        };
        let id = self.enqueue_task(request, span)?;
        self.runtime.push_value(Value::Task(id), span)
    }

    fn projected_task_insert_bytes(&self, request_bytes: usize) -> usize {
        let task_capacity =
            super::projected_capacity_after_push(self.tasks.len(), self.tasks.capacity());
        let ready_capacity = super::projected_capacity_after_push(
            self.ready_tasks.len(),
            self.ready_tasks.capacity(),
        );
        size_of::<BytecodeTask>()
            .saturating_add(request_bytes)
            .saturating_add(
                task_capacity
                    .saturating_sub(self.tasks.capacity())
                    .saturating_mul(size_of::<(u64, BytecodeTask)>()),
            )
            .saturating_add(
                ready_capacity
                    .saturating_sub(self.ready_tasks.capacity())
                    .saturating_mul(size_of::<u64>()),
            )
    }

    fn sleep_task(&mut self, image: &Image<'_>, span: Span) -> Result<(), RuntimeError> {
        if self.current_task_id.is_none() {
            return Err(self.runtime.error(
                RuntimeErrorKind::TypeError {
                    expected: "async task context",
                    found: "main execution context",
                },
                span,
            ));
        }
        let value = self.runtime.pop(span)?;
        let Value::Int(milliseconds) = value else {
            return Err(self
                .runtime
                .type_error("nonnegative Int milliseconds", &value, span));
        };
        let milliseconds = milliseconds.to_u64().ok_or_else(|| {
            self.runtime.error(
                RuntimeErrorKind::TypeError {
                    expected: "nonnegative representable Int milliseconds",
                    found: "Int",
                },
                span,
            )
        })?;
        let duration = std::time::Duration::from_millis(milliseconds);
        if self.runtime.execution_started.is_some_and(|started| {
            started.elapsed().saturating_add(duration) >= self.runtime.limits.max_execution_time
        }) {
            return Err(self
                .runtime
                .error(RuntimeErrorKind::ResourceLimit("execution time"), span));
        }
        let Some(deadline) = std::time::Instant::now().checked_add(duration) else {
            return Err(self
                .runtime
                .error(RuntimeErrorKind::ResourceLimit("execution time"), span));
        };
        while std::time::Instant::now() < deadline {
            if !self.run_next_task(image, span)? {
                std::thread::sleep(deadline.saturating_duration_since(std::time::Instant::now()));
                break;
            }
            self.runtime.check_resource_limits(span)?;
        }
        self.runtime.push_value(Value::Null, span)
    }

    fn cancel_task(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.runtime.pop(span)?;
        let Value::Task(id) = value else {
            return Err(self.runtime.type_error("Task", &value, span));
        };
        let Some(task) = self.tasks.get_mut(&id) else {
            return Err(self.runtime.error(
                RuntimeErrorKind::ReferenceError("task does not exist"),
                span,
            ));
        };
        let accepted = if matches!(task.status, BytecodeTaskStatus::Pending) {
            task.status = BytecodeTaskStatus::Cancelled;
            true
        } else {
            false
        };
        self.ready_tasks.retain(|queued| *queued != id);
        self.sync_external_memory();
        self.runtime.push_value(Value::Bool(accepted), span)
    }

    fn execute_instruction(
        &mut self,
        image: &Image<'_>,
        frames: &mut Vec<Frame>,
        instruction: Instruction,
        instruction_offset: usize,
        span: Span,
    ) -> Result<Option<Value>, RuntimeError> {
        let frame_index = frames.len().saturating_sub(1);
        match instruction {
            Instruction::PushConstant(index) => {
                let value = image.constants.get(index as usize).ok_or_else(|| {
                    self.runtime.error(
                        RuntimeErrorKind::CompileError("constant index out of range".into()),
                        span,
                    )
                })?;
                match value {
                    Constant::Value(value) => {
                        self.runtime.ensure_memory_budget(
                            super::estimated_unmanaged_payload_bytes(value, 0),
                            span,
                        )?;
                        self.runtime.push_value(value.clone(), span)?;
                    }
                    Constant::FunctionRef(function_index) => {
                        let function =
                            image
                                .functions
                                .get(*function_index as usize)
                                .ok_or_else(|| {
                                    self.runtime.error(
                                        RuntimeErrorKind::CompileError(
                                            "function constant index out of range".into(),
                                        ),
                                        span,
                                    )
                                })?;
                        let Some(function_id) = function.id.as_ref() else {
                            return Err(self.runtime.error(
                                RuntimeErrorKind::ReferenceError(
                                    "initializer is not a function value",
                                ),
                                span,
                            ));
                        };
                        self.runtime.ensure_memory_budget(
                            super::estimated_bigint_bytes(function_id),
                            span,
                        )?;
                        let id = function_id.clone();
                        self.runtime.push_value(
                            Value::Function(FunctionRef {
                                module_index: function.module_index,
                                id,
                            }),
                            span,
                        )?;
                    }
                    Constant::TypeRef => {
                        return Err(self.unsupported(span, "type constant values"));
                    }
                }
            }
            Instruction::Intrinsic(id) => {
                let operation = IrOp::from_intrinsic_id(id).ok_or_else(|| {
                    self.runtime.error(
                        RuntimeErrorKind::CompileError("unknown intrinsic ID".into()),
                        span,
                    )
                })?;
                self.check_capability(operation, span)?;
                let module_index = frames[frame_index].module_index;
                if let Some(request) = self.call_intrinsic(operation, span, module_index, image)? {
                    self.enter_function(request, span, instruction_offset, image, frames, false)?;
                }
            }
            Instruction::Jump { target, unwind } => {
                let frame = frames.last_mut().expect("active frame");
                self.unwind_scopes(frame, unwind as usize, span)?;
                frame.ip = target as usize;
            }
            Instruction::BranchIfFalse {
                false_target,
                true_target,
            } => {
                let condition = self.runtime.pop(span)?;
                let Value::Bool(condition) = condition else {
                    return Err(self.runtime.type_error("Bool", &condition, span));
                };
                frames.last_mut().expect("active frame").ip =
                    if condition { true_target } else { false_target };
            }
            Instruction::ForInit(loop_id) => {
                let frame = frames.last_mut().expect("active frame");
                self.for_init(loop_id, frame, span)?;
            }
            Instruction::ForNext {
                loop_id,
                body_target,
                exit_target,
            } => {
                let frame = frames.last_mut().expect("active frame");
                self.for_next(loop_id, body_target, exit_target, frame, span)?;
            }
            Instruction::ScopeEnter => self.runtime.scopes.push(HashMap::new()),
            Instruction::ScopeExit => {
                let frame = frames.last_mut().expect("active frame");
                self.scope_exit(frame, span)?;
            }
            Instruction::ReturnTop(unwind) => {
                let frame = frames.last_mut().expect("active frame");
                if frame.initializer {
                    return Err(self.runtime.error(
                        RuntimeErrorKind::CompileError("RETURN_TOP in module initializer".into()),
                        span,
                    ));
                }
                let action = frame.return_action.clone();
                let value = self.return_frame(unwind as usize, span, frame)?;
                frames.pop();
                self.runtime.call_trace.pop();
                if frames.is_empty() {
                    return Ok(Some(value));
                }
                match action {
                    ReturnAction::PushResult => self.runtime.push_value(value, span)?,
                    ReturnAction::PushValue(value) => self.runtime.push_value(value, span)?,
                }
            }
            Instruction::Halt => {
                if !frames[frame_index].initializer {
                    return Err(self.runtime.error(
                        RuntimeErrorKind::CompileError("HALT outside module initializer".into()),
                        span,
                    ));
                }
                return Ok(Some(Value::Null));
            }
            Instruction::Trap => {
                return Err(self
                    .runtime
                    .error(RuntimeErrorKind::Unsupported("reached compiler TRAP"), span));
            }
            Instruction::Rethrow => {
                let error = frames[frame_index]
                    .pending_exceptions
                    .iter()
                    .rev()
                    .find(|pending| {
                        instruction_offset >= pending.handler_start
                            && instruction_offset < pending.handler_end
                    })
                    .map(|pending| pending.error.clone())
                    .ok_or_else(|| {
                        self.runtime.error(
                            RuntimeErrorKind::CompileError(
                                "RETHROW without a pending exception".into(),
                            ),
                            span,
                        )
                    })?;
                return Err(error);
            }
        }
        Ok(None)
    }

    fn activate_exception_regions(&mut self, image: &Image<'_>, frame: &mut Frame) {
        frame.active_exceptions.retain(|index, _| {
            image.exceptions.get(*index).is_some_and(|region| {
                region.function_index as usize == frame.function_index
                    && frame.ip >= region.protected_start
                    && frame.ip < region.protected_end
            })
        });
        let active = image
            .exceptions
            .iter()
            .enumerate()
            .filter(|(_, region)| {
                region.function_index as usize == frame.function_index
                    && frame.ip >= region.protected_start
                    && frame.ip < region.protected_end
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let snapshot = self.runtime.execution_snapshot();
        for index in active {
            frame
                .active_exceptions
                .entry(index)
                .or_insert_with(|| snapshot.clone());
        }
    }

    fn expire_pending_exceptions(&self, frame: &mut Frame) {
        frame
            .pending_exceptions
            .retain(|pending| frame.ip >= pending.handler_start && frame.ip < pending.handler_end);
    }

    fn dispatch_exception(
        &mut self,
        image: &Image<'_>,
        frames: &mut Vec<Frame>,
        instruction_offset: usize,
        error: RuntimeError,
    ) -> bool {
        if error.kind.is_fatal() || frames.is_empty() {
            return false;
        }
        let mut target = None;
        for frame_index in (0..frames.len()).rev() {
            let site = if frame_index + 1 == frames.len() {
                instruction_offset
            } else {
                frames[frame_index + 1].call_site_offset
            };
            let frame = &frames[frame_index];
            target = image
                .exceptions
                .iter()
                .enumerate()
                .filter(|(index, region)| {
                    region.function_index as usize == frame.function_index
                        && site >= region.protected_start
                        && site < region.protected_end
                        && frame.active_exceptions.contains_key(index)
                })
                .min_by_key(|(_, region)| {
                    region.protected_end.saturating_sub(region.protected_start)
                })
                .map(|(index, region)| (frame_index, index, region.handler_offset));
            if target.is_some() {
                break;
            }
        }
        let Some((target_frame, region_index, handler_offset)) = target else {
            return false;
        };

        let expected_stack_depth = image.exceptions[region_index].stack_depth as usize;
        let stack_base = frames[target_frame].stack_base;
        let Some(snapshot) = frames[target_frame].active_exceptions.get(&region_index) else {
            return false;
        };
        if snapshot.stack.len().saturating_sub(stack_base) != expected_stack_depth {
            return false;
        }

        while frames.len() > target_frame + 1 {
            if let Some(frame) = frames.pop() {
                self.unwind_exception_frame(&frame);
            }
        }
        let frame = frames.last_mut().expect("exception target frame exists");
        let Some(snapshot) = frame.active_exceptions.remove(&region_index) else {
            return false;
        };
        let restored_scope_depth = snapshot.scope_depth;
        self.runtime.restore_execution_snapshot(&snapshot);
        frame.ip = handler_offset;
        frame
            .loop_states
            .retain(|_, state| state.scope_depth < restored_scope_depth);
        frame.pending_exceptions.retain(|pending| {
            handler_offset >= pending.handler_start && handler_offset < pending.handler_end
        });
        frame.pending_exceptions.push(PendingException {
            handler_start: handler_offset,
            handler_end: image.exceptions[region_index].handler_end,
            error,
        });
        true
    }

    fn unwind_exception_frame(&mut self, frame: &Frame) {
        self.runtime.stack.truncate(frame.stack_base);
        self.runtime.scopes.truncate(frame.scope_floor);
        self.runtime.scope_floors.pop();
        self.runtime.frame_stack_bases.pop();
        self.runtime.capture_vectors.pop();
        self.runtime.call_depth = self.runtime.call_depth.saturating_sub(1);
        self.runtime.call_trace.pop();
    }

    fn error_for(
        &self,
        image: &Image<'_>,
        frame: &Frame,
        ip: usize,
        message: &str,
    ) -> RuntimeError {
        self.runtime.error(
            RuntimeErrorKind::CompileError(message.to_owned()),
            image.span(frame.function_index, frame.function.code_offset + ip),
        )
    }

    fn unsupported(&self, span: Span, feature: &'static str) -> RuntimeError {
        self.runtime
            .error(RuntimeErrorKind::Unsupported(feature), span)
    }

    fn check_capability(&self, operation: IrOp, span: Span) -> Result<(), RuntimeError> {
        let required: &[Capability] = match operation {
            // Import permissions depend on the resolution path: check fs.read
            // on an actual file read and env.read only when STAR_PATH is used.
            // This runtime slice has no module resolver and rejects imports.
            IrOp::Import | IrOp::ImportDynamic => &[],
            IrOp::Sleep => &[Capability::Clock],
            IrOp::NativeLoad
            | IrOp::NativeSymbol
            | IrOp::NativeCall
            | IrOp::NativeClose
            | IrOp::NativeCallback
            | IrOp::NativeBuffer => &[Capability::Ffi],
            _ => &[],
        };
        for capability in required {
            if !self.policy.granted_capabilities.contains(capability) {
                return Err(self
                    .runtime
                    .error(RuntimeErrorKind::PermissionError(capability.name()), span));
            }
        }
        Ok(())
    }

    fn unwind_scopes(
        &mut self,
        frame: &mut Frame,
        count: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if count
            > self
                .runtime
                .scopes
                .len()
                .saturating_sub(frame.base_scope_depth)
        {
            return Err(self.runtime.error(
                RuntimeErrorKind::CompileError("scope unwind crosses function frame".into()),
                span,
            ));
        }
        for _ in 0..count {
            let depth = self.runtime.scopes.len() - 1;
            frame
                .loop_states
                .retain(|_, state| state.scope_depth != depth);
            self.runtime.scopes.pop();
        }
        Ok(())
    }

    fn scope_exit(&mut self, frame: &mut Frame, span: Span) -> Result<(), RuntimeError> {
        if self.runtime.scopes.len() <= frame.base_scope_depth {
            return Err(self.runtime.error(
                RuntimeErrorKind::CompileError("SCOPE_EXIT crosses function frame".into()),
                span,
            ));
        }
        let depth = self.runtime.scopes.len() - 1;
        frame
            .loop_states
            .retain(|_, state| state.scope_depth != depth);
        self.runtime.scopes.pop();
        Ok(())
    }

    fn for_init(
        &mut self,
        loop_id: u32,
        frame: &mut Frame,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let variable_id = self.runtime.pop_identifier(span)?;
        let iterator = self.runtime.pop(span)?;
        let Value::Iterator(iterator) = iterator else {
            return Err(self.runtime.type_error("Iterator", &iterator, span));
        };
        let scope_depth = self.runtime.scopes.len();
        self.runtime.scopes.push(HashMap::new());
        frame.loop_states.insert(
            loop_id,
            LoopState {
                iterator,
                variable_id,
                scope_depth,
            },
        );
        Ok(())
    }

    fn for_next(
        &mut self,
        loop_id: u32,
        body_target: usize,
        exit_target: usize,
        frame: &mut Frame,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        let state = frame.loop_states.get(&loop_id).ok_or_else(|| {
            self.runtime.error(
                RuntimeErrorKind::CompileError("unknown FOR state".into()),
                span,
            )
        })?;
        let iterator_ref = Rc::clone(&state.iterator);
        let variable_id = state.variable_id.clone();
        let scope_depth = state.scope_depth;
        let next_payload_bytes = {
            let iterator = iterator_ref.borrow();
            iterator
                .values
                .get(iterator.next_index)
                .map(|value| super::estimated_unmanaged_payload_bytes(value, 0))
        };
        if let Some(payload_bytes) = next_payload_bytes {
            self.runtime.ensure_memory_budget(payload_bytes, span)?;
        }
        let next = {
            let mut iterator = iterator_ref.borrow_mut();
            let value = iterator.values.get(iterator.next_index).cloned();
            if value.is_some() {
                iterator.next_index += 1;
            }
            value
        };
        if let Some(value) = next {
            if scope_depth >= self.runtime.scopes.len() {
                return Err(self.runtime.error(
                    RuntimeErrorKind::CompileError("FOR scope is missing".into()),
                    span,
                ));
            }
            let scope = &mut self.runtime.scopes[scope_depth];
            if let Some(binding) = scope.get(&variable_id) {
                binding.borrow_mut().value = value;
            } else {
                scope.insert(variable_id, binding_cell(value, true));
            }
            frame.ip = body_target;
        } else {
            frame.loop_states.remove(&loop_id);
            frame.ip = exit_target;
        }
        Ok(true)
    }

    fn return_frame(
        &mut self,
        unwind: usize,
        span: Span,
        frame: &Frame,
    ) -> Result<Value, RuntimeError> {
        let explicit_scopes = self
            .runtime
            .scopes
            .len()
            .saturating_sub(frame.base_scope_depth);
        if unwind != explicit_scopes {
            return Err(self.runtime.error(
                RuntimeErrorKind::CompileError("RETURN_TOP scope unwind mismatch".into()),
                span,
            ));
        }
        let value = if self.runtime.stack.len() > frame.stack_base {
            self.runtime.pop(span)?
        } else {
            Value::Null
        };
        self.runtime.stack.truncate(frame.stack_base);
        self.runtime.scopes.truncate(frame.scope_floor);
        self.runtime.scope_floors.pop();
        self.runtime.frame_stack_bases.pop();
        self.runtime.capture_vectors.pop();
        self.runtime.call_depth = self.runtime.call_depth.saturating_sub(1);
        Ok(value)
    }

    fn enter_function(
        &mut self,
        request: CallRequest,
        span: Span,
        call_site_offset: usize,
        image: &Image<'_>,
        frames: &mut Vec<Frame>,
        task_entry: bool,
    ) -> Result<(), RuntimeError> {
        let function_index = image
            .functions
            .iter()
            .position(|function| {
                function.module_index == request.module_index
                    && function.id.as_ref() == Some(&request.function_id)
            })
            .ok_or_else(|| {
                self.runtime.error(
                    RuntimeErrorKind::ReferenceError("function does not exist"),
                    span,
                )
            })?;
        let function_record = &image.functions[function_index];
        let parameter_count = function_record.parameter_ids.len();
        let required = if function_record.rest_parameter_code == 0 {
            parameter_count
        } else {
            parameter_count.saturating_sub(1)
        };
        if request.arguments.len() < required
            || (function_record.rest_parameter_code == 0
                && request.arguments.len() != parameter_count)
        {
            return Err(self.runtime.error(
                RuntimeErrorKind::TypeError {
                    expected: "argument count matching function parameters",
                    found: "wrong argument count",
                },
                span,
            ));
        }
        if function_record.asynchronous && !task_entry {
            let task_id = self.enqueue_task(request, span)?;
            self.runtime.push_value(Value::Task(task_id), span)?;
            return Ok(());
        }
        if self.runtime.call_depth >= self.runtime.limits.max_call_depth {
            return Err(self.runtime.error(RuntimeErrorKind::StackOverflow, span));
        }
        let fixed_count = if function_record.rest_parameter_code == 0 {
            parameter_count
        } else {
            parameter_count.saturating_sub(1)
        };
        let rest_count = request.arguments.len().saturating_sub(fixed_count);
        let function_copy_bytes = size_of::<FunctionRecord>()
            .saturating_add(function_record_payload_bytes(function_record));
        let argument_storage_bytes = request
            .arguments
            .capacity()
            .saturating_mul(size_of::<Value>())
            .saturating_add(super::estimate_cloned_value_payloads(&request.arguments));
        let capture_storage_bytes = request
            .captures
            .capacity()
            .saturating_mul(size_of::<(BigInt, BindingCell)>())
            .saturating_add(
                request
                    .captures
                    .iter()
                    .map(|(id, _)| super::estimated_bigint_bytes(id))
                    .fold(0usize, usize::saturating_add),
            );
        let capture_scope_bytes = if request.captures.is_empty() {
            0
        } else {
            request
                .captures
                .len()
                .saturating_mul(size_of::<(BigInt, BindingCell)>() + 2 * size_of::<usize>())
                .saturating_add(
                    request
                        .captures
                        .iter()
                        .map(|(id, _)| super::estimated_bigint_bytes(id))
                        .fold(0usize, usize::saturating_add),
                )
        };
        let parameter_storage_bytes = parameter_count
            .saturating_mul(size_of::<(BigInt, BindingCell)>() + 2 * size_of::<usize>())
            .saturating_add(
                function_record
                    .parameter_ids
                    .iter()
                    .map(super::estimated_bigint_bytes)
                    .fold(0usize, usize::saturating_add),
            )
            .saturating_add(
                parameter_count.saturating_mul(size_of::<Value>() + 4 * size_of::<usize>()),
            )
            .saturating_add(super::estimate_cloned_value_payloads(
                &request.arguments[..fixed_count],
            ));
        let rest_array_bytes = if function_record.rest_parameter_code == 0 {
            0
        } else {
            rest_count
                .saturating_mul(size_of::<Value>())
                .saturating_add(super::estimate_cloned_value_payloads(
                    &request.arguments[fixed_count..],
                ))
                .saturating_add(size_of::<RefCell<Vec<Value>>>() + 2 * size_of::<usize>())
        };
        let frame_capacity = frame_capacity_after_push(
            frames.capacity(),
            frames.len(),
            self.runtime.limits.max_call_depth.saturating_add(1).max(1),
        )
        .ok_or_else(|| self.runtime.error(RuntimeErrorKind::StackOverflow, span))?;
        let frame_capacity_growth = frame_capacity
            .saturating_sub(frames.capacity())
            .saturating_mul(size_of::<Frame>());
        let call_trace_capacity = frame_capacity_after_push(
            self.runtime.call_trace.capacity(),
            self.runtime.call_trace.len(),
            self.runtime.limits.max_call_depth.max(1),
        )
        .ok_or_else(|| self.runtime.error(RuntimeErrorKind::StackOverflow, span))?;
        let call_trace_capacity_growth = call_trace_capacity
            .saturating_sub(self.runtime.call_trace.capacity())
            .saturating_mul(size_of::<crate::runtime::StackTraceFrame>());
        self.runtime.ensure_memory_budget(
            function_copy_bytes
                .saturating_add(
                    super::estimated_bigint_bytes(&request.function_id).saturating_mul(3),
                )
                .saturating_add(argument_storage_bytes)
                .saturating_add(capture_storage_bytes)
                .saturating_add(capture_scope_bytes)
                .saturating_add(parameter_storage_bytes)
                .saturating_add(rest_array_bytes)
                .saturating_add(size_of::<Frame>())
                .saturating_add(frame_capacity_growth)
                .saturating_add(call_trace_capacity_growth.saturating_mul(2))
                .saturating_add(self.runtime.call_frame_bookkeeping_growth_bytes()),
            span,
        )?;
        self.runtime.reserve_call_frame_bookkeeping(span)?;
        if call_trace_capacity > self.runtime.call_trace.capacity() {
            self.runtime
                .call_trace
                .try_reserve_exact(
                    call_trace_capacity.saturating_sub(self.runtime.call_trace.len()),
                )
                .map_err(|_| {
                    self.runtime
                        .error(RuntimeErrorKind::ResourceLimit("call trace"), span)
                })?;
            self.update_external_frame_memory(frames);
        }
        if frame_capacity > frames.capacity() {
            frames
                .try_reserve_exact(frame_capacity.saturating_sub(frames.len()))
                .map_err(|_| {
                    self.runtime
                        .error(RuntimeErrorKind::ResourceLimit("call frames"), span)
                })?;
            self.update_external_frame_memory(frames);
        }
        let function = function_record.clone();
        let scope_floor = self.runtime.scopes.len();
        let stack_base = self.runtime.stack.len();
        if !request.captures.is_empty() {
            self.runtime.scopes.push(
                request
                    .captures
                    .iter()
                    .cloned()
                    .collect::<HashMap<BigInt, BindingCell>>(),
            );
        }
        let mut parameters = HashMap::with_capacity(function.parameter_ids.len());
        for (position, id) in function.parameter_ids.iter().enumerate() {
            let value = if function.rest_parameter_code != 0
                && position + 1 == function.parameter_ids.len()
            {
                self.runtime
                    .allocate_array(request.arguments[position..].to_vec())
            } else {
                request.arguments[position].clone()
            };
            parameters.insert(id.clone(), binding_cell(value, true));
        }
        self.runtime.scope_floors.push(scope_floor);
        self.runtime.frame_stack_bases.push(stack_base);
        self.runtime.scopes.push(parameters);
        self.runtime.capture_vectors.push(Vec::new());
        self.runtime.call_depth += 1;
        self.runtime
            .call_trace
            .push(crate::runtime::StackTraceFrame {
                function_id: request.function_id.clone(),
                call_site: span,
            });
        frames.push(Frame::function(
            function_index,
            function,
            stack_base,
            scope_floor,
            self.runtime.scopes.len(),
            request.return_action,
            call_site_offset,
        ));
        self.update_external_frame_memory(frames);
        Ok(())
    }

    fn call_intrinsic(
        &mut self,
        operation: IrOp,
        span: Span,
        module_index: u32,
        image: &Image<'_>,
    ) -> Result<Option<CallRequest>, RuntimeError> {
        use IrOp::*;
        match operation {
            Add => self.runtime.arithmetic(1, span)?,
            Sub => self.runtime.arithmetic(2, span)?,
            Mul => self.runtime.arithmetic(3, span)?,
            Div => self.runtime.arithmetic(4, span)?,
            Mod => self.runtime.arithmetic(5, span)?,
            Print => {
                let value = self.runtime.pop(span)?;
                let formatted = self.runtime.format_value(&value, span)?;
                self.runtime
                    .output
                    .write_all(formatted.as_bytes())
                    .map_err(|error| {
                        self.runtime
                            .error(RuntimeErrorKind::IOError(error.to_string()), span)
                    })?;
            }
            Input => {
                let line = self.runtime.read_input_line(span)?;
                self.runtime.push_value(
                    match line {
                        Some(bytes) => Value::String(String::from_utf8(bytes).map_err(|_| {
                            self.runtime.error(
                                RuntimeErrorKind::IOError(
                                    "standard input is not valid UTF-8".into(),
                                ),
                                span,
                            )
                        })?),
                        None => Value::Null,
                    },
                    span,
                )?;
            }
            Store => self.runtime.store(span)?,
            Load => self.runtime.load(span, false)?,
            Eq => self.runtime.equal(span)?,
            Gt => self.runtime.greater_than(span)?,
            PushNull => self.runtime.push_value(Value::Null, span)?,
            PushTrue => self.runtime.push_value(Value::Bool(true), span)?,
            PushFalse => self.runtime.push_value(Value::Bool(false), span)?,
            ToFloat => {
                let value = self.runtime.pop(span)?;
                let converted = match &value {
                    Value::Float(number) => *number,
                    Value::Int(integer) => super::to_float(&Value::Int(integer.clone())),
                    Value::Byte(byte) => f64::from(*byte),
                    Value::Bool(value) => f64::from(u8::from(*value)),
                    Value::String(value) if is_decimal_float(value) => {
                        value.parse::<f64>().map_err(|_| {
                            self.runtime.error(
                                RuntimeErrorKind::TypeError {
                                    expected: "ASCII decimal String",
                                    found: "String",
                                },
                                span,
                            )
                        })?
                    }
                    other => {
                        return Err(self
                            .runtime
                            .type_error("Float-convertible value", other, span))
                    }
                };
                self.runtime.push_value(Value::Float(converted), span)?;
            }
            ToBool => self.runtime.to_bool(span)?,
            FloatFromBits => {
                let value = self.runtime.pop(span)?;
                let Value::Int(bits) = value else {
                    return Err(self.runtime.type_error("Int in 0..=2^64-1", &value, span));
                };
                let bits = bits.to_string().parse::<u64>().map_err(|_| {
                    self.runtime.error(
                        RuntimeErrorKind::TypeError {
                            expected: "Int in 0..=2^64-1",
                            found: "Int",
                        },
                        span,
                    )
                })?;
                self.runtime
                    .push_value(Value::Float(f64::from_bits(bits)), span)?;
            }
            TypeOf => {
                let value = self.runtime.pop(span)?;
                self.runtime
                    .stack
                    .push(Value::String(value.type_name().to_owned()));
            }
            TypeCheck => {
                let type_name = self.runtime.pop(span)?;
                let Value::String(type_name) = type_name else {
                    return Err(self
                        .runtime
                        .type_error("String type name", &type_name, span));
                };
                const TYPES: &[&str] = &[
                    "Int",
                    "Float",
                    "Bool",
                    "Null",
                    "Byte",
                    "Char",
                    "String",
                    "Bytes",
                    "Array",
                    "Tuple",
                    "Map",
                    "Set",
                    "Struct",
                    "Function",
                    "Closure",
                    "NativeFunction",
                    "Iterator",
                    "Task",
                    "Error",
                    "Handle",
                    "Module",
                ];
                if !TYPES.contains(&type_name.as_str()) {
                    return Err(self.runtime.error(
                        RuntimeErrorKind::TypeError {
                            expected: "known type name",
                            found: "String",
                        },
                        span,
                    ));
                }
                let value = self.runtime.pop(span)?;
                self.runtime
                    .stack
                    .push(Value::Bool(value.type_name() == type_name));
            }
            IntToBytes => self.int_to_bytes(span)?,
            Pow => self.power(span)?,
            Neg => self.negate(span)?,
            Abs => self.absolute(span)?,
            And | Or | Xor => self.boolean_binary(operation, span)?,
            BitAnd | BitOr | BitXor | BitNot | Shl | Shr => self.bitwise(operation, span)?,
            Not => {
                let value = self.runtime.pop(span)?;
                match value {
                    Value::Bool(value) => self.runtime.push_value(Value::Bool(!value), span)?,
                    other => return Err(self.runtime.type_error("Bool", &other, span)),
                }
            }
            ToInt => self.runtime.to_int(span)?,
            ToBytes | Utf8Encode => self.runtime.utf8_encode(span)?,
            Utf8Decode => self.runtime.utf8_decode(span)?,
            ToString => {
                let value = self.runtime.pop(span)?;
                let formatted = self.runtime.format_value(&value, span)?;
                self.runtime.push_value(Value::String(formatted), span)?;
            }
            ToChar => self.runtime.to_char(span)?,
            ToByte => self.runtime.to_byte(span)?,
            Lt => self.runtime.less_than(span)?,
            ArrayNew => self.runtime.array_new(span)?,
            MapNew => self.runtime.map_new(span)?,
            SetNew => self.runtime.set_new(span)?,
            GetItem => self.runtime.get_item(span)?,
            SetItem => self.runtime.set_item(span)?,
            Append => self.runtime.append(span)?,
            Remove => self.runtime.remove(span)?,
            Length => self.runtime.length(span)?,
            Contains => self.runtime.contains(span)?,
            Iter => self.runtime.iter(span)?,
            Next => self.runtime.next(span)?,
            TupleNew => self.runtime.tuple_new(span)?,
            Slice => self.runtime.slice(span)?,
            Concat => self.runtime.concat(span)?,
            Declare => self.runtime.declare(span, true)?,
            Const => self.runtime.declare(span, false)?,
            Delete => self.runtime.delete(span)?,
            LoadGlobal => self.runtime.load(span, true)?,
            StoreGlobal => self.runtime.store_global(span)?,
            StructDef => self.runtime.define_struct(span)?,
            MethodDef => {
                let function_ids = image
                    .functions
                    .iter()
                    .filter(|function| function.module_index == module_index)
                    .filter_map(|function| function.id.clone())
                    .collect::<HashSet<_>>();
                self.runtime.define_method(span, &function_ids)?;
            }
            InterfaceDef => {
                let method_count = self
                    .runtime
                    .pop_count(span, "nonnegative interface method count")?;
                let mut method_ids = std::collections::HashSet::with_capacity(method_count);
                for _ in 0..method_count {
                    if !method_ids.insert(self.runtime.pop_identifier(span)?) {
                        return Err(self.runtime.error(
                            RuntimeErrorKind::CompileError("duplicate interface method ID".into()),
                            span,
                        ));
                    }
                }
                let interface_id = self.runtime.pop_identifier(span)?;
                if self.runtime.interfaces.contains_key(&interface_id)
                    || self.runtime.structs.contains_key(&interface_id)
                {
                    return Err(self.runtime.error(
                        RuntimeErrorKind::CompileError("duplicate or cross-kind type ID".into()),
                        span,
                    ));
                }
                self.runtime.interfaces.insert(
                    interface_id,
                    crate::value::InterfaceDefinition { method_ids },
                );
            }
            Implement => self.runtime.implement_interface(span)?,
            ObjectNew => {
                let struct_id = self.runtime.pop_identifier(span)?;
                let count = self
                    .runtime
                    .pop_count(span, "nonnegative Int argument count")?;
                if count > self.runtime.stack.len() {
                    return Err(self.runtime.error(RuntimeErrorKind::StackUnderflow, span));
                }
                self.runtime
                    .ensure_memory_budget(count.saturating_mul(size_of::<Value>()), span)?;
                let mut arguments = Vec::with_capacity(count);
                for _ in 0..count {
                    arguments.push(self.runtime.pop(span)?);
                }
                arguments.reverse();
                let Some(descriptor) = self.runtime.structs.get(&struct_id).cloned() else {
                    return Err(self.runtime.error(
                        RuntimeErrorKind::ReferenceError("struct does not exist"),
                        span,
                    ));
                };
                let object = self.runtime.allocate_object(ObjectState {
                    struct_id,
                    fields: descriptor
                        .field_ids
                        .iter()
                        .cloned()
                        .map(|field_id| (field_id, Value::Null))
                        .collect(),
                });
                if let Some(constructor_id) = descriptor.methods.get(&BigInt::zero()) {
                    let Some(function) = image.functions.iter().find(|function| {
                        function.module_index == module_index
                            && function.id.as_ref() == Some(constructor_id)
                    }) else {
                        return Err(self.runtime.error(
                            RuntimeErrorKind::ReferenceError("constructor function does not exist"),
                            span,
                        ));
                    };
                    if function.asynchronous {
                        return Err(self.unsupported(span, "async constructor scheduling"));
                    }
                    self.runtime.ensure_memory_budget(
                        arguments
                            .capacity()
                            .saturating_mul(size_of::<Value>())
                            .saturating_add(super::estimate_cloned_value_payloads(&arguments))
                            .saturating_add(
                                count.saturating_add(1).saturating_mul(size_of::<Value>()),
                            ),
                        span,
                    )?;
                    let mut arguments_with_receiver = Vec::with_capacity(count + 1);
                    arguments_with_receiver.push(object.clone());
                    arguments_with_receiver.extend(arguments);
                    return Ok(Some(CallRequest {
                        module_index,
                        function_id: constructor_id.clone(),
                        arguments: arguments_with_receiver,
                        captures: Vec::new(),
                        return_action: ReturnAction::PushValue(object),
                    }));
                }
                if !arguments.is_empty() {
                    return Err(self.runtime.error(
                        RuntimeErrorKind::TypeError {
                            expected: "zero arguments for a struct without a constructor",
                            found: "nonzero argument count",
                        },
                        span,
                    ));
                }
                self.runtime.push_value(object, span)?;
            }
            FieldGet => self.runtime.field_get(span)?,
            FieldSet => self.runtime.field_set(span)?,
            MethodCall => {
                let method_id = self.runtime.pop_identifier(span)?;
                if method_id.is_zero() {
                    return Err(self.runtime.error(
                        RuntimeErrorKind::ReferenceError("constructor is not an instance method"),
                        span,
                    ));
                }
                let receiver = self.runtime.pop(span)?;
                let Value::Object(object) = &receiver else {
                    return Err(self.runtime.type_error("Struct", &receiver, span));
                };
                let struct_id = object.borrow().struct_id.clone();
                let count = self
                    .runtime
                    .pop_count(span, "nonnegative Int argument count")?;
                if count > self.runtime.stack.len() {
                    return Err(self.runtime.error(RuntimeErrorKind::StackUnderflow, span));
                }
                self.runtime.ensure_memory_budget(
                    count
                        .saturating_add(1)
                        .saturating_mul(size_of::<Value>())
                        .saturating_add(super::estimated_unmanaged_payload_bytes(&receiver, 0)),
                    span,
                )?;
                let mut arguments = Vec::with_capacity(count + 1);
                for _ in 0..count {
                    arguments.push(self.runtime.pop(span)?);
                }
                arguments.reverse();
                arguments.insert(0, receiver);
                let function_id = self
                    .runtime
                    .structs
                    .get(&struct_id)
                    .and_then(|descriptor| descriptor.methods.get(&method_id))
                    .cloned()
                    .ok_or_else(|| {
                        self.runtime.error(
                            RuntimeErrorKind::ReferenceError("method does not exist"),
                            span,
                        )
                    })?;
                let Some(function) = image.functions.iter().find(|function| {
                    function.module_index == module_index
                        && function.id.as_ref() == Some(&function_id)
                }) else {
                    return Err(self.runtime.error(
                        RuntimeErrorKind::ReferenceError("method function does not exist"),
                        span,
                    ));
                };
                if function.asynchronous {
                    return Err(self.unsupported(span, "async method scheduling"));
                }
                return Ok(Some(CallRequest {
                    module_index,
                    function_id,
                    arguments,
                    captures: Vec::new(),
                    return_action: ReturnAction::PushResult,
                }));
            }
            ObjectIs => self.runtime.object_is(span)?,
            ObjectClone => self.runtime.object_clone(span)?,
            Capture => self.runtime.capture(span)?,
            Lambda => {
                let parameter_count = self
                    .runtime
                    .pop_count(span, "nonnegative parameter count")?;
                let body_id = self.runtime.pop_identifier(span)?;
                let Some(function) = image.functions.iter().find(|function| {
                    function.module_index == module_index && function.id.as_ref() == Some(&body_id)
                }) else {
                    return Err(self.runtime.error(
                        RuntimeErrorKind::ReferenceError("lambda body function does not exist"),
                        span,
                    ));
                };
                if function.parameter_ids.len() != parameter_count {
                    return Err(self.runtime.error(
                        RuntimeErrorKind::TypeError {
                            expected: "parameter count matching lambda body",
                            found: "wrong parameter count",
                        },
                        span,
                    ));
                }
                if self.runtime.capture_vectors.is_empty() {
                    return Err(self.runtime.error(
                        RuntimeErrorKind::CompileError("LAMBDA outside an execution frame".into()),
                        span,
                    ));
                }
                let captures = std::mem::take(self.runtime.capture_vectors.last_mut().unwrap());
                let function = FunctionRef {
                    module_index,
                    id: body_id,
                };
                if captures.is_empty() {
                    self.runtime.push_value(Value::Function(function), span)?;
                } else {
                    let closure = self.runtime.allocate_closure(ClosureState {
                        function,
                        captures: RefCell::new(captures),
                    });
                    self.runtime.push_value(closure, span)?;
                }
            }
            CallValue => {
                let callable = self.runtime.pop(span)?;
                let count = self
                    .runtime
                    .pop_count(span, "nonnegative Int argument count")?;
                if count > self.runtime.stack.len() {
                    return Err(self.runtime.error(RuntimeErrorKind::StackUnderflow, span));
                }
                self.runtime
                    .ensure_memory_budget(count.saturating_mul(size_of::<Value>()), span)?;
                let mut arguments = Vec::with_capacity(count);
                for _ in 0..count {
                    arguments.push(self.runtime.pop(span)?);
                }
                arguments.reverse();
                let (function, captures) = match callable {
                    Value::Function(function) => (function, Vec::new()),
                    Value::Closure(closure) => {
                        let captures = closure.captures.borrow();
                        self.runtime.ensure_memory_budget(
                            super::estimated_capture_clone_bytes(&captures)
                                .saturating_add(size_of::<FunctionRef>())
                                .saturating_add(super::estimated_bigint_bytes(&closure.function.id))
                                .saturating_add(
                                    arguments.capacity().saturating_mul(size_of::<Value>()),
                                )
                                .saturating_add(super::estimate_cloned_value_payloads(&arguments)),
                            span,
                        )?;
                        (closure.function.clone(), captures.clone())
                    }
                    other => {
                        return Err(self.runtime.type_error("Function or Closure", &other, span))
                    }
                };
                return Ok(Some(CallRequest {
                    module_index: function.module_index,
                    function_id: function.id,
                    arguments,
                    captures,
                    return_action: ReturnAction::PushResult,
                }));
            }
            Call => {
                let function_id = self.runtime.pop_identifier(span)?;
                let count = self
                    .runtime
                    .pop_count(span, "nonnegative Int argument count")?;
                if count > self.runtime.stack.len() {
                    return Err(self.runtime.error(RuntimeErrorKind::StackUnderflow, span));
                }
                self.runtime
                    .ensure_memory_budget(count.saturating_mul(size_of::<Value>()), span)?;
                let mut arguments = Vec::with_capacity(count);
                for _ in 0..count {
                    arguments.push(self.runtime.pop(span)?);
                }
                arguments.reverse();
                return Ok(Some(CallRequest {
                    module_index,
                    function_id,
                    arguments,
                    captures: Vec::new(),
                    return_action: ReturnAction::PushResult,
                }));
            }
            Await | Join => {
                let value = self.runtime.pop(span)?;
                let Value::Task(id) = value else {
                    return Err(self.runtime.type_error("Task", &value, span));
                };
                let result = self.await_task(image, id, span)?;
                self.runtime.push_value(result, span)?;
            }
            Spawn => self.spawn_task(span, image)?,
            Yield => {
                if self.current_task_id.is_none() {
                    return Err(self.runtime.error(
                        RuntimeErrorKind::TypeError {
                            expected: "async task context",
                            found: "main execution context",
                        },
                        span,
                    ));
                }
                self.run_next_task(image, span)?;
            }
            Sleep => self.sleep_task(image, span)?,
            Cancel => self.cancel_task(span)?,
            Import | ImportDynamic => {
                return Err(self.unsupported(span, "module import resolution"))
            }
            NativeLoad => self.runtime.execute_native_instruction(0xB0, span)?,
            NativeSymbol => self.runtime.execute_native_instruction(0xB1, span)?,
            NativeCall => self.runtime.execute_native_instruction(0xB2, span)?,
            NativeClose => self.runtime.execute_native_instruction(0xB3, span)?,
            NativeCallback => self.runtime.execute_native_instruction(0xB4, span)?,
            NativeBuffer => self.runtime.execute_native_instruction(0xB5, span)?,
            TailCall => return Err(self.unsupported(span, "TAIL_CALL")),
            _ => return Err(self.unsupported(span, "intrinsic runtime feature")),
        }
        Ok(None)
    }

    fn int_to_bytes(&mut self, span: Span) -> Result<(), RuntimeError> {
        let width = self
            .runtime
            .pop_count(span, "nonnegative Int byte length")?;
        if width > self.policy.limits.max_file_bytes {
            return Err(self.runtime.error(RuntimeErrorKind::ExecutionLimit, span));
        }
        let value = self.runtime.pop(span)?;
        let Value::Int(value) = value else {
            return Err(self.runtime.type_error("Int", &value, span));
        };
        if value.is_negative() {
            return Err(self.runtime.error(
                RuntimeErrorKind::TypeError {
                    expected: "nonnegative Int",
                    found: "Int",
                },
                span,
            ));
        }
        let value_bytes = usize::try_from(value.bits().saturating_add(7) / 8).unwrap_or(usize::MAX);
        self.runtime
            .ensure_memory_budget(width.saturating_add(value_bytes), span)?;
        let (_, mut bytes) = value.to_bytes_be();
        if value.is_zero() && width == 0 {
            bytes.clear();
        }
        if bytes.len() > width {
            return Err(self.runtime.error(
                RuntimeErrorKind::TypeError {
                    expected: "Int that fits requested byte length",
                    found: "Int",
                },
                span,
            ));
        }
        if width > bytes.len() {
            let mut padded = vec![0; width - bytes.len()];
            padded.extend(bytes);
            bytes = padded;
        }
        self.runtime.push_value(Value::Bytes(bytes), span)?;
        Ok(())
    }

    fn power(&mut self, span: Span) -> Result<(), RuntimeError> {
        let exponent = self.runtime.pop(span)?;
        let base = self.runtime.pop(span)?;
        if let (Value::Int(base_int), Value::Int(exponent_int)) = (&base, &exponent) {
            if exponent_int.is_negative() {
                if base_int.is_zero() {
                    return Err(self.runtime.error(RuntimeErrorKind::DivisionByZero, span));
                }
                if base_int == &BigInt::one() {
                    self.runtime.push_value(Value::Int(BigInt::one()), span)?;
                } else if base_int == &BigInt::from(-1) {
                    let odd = (exponent_int % BigInt::from(2)) != BigInt::zero();
                    self.runtime.push_value(
                        Value::Int(if odd { BigInt::from(-1) } else { BigInt::one() }),
                        span,
                    )?;
                } else {
                    let result = super::to_float(&base).powf(super::to_float(&exponent));
                    self.runtime.push_value(Value::Float(result), span)?;
                }
                return Ok(());
            }
            if exponent_int.is_zero() {
                self.runtime.push_value(Value::Int(BigInt::one()), span)?;
                return Ok(());
            }
            if base_int.is_zero() {
                self.runtime.push_value(Value::Int(BigInt::zero()), span)?;
                return Ok(());
            }
            if base_int == &BigInt::one() {
                self.runtime.push_value(Value::Int(BigInt::one()), span)?;
                return Ok(());
            }
            if base_int == &BigInt::from(-1) {
                let odd = (exponent_int % BigInt::from(2)) != BigInt::zero();
                self.runtime.push_value(
                    Value::Int(if odd { BigInt::from(-1) } else { BigInt::one() }),
                    span,
                )?;
                return Ok(());
            }
            let Some(power) = exponent_int.to_u32() else {
                return Err(self.runtime.error(RuntimeErrorKind::ExecutionLimit, span));
            };
            let max_bits = self.policy.limits.max_file_bytes.saturating_mul(8) as u64;
            let result_bits = base_int.bits().saturating_mul(power as u64);
            if result_bits > max_bits.saturating_add(8) {
                return Err(self.runtime.error(RuntimeErrorKind::ExecutionLimit, span));
            }
            let result_bytes =
                usize::try_from(result_bits.saturating_add(7) / 8).unwrap_or(usize::MAX);
            let operand_bytes = usize::try_from(
                base_int
                    .bits()
                    .saturating_add(exponent_int.bits())
                    .saturating_add(7)
                    / 8,
            )
            .unwrap_or(usize::MAX);
            self.runtime
                .ensure_memory_budget(result_bytes.saturating_add(operand_bytes), span)?;
            self.runtime
                .push_value(Value::Int(base_int.pow(power)), span)?;
            return Ok(());
        }
        let base_is_numeric = matches!(base, Value::Int(_) | Value::Float(_));
        let exponent_is_numeric = matches!(exponent, Value::Int(_) | Value::Float(_));
        if !base_is_numeric {
            return Err(self.runtime.type_error("Int or Float", &base, span));
        }
        if !exponent_is_numeric {
            return Err(self.runtime.type_error("Int or Float", &exponent, span));
        }
        let result = super::to_float(&base).powf(super::to_float(&exponent));
        if super::to_float(&base) == 0.0 && super::to_float(&exponent) < 0.0 {
            return Err(self.runtime.error(RuntimeErrorKind::DivisionByZero, span));
        }
        self.runtime.push_value(Value::Float(result), span)?;
        Ok(())
    }

    fn negate(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.runtime.pop(span)?;
        match value {
            Value::Int(value) => self.runtime.push_value(Value::Int(-value), span)?,
            Value::Float(value) => self.runtime.push_value(Value::Float(-value), span)?,
            other => return Err(self.runtime.type_error("Int or Float", &other, span)),
        }
        Ok(())
    }

    fn absolute(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.runtime.pop(span)?;
        match value {
            Value::Int(value) => self.runtime.push_value(Value::Int(value.abs()), span)?,
            Value::Float(value) => self.runtime.push_value(Value::Float(value.abs()), span)?,
            other => return Err(self.runtime.type_error("Int or Float", &other, span)),
        }
        Ok(())
    }

    fn boolean_binary(&mut self, operation: IrOp, span: Span) -> Result<(), RuntimeError> {
        let right = self.runtime.pop(span)?;
        let left = self.runtime.pop(span)?;
        let left = match left {
            Value::Bool(value) => value,
            other => return Err(self.runtime.type_error("Bool", &other, span)),
        };
        let right = match right {
            Value::Bool(value) => value,
            other => return Err(self.runtime.type_error("Bool", &other, span)),
        };
        let result = match operation {
            IrOp::And => left && right,
            IrOp::Or => left || right,
            IrOp::Xor => left ^ right,
            _ => return Err(self.unsupported(span, "invalid boolean intrinsic")),
        };
        self.runtime.push_value(Value::Bool(result), span)?;
        Ok(())
    }

    fn bitwise(&mut self, operation: IrOp, span: Span) -> Result<(), RuntimeError> {
        if operation == IrOp::BitNot {
            let value = self.runtime.pop(span)?;
            let Value::Int(value) = value else {
                return Err(self.runtime.type_error("Int", &value, span));
            };
            let payload_bytes = super::estimated_bigint_bytes(&value);
            self.runtime.ensure_memory_budget(
                payload_bytes.saturating_add(payload_bytes.saturating_add(1)),
                span,
            )?;
            self.runtime.push_value(Value::Int(!value), span)?;
            return Ok(());
        }
        let right = self.runtime.pop(span)?;
        let left = self.runtime.pop(span)?;
        let left = match left {
            Value::Int(value) => value,
            other => return Err(self.runtime.type_error("Int", &other, span)),
        };
        let right = match right {
            Value::Int(value) => value,
            other => return Err(self.runtime.type_error("Int", &other, span)),
        };
        let operand_bytes = super::estimated_bigint_bytes(&left)
            .saturating_add(super::estimated_bigint_bytes(&right));
        if matches!(operation, IrOp::BitAnd | IrOp::BitOr | IrOp::BitXor) {
            let result_bits = left.bits().max(right.bits()).saturating_add(1);
            let result_bytes =
                usize::try_from(result_bits.saturating_add(7) / 8).unwrap_or(usize::MAX);
            self.runtime
                .ensure_memory_budget(operand_bytes.saturating_add(result_bytes), span)?;
        }
        let result = match operation {
            IrOp::BitAnd => &left & &right,
            IrOp::BitOr => &left | &right,
            IrOp::BitXor => &left ^ &right,
            IrOp::Shl | IrOp::Shr => {
                if right.is_negative() {
                    return Err(self.runtime.error(
                        RuntimeErrorKind::TypeError {
                            expected: "nonnegative shift count",
                            found: "Int",
                        },
                        span,
                    ));
                }
                let Some(count) = right.to_usize() else {
                    return Err(self.runtime.error(RuntimeErrorKind::ExecutionLimit, span));
                };
                let limit = self.policy.limits.max_file_bytes.saturating_mul(8);
                if count > limit {
                    if operation == IrOp::Shr {
                        self.runtime.ensure_memory_budget(operand_bytes, span)?;
                        self.runtime.push_value(
                            Value::Int(if left.is_negative() {
                                BigInt::from(-1)
                            } else {
                                BigInt::zero()
                            }),
                            span,
                        )?;
                        return Ok(());
                    }
                    return Err(self.runtime.error(RuntimeErrorKind::ExecutionLimit, span));
                }
                if operation == IrOp::Shl {
                    let result_bits = left
                        .bits()
                        .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
                    let result_bytes =
                        usize::try_from(result_bits.saturating_add(7) / 8).unwrap_or(usize::MAX);
                    self.runtime
                        .ensure_memory_budget(operand_bytes.saturating_add(result_bytes), span)?;
                    &left << count
                } else {
                    self.runtime.ensure_memory_budget(operand_bytes, span)?;
                    &left >> count
                }
            }
            _ => return Err(self.unsupported(span, "invalid bitwise intrinsic")),
        };
        self.runtime.push_value(Value::Int(result), span)?;
        Ok(())
    }
}

fn is_decimal_float(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut index = usize::from(matches!(bytes.first(), Some(b'+') | Some(b'-')));
    let mut integer_digits = 0;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        integer_digits += 1;
        index += 1;
    }
    let mut fraction_digits = 0;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            fraction_digits += 1;
            index += 1;
        }
    }
    if integer_digits + fraction_digits == 0 {
        return false;
    }
    if matches!(bytes.get(index), Some(b'e') | Some(b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+') | Some(b'-')) {
            index += 1;
        }
        let exponent_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == exponent_start {
            return false;
        }
    }
    index == bytes.len()
}

#[derive(Clone, Debug)]
enum Constant {
    Value(Value),
    FunctionRef(u32),
    TypeRef,
}

#[derive(Clone, Debug)]
struct FunctionRecord {
    module_index: u32,
    id: Option<BigInt>,
    asynchronous: bool,
    initializer: bool,
    parameter_ids: Vec<BigInt>,
    rest_parameter_code: u32,
    max_stack: u32,
    code_offset: usize,
    code_length: usize,
}

#[derive(Clone, Debug)]
struct ModuleRecord {
    initializer_index: u32,
    has_imports: bool,
}

struct Image<'a> {
    code: &'a [u8],
    constants: Vec<Constant>,
    functions: Vec<FunctionRecord>,
    modules: Vec<ModuleRecord>,
    exceptions: Vec<ExceptionRegion>,
    entry_module: u32,
    source_spans: HashMap<(usize, usize), Span>,
}

#[derive(Clone, Debug)]
struct ExceptionRegion {
    function_index: u32,
    protected_start: usize,
    protected_end: usize,
    handler_offset: usize,
    handler_end: usize,
    stack_depth: u32,
}

#[derive(Clone, Debug)]
struct PendingException {
    handler_start: usize,
    handler_end: usize,
    error: RuntimeError,
}

impl<'a> Image<'a> {
    fn parse(program: &VerifiedProgram<'a>) -> Result<Self, String> {
        let bytes = program.bytes();
        if program.module_count != 1 {
            return Err("the runtime slice supports one module".into());
        }
        let sections = sections(bytes)?;
        let constants = parse_constants(section(bytes, &sections, 1)?)?;
        let (modules, entry_module) = parse_modules(section(bytes, &sections, 2)?)?;
        if modules.iter().any(|module| module.has_imports) {
            return Err("module imports are not implemented by this runtime slice".into());
        }
        let functions = parse_functions(section(bytes, &sections, 3)?, &constants)?;
        let exceptions = parse_exceptions(section(bytes, &sections, 6)?)?;
        let code = section(bytes, &sections, 4)?;
        let entry_function = functions
            .get(modules[entry_module as usize].initializer_index as usize)
            .ok_or("entry initializer index out of range")?;
        if !entry_function.initializer {
            return Err("entry module does not point to its initializer".into());
        }
        let source_spans = match sections.get(&7) {
            Some(section) => parse_debug(section_slice(bytes, *section)?, code.len())?,
            None => HashMap::new(),
        };
        if functions.len() != program.function_count as usize {
            return Err("verified function count does not match FunctionTable".into());
        }
        Ok(Self {
            code,
            constants,
            functions,
            modules,
            exceptions,
            entry_module,
            source_spans,
        })
    }

    fn span(&self, function_index: usize, code_offset: usize) -> Span {
        self.source_spans
            .get(&(function_index, code_offset))
            .copied()
            .unwrap_or_else(empty_span)
    }
}

#[derive(Clone, Debug)]
struct Frame {
    function_index: usize,
    function: FunctionRecord,
    module_index: u32,
    ip: usize,
    stack_base: usize,
    scope_floor: usize,
    base_scope_depth: usize,
    initializer: bool,
    return_action: ReturnAction,
    loop_states: HashMap<u32, LoopState>,
    call_site_offset: usize,
    pending_exceptions: Vec<PendingException>,
    active_exceptions: HashMap<usize, super::ExecutionSnapshot>,
}

impl Frame {
    fn initializer(function_index: usize, function: FunctionRecord) -> Self {
        Self {
            function_index,
            module_index: function.module_index,
            ip: 0,
            stack_base: 0,
            scope_floor: 1,
            base_scope_depth: 1,
            initializer: true,
            return_action: ReturnAction::PushResult,
            loop_states: HashMap::new(),
            call_site_offset: 0,
            pending_exceptions: Vec::new(),
            active_exceptions: HashMap::new(),
            function,
        }
    }

    fn function(
        function_index: usize,
        function: FunctionRecord,
        stack_base: usize,
        scope_floor: usize,
        base_scope_depth: usize,
        return_action: ReturnAction,
        call_site_offset: usize,
    ) -> Self {
        Self {
            function_index,
            module_index: function.module_index,
            ip: 0,
            stack_base,
            scope_floor,
            base_scope_depth,
            initializer: false,
            return_action,
            loop_states: HashMap::new(),
            call_site_offset,
            pending_exceptions: Vec::new(),
            active_exceptions: HashMap::new(),
            function,
        }
    }
}

fn frame_capacity_after_push(
    current_capacity: usize,
    current_length: usize,
    maximum_frames: usize,
) -> Option<usize> {
    let required = current_length.checked_add(1)?;
    if required > maximum_frames {
        return None;
    }
    if required <= current_capacity {
        return Some(current_capacity);
    }
    Some(
        current_capacity
            .saturating_mul(2)
            .max(required)
            .min(maximum_frames),
    )
}

fn function_record_payload_bytes(function: &FunctionRecord) -> usize {
    function
        .parameter_ids
        .capacity()
        .saturating_mul(size_of::<BigInt>())
        .saturating_add(
            function
                .id
                .as_ref()
                .map(super::estimated_bigint_bytes)
                .unwrap_or(0),
        )
        .saturating_add(
            function
                .parameter_ids
                .iter()
                .map(super::estimated_bigint_bytes)
                .fold(0usize, usize::saturating_add),
        )
}

fn snapshot_payload_bytes(snapshot: &super::ExecutionSnapshot) -> usize {
    snapshot
        .stack
        .capacity()
        .saturating_mul(size_of::<Value>())
        .saturating_add(super::estimate_cloned_value_payloads(&snapshot.stack))
        .saturating_add(
            snapshot
                .capture_lengths
                .capacity()
                .saturating_mul(size_of::<usize>()),
        )
}

fn pending_exception_bytes(exception: &PendingException) -> usize {
    exception
        .error
        .stack_trace
        .capacity()
        .saturating_mul(size_of::<super::StackTraceFrame>())
        .saturating_add(
            exception
                .error
                .stack_trace
                .iter()
                .map(|frame| super::estimated_bigint_bytes(&frame.function_id))
                .fold(0usize, usize::saturating_add),
        )
}

fn frame_storage_bytes(frames: &[Frame], capacity: usize) -> usize {
    frames.iter().fold(
        capacity.saturating_mul(size_of::<Frame>()),
        |total, frame| {
            let loop_bytes = frame
                .loop_states
                .capacity()
                .saturating_mul(size_of::<(u32, LoopState)>() + 2 * size_of::<usize>())
                .saturating_add(
                    frame
                        .loop_states
                        .values()
                        .map(|state| super::estimated_bigint_bytes(&state.variable_id))
                        .fold(0usize, usize::saturating_add),
                );
            let pending_bytes = frame
                .pending_exceptions
                .capacity()
                .saturating_mul(size_of::<PendingException>())
                .saturating_add(
                    frame
                        .pending_exceptions
                        .iter()
                        .map(pending_exception_bytes)
                        .fold(0usize, usize::saturating_add),
                );
            let active_bytes = frame
                .active_exceptions
                .capacity()
                .saturating_mul(
                    size_of::<(usize, super::ExecutionSnapshot)>() + 2 * size_of::<usize>(),
                )
                .saturating_add(
                    frame
                        .active_exceptions
                        .values()
                        .map(snapshot_payload_bytes)
                        .fold(0usize, usize::saturating_add),
                );
            let return_payload = match &frame.return_action {
                ReturnAction::PushResult => 0,
                ReturnAction::PushValue(value) => {
                    super::estimated_unmanaged_payload_bytes(value, 0)
                }
            };
            total
                .saturating_add(function_record_payload_bytes(&frame.function))
                .saturating_add(loop_bytes)
                .saturating_add(pending_bytes)
                .saturating_add(active_bytes)
                .saturating_add(return_payload)
        },
    )
}

#[derive(Clone, Debug)]
struct LoopState {
    iterator: Rc<RefCell<IteratorState>>,
    variable_id: BigInt,
    scope_depth: usize,
}

struct BytecodeTask {
    request: Option<CallRequest>,
    status: BytecodeTaskStatus,
}

enum BytecodeTaskStatus {
    Pending,
    Running,
    Completed(Value),
    Failed(RuntimeError),
    Cancelled,
}

enum BytecodeTaskStatusKind {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

struct CallRequest {
    module_index: u32,
    function_id: BigInt,
    arguments: Vec<Value>,
    captures: Vec<(BigInt, BindingCell)>,
    return_action: ReturnAction,
}

#[derive(Clone, Debug)]
enum ReturnAction {
    PushResult,
    PushValue(Value),
}

fn bytecode_task_memory_bytes(
    tasks: &HashMap<u64, BytecodeTask>,
    task_capacity: usize,
    ready_capacity: usize,
) -> usize {
    let entries = task_capacity
        .saturating_mul(size_of::<(u64, BytecodeTask)>())
        .saturating_add(ready_capacity.saturating_mul(size_of::<u64>()));
    tasks.iter().fold(entries, |total, (id, task)| {
        let request_bytes = task
            .request
            .as_ref()
            .map(call_request_memory_bytes)
            .unwrap_or(0);
        let status_bytes = match &task.status {
            BytecodeTaskStatus::Completed(value) => size_of::<Value>()
                .saturating_add(super::estimated_unmanaged_payload_bytes(value, 0)),
            BytecodeTaskStatus::Failed(error) => super::estimated_runtime_error_bytes(error),
            BytecodeTaskStatus::Pending
            | BytecodeTaskStatus::Running
            | BytecodeTaskStatus::Cancelled => 0,
        };
        total
            .saturating_add(size_of::<BytecodeTask>())
            .saturating_add(size_of::<u64>())
            .saturating_add(super::estimated_bigint_bytes(&BigInt::from(*id)))
            .saturating_add(request_bytes)
            .saturating_add(status_bytes)
    })
}

fn call_request_memory_bytes(request: &CallRequest) -> usize {
    request
        .arguments
        .capacity()
        .saturating_mul(size_of::<Value>())
        + super::estimate_cloned_value_payloads(&request.arguments)
        + request
            .captures
            .capacity()
            .saturating_mul(size_of::<(BigInt, BindingCell)>())
        + request
            .captures
            .iter()
            .map(|(id, _)| super::estimated_bigint_bytes(id))
            .fold(0usize, usize::saturating_add)
        + super::estimated_bigint_bytes(&request.function_id)
        + match &request.return_action {
            ReturnAction::PushResult => 0,
            ReturnAction::PushValue(value) => super::estimated_unmanaged_payload_bytes(value, 0),
        }
}

enum Instruction {
    PushConstant(u32),
    Intrinsic(u16),
    Jump {
        target: usize,
        unwind: u32,
    },
    BranchIfFalse {
        false_target: usize,
        true_target: usize,
    },
    ForInit(u32),
    ForNext {
        loop_id: u32,
        body_target: usize,
        exit_target: usize,
    },
    ScopeEnter,
    ScopeExit,
    ReturnTop(u32),
    Trap,
    Rethrow,
    Halt,
}

#[derive(Clone, Copy)]
struct Section {
    offset: usize,
    length: usize,
}

fn sections(bytes: &[u8]) -> Result<HashMap<u32, Section>, String> {
    let mut reader = Cursor::new(bytes);
    reader.skip(12)?;
    let count = reader.u32()? as usize;
    let mut entries = HashMap::with_capacity(count);
    for _ in 0..count {
        let id = reader.u32()?;
        reader.skip(4)?;
        let offset = usize::try_from(reader.u64()?).map_err(|_| "section offset overflow")?;
        let length = usize::try_from(reader.u64()?).map_err(|_| "section length overflow")?;
        entries.insert(id, Section { offset, length });
    }
    Ok(entries)
}

fn section<'a>(
    bytes: &'a [u8],
    sections: &HashMap<u32, Section>,
    id: u32,
) -> Result<&'a [u8], String> {
    let section = sections
        .get(&id)
        .copied()
        .ok_or("required section is missing")?;
    section_slice(bytes, section)
}

fn section_slice(bytes: &[u8], section: Section) -> Result<&[u8], String> {
    let end = section
        .offset
        .checked_add(section.length)
        .ok_or("section range overflow")?;
    bytes
        .get(section.offset..end)
        .ok_or_else(|| "section exceeds file bounds".into())
}

fn parse_constants(bytes: &[u8]) -> Result<Vec<Constant>, String> {
    let mut reader = Cursor::new(bytes);
    let count = reader.count()?;
    let mut constants = Vec::with_capacity(count);
    for _ in 0..count {
        let tag = reader.u8()?;
        let payload_length = reader
            .uleb128()?
            .to_usize()
            .ok_or("constant payload length overflow")?;
        let mut payload = Cursor::new(reader.take(payload_length)?);
        let constant = match tag {
            0 => Constant::Value(Value::Null),
            1 => Constant::Value(Value::Bool(match payload.u8()? {
                0 => false,
                1 => true,
                _ => return Err("invalid boolean constant".into()),
            })),
            2 => Constant::Value(Value::Int(payload.sleb128()?)),
            3 => Constant::Value(Value::Float(f64::from_bits(payload.u64()?))),
            4 => {
                let value = std::str::from_utf8(payload.take(payload.bytes.len())?)
                    .map_err(|_| "invalid UTF-8 string constant")?
                    .to_owned();
                Constant::Value(Value::String(value))
            }
            5 => Constant::Value(Value::Bytes(payload.take(payload.bytes.len())?.to_vec())),
            6 => {
                let scalar = payload
                    .uleb128()?
                    .to_u32()
                    .ok_or("character scalar overflow")?;
                let value = char::from_u32(scalar).ok_or("invalid Unicode scalar constant")?;
                Constant::Value(Value::Char(value))
            }
            7 => Constant::FunctionRef(payload.u32()?),
            8 => {
                payload.u32()?;
                Constant::TypeRef
            }
            _ => return Err("unknown constant tag".into()),
        };
        payload.finish("constant payload")?;
        constants.push(constant);
    }
    reader.finish("ConstantPool")?;
    Ok(constants)
}

fn parse_exceptions(bytes: &[u8]) -> Result<Vec<ExceptionRegion>, String> {
    let mut reader = Cursor::new(bytes);
    let count = reader.count()?;
    let mut regions = Vec::with_capacity(count);
    for _ in 0..count {
        let function_index = reader.u32()?;
        let protected_start =
            usize::try_from(reader.u64()?).map_err(|_| "exception start offset overflow")?;
        let protected_end =
            usize::try_from(reader.u64()?).map_err(|_| "exception end offset overflow")?;
        let handler_offset =
            usize::try_from(reader.u64()?).map_err(|_| "exception handler offset overflow")?;
        let handler_end =
            usize::try_from(reader.u64()?).map_err(|_| "exception handler end overflow")?;
        let stack_depth = reader.u32()?;
        let type_constant_index = reader.u32()?;
        let flags = reader.u32()?;
        if type_constant_index != u32::MAX {
            return Err("typed exception handlers are not supported by this runtime slice".into());
        }
        if flags != 0 {
            return Err("exception flags must be zero".into());
        }
        regions.push(ExceptionRegion {
            function_index,
            protected_start,
            protected_end,
            handler_offset,
            handler_end,
            stack_depth,
        });
    }
    reader.finish("ExceptionTable")?;
    Ok(regions)
}

fn parse_modules(bytes: &[u8]) -> Result<(Vec<ModuleRecord>, u32), String> {
    let mut reader = Cursor::new(bytes);
    let count = reader.count()?;
    let entry = reader.u32()?;
    let mut modules = Vec::with_capacity(count);
    for _ in 0..count {
        reader.u32()?; // name constant
        let initializer_index = reader.u32()?;
        reader.u32()?; // first function
        reader.u32()?; // function count
        reader.u32()?; // flags
        let globals = reader.count()?;
        reader.skip(globals.checked_mul(8).ok_or("global table overflow")?)?;
        let import_count = reader.count()?;
        reader.skip(
            import_count
                .checked_mul(16)
                .ok_or("import table overflow")?,
        )?;
        let export_count = reader.count()?;
        reader.skip(
            export_count
                .checked_mul(12)
                .ok_or("export table overflow")?,
        )?;
        modules.push(ModuleRecord {
            initializer_index,
            has_imports: import_count != 0,
        });
    }
    reader.finish("ModuleTable")?;
    if entry as usize >= modules.len() {
        return Err("entry module index out of range".into());
    }
    Ok((modules, entry))
}

fn parse_functions(bytes: &[u8], constants: &[Constant]) -> Result<Vec<FunctionRecord>, String> {
    let mut reader = Cursor::new(bytes);
    let count = reader.count()?;
    let mut functions = Vec::with_capacity(count);
    for _ in 0..count {
        let module_index = reader.u32()?;
        let id_constant = reader.u32()?;
        let _name_constant = reader.u32()?;
        let flags = reader.u32()?;
        let parameter_count = reader.count()?;
        let rest_parameter_code = reader.u32()?;
        let mut parameter_ids = Vec::with_capacity(parameter_count);
        for _ in 0..parameter_count {
            parameter_ids.push(constant_integer(constants, reader.u32()?)?);
        }
        let max_stack = reader.u32()?;
        let code_offset =
            usize::try_from(reader.u64()?).map_err(|_| "function code offset overflow")?;
        let code_length =
            usize::try_from(reader.u64()?).map_err(|_| "function code length overflow")?;
        let id = if id_constant == u32::MAX {
            None
        } else {
            Some(constant_integer(constants, id_constant)?)
        };
        functions.push(FunctionRecord {
            module_index,
            id,
            asynchronous: flags & 1 != 0,
            initializer: flags & 2 != 0,
            parameter_ids,
            rest_parameter_code,
            max_stack,
            code_offset,
            code_length,
        });
    }
    reader.finish("FunctionTable")?;
    Ok(functions)
}

fn constant_integer(constants: &[Constant], index: u32) -> Result<BigInt, String> {
    match constants.get(index as usize) {
        Some(Constant::Value(Value::Int(value))) => Ok(value.clone()),
        _ => Err("integer constant reference is invalid".into()),
    }
}

fn parse_debug(bytes: &[u8], code_length: usize) -> Result<HashMap<(usize, usize), Span>, String> {
    let mut reader = Cursor::new(bytes);
    let source_count = reader.count()?;
    reader.skip(
        source_count
            .checked_mul(4)
            .ok_or("source file table overflow")?,
    )?;
    let count = reader.count()?;
    let mut spans = HashMap::with_capacity(count);
    for _ in 0..count {
        let function_index = reader.u32()? as usize;
        let offset = usize::try_from(reader.u64()?).map_err(|_| "debug offset overflow")?;
        if offset >= code_length {
            return Err("debug offset is outside the Code section".into());
        }
        reader.u32()?; // source file index, validated by verifier
        let start_byte_offset =
            usize::try_from(reader.u64()?).map_err(|_| "source byte offset overflow")?;
        let end_byte_offset =
            usize::try_from(reader.u64()?).map_err(|_| "source byte offset overflow")?;
        let start_line = reader.u32()? as usize;
        let start_column = reader.u32()? as usize;
        let end_line = reader.u32()? as usize;
        let end_column = reader.u32()? as usize;
        let start = SourcePosition {
            byte_offset: start_byte_offset,
            line: start_line,
            column: start_column,
        };
        let end = SourcePosition {
            byte_offset: end_byte_offset,
            line: end_line,
            column: end_column,
        };
        spans.insert((function_index, offset), Span { start, end });
    }
    reader.finish("DebugInfo")?;
    Ok(spans)
}

fn decode_instruction<R: BufRead, W: Write>(
    image: &Image<'_>,
    frame: &mut Frame,
    span: Span,
    runtime: &Interpreter<R, W>,
) -> Result<Instruction, RuntimeError> {
    let start = frame.function.code_offset + frame.ip;
    let code_end = frame.function.code_offset + frame.function.code_length;
    let code = image.code.get(start..code_end).ok_or_else(|| {
        runtime.error(
            RuntimeErrorKind::CompileError("function code range is invalid".into()),
            span,
        )
    })?;
    let mut reader = Cursor::new(code);
    let opcode = reader
        .u8()
        .map_err(|message| runtime.error(RuntimeErrorKind::CompileError(message), span))?;
    let instruction =
        match opcode {
            0x01 => {
                Instruction::PushConstant(reader.u32().map_err(|message| {
                    runtime.error(RuntimeErrorKind::CompileError(message), span)
                })?)
            }
            0xE0 => {
                Instruction::Intrinsic(reader.u16().map_err(|message| {
                    runtime.error(RuntimeErrorKind::CompileError(message), span)
                })?)
            }
            0xE1 => Instruction::Jump {
                target: reader.u32().map_err(|message| {
                    runtime.error(RuntimeErrorKind::CompileError(message), span)
                })? as usize,
                unwind: reader.u32().map_err(|message| {
                    runtime.error(RuntimeErrorKind::CompileError(message), span)
                })?,
            },
            0xE2 => Instruction::BranchIfFalse {
                false_target: reader.u32().map_err(|message| {
                    runtime.error(RuntimeErrorKind::CompileError(message), span)
                })? as usize,
                true_target: reader.u32().map_err(|message| {
                    runtime.error(RuntimeErrorKind::CompileError(message), span)
                })? as usize,
            },
            0xE3 => {
                Instruction::ForInit(reader.u32().map_err(|message| {
                    runtime.error(RuntimeErrorKind::CompileError(message), span)
                })?)
            }
            0xE4 => Instruction::ForNext {
                loop_id: reader.u32().map_err(|message| {
                    runtime.error(RuntimeErrorKind::CompileError(message), span)
                })?,
                body_target: reader.u32().map_err(|message| {
                    runtime.error(RuntimeErrorKind::CompileError(message), span)
                })? as usize,
                exit_target: reader.u32().map_err(|message| {
                    runtime.error(RuntimeErrorKind::CompileError(message), span)
                })? as usize,
            },
            0xE5 => Instruction::ScopeEnter,
            0xE6 => Instruction::ScopeExit,
            0xE7 => {
                Instruction::ReturnTop(reader.u32().map_err(|message| {
                    runtime.error(RuntimeErrorKind::CompileError(message), span)
                })?)
            }
            0xE8 => Instruction::Trap,
            0xE9 => Instruction::Rethrow,
            0xFF => Instruction::Halt,
            _ => {
                return Err(runtime.error(
                    RuntimeErrorKind::CompileError(format!("unknown VM opcode 0x{opcode:02X}")),
                    span,
                ));
            }
        };
    frame.ip = frame.ip.checked_add(reader.offset).ok_or_else(|| {
        runtime.error(
            RuntimeErrorKind::CompileError("instruction pointer overflow".into()),
            span,
        )
    })?;
    Ok(instruction)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or("byte range overflow")?;
        let result = self
            .bytes
            .get(self.offset..end)
            .ok_or("unexpected end of section")?;
        self.offset = end;
        Ok(result)
    }

    fn skip(&mut self, count: usize) -> Result<(), String> {
        self.take(count).map(|_| ())
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn count(&mut self) -> Result<usize, String> {
        usize::try_from(self.u32()?).map_err(|_| "count overflow".into())
    }

    fn uleb128(&mut self) -> Result<BigInt, String> {
        let mut value = BigInt::zero();
        let mut shift = 0usize;
        loop {
            let byte = self.u8()?;
            value |= BigInt::from(byte & 0x7f) << shift;
            shift = shift.checked_add(7).ok_or("ULEB128 shift overflow")?;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
    }

    fn sleb128(&mut self) -> Result<BigInt, String> {
        let mut value = BigInt::zero();
        let mut shift = 0usize;
        let last;
        loop {
            let byte = self.u8()?;
            value |= BigInt::from(byte & 0x7f) << shift;
            shift = shift.checked_add(7).ok_or("SLEB128 shift overflow")?;
            if byte & 0x80 == 0 {
                last = byte;
                break;
            }
        }
        if last & 0x40 != 0 {
            value -= BigInt::from(1u8) << shift;
        }
        Ok(value)
    }

    fn finish(&self, section: &str) -> Result<(), String> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(format!("trailing bytes in {section}"))
        }
    }
}
