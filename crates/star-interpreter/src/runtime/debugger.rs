//! Host-controlled source debugging for the reference interpreter.

use std::io::{BufRead, Write};

use num_bigint::BigInt;
use star_common::SourcePosition;
use star_parser::{Program, Span};

use crate::value::{BindingCell, Value};

use super::{Interpreter, RuntimeError, RuntimeErrorKind, StackTraceFrame};

const MAX_INSPECTED_VALUES: usize = 256;
const MAX_CALL_FRAMES: usize = 256;
const MAX_PREVIEW_CHARS: usize = 96;

/// A source breakpoint, using the parser's one-based line and column values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DebugBreakpoint {
    /// One-based source line.
    pub line: usize,
    /// Optional one-based source column. `None` matches any column on the line.
    pub column: Option<usize>,
}

/// Initial debugger behavior and source breakpoints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebugOptions {
    /// Pause before the first executable statement.
    pub pause_on_entry: bool,
    /// Breakpoints in the source submitted to `execute_debug`.
    pub breakpoints: Vec<DebugBreakpoint>,
}

impl Default for DebugOptions {
    fn default() -> Self {
        Self {
            pause_on_entry: true,
            breakpoints: Vec::new(),
        }
    }
}

/// Why the interpreter paused before a source statement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DebugStopReason {
    /// The first executable statement is about to run.
    Entry,
    /// A configured breakpoint matches this statement.
    Breakpoint,
    /// A stepping command reached its next stop.
    Step,
}

/// The action to take after inspecting a stopped interpreter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DebugCommand {
    /// Resume until a breakpoint or program completion.
    Continue,
    /// Stop before the next executed source statement.
    StepInto,
    /// Stop at the next statement at this or a shallower execution depth.
    StepOver,
    /// Stop after returning to a shallower execution depth.
    StepOut,
    /// End this debug execution.
    Quit,
}

/// A bounded, display-ready summary of one runtime value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebugValue {
    /// Runtime type name.
    pub type_name: &'static str,
    /// Shallow preview that does not recursively traverse guest collections.
    pub preview: String,
}

/// One inspected local or global binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebugBinding {
    /// Numeric source binding identifier, shortened when unusually large.
    pub id: String,
    /// Lexical scope depth, with globals at depth zero.
    pub scope_depth: usize,
    /// Whether the binding can be reassigned.
    pub mutable: bool,
    /// Current value summary.
    pub value: DebugValue,
}

/// A source-interpreter call frame shown by stack inspection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebugCallFrame {
    /// Numeric function identifier, or `None` for the top-level program.
    pub function_id: Option<String>,
    /// Source location of the call that entered this function.
    pub call_site: Option<Span>,
}

/// State captured at a debugger stop. Value and collection previews are bounded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebugSnapshot {
    /// Source path or display name passed to `execute_debug`.
    pub source_name: String,
    /// Original source span of the statement about to execute.
    pub location: Span,
    /// Call frames in outermost-to-innermost order.
    pub call_stack: Vec<DebugCallFrame>,
    /// Operand stack in bottom-to-top order, limited to its last 256 entries.
    pub operand_stack: Vec<DebugValue>,
    /// Module-global bindings.
    pub globals: Vec<DebugBinding>,
    /// Bindings in active lexical scopes, excluding globals.
    pub locals: Vec<DebugBinding>,
    /// Whether older operand-stack entries were omitted.
    pub operand_stack_truncated: bool,
    /// Whether bindings beyond the inspection limit were omitted.
    pub bindings_truncated: bool,
    /// Whether older call frames were omitted.
    pub call_stack_truncated: bool,
}

/// Receives debugger stops and chooses how execution should resume.
pub trait DebugController {
    /// Inspect a bounded state snapshot and return the next debugger action.
    fn on_stop(&mut self, reason: DebugStopReason, snapshot: &DebugSnapshot) -> DebugCommand;
}

/// Whether a debug execution completed or was explicitly quit by its controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DebugExecution {
    /// The program ran to completion.
    Completed,
    /// The controller ended execution at a debugger stop.
    Quit,
}

#[derive(Clone, Copy, Debug)]
enum StepPlan {
    Into { after: u64 },
    Over { after: u64, depth: usize },
    Out { after: u64, depth: usize },
}

pub(super) struct DebugSession {
    source_name: String,
    options: DebugOptions,
    controller: Box<dyn DebugController>,
    started: bool,
    sequence: u64,
    step_plan: Option<StepPlan>,
}

impl DebugSession {
    pub(super) fn new(
        source_name: String,
        options: DebugOptions,
        controller: Box<dyn DebugController>,
    ) -> Self {
        Self {
            source_name,
            options,
            controller,
            started: false,
            sequence: 0,
            step_plan: None,
        }
    }

    pub(super) fn should_stop(
        &mut self,
        position: SourcePosition,
        depth: usize,
    ) -> Option<DebugStopReason> {
        self.sequence = self.sequence.saturating_add(1);
        if !self.started {
            self.started = true;
            if self.options.pause_on_entry {
                return Some(DebugStopReason::Entry);
            }
        }
        if self.options.breakpoints.iter().any(|breakpoint| {
            breakpoint.line == position.line
                && breakpoint
                    .column
                    .map_or(true, |column| column == position.column)
        }) {
            return Some(DebugStopReason::Breakpoint);
        }
        match self.step_plan {
            Some(StepPlan::Into { after }) if self.sequence > after => Some(DebugStopReason::Step),
            Some(StepPlan::Over {
                after,
                depth: origin,
            }) if self.sequence > after && depth <= origin => Some(DebugStopReason::Step),
            Some(StepPlan::Out {
                after,
                depth: origin,
            }) if self.sequence > after && depth < origin => Some(DebugStopReason::Step),
            _ => None,
        }
    }

    pub(super) fn stop(
        &mut self,
        reason: DebugStopReason,
        snapshot: &DebugSnapshot,
        depth: usize,
    ) -> DebugCommand {
        let command = self.controller.on_stop(reason, snapshot);
        self.step_plan = match command {
            DebugCommand::Continue | DebugCommand::Quit => None,
            DebugCommand::StepInto => Some(StepPlan::Into {
                after: self.sequence,
            }),
            DebugCommand::StepOver => Some(StepPlan::Over {
                after: self.sequence,
                depth,
            }),
            DebugCommand::StepOut => Some(StepPlan::Out {
                after: self.sequence,
                depth,
            }),
        };
        command
    }

    pub(super) fn snapshot<R: BufRead, W: Write>(
        &self,
        interpreter: &Interpreter<R, W>,
        location: Span,
    ) -> DebugSnapshot {
        let stack_start = interpreter.stack.len().saturating_sub(MAX_INSPECTED_VALUES);
        let operand_stack = interpreter.stack[stack_start..]
            .iter()
            .map(debug_value)
            .collect();

        let mut bindings_truncated = false;
        let globals = interpreter
            .scopes
            .first()
            .map(|scope| debug_bindings(scope, 0, &mut bindings_truncated))
            .unwrap_or_default();
        let mut locals = Vec::new();
        for (scope_depth, scope) in interpreter.scopes.iter().enumerate().skip(1) {
            let mut remaining = MAX_INSPECTED_VALUES.saturating_sub(locals.len());
            if remaining == 0 {
                bindings_truncated = true;
                break;
            }
            let mut truncated = false;
            let mut entries = debug_bindings_limited(scope, scope_depth, remaining, &mut truncated);
            locals.append(&mut entries);
            if truncated {
                bindings_truncated = true;
            }
            remaining = MAX_INSPECTED_VALUES.saturating_sub(locals.len());
            if remaining == 0 && scope_depth + 1 < interpreter.scopes.len() {
                bindings_truncated = true;
                break;
            }
        }

        let call_stack_start = interpreter
            .call_trace
            .len()
            .saturating_sub(MAX_CALL_FRAMES.saturating_sub(1));
        let mut call_stack: Vec<_> = interpreter.call_trace[call_stack_start..]
            .iter()
            .map(debug_call_frame)
            .collect();
        let call_stack_truncated = call_stack_start > 0;
        call_stack.insert(
            0,
            DebugCallFrame {
                function_id: None,
                call_site: None,
            },
        );

        DebugSnapshot {
            source_name: self.source_name.clone(),
            location,
            call_stack,
            operand_stack,
            globals,
            locals,
            operand_stack_truncated: stack_start > 0,
            bindings_truncated,
            call_stack_truncated,
        }
    }
}

impl<R: BufRead, W: Write> Interpreter<R, W> {
    /// Executes a source program under a host-controlled breakpoint and stepping session.
    pub fn execute_debug(
        &mut self,
        program: &Program,
        source_name: impl Into<String>,
        options: DebugOptions,
        controller: Box<dyn DebugController>,
    ) -> Result<DebugExecution, RuntimeError> {
        self.debug_session = Some(DebugSession::new(source_name.into(), options, controller));
        self.debug_nesting = 0;
        let result = self.execute(program);
        self.debug_session = None;
        self.debug_nesting = 0;
        match result {
            Ok(()) => Ok(DebugExecution::Completed),
            Err(error) if error.kind == RuntimeErrorKind::DebuggerStopped => {
                Ok(DebugExecution::Quit)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn debug_before_statement(&mut self, span: Span) -> Result<(), RuntimeError> {
        let depth = self.call_depth.saturating_add(self.debug_nesting);
        let reason = self
            .debug_session
            .as_mut()
            .and_then(|session| session.should_stop(span.start, depth));
        let Some(reason) = reason else {
            return Ok(());
        };
        let snapshot = self
            .debug_session
            .as_ref()
            .expect("debug session remains installed while executing")
            .snapshot(self, span);
        let command = self
            .debug_session
            .as_mut()
            .expect("debug session remains installed while executing")
            .stop(reason, &snapshot, depth);
        if command == DebugCommand::Quit {
            return Err(self.error(RuntimeErrorKind::DebuggerStopped, span));
        }
        Ok(())
    }
}

fn debug_call_frame(frame: &StackTraceFrame) -> DebugCallFrame {
    DebugCallFrame {
        function_id: Some(format_identifier(&frame.function_id)),
        call_site: Some(frame.call_site),
    }
}

fn debug_bindings(
    scope: &std::collections::HashMap<BigInt, BindingCell>,
    scope_depth: usize,
    truncated: &mut bool,
) -> Vec<DebugBinding> {
    debug_bindings_limited(scope, scope_depth, MAX_INSPECTED_VALUES, truncated)
}

fn debug_bindings_limited(
    scope: &std::collections::HashMap<BigInt, BindingCell>,
    scope_depth: usize,
    limit: usize,
    truncated: &mut bool,
) -> Vec<DebugBinding> {
    let mut entries = Vec::with_capacity(limit.min(scope.len()));
    for (id, cell) in scope.iter().take(limit) {
        let Ok(binding) = cell.try_borrow() else {
            entries.push(DebugBinding {
                id: format_identifier(id),
                scope_depth,
                mutable: false,
                value: DebugValue {
                    type_name: "Borrowed",
                    preview: "<binding is mutably borrowed>".to_owned(),
                },
            });
            continue;
        };
        entries.push(DebugBinding {
            id: format_identifier(id),
            scope_depth,
            mutable: binding.mutable,
            value: debug_value(&binding.value),
        });
    }
    if scope.len() > limit {
        *truncated = true;
    }
    entries.sort_by(|left, right| left.id.cmp(&right.id));
    entries
}

fn debug_value(value: &Value) -> DebugValue {
    let preview = match value {
        Value::Int(value) if value.bits() <= 128 => value.to_string(),
        Value::Int(value) => format!("<{} bits>", value.bits()),
        Value::Float(value) => format!("{value:?}"),
        Value::Bool(value) => value.to_string(),
        Value::String(value) => debug_string_preview(value),
        Value::Char(value) => format!("{value:?}"),
        Value::Byte(value) => value.to_string(),
        Value::Bytes(value) => format!("<{} bytes>", value.len()),
        Value::Array(values) => values
            .try_borrow()
            .map(|values| format!("Array(len={})", values.len()))
            .unwrap_or_else(|_| "Array(<borrowed>)".to_owned()),
        Value::Tuple(tuple) => tuple
            .values
            .try_borrow()
            .map(|values| format!("Tuple(len={})", values.len()))
            .unwrap_or_else(|_| "Tuple(<borrowed>)".to_owned()),
        Value::Map(map) => map
            .try_borrow()
            .map(|map| format!("Map(len={})", map.order.len()))
            .unwrap_or_else(|_| "Map(<borrowed>)".to_owned()),
        Value::Set(set) => set
            .try_borrow()
            .map(|set| format!("Set(len={})", set.order.len()))
            .unwrap_or_else(|_| "Set(<borrowed>)".to_owned()),
        _ => format!("<{}>", value.type_name()),
    };
    DebugValue {
        type_name: value.type_name(),
        preview: bounded_preview(&preview),
    }
}

fn bounded_preview(value: &str) -> String {
    let mut chars = value.chars();
    let mut preview: String = chars.by_ref().take(MAX_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        preview.push_str("…");
    }
    preview
}

fn debug_string_preview(value: &str) -> String {
    let mut preview = String::with_capacity(MAX_PREVIEW_CHARS.saturating_mul(2));
    preview.push('"');
    let mut chars = value.chars();
    for character in chars.by_ref().take(MAX_PREVIEW_CHARS) {
        preview.extend(character.escape_debug());
    }
    if chars.next().is_some() {
        preview.push('…');
    }
    preview.push('"');
    preview
}

fn format_identifier(value: &BigInt) -> String {
    if value.bits() <= 128 {
        value.to_string()
    } else {
        format!("<{} bits>", value.bits())
    }
}
