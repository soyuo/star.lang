//! IR-to-`.starc` compiler and source-location emitter.

use std::collections::{HashMap, HashSet};
use std::fmt;

use num_bigint::BigInt;
use num_traits::{ToPrimitive, Zero};
use sha2::{Digest, Sha256};
use star_common::Span;

mod intrinsics;
pub use intrinsics::IrOp;
mod verifier;
pub use verifier::{
    verify_bytecode, Capability, VerificationError, VerificationLimits, VerificationPolicy,
    VerifiedProgram,
};

/// Function-local block identifier.
pub type BlockId = u32;

/// One IR operation. Source operations retain symbolic identities; generated operations are explicit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operation {
    /// A nonstructural source instruction represented by its IR operation.
    Source(IrOp),
    /// Push an arbitrary-precision integer constant.
    PushInt(BigInt),
    /// Enter a lexical scope.
    ScopeEnter,
    /// Exit a lexical scope.
    ScopeExit,
    /// Create hidden iterator state for a `FOR` loop.
    ForInit { loop_state: u32 },
}

/// An operation and the original source range that caused it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IrInstruction {
    pub operation: Operation,
    pub span: Span,
}

/// A function-local control-flow terminator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Terminator {
    Jump {
        target: BlockId,
        unwind_scopes: u32,
        span: Span,
    },
    BranchIfFalse {
        false_target: BlockId,
        true_target: BlockId,
        span: Span,
    },
    ForNext {
        loop_state: u32,
        body_target: BlockId,
        exit_target: BlockId,
        span: Span,
    },
    ReturnTop {
        unwind_to_frame_base: u32,
        span: Span,
    },
    /// Rethrow the active catch-all exception to an enclosing handler.
    Rethrow {
        span: Span,
    },
    Halt {
        span: Span,
    },
    Unreachable {
        span: Span,
    },
}

impl Terminator {
    fn span(&self) -> Span {
        match self {
            Self::Jump { span, .. }
            | Self::BranchIfFalse { span, .. }
            | Self::ForNext { span, .. }
            | Self::ReturnTop { span, .. }
            | Self::Rethrow { span }
            | Self::Halt { span }
            | Self::Unreachable { span } => *span,
        }
    }
}

/// A basic block with one terminator and a declared incoming scope depth.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BasicBlock {
    pub id: BlockId,
    pub entry_scope_depth: u32,
    pub operations: Vec<IrInstruction>,
    pub terminator: Terminator,
}

/// A function-local catch-all handler range. The protected interval starts at
/// `protected_start` and ends at `protected_end` (exclusive), or at function
/// end when `protected_end` is `None`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExceptionRegionIr {
    pub protected_start: BlockId,
    pub protected_end: Option<BlockId>,
    pub handler: BlockId,
    /// Operand-stack depth restored before control enters the handler.
    pub stack_depth: u32,
}

/// One Star function in stack-based control-flow-graph IR.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionIr {
    /// `None` is reserved for the module initializer.
    pub id: Option<BigInt>,
    pub parameter_ids: Vec<BigInt>,
    /// Zero means fixed arity; otherwise this equals the parameter count.
    pub rest_parameter_code: u32,
    pub asynchronous: bool,
    pub entry: BlockId,
    /// Maximum operand-stack depth computed by semantic analysis.
    pub max_stack: u32,
    pub blocks: Vec<BasicBlock>,
    /// Catch-all regions, serialized into this function's ExceptionTable records.
    pub exception_regions: Vec<ExceptionRegionIr>,
}

/// The single-module IR accepted by this compiler slice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleIr {
    pub initializer: FunctionIr,
    /// Function records are sorted by numeric function ID during emission.
    pub functions: Vec<FunctionIr>,
}

/// Compiler error for malformed IR or a value that cannot fit the `.starc` format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompileError {
    message: String,
}

impl CompileError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CompileError {}

/// Compile one module to a version 1.0 `.starc` container.
///
/// `module_name` and `source_file` are written to the module table and DebugInfo section.
/// This stage consumes validated IR; it does not parse source or perform semantic analysis.
pub fn compile(
    module: &ModuleIr,
    module_name: &str,
    source_file: &str,
) -> Result<Vec<u8>, CompileError> {
    validate_function(&module.initializer, true)?;
    if !module
        .initializer
        .blocks
        .iter()
        .any(|block| matches!(&block.terminator, Terminator::Halt { .. }))
    {
        return Err(CompileError::new(
            "module initializer has no HALT terminator",
        ));
    }

    let mut ordered_functions: Vec<&FunctionIr> = module.functions.iter().collect();
    ordered_functions.sort_by(|left, right| left.id.cmp(&right.id));
    let mut seen_function_ids = HashSet::new();
    for function in &ordered_functions {
        validate_function(function, false)?;
        let id = function
            .id
            .as_ref()
            .ok_or_else(|| CompileError::new("non-initializer function is missing its ID"))?;
        if !seen_function_ids.insert(id.clone()) {
            return Err(CompileError::new("duplicate function ID in module"));
        }
    }

    let mut pool = ConstantPool::default();
    let module_name_index = pool.intern_string(module_name)?;
    let source_file_index = pool.intern_string(source_file)?;

    let mut functions = Vec::with_capacity(ordered_functions.len() + 1);
    functions.push(FunctionMetadata::new(&module.initializer, true, &mut pool)?);
    for function in ordered_functions {
        functions.push(FunctionMetadata::new(function, false, &mut pool)?);
    }

    let mut code = Vec::new();
    let mut records: Vec<FunctionRecordData> = Vec::with_capacity(functions.len());
    let mut locations = Vec::new();
    let mut exception_records = Vec::new();
    for (function_index, metadata) in functions.iter().enumerate() {
        let function_index = checked_u32(function_index, "function index")?;
        let function_offset = checked_u64(code.len(), "function code offset")?;
        let encoded = encode_function(metadata.function, metadata.is_initializer, &mut pool)?;
        let function_length = checked_u64(encoded.code.len(), "function code length")?;
        exception_records.extend(encoded.exception_regions.into_iter().map(|region| {
            ExceptionRecordData {
                function_index,
                protected_start: region.protected_start,
                protected_end: region.protected_end,
                handler_offset: region.handler_offset,
                stack_depth: region.stack_depth,
            }
        }));
        for location in encoded.locations {
            let absolute_code_offset = function_offset
                .checked_add(checked_u64(location.offset, "debug code offset")?)
                .ok_or_else(|| CompileError::new("debug code offset overflow"))?;
            locations.push(DebugLocation {
                function_index,
                code_offset: absolute_code_offset,
                span: location.span,
            });
        }
        code.extend_from_slice(&encoded.code);
        records.push(FunctionRecordData {
            id_index: metadata.id_index,
            parameter_indexes: metadata.parameter_indexes.clone(),
            asynchronous: metadata.function.asynchronous,
            is_initializer: metadata.is_initializer,
            rest_parameter_code: metadata.function.rest_parameter_code,
            max_stack: metadata.function.max_stack,
            code_offset: function_offset,
            code_length: function_length,
        });
    }

    let constant_pool = pool.encode()?;
    let module_table = encode_module_table(module_name_index, records.len())?;
    let function_table = encode_function_table(&records)?;
    let exception_table = encode_exception_table(&exception_records)?;
    let debug_info = encode_debug_info(source_file_index, &locations)?;
    let sections = vec![
        (1, true, constant_pool),
        (2, true, module_table),
        (3, true, function_table),
        (4, true, code),
        (5, true, 0_u32.to_le_bytes().to_vec()),
        (6, true, exception_table),
        (7, false, debug_info),
        (u32::MAX, true, vec![0; 32]),
    ];
    assemble_container(sections)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum ConstantKey {
    Int(BigInt),
    String(String),
}

#[derive(Clone, Debug)]
struct Constant {
    tag: u8,
    payload: Vec<u8>,
}

#[derive(Default)]
struct ConstantPool {
    entries: Vec<Constant>,
    indexes: HashMap<ConstantKey, u32>,
}

impl ConstantPool {
    fn intern_int(&mut self, value: &BigInt) -> Result<u32, CompileError> {
        let key = ConstantKey::Int(value.clone());
        if let Some(index) = self.indexes.get(&key) {
            return Ok(*index);
        }
        self.insert(key, 0x02, encode_sleb128(value)?)
    }

    fn intern_string(&mut self, value: &str) -> Result<u32, CompileError> {
        let key = ConstantKey::String(value.to_owned());
        if let Some(index) = self.indexes.get(&key) {
            return Ok(*index);
        }
        self.insert(key, 0x04, value.as_bytes().to_vec())
    }

    fn insert(&mut self, key: ConstantKey, tag: u8, payload: Vec<u8>) -> Result<u32, CompileError> {
        let index = checked_u32(self.entries.len(), "constant-pool index")?;
        self.entries.push(Constant { tag, payload });
        self.indexes.insert(key, index);
        Ok(index)
    }

    fn encode(&self) -> Result<Vec<u8>, CompileError> {
        let mut output = Vec::new();
        write_u32(
            &mut output,
            checked_u32(self.entries.len(), "constant-pool count")?,
        );
        for entry in &self.entries {
            output.push(entry.tag);
            write_uleb128(
                &mut output,
                checked_u64(entry.payload.len(), "constant payload length")?,
            );
            output.extend_from_slice(&entry.payload);
        }
        Ok(output)
    }
}

struct FunctionMetadata<'a> {
    function: &'a FunctionIr,
    is_initializer: bool,
    id_index: Option<u32>,
    parameter_indexes: Vec<u32>,
}

impl<'a> FunctionMetadata<'a> {
    fn new(
        function: &'a FunctionIr,
        is_initializer: bool,
        pool: &mut ConstantPool,
    ) -> Result<Self, CompileError> {
        let id_index = function
            .id
            .as_ref()
            .map(|id| pool.intern_int(id))
            .transpose()?;
        let parameter_indexes = function
            .parameter_ids
            .iter()
            .map(|id| pool.intern_int(id))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            function,
            is_initializer,
            id_index,
            parameter_indexes,
        })
    }
}

struct EncodedLocation {
    offset: usize,
    span: Span,
}

struct EncodedFunction {
    code: Vec<u8>,
    locations: Vec<EncodedLocation>,
    exception_regions: Vec<EncodedExceptionRegion>,
}

#[derive(Clone)]
struct EncodedExceptionRegion {
    protected_start: u64,
    protected_end: u64,
    handler_offset: u64,
    stack_depth: u32,
}

struct ExceptionRecordData {
    function_index: u32,
    protected_start: u64,
    protected_end: u64,
    handler_offset: u64,
    stack_depth: u32,
}

#[derive(Clone, Copy)]
struct DebugLocation {
    function_index: u32,
    code_offset: u64,
    span: Span,
}

fn validate_function(function: &FunctionIr, is_initializer: bool) -> Result<(), CompileError> {
    if is_initializer {
        if function.id.is_some()
            || !function.parameter_ids.is_empty()
            || function.rest_parameter_code != 0
            || function.asynchronous
        {
            return Err(CompileError::new("invalid module initializer descriptor"));
        }
    } else {
        let id = function
            .id
            .as_ref()
            .ok_or_else(|| CompileError::new("function descriptor is missing its ID"))?;
        if id < &BigInt::zero() {
            return Err(CompileError::new("function ID must be nonnegative"));
        }
    }
    let parameter_count = checked_u32(function.parameter_ids.len(), "parameter count")?;
    if function.rest_parameter_code != 0 && function.rest_parameter_code != parameter_count {
        return Err(CompileError::new(
            "rest_parameter_code must be zero or equal parameter_count",
        ));
    }
    let mut parameter_ids = HashSet::new();
    for id in &function.parameter_ids {
        if id < &BigInt::zero() {
            return Err(CompileError::new("parameter ID must be nonnegative"));
        }
        if !parameter_ids.insert(id) {
            return Err(CompileError::new("duplicate parameter ID"));
        }
    }
    if function.blocks.is_empty() {
        return Err(CompileError::new("function has no basic blocks"));
    }

    let mut blocks = HashMap::new();
    let mut loop_states = HashSet::new();
    for block in &function.blocks {
        if blocks.insert(block.id, block).is_some() {
            return Err(CompileError::new("duplicate basic-block ID"));
        }
        for instruction in &block.operations {
            validate_span(instruction.span)?;
            if let Operation::ForInit { loop_state } = &instruction.operation {
                if !loop_states.insert(*loop_state) {
                    return Err(CompileError::new("duplicate FOR loop-state ID"));
                }
            }
        }
        validate_span(block.terminator.span())?;
        match &block.terminator {
            Terminator::Halt { .. } if !is_initializer => {
                return Err(CompileError::new(
                    "HALT is only valid in a module initializer",
                ));
            }
            Terminator::ReturnTop { .. } if is_initializer => {
                return Err(CompileError::new(
                    "module initializer must terminate with HALT",
                ));
            }
            _ => {}
        }
    }
    if !blocks.contains_key(&function.entry) {
        return Err(CompileError::new("entry block does not exist"));
    }
    for region in &function.exception_regions {
        require_target(&blocks, region.protected_start)?;
        if let Some(end) = region.protected_end {
            require_target(&blocks, end)?;
        }
        require_target(&blocks, region.handler)?;
        if region.stack_depth > function.max_stack {
            return Err(CompileError::new(
                "exception stack depth exceeds function max_stack",
            ));
        }
    }
    for block in &function.blocks {
        match &block.terminator {
            Terminator::Jump { target, .. } => require_target(&blocks, *target)?,
            Terminator::BranchIfFalse {
                false_target,
                true_target,
                ..
            } => {
                require_target(&blocks, *false_target)?;
                require_target(&blocks, *true_target)?;
            }
            Terminator::ForNext {
                loop_state,
                body_target,
                exit_target,
                ..
            } => {
                require_target(&blocks, *body_target)?;
                require_target(&blocks, *exit_target)?;
                if !loop_states.contains(loop_state) {
                    return Err(CompileError::new(
                        "FOR_NEXT refers to an unknown loop state",
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn require_target(
    blocks: &HashMap<BlockId, &BasicBlock>,
    target: BlockId,
) -> Result<(), CompileError> {
    if blocks.contains_key(&target) {
        Ok(())
    } else {
        Err(CompileError::new("control-flow target does not exist"))
    }
}

fn validate_span(span: Span) -> Result<(), CompileError> {
    if span.start.byte_offset > span.end.byte_offset
        || span.start.line == 0
        || span.start.column == 0
        || span.end.line == 0
        || span.end.column == 0
    {
        return Err(CompileError::new("invalid source span"));
    }
    checked_u64(span.start.byte_offset, "source byte offset")?;
    checked_u64(span.end.byte_offset, "source byte offset")?;
    checked_u32(span.start.line, "source line")?;
    checked_u32(span.start.column, "source byte column")?;
    checked_u32(span.end.line, "source line")?;
    checked_u32(span.end.column, "source byte column")?;
    Ok(())
}

fn ordered_blocks(function: &FunctionIr) -> Vec<&BasicBlock> {
    let mut result = Vec::with_capacity(function.blocks.len());
    if let Some(entry) = function
        .blocks
        .iter()
        .find(|block| block.id == function.entry)
    {
        result.push(entry);
    }
    result.extend(
        function
            .blocks
            .iter()
            .filter(|block| block.id != function.entry),
    );
    result
}

fn operation_size(operation: &Operation) -> usize {
    match operation {
        Operation::Source(_) => 3,
        Operation::PushInt(_) | Operation::ForInit { .. } => 5,
        Operation::ScopeEnter | Operation::ScopeExit => 1,
    }
}

fn terminator_size(terminator: &Terminator) -> usize {
    match terminator {
        Terminator::Jump { .. } | Terminator::BranchIfFalse { .. } => 9,
        Terminator::ForNext { .. } => 13,
        Terminator::ReturnTop { .. } => 5,
        Terminator::Halt { .. } | Terminator::Unreachable { .. } | Terminator::Rethrow { .. } => 1,
    }
}

fn encode_function(
    function: &FunctionIr,
    is_initializer: bool,
    pool: &mut ConstantPool,
) -> Result<EncodedFunction, CompileError> {
    let blocks = ordered_blocks(function);
    let mut offsets = HashMap::new();
    let mut next_offset = 0_usize;
    for block in &blocks {
        offsets.insert(
            block.id,
            checked_u32(next_offset, "function-relative block offset")?,
        );
        for operation in &block.operations {
            next_offset = next_offset
                .checked_add(operation_size(&operation.operation))
                .ok_or_else(|| CompileError::new("function code length overflow"))?;
        }
        next_offset = next_offset
            .checked_add(terminator_size(&block.terminator))
            .ok_or_else(|| CompileError::new("function code length overflow"))?;
    }
    checked_u32(next_offset, "function code length")?;

    let mut code = Vec::with_capacity(next_offset);
    let mut locations = Vec::new();
    let mut exception_regions = Vec::with_capacity(function.exception_regions.len());
    for region in &function.exception_regions {
        let protected_start = u64::from(block_offset(&offsets, region.protected_start)?);
        let protected_end = match region.protected_end {
            Some(end) => u64::from(block_offset(&offsets, end)?),
            None => checked_u64(next_offset, "exception range end")?,
        };
        let handler_offset = u64::from(block_offset(&offsets, region.handler)?);
        if protected_start >= protected_end {
            return Err(CompileError::new(
                "exception protected range must be nonempty and forward ordered",
            ));
        }
        if (protected_start..protected_end).contains(&handler_offset) {
            return Err(CompileError::new(
                "exception handler must be outside its protected range",
            ));
        }
        exception_regions.push(EncodedExceptionRegion {
            protected_start,
            protected_end,
            handler_offset,
            stack_depth: region.stack_depth,
        });
    }
    let mut ranges = exception_regions
        .iter()
        .map(|region| (region.protected_start, region.protected_end))
        .collect::<Vec<_>>();
    ranges.sort_by_key(|(start, end)| (*start, std::cmp::Reverse(*end)));
    let mut nesting = Vec::<(u64, u64)>::new();
    for (start, end) in ranges {
        while nesting
            .last()
            .is_some_and(|(_, parent_end)| start >= *parent_end)
        {
            nesting.pop();
        }
        if nesting.last().is_some_and(|(parent_start, parent_end)| {
            (start == *parent_start && end == *parent_end) || end > *parent_end
        }) {
            return Err(CompileError::new(
                "exception ranges overlap without proper nesting",
            ));
        }
        nesting.push((start, end));
    }
    exception_regions.sort_by_key(|region| (region.protected_start, region.protected_end));
    for block in blocks {
        for operation in &block.operations {
            locations.push(EncodedLocation {
                offset: code.len(),
                span: operation.span,
            });
            encode_operation(&mut code, &operation.operation, pool)?;
        }
        locations.push(EncodedLocation {
            offset: code.len(),
            span: block.terminator.span(),
        });
        encode_terminator(&mut code, &block.terminator, &offsets, is_initializer)?;
    }
    if code.len() != next_offset {
        return Err(CompileError::new("internal bytecode sizing mismatch"));
    }
    Ok(EncodedFunction {
        code,
        locations,
        exception_regions,
    })
}

fn encode_operation(
    output: &mut Vec<u8>,
    operation: &Operation,
    pool: &mut ConstantPool,
) -> Result<(), CompileError> {
    match operation {
        Operation::Source(op) => {
            output.push(0xE0);
            write_u16(output, op.intrinsic_id());
        }
        Operation::PushInt(value) => {
            output.push(0x01);
            write_u32(output, pool.intern_int(value)?);
        }
        Operation::ScopeEnter => output.push(0xE5),
        Operation::ScopeExit => output.push(0xE6),
        Operation::ForInit { loop_state } => {
            output.push(0xE3);
            write_u32(output, *loop_state);
        }
    }
    Ok(())
}

fn encode_terminator(
    output: &mut Vec<u8>,
    terminator: &Terminator,
    offsets: &HashMap<BlockId, u32>,
    is_initializer: bool,
) -> Result<(), CompileError> {
    match terminator {
        Terminator::Jump {
            target,
            unwind_scopes,
            ..
        } => {
            output.push(0xE1);
            write_u32(output, block_offset(offsets, *target)?);
            write_u32(output, *unwind_scopes);
        }
        Terminator::BranchIfFalse {
            false_target,
            true_target,
            ..
        } => {
            output.push(0xE2);
            write_u32(output, block_offset(offsets, *false_target)?);
            write_u32(output, block_offset(offsets, *true_target)?);
        }
        Terminator::ForNext {
            loop_state,
            body_target,
            exit_target,
            ..
        } => {
            output.push(0xE4);
            write_u32(output, *loop_state);
            write_u32(output, block_offset(offsets, *body_target)?);
            write_u32(output, block_offset(offsets, *exit_target)?);
        }
        Terminator::ReturnTop {
            unwind_to_frame_base,
            ..
        } => {
            if is_initializer {
                return Err(CompileError::new("module initializer cannot return"));
            }
            output.push(0xE7);
            write_u32(output, *unwind_to_frame_base);
        }
        Terminator::Halt { .. } => {
            if !is_initializer {
                return Err(CompileError::new(
                    "HALT is only valid in a module initializer",
                ));
            }
            output.push(0xFF);
        }
        Terminator::Rethrow { .. } => output.push(0xE9),
        Terminator::Unreachable { .. } => output.push(0xE8),
    }
    Ok(())
}

fn block_offset(offsets: &HashMap<BlockId, u32>, target: BlockId) -> Result<u32, CompileError> {
    offsets
        .get(&target)
        .copied()
        .ok_or_else(|| CompileError::new("control-flow target does not exist"))
}

struct FunctionRecordData {
    id_index: Option<u32>,
    parameter_indexes: Vec<u32>,
    asynchronous: bool,
    is_initializer: bool,
    rest_parameter_code: u32,
    max_stack: u32,
    code_offset: u64,
    code_length: u64,
}

fn encode_exception_table(records: &[ExceptionRecordData]) -> Result<Vec<u8>, CompileError> {
    let mut output = Vec::new();
    write_u32(
        &mut output,
        checked_u32(records.len(), "exception handler count")?,
    );
    for record in records {
        write_u32(&mut output, record.function_index);
        write_u64(&mut output, record.protected_start);
        write_u64(&mut output, record.protected_end);
        write_u64(&mut output, record.handler_offset);
        write_u32(&mut output, record.stack_depth);
        write_u32(&mut output, u32::MAX); // catch-all handler
        write_u32(&mut output, 0); // version 1.0 flags
    }
    Ok(output)
}

fn encode_module_table(
    module_name_index: u32,
    function_count: usize,
) -> Result<Vec<u8>, CompileError> {
    let function_count = checked_u32(function_count, "module function count")?;
    let mut output = Vec::new();
    write_u32(&mut output, 1); // module_count
    write_u32(&mut output, 0); // entry_module_index
    write_u32(&mut output, module_name_index);
    write_u32(&mut output, 0); // initializer_function_index
    write_u32(&mut output, 0); // first_function_index
    write_u32(&mut output, function_count);
    write_u32(&mut output, 0); // flags
    write_u32(&mut output, 0); // global_count
    write_u32(&mut output, 0); // import_count
    write_u32(&mut output, 0); // export_count
    Ok(output)
}

fn encode_function_table(records: &[FunctionRecordData]) -> Result<Vec<u8>, CompileError> {
    let mut output = Vec::new();
    write_u32(&mut output, checked_u32(records.len(), "function count")?);
    for record in records {
        write_u32(&mut output, 0); // module_index
        write_u32(&mut output, record.id_index.unwrap_or(u32::MAX));
        write_u32(&mut output, u32::MAX); // unnamed function
        let flags = (if record.asynchronous { 1 } else { 0 })
            | (if record.is_initializer { 1 << 1 } else { 0 });
        write_u32(&mut output, flags);
        write_u32(
            &mut output,
            checked_u32(record.parameter_indexes.len(), "parameter count")?,
        );
        write_u32(&mut output, record.rest_parameter_code);
        for index in &record.parameter_indexes {
            write_u32(&mut output, *index);
        }
        write_u32(&mut output, record.max_stack);
        write_u64(&mut output, record.code_offset);
        write_u64(&mut output, record.code_length);
    }
    Ok(output)
}

fn encode_debug_info(
    source_file_index: u32,
    locations: &[DebugLocation],
) -> Result<Vec<u8>, CompileError> {
    let mut output = Vec::new();
    write_u32(&mut output, 1); // source_file_count
    write_u32(&mut output, source_file_index);
    write_u32(
        &mut output,
        checked_u32(locations.len(), "debug location count")?,
    );
    for location in locations {
        write_u32(&mut output, location.function_index);
        write_u64(&mut output, location.code_offset);
        write_u32(&mut output, 0); // source_file_index
        write_u64(
            &mut output,
            checked_u64(location.span.start.byte_offset, "source byte offset")?,
        );
        write_u64(
            &mut output,
            checked_u64(location.span.end.byte_offset, "source byte offset")?,
        );
        write_u32(
            &mut output,
            checked_u32(location.span.start.line, "source line")?,
        );
        write_u32(
            &mut output,
            checked_u32(location.span.start.column, "source byte column")?,
        );
        write_u32(
            &mut output,
            checked_u32(location.span.end.line, "source line")?,
        );
        write_u32(
            &mut output,
            checked_u32(location.span.end.column, "source byte column")?,
        );
    }
    Ok(output)
}

fn assemble_container(sections: Vec<(u32, bool, Vec<u8>)>) -> Result<Vec<u8>, CompileError> {
    let section_count = checked_u32(sections.len(), "section count")?;
    let directory_size = sections
        .len()
        .checked_mul(24)
        .ok_or_else(|| CompileError::new("section directory size overflow"))?;
    let mut payload_offset = 16_usize
        .checked_add(directory_size)
        .ok_or_else(|| CompileError::new("section payload offset overflow"))?;
    let mut directory = Vec::with_capacity(directory_size);
    for (id, required, payload) in &sections {
        write_u32(&mut directory, *id);
        write_u32(&mut directory, u32::from(*required));
        write_u64(
            &mut directory,
            checked_u64(payload_offset, "section payload offset")?,
        );
        write_u64(
            &mut directory,
            checked_u64(payload.len(), "section payload length")?,
        );
        payload_offset = payload_offset
            .checked_add(payload.len())
            .ok_or_else(|| CompileError::new("section payload offset overflow"))?;
    }

    let mut output = Vec::with_capacity(payload_offset);
    output.extend_from_slice(b"STAR");
    write_u16(&mut output, 1);
    write_u16(&mut output, 0);
    write_u32(&mut output, 0); // file flags
    write_u32(&mut output, section_count);
    output.extend_from_slice(&directory);
    let mut checksum_range = None;
    for (id, _, payload) in sections {
        if id == u32::MAX {
            let start = output.len();
            checksum_range = Some(start..start + payload.len());
        }
        output.extend_from_slice(&payload);
    }
    let checksum_range =
        checksum_range.ok_or_else(|| CompileError::new("checksum section is missing"))?;
    if checksum_range.len() != 32 {
        return Err(CompileError::new("checksum section must contain 32 bytes"));
    }
    let digest = Sha256::digest(&output);
    output[checksum_range].copy_from_slice(&digest);
    Ok(output)
}

fn encode_sleb128(value: &BigInt) -> Result<Vec<u8>, CompileError> {
    let mut remaining = value.clone();
    let mask = BigInt::from(0x7F_u8);
    let minus_one = BigInt::from(-1);
    let mut output = Vec::new();
    loop {
        let byte = (&remaining & &mask)
            .to_u8()
            .ok_or_else(|| CompileError::new("failed to encode signed integer constant"))?;
        remaining >>= 7_usize;
        let done = (remaining.is_zero() && byte & 0x40 == 0)
            || (remaining == minus_one && byte & 0x40 != 0);
        output.push(if done { byte } else { byte | 0x80 });
        if done {
            return Ok(output);
        }
    }
}

fn write_uleb128(output: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            return;
        }
    }
}

fn checked_u32(value: usize, field: &str) -> Result<u32, CompileError> {
    u32::try_from(value).map_err(|_| CompileError::new(format!("{field} exceeds u32")))
}

fn checked_u64(value: usize, field: &str) -> Result<u64, CompileError> {
    u64::try_from(value).map_err(|_| CompileError::new(format!("{field} exceeds u64")))
}

fn write_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn write_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn write_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}
