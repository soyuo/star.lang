//! Static checks for Star names, operand types, and control-flow context.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::BigInt;
use star_common::{SourcePosition, Span};
use star_parser::{
    DataLiteral, FunctionBlock, FunctionKind, Instruction, InstructionOpcode, LoopBlock, Program,
    ScopeBlock, Statement,
};

/// A compile-time semantic diagnostic with its original source location.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticError {
    /// The failed semantic rule.
    pub kind: SemanticErrorKind,
    /// The original source range associated with the failure.
    pub span: Span,
}

/// A statically detectable Star semantic error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticErrorKind {
    /// The operand stack is statically known to be too shallow.
    StackUnderflow,
    /// A value has a known type outside the instruction's accepted types.
    TypeMismatch {
        expected: &'static str,
        found: &'static str,
    },
    /// An identifier operand is known to be negative or is not an integer.
    InvalidIdentifier,
    /// A binding is declared more than once in one lexical scope.
    DuplicateBinding,
    /// A statically known binding is absent from its required scope.
    UnknownBinding,
    /// A statically known binding is immutable at a write or delete.
    ImmutableBinding,
    /// A function ID is duplicated in the module.
    DuplicateFunction,
    /// A direct call names no function in the module.
    UnknownFunction,
    /// A direct call's known argument count violates its function descriptor.
    InvalidCallArity,
    /// A function header is not statically well formed.
    InvalidFunctionHeader,
    /// A function-local parameter ID is repeated.
    DuplicateParameter,
    /// Control flow is used outside its valid function, loop, or async context.
    InvalidControlContext { instruction: &'static str },
    /// Two reachable branches disagree about operand-stack height.
    IncompatibleStackJoin,
    /// A loop body or back-edge does not restore its required stack state.
    IncompatibleLoopStack,
    /// Closure capture state differs across a branch or remains pending at exit.
    InvalidCaptureState,
    /// The same variable ID is captured twice before a closure is constructed.
    DuplicateCapture,
}

impl fmt::Display for SemanticErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StackUnderflow => {
                formatter.write_str("operand stack underflow is statically certain")
            }
            Self::TypeMismatch { expected, found } => {
                write!(formatter, "expected {expected}, found {found}")
            }
            Self::InvalidIdentifier => formatter.write_str("identifier must be a nonnegative Int"),
            Self::DuplicateBinding => formatter.write_str("duplicate binding in the same scope"),
            Self::UnknownBinding => {
                formatter.write_str("binding is not declared in an accessible scope")
            }
            Self::ImmutableBinding => {
                formatter.write_str("cannot modify or delete an immutable binding")
            }
            Self::DuplicateFunction => {
                formatter.write_str("function ID is already defined in this module")
            }
            Self::UnknownFunction => {
                formatter.write_str("function ID is not defined in this module")
            }
            Self::InvalidCallArity => {
                formatter.write_str("call argument count does not match the function descriptor")
            }
            Self::InvalidFunctionHeader => {
                formatter.write_str("function header must use valid constant integer metadata")
            }
            Self::DuplicateParameter => {
                formatter.write_str("parameter IDs must be distinct within a function")
            }
            Self::InvalidControlContext { instruction } => {
                write!(
                    formatter,
                    "{instruction} is invalid in this control-flow context"
                )
            }
            Self::IncompatibleStackJoin => {
                formatter.write_str("reachable branches have incompatible operand-stack states")
            }
            Self::IncompatibleLoopStack => formatter.write_str(
                "loop condition, back-edge, or exit has an incompatible operand-stack state",
            ),
            Self::InvalidCaptureState => {
                formatter.write_str("pending closure captures are inconsistent or not consumed")
            }
            Self::DuplicateCapture => {
                formatter.write_str("a variable ID is captured more than once before LAMBDA")
            }
        }
    }
}

impl fmt::Display for SemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at line {}, column {} (byte offset {})",
            self.kind, self.span.start.line, self.span.start.column, self.span.start.byte_offset
        )
    }
}

impl std::error::Error for SemanticError {}

/// A statically resolved named function available to the reference runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedFunction {
    /// The module-level function identifier.
    pub id: BigInt,
    /// Parameter identifiers in declaration order.
    pub parameter_ids: Vec<BigInt>,
    /// Whether the final parameter collects extra arguments.
    pub variadic: bool,
    /// Whether the function is synchronous or asynchronous.
    pub kind: FunctionKind,
    /// The function body statements.
    pub body: Vec<Statement>,
    /// Original source range of the function definition.
    pub span: Span,
}

/// Checks statically provable name, type, stack, and control-context rules.
///
/// Values that remain dynamic are left for the runtime's specified checks. This
/// function does not execute the program or resolve external modules.
pub fn analyze(program: &Program) -> Result<(), SemanticError> {
    resolve_functions(program).map(|_| ())
}

/// Performs semantic analysis and returns the statically declared functions.
///
/// The reference interpreter uses this table to support forward and recursive
/// direct calls without executing function bodies at definition time.
pub fn resolve_functions(program: &Program) -> Result<Vec<ResolvedFunction>, SemanticError> {
    Analyzer::new().analyze(program)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValueType {
    Any,
    Int,
    Float,
    Bool,
    Null,
    Byte,
    Char,
    String,
    Bytes,
    Array,
    Tuple,
    Map,
    Set,
    Struct,
    Function,
    Closure,
    NativeFunction,
    Iterator,
    Module,
    Task,
    Channel,
    Mutex,
    Handle,
}

impl ValueType {
    fn name(self) -> &'static str {
        match self {
            Self::Any => "a dynamic value",
            Self::Int => "Int",
            Self::Float => "Float",
            Self::Bool => "Bool",
            Self::Null => "Null",
            Self::Byte => "Byte",
            Self::Char => "Char",
            Self::String => "String",
            Self::Bytes => "Bytes",
            Self::Array => "Array",
            Self::Tuple => "Tuple",
            Self::Map => "Map",
            Self::Set => "Set",
            Self::Struct => "Struct",
            Self::Function => "Function",
            Self::Closure => "Closure",
            Self::NativeFunction => "NativeFunction",
            Self::Iterator => "Iterator",
            Self::Module => "Module",
            Self::Task => "Task",
            Self::Channel => "Channel",
            Self::Mutex => "Mutex",
            Self::Handle => "Handle",
        }
    }

    fn join(self, other: Self) -> Self {
        if self == other {
            self
        } else {
            Self::Any
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct IntegerConstant {
    negative: bool,
    bits: Vec<u8>,
}

impl IntegerConstant {
    fn from_literal(literal: &DataLiteral) -> Self {
        let first = literal.bits.iter().position(|bit| *bit != 0);
        let bits = first
            .map(|index| literal.bits[index..].to_vec())
            .unwrap_or_else(|| vec![0]);
        Self {
            negative: false,
            bits,
        }
    }

    fn negated(mut self) -> Self {
        if self.bits.iter().any(|bit| *bit != 0) {
            self.negative = !self.negative;
        }
        self
    }

    fn to_usize(&self) -> Option<usize> {
        if self.negative {
            return None;
        }
        self.bits.iter().try_fold(0_usize, |value, bit| {
            value.checked_mul(2)?.checked_add(usize::from(*bit))
        })
    }

    fn identifier(&self) -> Option<Identifier> {
        (!self.negative).then(|| Identifier(self.bits.clone()))
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Identifier(Vec<u8>);

fn identifier_to_bigint(identifier: &Identifier) -> BigInt {
    identifier
        .0
        .iter()
        .fold(BigInt::from(0), |value, bit| value * 2 + BigInt::from(*bit))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AbstractValue {
    value_type: ValueType,
    integer: Option<IntegerConstant>,
    nominal_id: Option<Identifier>,
}

impl AbstractValue {
    fn new(value_type: ValueType) -> Self {
        Self {
            value_type,
            integer: None,
            nominal_id: None,
        }
    }

    fn integer(value: IntegerConstant) -> Self {
        Self {
            value_type: ValueType::Int,
            integer: Some(value),
            nominal_id: None,
        }
    }

    fn joined(&self, other: &Self) -> Self {
        Self {
            value_type: self.value_type.join(other.value_type),
            integer: (self.integer == other.integer)
                .then(|| self.integer.clone())
                .flatten(),
            nominal_id: (self.nominal_id == other.nominal_id)
                .then(|| self.nominal_id.clone())
                .flatten(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AbstractStack {
    /// Known suffix of the runtime stack, bottom to top.
    values: Vec<AbstractValue>,
    /// The stack may have an unknown prefix below `values` after dynamic arity.
    opaque_prefix: bool,
}

impl AbstractStack {
    fn new() -> Self {
        Self {
            values: Vec::new(),
            opaque_prefix: false,
        }
    }

    fn pop(&mut self, span: Span) -> Result<AbstractValue, SemanticError> {
        self.values
            .pop()
            .or_else(|| {
                self.opaque_prefix
                    .then(|| AbstractValue::new(ValueType::Any))
            })
            .ok_or(SemanticError {
                kind: SemanticErrorKind::StackUnderflow,
                span,
            })
    }

    fn push(&mut self, value: AbstractValue) {
        self.values.push(value);
    }

    fn make_opaque(&mut self) {
        self.values.clear();
        self.opaque_prefix = true;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Binding {
    mutable: bool,
    value_type: ValueType,
    declaration_span: Option<Span>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FlowState {
    stack: AbstractStack,
    scopes: Vec<HashMap<Identifier, Binding>>,
    captures: Vec<Option<Identifier>>,
    reachable: bool,
}

impl FlowState {
    fn root() -> Self {
        Self {
            stack: AbstractStack::new(),
            scopes: vec![HashMap::new()],
            captures: Vec::new(),
            reachable: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FunctionSignature {
    parameter_count: usize,
    variadic: bool,
    kind: FunctionKind,
}

#[derive(Clone, Debug)]
struct CallSite {
    function_id: Identifier,
    argument_count: Option<usize>,
    span: Span,
}

struct LoopFrame {
    header: AbstractStack,
    continue_scope_depth: usize,
    exit_scope_depth: usize,
    breaks: Vec<FlowState>,
    continues: Vec<FlowState>,
}

#[derive(Default)]
struct Context {
    in_function: bool,
    in_async_function: bool,
    function_scope_depth: usize,
    loops: Vec<LoopFrame>,
}

struct Analyzer {
    functions: HashMap<Identifier, FunctionSignature>,
    resolved_functions: Vec<ResolvedFunction>,
    call_sites: Vec<CallSite>,
    global_load_sites: Vec<(Identifier, Span)>,
    global_store_ids: HashSet<Identifier>,
    completed_globals: Option<HashMap<Identifier, Binding>>,
    reanalysis: bool,
    loop_reanalysis: bool,
    structs: HashMap<Identifier, HashSet<Identifier>>,
    struct_methods: HashMap<Identifier, HashSet<Identifier>>,
    interfaces: HashMap<Identifier, HashSet<Identifier>>,
    methods: HashMap<(Identifier, Identifier), (Identifier, Span)>,
    implementations: Vec<(Identifier, Identifier, Span)>,
    export_sites: Vec<(Identifier, Span)>,
}

impl Analyzer {
    fn new() -> Self {
        Self {
            functions: HashMap::new(),
            resolved_functions: Vec::new(),
            call_sites: Vec::new(),
            global_load_sites: Vec::new(),
            global_store_ids: HashSet::new(),
            completed_globals: None,
            reanalysis: false,
            loop_reanalysis: false,
            structs: HashMap::new(),
            struct_methods: HashMap::new(),
            interfaces: HashMap::new(),
            methods: HashMap::new(),
            implementations: Vec::new(),
            export_sites: Vec::new(),
        }
    }

    fn analyze(mut self, program: &Program) -> Result<Vec<ResolvedFunction>, SemanticError> {
        let mut state = FlowState::root();
        let mut context = Context::default();
        self.analyze_sequence(&program.statements, &mut state, &mut context)?;
        self.completed_globals = Some(state.scopes.first().cloned().unwrap_or_default());

        self.reanalysis = true;
        let mut final_state = FlowState::root();
        let mut final_context = Context::default();
        let final_analysis =
            self.analyze_sequence(&program.statements, &mut final_state, &mut final_context);
        self.reanalysis = false;
        final_analysis?;
        state = final_state;

        if !state.captures.is_empty() {
            return Err(SemanticError {
                kind: SemanticErrorKind::InvalidCaptureState,
                span: program
                    .statements
                    .last()
                    .map(statement_span)
                    .unwrap_or_else(empty_span),
            });
        }
        let root_scope = state.scopes.first().cloned().unwrap_or_default();
        for (id, span) in &self.global_load_sites {
            if !root_scope.contains_key(id) && !self.global_store_ids.contains(id) {
                return Err(SemanticError {
                    kind: SemanticErrorKind::UnknownBinding,
                    span: *span,
                });
            }
        }
        for call in self.call_sites {
            let Some(signature) = self.functions.get(&call.function_id) else {
                return Err(SemanticError {
                    kind: SemanticErrorKind::UnknownFunction,
                    span: call.span,
                });
            };
            if call
                .argument_count
                .is_some_and(|count| !accepts_arity(signature, count))
            {
                return Err(SemanticError {
                    kind: SemanticErrorKind::InvalidCallArity,
                    span: call.span,
                });
            }
        }
        for (function_id, span) in self.methods.values() {
            if !self.functions.contains_key(function_id) {
                return Err(SemanticError {
                    kind: SemanticErrorKind::UnknownFunction,
                    span: *span,
                });
            }
        }
        for (struct_id, interface_id, span) in self.implementations {
            let Some(required) = self.interfaces.get(&interface_id) else {
                return Err(SemanticError {
                    kind: SemanticErrorKind::UnknownBinding,
                    span,
                });
            };
            let Some(methods) = self.struct_methods.get(&struct_id) else {
                return Err(SemanticError {
                    kind: SemanticErrorKind::UnknownBinding,
                    span,
                });
            };
            for method_id in required {
                if !methods.contains(method_id) {
                    return Err(SemanticError {
                        kind: SemanticErrorKind::UnknownBinding,
                        span,
                    });
                }
            }
        }
        let mut exported = HashSet::new();
        for (id, span) in self.export_sites {
            let symbol_count = usize::from(root_scope.contains_key(&id))
                + usize::from(self.functions.contains_key(&id))
                + usize::from(self.structs.contains_key(&id))
                + usize::from(self.interfaces.contains_key(&id));
            if symbol_count != 1 || !exported.insert(id) {
                return Err(SemanticError {
                    kind: SemanticErrorKind::UnknownBinding,
                    span,
                });
            }
        }
        Ok(self.resolved_functions)
    }

    fn analyze_sequence(
        &mut self,
        statements: &[Statement],
        state: &mut FlowState,
        context: &mut Context,
    ) -> Result<(), SemanticError> {
        for statement in statements {
            if !state.reachable {
                break;
            }
            match statement {
                Statement::DataLiteral(literal) => {
                    state
                        .stack
                        .push(AbstractValue::integer(IntegerConstant::from_literal(
                            literal,
                        )))
                }
                Statement::Instruction(instruction) => {
                    if self.loop_reanalysis && is_compile_time_opcode(instruction.opcode) {
                        continue;
                    }
                    self.analyze_instruction(*instruction, state, context)?;
                }
                Statement::If(block) => self.analyze_if(block, state, context)?,
                Statement::Loop(block) => self.analyze_loop(block, state, context)?,
                Statement::For(block) => self.analyze_for(block, state, context)?,
                Statement::Try(block) => self.analyze_try(block, state, context)?,
                Statement::Function(block) if !self.loop_reanalysis => {
                    self.analyze_function(block, state)?;
                }
                Statement::Function(_) => {}
                Statement::Scope(block) => self.analyze_scope(block, state, context)?,
            }
        }
        Ok(())
    }

    fn analyze_scope(
        &mut self,
        block: &ScopeBlock,
        state: &mut FlowState,
        context: &mut Context,
    ) -> Result<(), SemanticError> {
        let depth = state.scopes.len();
        state.scopes.push(HashMap::new());
        self.analyze_sequence(&block.body, state, context)?;
        state.scopes.truncate(depth);
        Ok(())
    }

    fn analyze_try(
        &mut self,
        block: &star_parser::TryBlock,
        state: &mut FlowState,
        context: &mut Context,
    ) -> Result<(), SemanticError> {
        let entry = state.clone();
        let mut protected = entry.clone();
        self.analyze_sequence(&block.try_body, &mut protected, context)?;

        let mut result = if let Some(catch_body) = &block.catch_body {
            let mut caught = entry;
            self.analyze_sequence(catch_body, &mut caught, context)?;
            join_flows(protected, caught, block.span)?
        } else {
            protected
        };

        if let Some(finally_body) = &block.finally_body {
            let pending_transfer = !result.reachable;
            result.reachable = true;
            self.analyze_sequence(finally_body, &mut result, context)?;
            if pending_transfer && result.reachable {
                result.reachable = false;
            }
        }

        *state = result;
        Ok(())
    }

    fn analyze_if(
        &mut self,
        block: &star_parser::IfBlock,
        state: &mut FlowState,
        context: &mut Context,
    ) -> Result<(), SemanticError> {
        let condition = state.stack.pop(block.span)?;
        expect_type(&condition, &[ValueType::Bool], block.span)?;

        let mut then_state = state.clone();
        self.analyze_sequence(&block.then_body, &mut then_state, context)?;
        let mut else_state = state.clone();
        if let Some(body) = &block.else_body {
            self.analyze_sequence(body, &mut else_state, context)?;
        }
        *state = join_flows(then_state, else_state, block.span)?;
        Ok(())
    }

    fn analyze_loop(
        &mut self,
        block: &LoopBlock,
        state: &mut FlowState,
        context: &mut Context,
    ) -> Result<(), SemanticError> {
        let entry = state.clone();
        let scope_depth = state.scopes.len();
        context.loops.push(LoopFrame {
            header: state.stack.clone(),
            continue_scope_depth: scope_depth,
            exit_scope_depth: scope_depth,
            breaks: Vec::new(),
            continues: Vec::new(),
        });
        let mut pending = vec![entry.clone()];
        let mut seen = vec![entry.clone()];
        let mut exits = Vec::new();
        let mut break_states = Vec::new();
        let mut pass = 0;

        while let Some(header_state) = pending.pop() {
            let previous_reanalysis = self.reanalysis;
            let previous_loop_reanalysis = self.loop_reanalysis;
            if pass > 0 {
                self.reanalysis = true;
                self.loop_reanalysis = true;
            }
            let result = (|| {
                let body_entry = if let Some(condition) = &block.condition {
                    let mut condition_state = header_state.clone();
                    self.analyze_sequence(condition, &mut condition_state, context)?;
                    if condition_state.reachable {
                        let condition_value = condition_state.stack.pop(block.span)?;
                        expect_type(&condition_value, &[ValueType::Bool], block.span)?;
                        if condition_state.captures != header_state.captures {
                            return Err(SemanticError {
                                kind: SemanticErrorKind::InvalidCaptureState,
                                span: block.span,
                            });
                        }
                        if !same_stack(&condition_state.stack, &header_state.stack) {
                            return Err(SemanticError {
                                kind: SemanticErrorKind::IncompatibleLoopStack,
                                span: block.span,
                            });
                        }
                        exits.push(condition_state.clone());
                        Some(condition_state)
                    } else {
                        None
                    }
                } else {
                    Some(header_state.clone())
                };

                if let Some(mut body_state) = body_entry {
                    self.analyze_sequence(&block.body, &mut body_state, context)?;
                    if body_state.reachable {
                        if !same_stack(&body_state.stack, &header_state.stack) {
                            return Err(SemanticError {
                                kind: SemanticErrorKind::IncompatibleLoopStack,
                                span: block.span,
                            });
                        }
                        if body_state.captures != header_state.captures {
                            return Err(SemanticError {
                                kind: SemanticErrorKind::InvalidCaptureState,
                                span: block.span,
                            });
                        }
                        enqueue_flow(body_state, &mut seen, &mut pending);
                    }
                }

                let loop_frame = context.loops.last_mut().expect("loop context exists");
                let continues = std::mem::take(&mut loop_frame.continues);
                for continue_state in continues {
                    if !same_stack(&continue_state.stack, &header_state.stack) {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::IncompatibleLoopStack,
                            span: block.span,
                        });
                    }
                    if continue_state.captures != header_state.captures {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::InvalidCaptureState,
                            span: block.span,
                        });
                    }
                    enqueue_flow(continue_state, &mut seen, &mut pending);
                }
                for break_state in std::mem::take(&mut loop_frame.breaks) {
                    if !same_stack(&break_state.stack, &header_state.stack) {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::IncompatibleLoopStack,
                            span: block.span,
                        });
                    }
                    if break_state.captures != header_state.captures {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::InvalidCaptureState,
                            span: block.span,
                        });
                    }
                    push_unique_flow(break_state, &mut break_states);
                }
                Ok(())
            })();
            self.reanalysis = previous_reanalysis;
            self.loop_reanalysis = previous_loop_reanalysis;
            result?;
            pass += 1;
        }

        context.loops.pop().expect("loop context was pushed");
        exits.extend(break_states);
        if exits.is_empty() {
            state.reachable = false;
            return Ok(());
        }
        let mut merged = exits.remove(0);
        for exit in exits {
            merged = join_flows(merged, exit, block.span)?;
        }
        *state = merged;
        Ok(())
    }

    fn analyze_for(
        &mut self,
        block: &star_parser::ForBlock,
        state: &mut FlowState,
        context: &mut Context,
    ) -> Result<(), SemanticError> {
        let span = block.span;
        let variable_id = pop_identifier(state, span)?;
        let iterator = state.stack.pop(span)?;
        expect_type(&iterator, &[ValueType::Iterator], span)?;

        let exit_depth = state.scopes.len();
        state.scopes.push(HashMap::new());
        if let Some(id) = &variable_id {
            state.scopes.last_mut().expect("loop scope exists").insert(
                id.clone(),
                Binding {
                    mutable: true,
                    value_type: ValueType::Any,
                    declaration_span: None,
                },
            );
        }
        let entry = state.clone();
        let loop_variable_id = variable_id;
        context.loops.push(LoopFrame {
            header: entry.stack.clone(),
            continue_scope_depth: entry.scopes.len(),
            exit_scope_depth: exit_depth,
            breaks: Vec::new(),
            continues: Vec::new(),
        });
        let mut pending = vec![entry.clone()];
        let mut seen = vec![entry.clone()];
        let mut exits = Vec::new();
        let mut break_states = Vec::new();
        let mut pass = 0;

        while let Some(header_state) = pending.pop() {
            let mut exhaustion = header_state.clone();
            exhaustion.scopes.truncate(exit_depth);
            push_unique_flow(exhaustion, &mut exits);

            let previous_reanalysis = self.reanalysis;
            let previous_loop_reanalysis = self.loop_reanalysis;
            if pass > 0 {
                self.reanalysis = true;
                self.loop_reanalysis = true;
            }
            let result = (|| {
                let mut body_state = header_state.clone();
                self.analyze_sequence(&block.body, &mut body_state, context)?;
                if body_state.reachable {
                    if !same_stack(&body_state.stack, &header_state.stack) {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::IncompatibleLoopStack,
                            span,
                        });
                    }
                    if body_state.captures != header_state.captures {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::InvalidCaptureState,
                            span,
                        });
                    }
                    let mut body_exit = body_state.clone();
                    body_exit.scopes.truncate(exit_depth);
                    push_unique_flow(body_exit, &mut exits);
                    if let Some(id) = &loop_variable_id {
                        reset_binding_type(&mut body_state, id);
                    }
                    enqueue_flow(body_state, &mut seen, &mut pending);
                }

                let loop_frame = context.loops.last_mut().expect("loop context exists");
                let continues = std::mem::take(&mut loop_frame.continues);
                for mut continue_state in continues {
                    if !same_stack(&continue_state.stack, &header_state.stack) {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::IncompatibleLoopStack,
                            span,
                        });
                    }
                    if continue_state.captures != header_state.captures {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::InvalidCaptureState,
                            span,
                        });
                    }
                    let mut continue_exit = continue_state.clone();
                    continue_exit.scopes.truncate(exit_depth);
                    push_unique_flow(continue_exit, &mut exits);
                    if let Some(id) = &loop_variable_id {
                        reset_binding_type(&mut continue_state, id);
                    }
                    enqueue_flow(continue_state, &mut seen, &mut pending);
                }
                for mut break_state in std::mem::take(&mut loop_frame.breaks) {
                    if !same_stack(&break_state.stack, &header_state.stack) {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::IncompatibleLoopStack,
                            span,
                        });
                    }
                    if break_state.captures != header_state.captures {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::InvalidCaptureState,
                            span,
                        });
                    }
                    break_state.scopes.truncate(exit_depth);
                    push_unique_flow(break_state, &mut break_states);
                }
                Ok(())
            })();
            self.reanalysis = previous_reanalysis;
            self.loop_reanalysis = previous_loop_reanalysis;
            result?;
            pass += 1;
        }

        context.loops.pop().expect("loop context was pushed");
        exits.extend(break_states);
        let mut after = exits.remove(0);
        for exit in exits {
            after = join_flows(after, exit, span)?;
        }
        *state = after;
        Ok(())
    }

    fn analyze_function(
        &mut self,
        block: &FunctionBlock,
        outer: &mut FlowState,
    ) -> Result<(), SemanticError> {
        let signature = take_function_header(outer, block.span, block.kind)?;
        if !self.reanalysis
            && !self.loop_reanalysis
            && !self
                .resolved_functions
                .iter()
                .any(|function| function.span == block.span)
        {
            self.resolved_functions.push(ResolvedFunction {
                id: identifier_to_bigint(&signature.0),
                parameter_ids: signature.2.iter().rev().map(identifier_to_bigint).collect(),
                variadic: signature.1.variadic,
                kind: block.kind,
                body: block.body.clone(),
                span: block.span,
            });
        }
        if let Some(existing) = self.functions.get(&signature.0) {
            if !self.reanalysis || existing != &signature.1 {
                return Err(SemanticError {
                    kind: SemanticErrorKind::DuplicateFunction,
                    span: block.span,
                });
            }
        } else {
            self.functions
                .insert(signature.0.clone(), signature.1.clone());
        }

        let mut body_state = FlowState::root();
        body_state.scopes[0] = self
            .completed_globals
            .clone()
            .or_else(|| outer.scopes.first().cloned())
            .unwrap_or_default();
        body_state.scopes.push(HashMap::new());
        for id in signature.2 {
            if body_state
                .scopes
                .last_mut()
                .unwrap()
                .insert(
                    id,
                    Binding {
                        mutable: true,
                        value_type: ValueType::Any,
                        declaration_span: None,
                    },
                )
                .is_some()
            {
                return Err(SemanticError {
                    kind: SemanticErrorKind::DuplicateParameter,
                    span: block.span,
                });
            }
        }
        let mut context = Context {
            in_function: true,
            in_async_function: block.kind == FunctionKind::Asynchronous,
            function_scope_depth: body_state.scopes.len(),
            loops: Vec::new(),
        };
        self.analyze_sequence(&block.body, &mut body_state, &mut context)?;
        if !body_state.captures.is_empty() {
            return Err(SemanticError {
                kind: SemanticErrorKind::InvalidCaptureState,
                span: block.span,
            });
        }
        Ok(())
    }

    fn analyze_instruction(
        &mut self,
        instruction: Instruction,
        state: &mut FlowState,
        context: &mut Context,
    ) -> Result<(), SemanticError> {
        let span = instruction.span;
        let opcode = match instruction.opcode {
            InstructionOpcode::Basic(ordinal) => ordinal,
            InstructionOpcode::Extended(opcode) => opcode,
        };
        match opcode {
            1 | 2 | 3 | 4 | 5 | 11 | 0x20 | 0x23 => {
                let right = state.stack.pop(span)?;
                let left = state.stack.pop(span)?;
                let numeric = &[ValueType::Int, ValueType::Float];
                expect_type(&left, numeric, span)?;
                expect_type(&right, numeric, span)?;
                if opcode == 5 {
                    expect_type(&left, &[ValueType::Int], span)?;
                    expect_type(&right, &[ValueType::Int], span)?;
                }
                let result_type = match opcode {
                    11 | 0x23 => ValueType::Bool,
                    5 => ValueType::Int,
                    0x20 => {
                        if left.value_type == ValueType::Float
                            || right.value_type == ValueType::Float
                        {
                            ValueType::Float
                        } else {
                            ValueType::Any
                        }
                    }
                    _ => numeric_result_type(left.value_type, right.value_type),
                };
                state.stack.push(AbstractValue::new(result_type));
            }
            6 => {
                state.stack.pop(span)?;
            }
            7 => state.stack.push(AbstractValue::new(ValueType::Any)),
            8 => self.store(state, span, false, false)?,
            9 => self.load(state, span, false, false)?,
            10 => {
                state.stack.pop(span)?;
                state.stack.pop(span)?;
                state.stack.push(AbstractValue::new(ValueType::Bool));
            }
            12 => {
                let value = state.stack.pop(span)?;
                expect_type(&value, &[ValueType::Bool], span)?;
                state.stack.push(AbstractValue::new(ValueType::Bool));
            }
            0x10 => {
                let value = state.stack.pop(span)?;
                expect_type(
                    &value,
                    &[
                        ValueType::Int,
                        ValueType::Byte,
                        ValueType::Float,
                        ValueType::Bool,
                        ValueType::String,
                    ],
                    span,
                )?;
                state.stack.push(AbstractValue::new(ValueType::Int));
            }
            0x11 => {
                let value = state.stack.pop(span)?;
                expect_type(
                    &value,
                    &[
                        ValueType::Int,
                        ValueType::Byte,
                        ValueType::Float,
                        ValueType::Bool,
                        ValueType::String,
                    ],
                    span,
                )?;
                state.stack.push(AbstractValue::new(ValueType::Float));
            }
            0x12 | 0x13 => {
                state.stack.pop(span)?;
                state.stack.push(AbstractValue::new(if opcode == 0x12 {
                    ValueType::Bool
                } else {
                    ValueType::String
                }));
            }
            0x14 | 0x16 => {
                let value = state.stack.pop(span)?;
                expect_type(&value, &[ValueType::String], span)?;
                state.stack.push(AbstractValue::new(ValueType::Bytes));
            }
            0x15 => {
                let value = state.stack.pop(span)?;
                expect_type(&value, &[ValueType::Bytes], span)?;
                state.stack.push(AbstractValue::new(ValueType::String));
            }
            0x17 | 0x1D | 0x1E => {
                let value = state.stack.pop(span)?;
                expect_type(&value, &[ValueType::Int], span)?;
                if let Some(integer) = &value.integer {
                    if opcode == 0x17 && (integer.negative || integer.bits.len() > 64) {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::TypeMismatch {
                                expected: "unsigned 64-bit Int",
                                found: "out-of-range Int",
                            },
                            span,
                        });
                    }
                    if opcode == 0x1D {
                        let scalar = integer.to_usize();
                        if !scalar.is_some_and(|scalar| {
                            scalar <= 0x10_FFFF && !(0xD800..=0xDFFF).contains(&scalar)
                        }) {
                            return Err(SemanticError {
                                kind: SemanticErrorKind::TypeMismatch {
                                    expected: "Unicode scalar value Int",
                                    found: "out-of-range Int",
                                },
                                span,
                            });
                        }
                    }
                    if opcode == 0x1E && !integer.to_usize().is_some_and(|byte| byte <= 255) {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::TypeMismatch {
                                expected: "Int in 0..255",
                                found: "out-of-range Int",
                            },
                            span,
                        });
                    }
                }
                state.stack.push(AbstractValue::new(match opcode {
                    0x17 => ValueType::Float,
                    0x1D => ValueType::Char,
                    _ => ValueType::Byte,
                }));
            }
            0x18 => state.stack.push(AbstractValue::new(ValueType::Null)),
            0x19 | 0x1A => state.stack.push(AbstractValue::new(ValueType::Bool)),
            0x1B => {
                state.stack.pop(span)?;
                state.stack.push(AbstractValue::new(ValueType::String));
            }
            0x1C => {
                let name = state.stack.pop(span)?;
                expect_type(&name, &[ValueType::String], span)?;
                state.stack.pop(span)?;
                state.stack.push(AbstractValue::new(ValueType::Bool));
            }
            0x1F => {
                let length = state.stack.pop(span)?;
                let value = state.stack.pop(span)?;
                expect_nonnegative_integer(&length, span)?;
                expect_nonnegative_integer(&value, span)?;
                state.stack.push(AbstractValue::new(ValueType::Bytes));
            }
            0x21 | 0x22 => {
                let value = state.stack.pop(span)?;
                expect_type(&value, &[ValueType::Int, ValueType::Float], span)?;
                state.stack.push(AbstractValue {
                    value_type: value.value_type,
                    integer: (opcode == 0x21)
                        .then(|| value.integer.map(IntegerConstant::negated))
                        .flatten(),
                    nominal_id: value.nominal_id,
                });
            }
            0x24 | 0x25 | 0x26 => {
                let right = state.stack.pop(span)?;
                let left = state.stack.pop(span)?;
                expect_type(&left, &[ValueType::Bool], span)?;
                expect_type(&right, &[ValueType::Bool], span)?;
                state.stack.push(AbstractValue::new(ValueType::Bool));
            }
            0x27 | 0x28 | 0x29 => {
                let right = state.stack.pop(span)?;
                let left = state.stack.pop(span)?;
                expect_type(&left, &[ValueType::Int], span)?;
                expect_type(&right, &[ValueType::Int], span)?;
                state.stack.push(AbstractValue::new(ValueType::Int));
            }
            0x2A => {
                let value = state.stack.pop(span)?;
                expect_type(&value, &[ValueType::Int], span)?;
                state.stack.push(AbstractValue::new(ValueType::Int));
            }
            0x2B | 0x2C => {
                let count = state.stack.pop(span)?;
                let value = state.stack.pop(span)?;
                expect_type(&value, &[ValueType::Int], span)?;
                expect_nonnegative_integer(&count, span)?;
                state.stack.push(AbstractValue::new(ValueType::Int));
            }
            0x30..=0x34 | 0x37 | 0x40 | 0xA0 | 0x55 | 0x56 => {
                return Err(SemanticError {
                    kind: SemanticErrorKind::InvalidControlContext {
                        instruction: "structural opcode",
                    },
                    span,
                });
            }
            0x35 => self.break_loop(state, context, span, false)?,
            0x36 => self.break_loop(state, context, span, true)?,
            0x41 | 0x45 => {
                if opcode == 0x45 && !context.in_function {
                    return Err(SemanticError {
                        kind: SemanticErrorKind::InvalidControlContext {
                            instruction: "TAIL_CALL",
                        },
                        span,
                    });
                }
                self.direct_call(state, span, opcode == 0x45)?;
            }
            0x42 => {
                if !context.in_function {
                    return Err(SemanticError {
                        kind: SemanticErrorKind::InvalidControlContext {
                            instruction: "RETURN",
                        },
                        span,
                    });
                }
                if !state.captures.is_empty() {
                    return Err(SemanticError {
                        kind: SemanticErrorKind::InvalidCaptureState,
                        span,
                    });
                }
                state.scopes.truncate(context.function_scope_depth);
                state.reachable = false;
            }
            0x43 => {
                let parameter_count = pop_count(state, span)?;
                let _body_id = pop_identifier(state, span)?;
                if parameter_count.is_none() {
                    state.stack.make_opaque();
                }
                let callable_type = if state.captures.is_empty() {
                    ValueType::Function
                } else {
                    ValueType::Closure
                };
                state.captures.clear();
                state.stack.push(AbstractValue::new(callable_type));
            }
            0x44 => {
                let id = pop_identifier(state, span)?;
                if let Some(id) = id {
                    if lookup_binding(&state.scopes, &id).is_none() {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::UnknownBinding,
                            span,
                        });
                    }
                    if state.captures.contains(&Some(id.clone())) {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::DuplicateCapture,
                            span,
                        });
                    }
                    state.captures.push(Some(id));
                } else {
                    state.captures.push(None);
                }
            }
            0x46 => self.indirect_call(state, span)?,
            0x50 => self.declare(state, span, true)?,
            0x51 => self.declare(state, span, false)?,
            0x52 => self.delete(state, span)?,
            0x53 => self.load(state, span, true, context.in_function)?,
            0x54 => self.store(state, span, true, context.in_function)?,
            0x57 => self.annotate(state, span)?,
            0x60 | 0x62 | 0x6B => {
                let count = pop_count(state, span)?;
                pop_many(state, count, span)?;
                state.stack.push(AbstractValue::new(match opcode {
                    0x60 => ValueType::Array,
                    0x62 => ValueType::Set,
                    _ => ValueType::Tuple,
                }));
            }
            0x61 => {
                let count = pop_count(state, span)?;
                let pair_items = count.and_then(|value| value.checked_mul(2));
                pop_many(state, pair_items, span)?;
                state.stack.push(AbstractValue::new(ValueType::Map));
            }
            0x63 => self.get_item(state, span)?,
            0x64 => self.set_item(state, span)?,
            0x65 => self.append(state, span)?,
            0x66 => self.remove(state, span)?,
            0x67 => {
                let collection = state.stack.pop(span)?;
                expect_type(
                    &collection,
                    &[
                        ValueType::String,
                        ValueType::Bytes,
                        ValueType::Array,
                        ValueType::Tuple,
                        ValueType::Map,
                        ValueType::Set,
                    ],
                    span,
                )?;
                state.stack.push(AbstractValue::new(ValueType::Int));
            }
            0x68 => {
                state.stack.pop(span)?;
                let collection = state.stack.pop(span)?;
                expect_type(
                    &collection,
                    &[
                        ValueType::Array,
                        ValueType::Tuple,
                        ValueType::String,
                        ValueType::Bytes,
                        ValueType::Map,
                        ValueType::Set,
                    ],
                    span,
                )?;
                state.stack.push(AbstractValue::new(ValueType::Bool));
            }
            0x69 => {
                let collection = state.stack.pop(span)?;
                expect_type(
                    &collection,
                    &[
                        ValueType::Array,
                        ValueType::Tuple,
                        ValueType::String,
                        ValueType::Bytes,
                        ValueType::Map,
                        ValueType::Set,
                    ],
                    span,
                )?;
                state.stack.push(AbstractValue::new(ValueType::Iterator));
            }
            0x6A => {
                let iterator = state.stack.pop(span)?;
                expect_type(&iterator, &[ValueType::Iterator], span)?;
                state.stack.push(AbstractValue::new(ValueType::Any));
                state.stack.push(AbstractValue::new(ValueType::Bool));
            }
            0x6C => {
                let end = state.stack.pop(span)?;
                let start = state.stack.pop(span)?;
                let collection = state.stack.pop(span)?;
                expect_type(&start, &[ValueType::Int], span)?;
                expect_type(&end, &[ValueType::Int], span)?;
                expect_type(
                    &collection,
                    &[
                        ValueType::Array,
                        ValueType::Tuple,
                        ValueType::String,
                        ValueType::Bytes,
                    ],
                    span,
                )?;
                state.stack.push(AbstractValue::new(collection.value_type));
            }
            0x6D => {
                let right = state.stack.pop(span)?;
                let left = state.stack.pop(span)?;
                let supported = &[
                    ValueType::Array,
                    ValueType::Tuple,
                    ValueType::String,
                    ValueType::Bytes,
                ];
                expect_type(&left, supported, span)?;
                expect_type(&right, supported, span)?;
                if left.value_type != ValueType::Any
                    && right.value_type != ValueType::Any
                    && left.value_type != right.value_type
                {
                    return Err(type_mismatch(
                        left.value_type.name(),
                        right.value_type,
                        span,
                    ));
                }
                state
                    .stack
                    .push(AbstractValue::new(left.value_type.join(right.value_type)));
            }
            0x70 => {
                if context.in_function {
                    return Err(SemanticError {
                        kind: SemanticErrorKind::InvalidControlContext {
                            instruction: "STRUCT_DEF",
                        },
                        span,
                    });
                }
                self.define_struct(state, span)?;
            }
            0x71 => self.object_new(state, span)?,
            0x72 => self.field_get(state, span)?,
            0x73 => self.field_set(state, span)?,
            0x74 => {
                if context.in_function {
                    return Err(SemanticError {
                        kind: SemanticErrorKind::InvalidControlContext {
                            instruction: "METHOD_DEF",
                        },
                        span,
                    });
                }
                self.define_method(state, span)?;
            }
            0x75 => self.method_call(state, span)?,
            0x76 => {
                if context.in_function {
                    return Err(SemanticError {
                        kind: SemanticErrorKind::InvalidControlContext {
                            instruction: "INTERFACE_DEF",
                        },
                        span,
                    });
                }
                self.define_interface(state, span)?;
            }
            0x77 => {
                if context.in_function {
                    return Err(SemanticError {
                        kind: SemanticErrorKind::InvalidControlContext {
                            instruction: "IMPLEMENT",
                        },
                        span,
                    });
                }
                self.implement(state, span)?;
            }
            0x78 => {
                let type_id = pop_identifier(state, span)?;
                let object = state.stack.pop(span)?;
                expect_type(&object, &[ValueType::Struct], span)?;
                if let Some(type_id) = type_id {
                    if !self.structs.contains_key(&type_id)
                        && !self.interfaces.contains_key(&type_id)
                    {
                        return Err(SemanticError {
                            kind: SemanticErrorKind::UnknownBinding,
                            span,
                        });
                    }
                } else {
                    state.stack.make_opaque();
                }
                state.stack.push(AbstractValue::new(ValueType::Bool));
            }
            0x79 => {
                let object = state.stack.pop(span)?;
                expect_type(&object, &[ValueType::Struct], span)?;
                state.stack.push(AbstractValue {
                    value_type: ValueType::Struct,
                    integer: None,
                    nominal_id: object.nominal_id,
                });
            }
            0x90 | 0x95 => {
                let path = state.stack.pop(span)?;
                expect_type(
                    &path,
                    &[if opcode == 0x90 {
                        ValueType::Bytes
                    } else {
                        ValueType::String
                    }],
                    span,
                )?;
                state.stack.push(AbstractValue::new(ValueType::Module));
            }
            0x91 => {
                if context.in_function {
                    return Err(SemanticError {
                        kind: SemanticErrorKind::InvalidControlContext {
                            instruction: "EXPORT",
                        },
                        span,
                    });
                }
                self.export(state, span)?;
            }
            0x92 => {
                let symbol_id = pop_identifier(state, span)?;
                let module = state.stack.pop(span)?;
                expect_type(&module, &[ValueType::Module], span)?;
                if symbol_id.is_none() {
                    state.stack.make_opaque();
                }
                state.stack.push(AbstractValue::new(ValueType::Any));
            }
            0x93 => self.module_call(state, span)?,
            0x94 => {
                let module = state.stack.pop(span)?;
                expect_type(&module, &[ValueType::Module], span)?;
            }
            0xA1 => {
                if !context.in_async_function {
                    return Err(SemanticError {
                        kind: SemanticErrorKind::InvalidControlContext {
                            instruction: "AWAIT",
                        },
                        span,
                    });
                }
                let task = state.stack.pop(span)?;
                expect_type(&task, &[ValueType::Task], span)?;
                state.stack.push(AbstractValue::new(ValueType::Any));
            }
            0xA2 => {
                let function = state.stack.pop(span)?;
                expect_type(&function, &[ValueType::Function], span)?;
                state.stack.push(AbstractValue::new(ValueType::Task));
            }
            0xA3 => self.require_async(state, context, span, "YIELD")?,
            0xA4 => {
                self.require_async(state, context, span, "SLEEP")?;
                let duration = state.stack.pop(span)?;
                expect_nonnegative_integer(&duration, span)?;
                state.stack.push(AbstractValue::new(ValueType::Null));
            }
            0xA5 | 0xA6 => {
                let task = state.stack.pop(span)?;
                expect_type(&task, &[ValueType::Task], span)?;
                state.stack.push(AbstractValue::new(if opcode == 0xA5 {
                    ValueType::Any
                } else {
                    ValueType::Bool
                }));
            }
            0xA7 => {
                let capacity = state.stack.pop(span)?;
                expect_nonnegative_integer(&capacity, span)?;
                state.stack.push(AbstractValue::new(ValueType::Channel));
            }
            0xA8 => {
                self.require_async(state, context, span, "CHANNEL_SEND")?;
                state.stack.pop(span)?;
                let channel = state.stack.pop(span)?;
                expect_type(&channel, &[ValueType::Channel], span)?;
                state.stack.push(AbstractValue::new(ValueType::Null));
            }
            0xA9 => {
                self.require_async(state, context, span, "CHANNEL_RECV")?;
                let channel = state.stack.pop(span)?;
                expect_type(&channel, &[ValueType::Channel], span)?;
                state.stack.push(AbstractValue::new(ValueType::Any));
            }
            0xAA => state.stack.push(AbstractValue::new(ValueType::Mutex)),
            0xAB | 0xAC => {
                if opcode == 0xAB {
                    self.require_async(state, context, span, "MUTEX_LOCK")?;
                }
                let mutex = state.stack.pop(span)?;
                expect_type(&mutex, &[ValueType::Mutex], span)?;
                state.stack.push(AbstractValue::new(ValueType::Null));
            }
            0xB0 => {
                let path = state.stack.pop(span)?;
                expect_type(&path, &[ValueType::String], span)?;
                state.stack.push(AbstractValue::new(ValueType::Handle));
            }
            0xB1 => {
                state.stack.pop(span)?;
                let name = state.stack.pop(span)?;
                let library = state.stack.pop(span)?;
                expect_type(&name, &[ValueType::String], span)?;
                expect_type(&library, &[ValueType::Handle], span)?;
                state.stack.push(AbstractValue::new(ValueType::Handle));
            }
            0xB2 => self.native_call(state, span)?,
            0xB3 => {
                let handle = state.stack.pop(span)?;
                expect_type(&handle, &[ValueType::Handle], span)?;
            }
            0xB4 => {
                state.stack.pop(span)?;
                let function = state.stack.pop(span)?;
                expect_type(&function, &[ValueType::Function], span)?;
                state.stack.push(AbstractValue::new(ValueType::Handle));
            }
            0xB5 => {
                let length = state.stack.pop(span)?;
                expect_nonnegative_integer(&length, span)?;
                state.stack.push(AbstractValue::new(ValueType::Handle));
            }
            _ => {
                return Err(SemanticError {
                    kind: SemanticErrorKind::InvalidControlContext {
                        instruction: "unknown instruction",
                    },
                    span,
                })
            }
        }
        Ok(())
    }

    fn declare(
        &mut self,
        state: &mut FlowState,
        span: Span,
        mutable: bool,
    ) -> Result<(), SemanticError> {
        let id = pop_identifier(state, span)?;
        let value = state.stack.pop(span)?;
        if let Some(id) = id {
            let current = state
                .scopes
                .last_mut()
                .expect("module or function scope exists");
            if current
                .get(&id)
                .is_some_and(|binding| !(self.reanalysis && binding.declaration_span == Some(span)))
            {
                return Err(SemanticError {
                    kind: SemanticErrorKind::DuplicateBinding,
                    span,
                });
            }
            current.insert(
                id,
                Binding {
                    mutable,
                    value_type: value.value_type,
                    declaration_span: Some(span),
                },
            );
        }
        Ok(())
    }

    fn load(
        &mut self,
        state: &mut FlowState,
        span: Span,
        global: bool,
        in_function: bool,
    ) -> Result<(), SemanticError> {
        let id = pop_identifier(state, span)?;
        let binding = id.as_ref().and_then(|id| {
            if global {
                state.scopes.first().and_then(|scope| scope.get(id))
            } else {
                lookup_binding(&state.scopes, id)
            }
        });
        if global && binding.is_none() {
            if let Some(id) = id {
                if !in_function {
                    return Err(SemanticError {
                        kind: SemanticErrorKind::UnknownBinding,
                        span,
                    });
                }
                if !self
                    .global_load_sites
                    .iter()
                    .any(|(seen_id, seen_span)| seen_id == &id && *seen_span == span)
                {
                    self.global_load_sites.push((id, span));
                }
            }
        }
        state.stack.push(AbstractValue::new(
            binding.map_or(ValueType::Any, |binding| binding.value_type),
        ));
        Ok(())
    }

    fn store(
        &mut self,
        state: &mut FlowState,
        span: Span,
        global: bool,
        in_function: bool,
    ) -> Result<(), SemanticError> {
        let id = pop_identifier(state, span)?;
        let value = state.stack.pop(span)?;
        let Some(id) = id else {
            return Ok(());
        };
        if global {
            if in_function {
                self.global_store_ids.insert(id.clone());
            }
            let root = state.scopes.first_mut().expect("root scope exists");
            if let Some(binding) = root.get_mut(&id) {
                if !binding.mutable {
                    return Err(SemanticError {
                        kind: SemanticErrorKind::ImmutableBinding,
                        span,
                    });
                }
                binding.value_type = value.value_type;
            } else {
                root.insert(
                    id,
                    Binding {
                        mutable: true,
                        value_type: value.value_type,
                        declaration_span: None,
                    },
                );
            }
        } else if let Some(index) = state
            .scopes
            .iter()
            .rposition(|scope| scope.contains_key(&id))
        {
            let binding = state.scopes[index].get_mut(&id).unwrap();
            if !binding.mutable {
                return Err(SemanticError {
                    kind: SemanticErrorKind::ImmutableBinding,
                    span,
                });
            }
            binding.value_type = value.value_type;
        } else {
            state
                .scopes
                .last_mut()
                .expect("current scope exists")
                .insert(
                    id,
                    Binding {
                        mutable: true,
                        value_type: value.value_type,
                        declaration_span: None,
                    },
                );
        }
        Ok(())
    }

    fn delete(&self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let id = pop_identifier(state, span)?;
        if let Some(id) = id {
            let current = state.scopes.last_mut().expect("current scope exists");
            match current.get(&id) {
                None => {
                    return Err(SemanticError {
                        kind: SemanticErrorKind::UnknownBinding,
                        span,
                    })
                }
                Some(binding) if !binding.mutable => {
                    return Err(SemanticError {
                        kind: SemanticErrorKind::ImmutableBinding,
                        span,
                    })
                }
                Some(_) => {
                    current.remove(&id);
                }
            }
        }
        Ok(())
    }

    fn annotate(&self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let id = pop_identifier(state, span)?;
        let name = state.stack.pop(span)?;
        expect_type(&name, &[ValueType::String], span)?;
        if let Some(id) = id {
            let Some(binding) = lookup_binding_mut(&mut state.scopes, &id) else {
                return Err(SemanticError {
                    kind: SemanticErrorKind::UnknownBinding,
                    span,
                });
            };
            // The annotation name may be dynamic; its restriction is enforced at runtime.
            binding.value_type = ValueType::Any;
        }
        Ok(())
    }

    fn direct_call(
        &mut self,
        state: &mut FlowState,
        span: Span,
        tail: bool,
    ) -> Result<(), SemanticError> {
        let function_id = pop_identifier(state, span)?;
        let argument_count = pop_count(state, span)?;
        let arguments = pop_many(state, argument_count, span)?;
        let dynamic_call = function_id.is_none() || argument_count.is_none() || arguments.is_none();
        let result_type = function_id
            .as_ref()
            .and_then(|id| self.functions.get(id))
            .map_or(ValueType::Any, |signature| {
                if signature.kind == FunctionKind::Asynchronous {
                    ValueType::Task
                } else {
                    ValueType::Any
                }
            });
        if let Some(function_id) = function_id {
            if !self.call_sites.iter().any(|site| {
                site.function_id == function_id
                    && site.argument_count == argument_count
                    && site.span == span
            }) {
                self.call_sites.push(CallSite {
                    function_id,
                    argument_count,
                    span,
                });
            }
        }
        if tail {
            state.reachable = false;
            if dynamic_call {
                state.stack.make_opaque();
            }
        } else if dynamic_call {
            state.stack.make_opaque();
            state.stack.push(AbstractValue::new(result_type));
        } else {
            state.stack.push(AbstractValue::new(result_type));
        }
        Ok(())
    }

    fn indirect_call(&self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let callable = state.stack.pop(span)?;
        expect_type(
            &callable,
            &[
                ValueType::Function,
                ValueType::Closure,
                ValueType::NativeFunction,
            ],
            span,
        )?;
        let argument_count = pop_count(state, span)?;
        let arguments = pop_many(state, argument_count, span)?;
        if arguments.is_none() || argument_count.is_none() {
            state.stack.make_opaque();
        }
        state.stack.push(AbstractValue::new(ValueType::Any));
        Ok(())
    }

    fn break_loop(
        &self,
        state: &mut FlowState,
        context: &mut Context,
        span: Span,
        is_continue: bool,
    ) -> Result<(), SemanticError> {
        let Some(loop_frame) = context.loops.last_mut() else {
            return Err(SemanticError {
                kind: SemanticErrorKind::InvalidControlContext {
                    instruction: if is_continue { "CONTINUE" } else { "BREAK" },
                },
                span,
            });
        };
        if !same_stack(&state.stack, &loop_frame.header) {
            return Err(SemanticError {
                kind: SemanticErrorKind::IncompatibleLoopStack,
                span,
            });
        }
        if is_continue {
            state.scopes.truncate(loop_frame.continue_scope_depth);
            loop_frame.continues.push(state.clone());
        } else {
            state.scopes.truncate(loop_frame.exit_scope_depth);
            loop_frame.breaks.push(state.clone());
        }
        state.reachable = false;
        Ok(())
    }

    fn require_async(
        &self,
        _state: &FlowState,
        context: &Context,
        span: Span,
        instruction: &'static str,
    ) -> Result<(), SemanticError> {
        if context.in_async_function {
            Ok(())
        } else {
            Err(SemanticError {
                kind: SemanticErrorKind::InvalidControlContext { instruction },
                span,
            })
        }
    }

    fn get_item(&self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let key = state.stack.pop(span)?;
        let collection = state.stack.pop(span)?;
        match collection.value_type {
            ValueType::Array | ValueType::Tuple => expect_type(&key, &[ValueType::Int], span)?,
            ValueType::String => {
                expect_type(&key, &[ValueType::Int], span)?;
                state.stack.push(AbstractValue::new(ValueType::Char));
                return Ok(());
            }
            ValueType::Bytes => {
                expect_type(&key, &[ValueType::Int], span)?;
                state.stack.push(AbstractValue::new(ValueType::Byte));
                return Ok(());
            }
            ValueType::Map | ValueType::Any => {}
            found => {
                return Err(type_mismatch(
                    "Array, Tuple, String, Bytes, or Map",
                    found,
                    span,
                ))
            }
        }
        state.stack.push(AbstractValue::new(ValueType::Any));
        Ok(())
    }

    fn set_item(&self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        state.stack.pop(span)?;
        let key = state.stack.pop(span)?;
        let collection = state.stack.pop(span)?;
        expect_type(&collection, &[ValueType::Array, ValueType::Map], span)?;
        if collection.value_type == ValueType::Array {
            expect_type(&key, &[ValueType::Int], span)?;
        }
        Ok(())
    }

    fn append(&self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        state.stack.pop(span)?;
        let collection = state.stack.pop(span)?;
        expect_type(&collection, &[ValueType::Array], span)
    }

    fn remove(&self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let key = state.stack.pop(span)?;
        let collection = state.stack.pop(span)?;
        expect_type(
            &collection,
            &[ValueType::Array, ValueType::Map, ValueType::Set],
            span,
        )?;
        if collection.value_type == ValueType::Array {
            expect_type(&key, &[ValueType::Int], span)?;
        }
        state.stack.push(AbstractValue::new(ValueType::Any));
        Ok(())
    }

    fn direct_struct_id(
        &self,
        state: &mut FlowState,
        span: Span,
    ) -> Result<Option<Identifier>, SemanticError> {
        pop_identifier(state, span)
    }

    fn define_struct(&mut self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let count = pop_count(state, span)?.ok_or(SemanticError {
            kind: SemanticErrorKind::InvalidFunctionHeader,
            span,
        })?;
        let field_ids = pop_identifiers(state, Some(count), span)?;
        let struct_id = pop_identifier(state, span)?;
        let field_ids = field_ids.ok_or(SemanticError {
            kind: SemanticErrorKind::InvalidIdentifier,
            span,
        })?;
        let struct_id = struct_id.ok_or(SemanticError {
            kind: SemanticErrorKind::InvalidIdentifier,
            span,
        })?;
        let unique: HashSet<_> = field_ids.iter().cloned().collect();
        if unique.len() != field_ids.len() {
            return Err(SemanticError {
                kind: SemanticErrorKind::DuplicateBinding,
                span,
            });
        }
        if let Some(existing) = self.structs.get(&struct_id) {
            if self.reanalysis && existing == &unique {
                return Ok(());
            }
            return Err(SemanticError {
                kind: SemanticErrorKind::DuplicateBinding,
                span,
            });
        }
        self.structs.insert(struct_id.clone(), unique);
        self.struct_methods.entry(struct_id).or_default();
        Ok(())
    }

    fn object_new(&self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let struct_id = self.direct_struct_id(state, span)?;
        let count = pop_count(state, span)?;
        pop_many(state, count, span)?;
        if let Some(id) = &struct_id {
            if !self.structs.contains_key(id) {
                return Err(SemanticError {
                    kind: SemanticErrorKind::UnknownBinding,
                    span,
                });
            }
        }
        if struct_id.is_none() {
            state.stack.make_opaque();
        }
        state.stack.push(AbstractValue {
            value_type: ValueType::Struct,
            integer: None,
            nominal_id: struct_id,
        });
        Ok(())
    }

    fn field_get(&self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let field = pop_identifier(state, span)?;
        let object = state.stack.pop(span)?;
        expect_type(&object, &[ValueType::Struct], span)?;
        if let (Some(struct_id), Some(field_id)) = (&object.nominal_id, field) {
            if !self
                .structs
                .get(struct_id)
                .is_some_and(|fields| fields.contains(&field_id))
            {
                return Err(SemanticError {
                    kind: SemanticErrorKind::UnknownBinding,
                    span,
                });
            }
        }
        state.stack.push(AbstractValue::new(ValueType::Any));
        Ok(())
    }

    fn field_set(&self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        state.stack.pop(span)?;
        let field = pop_identifier(state, span)?;
        let object = state.stack.pop(span)?;
        expect_type(&object, &[ValueType::Struct], span)?;
        if let (Some(struct_id), Some(field_id)) = (&object.nominal_id, field) {
            if !self
                .structs
                .get(struct_id)
                .is_some_and(|fields| fields.contains(&field_id))
            {
                return Err(SemanticError {
                    kind: SemanticErrorKind::UnknownBinding,
                    span,
                });
            }
        }
        Ok(())
    }

    fn define_method(&mut self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let function_id = pop_identifier(state, span)?.ok_or(SemanticError {
            kind: SemanticErrorKind::InvalidIdentifier,
            span,
        })?;
        let method_id = pop_identifier(state, span)?.ok_or(SemanticError {
            kind: SemanticErrorKind::InvalidIdentifier,
            span,
        })?;
        let struct_id = pop_identifier(state, span)?.ok_or(SemanticError {
            kind: SemanticErrorKind::InvalidIdentifier,
            span,
        })?;
        if !self.structs.contains_key(&struct_id) {
            return Err(SemanticError {
                kind: SemanticErrorKind::UnknownBinding,
                span,
            });
        }
        let key = (struct_id.clone(), method_id.clone());
        if let Some((existing_function, _)) = self.methods.get(&key) {
            if self.reanalysis && existing_function == &function_id {
                return Ok(());
            }
            return Err(SemanticError {
                kind: SemanticErrorKind::DuplicateBinding,
                span,
            });
        }
        let methods = self.struct_methods.entry(struct_id.clone()).or_default();
        if !methods.insert(method_id.clone()) {
            return Err(SemanticError {
                kind: SemanticErrorKind::DuplicateBinding,
                span,
            });
        }
        self.methods.insert(key, (function_id, span));
        Ok(())
    }

    fn define_interface(&mut self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let count = pop_count(state, span)?.ok_or(SemanticError {
            kind: SemanticErrorKind::InvalidFunctionHeader,
            span,
        })?;
        let methods = pop_identifiers(state, Some(count), span)?.ok_or(SemanticError {
            kind: SemanticErrorKind::InvalidIdentifier,
            span,
        })?;
        let interface_id = pop_identifier(state, span)?.ok_or(SemanticError {
            kind: SemanticErrorKind::InvalidIdentifier,
            span,
        })?;
        let unique: HashSet<_> = methods.iter().cloned().collect();
        if unique.len() != methods.len() {
            return Err(SemanticError {
                kind: SemanticErrorKind::DuplicateBinding,
                span,
            });
        }
        if let Some(existing) = self.interfaces.get(&interface_id) {
            if self.reanalysis && existing == &unique {
                return Ok(());
            }
            return Err(SemanticError {
                kind: SemanticErrorKind::DuplicateBinding,
                span,
            });
        }
        self.interfaces.insert(interface_id, unique);
        Ok(())
    }

    fn implement(&mut self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let interface_id = pop_identifier(state, span)?.ok_or(SemanticError {
            kind: SemanticErrorKind::InvalidIdentifier,
            span,
        })?;
        let struct_id = pop_identifier(state, span)?.ok_or(SemanticError {
            kind: SemanticErrorKind::InvalidIdentifier,
            span,
        })?;
        if self
            .implementations
            .iter()
            .any(|(existing_struct, existing_interface, _)| {
                existing_struct == &struct_id && existing_interface == &interface_id
            })
        {
            if self.reanalysis {
                return Ok(());
            }
            return Err(SemanticError {
                kind: SemanticErrorKind::DuplicateBinding,
                span,
            });
        }
        self.implementations.push((struct_id, interface_id, span));
        Ok(())
    }

    fn export(&mut self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let id = pop_identifier(state, span)?.ok_or(SemanticError {
            kind: SemanticErrorKind::InvalidIdentifier,
            span,
        })?;
        if !self.reanalysis {
            self.export_sites.push((id, span));
        }
        Ok(())
    }

    fn module_call(&self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let symbol_id = pop_identifier(state, span)?;
        let module = state.stack.pop(span)?;
        expect_type(&module, &[ValueType::Module], span)?;
        let count = pop_count(state, span)?;
        let args = pop_many(state, count, span)?;
        if symbol_id.is_none() || args.is_none() {
            state.stack.make_opaque();
        }
        state.stack.push(AbstractValue::new(ValueType::Any));
        Ok(())
    }

    fn method_call(&self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let method_id = pop_identifier(state, span)?;
        let receiver = state.stack.pop(span)?;
        expect_type(&receiver, &[ValueType::Struct], span)?;
        let mut result_type = ValueType::Any;
        if let (Some(struct_id), Some(method_id)) = (&receiver.nominal_id, method_id) {
            let function_id = self
                .methods
                .get(&(struct_id.clone(), method_id))
                .map(|(function_id, _)| function_id)
                .ok_or(SemanticError {
                    kind: SemanticErrorKind::UnknownBinding,
                    span,
                })?;
            if self
                .functions
                .get(function_id)
                .is_some_and(|signature| signature.kind == FunctionKind::Asynchronous)
            {
                result_type = ValueType::Task;
            }
        }
        let count = pop_count(state, span)?;
        let args = pop_many(state, count, span)?;
        if args.is_none() {
            state.stack.make_opaque();
        }
        state.stack.push(AbstractValue::new(result_type));
        Ok(())
    }

    fn native_call(&self, state: &mut FlowState, span: Span) -> Result<(), SemanticError> {
        let handle = state.stack.pop(span)?;
        expect_type(&handle, &[ValueType::Handle], span)?;
        let count = pop_count(state, span)?;
        let args = pop_many(state, count, span)?;
        if args.is_none() {
            state.stack.make_opaque();
        }
        state.stack.push(AbstractValue::new(ValueType::Any));
        Ok(())
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

fn accepts_arity(signature: &FunctionSignature, argument_count: usize) -> bool {
    if signature.variadic {
        argument_count >= signature.parameter_count.saturating_sub(1)
    } else {
        argument_count == signature.parameter_count
    }
}

fn numeric_result_type(left: ValueType, right: ValueType) -> ValueType {
    if left == ValueType::Int && right == ValueType::Int {
        ValueType::Int
    } else if matches!(left, ValueType::Int | ValueType::Float)
        && matches!(right, ValueType::Int | ValueType::Float)
    {
        ValueType::Float
    } else {
        ValueType::Any
    }
}

fn take_function_header(
    state: &mut FlowState,
    span: Span,
    kind: FunctionKind,
) -> Result<(Identifier, FunctionSignature, Vec<Identifier>), SemanticError> {
    let rest_code = pop_count(state, span)?.ok_or_else(|| SemanticError {
        kind: SemanticErrorKind::InvalidFunctionHeader,
        span,
    })?;
    let parameter_count = pop_count(state, span)?.ok_or_else(|| SemanticError {
        kind: SemanticErrorKind::InvalidFunctionHeader,
        span,
    })?;
    if rest_code != 0 && rest_code != parameter_count {
        return Err(SemanticError {
            kind: SemanticErrorKind::InvalidFunctionHeader,
            span,
        });
    }
    let parameters =
        pop_identifiers(state, Some(parameter_count), span)?.ok_or_else(|| SemanticError {
            kind: SemanticErrorKind::InvalidFunctionHeader,
            span,
        })?;
    let function_id = pop_identifier(state, span)?.ok_or_else(|| SemanticError {
        kind: SemanticErrorKind::InvalidFunctionHeader,
        span,
    })?;
    let unique: HashSet<_> = parameters.iter().cloned().collect();
    if unique.len() != parameters.len() {
        return Err(SemanticError {
            kind: SemanticErrorKind::DuplicateParameter,
            span,
        });
    }
    Ok((
        function_id,
        FunctionSignature {
            parameter_count,
            variadic: rest_code != 0,
            kind,
        },
        parameters,
    ))
}

fn expect_type(
    value: &AbstractValue,
    expected: &[ValueType],
    span: Span,
) -> Result<(), SemanticError> {
    if value.value_type == ValueType::Any || expected.contains(&value.value_type) {
        Ok(())
    } else {
        let expected_name = match expected {
            [only] => only.name(),
            _ => "one of the required types",
        };
        Err(type_mismatch(expected_name, value.value_type, span))
    }
}

fn expect_nonnegative_integer(value: &AbstractValue, span: Span) -> Result<(), SemanticError> {
    expect_type(value, &[ValueType::Int], span)?;
    if value
        .integer
        .as_ref()
        .is_some_and(|integer| integer.negative)
    {
        return Err(SemanticError {
            kind: SemanticErrorKind::TypeMismatch {
                expected: "nonnegative Int",
                found: "negative Int",
            },
            span,
        });
    }
    Ok(())
}

fn type_mismatch(expected: &'static str, found: ValueType, span: Span) -> SemanticError {
    SemanticError {
        kind: SemanticErrorKind::TypeMismatch {
            expected,
            found: found.name(),
        },
        span,
    }
}

fn pop_identifier(state: &mut FlowState, span: Span) -> Result<Option<Identifier>, SemanticError> {
    let value = state.stack.pop(span)?;
    expect_type(&value, &[ValueType::Int], span)?;
    match value.integer {
        Some(integer) => integer.identifier().map(Some).ok_or_else(|| SemanticError {
            kind: SemanticErrorKind::InvalidIdentifier,
            span,
        }),
        None => Ok(None),
    }
}

fn pop_count(state: &mut FlowState, span: Span) -> Result<Option<usize>, SemanticError> {
    let value = state.stack.pop(span)?;
    expect_type(&value, &[ValueType::Int], span)?;
    match value.integer {
        Some(integer) => integer.to_usize().map(Some).ok_or_else(|| SemanticError {
            kind: SemanticErrorKind::TypeMismatch {
                expected: "nonnegative Int count in range",
                found: "out-of-range Int",
            },
            span,
        }),
        None => Ok(None),
    }
}

fn pop_many(
    state: &mut FlowState,
    count: Option<usize>,
    span: Span,
) -> Result<Option<Vec<AbstractValue>>, SemanticError> {
    let Some(count) = count else {
        state.stack.make_opaque();
        return Ok(None);
    };
    if state.stack.opaque_prefix && count > state.stack.values.len() {
        state.stack.make_opaque();
        return Ok(None);
    }
    if count > state.stack.values.len() && !state.stack.opaque_prefix {
        return Err(SemanticError {
            kind: SemanticErrorKind::StackUnderflow,
            span,
        });
    }
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(state.stack.pop(span)?);
    }
    Ok(Some(values))
}

fn pop_identifiers(
    state: &mut FlowState,
    count: Option<usize>,
    span: Span,
) -> Result<Option<Vec<Identifier>>, SemanticError> {
    let Some(count) = count else {
        state.stack.make_opaque();
        return Ok(None);
    };
    if state.stack.opaque_prefix && count > state.stack.values.len() {
        state.stack.make_opaque();
        return Ok(None);
    }
    if count > state.stack.values.len() && !state.stack.opaque_prefix {
        return Err(SemanticError {
            kind: SemanticErrorKind::StackUnderflow,
            span,
        });
    }
    let mut ids = Vec::with_capacity(count);
    for _ in 0..count {
        if let Some(id) = pop_identifier(state, span)? {
            ids.push(id);
        } else {
            state.stack.make_opaque();
            return Ok(None);
        }
    }
    Ok(Some(ids))
}

fn lookup_binding<'a>(
    scopes: &'a [HashMap<Identifier, Binding>],
    id: &Identifier,
) -> Option<&'a Binding> {
    scopes.iter().rev().find_map(|scope| scope.get(id))
}

fn lookup_binding_mut<'a>(
    scopes: &'a mut [HashMap<Identifier, Binding>],
    id: &Identifier,
) -> Option<&'a mut Binding> {
    scopes.iter_mut().rev().find_map(|scope| scope.get_mut(id))
}

fn is_compile_time_opcode(opcode: InstructionOpcode) -> bool {
    matches!(
        opcode,
        InstructionOpcode::Extended(0x70 | 0x74 | 0x76 | 0x77 | 0x91)
    )
}

fn enqueue_flow(state: FlowState, seen: &mut Vec<FlowState>, pending: &mut Vec<FlowState>) {
    if state.reachable && !seen.contains(&state) {
        seen.push(state.clone());
        pending.push(state);
    }
}

fn push_unique_flow(state: FlowState, states: &mut Vec<FlowState>) {
    if !states.contains(&state) {
        states.push(state);
    }
}

fn reset_binding_type(state: &mut FlowState, id: &Identifier) {
    if let Some(binding) = state
        .scopes
        .iter_mut()
        .rev()
        .find_map(|scope| scope.get_mut(id))
    {
        binding.value_type = ValueType::Any;
    }
}

fn same_stack(left: &AbstractStack, right: &AbstractStack) -> bool {
    left.opaque_prefix == right.opaque_prefix
        && left.values.len() == right.values.len()
        && left.values.iter().zip(&right.values).all(|(left, right)| {
            left.value_type == ValueType::Any
                || right.value_type == ValueType::Any
                || (left.value_type == right.value_type
                    && (left.value_type != ValueType::Struct
                        || left.nominal_id.is_none()
                        || right.nominal_id.is_none()
                        || left.nominal_id == right.nominal_id))
        })
}

fn join_flows(
    mut left: FlowState,
    right: FlowState,
    span: Span,
) -> Result<FlowState, SemanticError> {
    if !left.reachable {
        return Ok(right);
    }
    if !right.reachable {
        return Ok(left);
    }
    if !same_stack(&left.stack, &right.stack) || left.scopes.len() != right.scopes.len() {
        return Err(SemanticError {
            kind: SemanticErrorKind::IncompatibleStackJoin,
            span,
        });
    }
    if left.captures != right.captures {
        return Err(SemanticError {
            kind: SemanticErrorKind::InvalidCaptureState,
            span,
        });
    }
    for (index, right_scope) in right.scopes.iter().enumerate() {
        let left_scope = &mut left.scopes[index];
        left_scope.retain(|id, _| right_scope.contains_key(id));
        for (id, binding) in left_scope.iter_mut() {
            let other = right_scope.get(id).expect("retained binding exists");
            binding.mutable &= other.mutable;
            binding.value_type = binding.value_type.join(other.value_type);
        }
    }
    for (left_value, right_value) in left.stack.values.iter_mut().zip(&right.stack.values) {
        *left_value = left_value.joined(right_value);
    }
    Ok(left)
}
