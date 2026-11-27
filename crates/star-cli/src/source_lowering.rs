//! Lower the parsed source subset into the compiler's block-based IR.

use num_bigint::BigInt;
use star_common::Span;
use star_compiler::{BasicBlock, FunctionIr, IrInstruction, IrOp, ModuleIr, Operation, Terminator};
use star_interpreter::semantic::{resolve_functions, ResolvedFunction};
use star_parser::{FunctionKind, Program, Statement};
use std::collections::HashSet;
use std::fmt;

#[derive(Clone, Debug)]
pub struct LoweringError {
    pub span: Span,
    pub message: String,
}

impl fmt::Display for LoweringError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at line {}, column {} (byte offset {})",
            self.message, self.span.start.line, self.span.start.column, self.span.start.byte_offset
        )
    }
}

impl std::error::Error for LoweringError {}

/// Lowers a semantically valid source program using `max_stack` as the
/// declared per-function operand-stack ceiling.
pub fn lower(program: &Program, max_stack: u32) -> Result<ModuleIr, LoweringError> {
    let functions = resolve_functions(program).map_err(|error| LoweringError {
        span: error.span,
        message: error.to_string(),
    })?;
    let initializer = lower_function(
        &program.statements,
        None,
        &functions,
        max_stack,
        program
            .statements
            .first()
            .map(statement_span)
            .unwrap_or_else(empty_span),
    )?;
    let mut lowered_functions = Vec::with_capacity(functions.len());
    for function in &functions {
        let span = function.span;
        lowered_functions.push(lower_function(
            function.body.as_slice(),
            Some(function),
            &functions,
            max_stack,
            span,
        )?);
    }
    Ok(ModuleIr {
        initializer,
        functions: lowered_functions,
    })
}

#[derive(Clone, Copy)]
struct LoopTargets {
    break_target: u32,
    break_scope_depth: u32,
    continue_target: u32,
    continue_scope_depth: u32,
    has_break: bool,
}

struct FunctionBuilder {
    blocks: Vec<BasicBlock>,
    current: Option<u32>,
    scope_depth: u32,
    next_loop_state: u32,
    loops: Vec<LoopTargets>,
    max_stack: u32,
}

impl FunctionBuilder {
    fn new(max_stack: u32, span: Span) -> Self {
        Self {
            blocks: vec![BasicBlock {
                id: 0,
                entry_scope_depth: 0,
                operations: Vec::new(),
                terminator: Terminator::Unreachable { span },
            }],
            current: Some(0),
            scope_depth: 0,
            next_loop_state: 0,
            loops: Vec::new(),
            max_stack,
        }
    }

    fn new_block(&mut self, scope_depth: u32, span: Span) -> u32 {
        let id = u32::try_from(self.blocks.len()).expect("basic block count exceeds u32");
        self.blocks.push(BasicBlock {
            id,
            entry_scope_depth: scope_depth,
            operations: Vec::new(),
            terminator: Terminator::Unreachable { span },
        });
        id
    }

    fn select(&mut self, block: u32, scope_depth: u32) {
        self.current = Some(block);
        self.scope_depth = scope_depth;
    }

    fn emit(&mut self, operation: Operation, span: Span) {
        if let Some(block) = self.current {
            self.blocks[block as usize]
                .operations
                .push(IrInstruction { operation, span });
        }
    }

    fn terminate(&mut self, terminator: Terminator) {
        if let Some(block) = self.current.take() {
            self.blocks[block as usize].terminator = terminator;
        }
    }

    fn compile_sequence(
        &mut self,
        statements: &[Statement],
        functions: &[ResolvedFunction],
    ) -> Result<(), LoweringError> {
        let header_literals = function_header_literals(statements, functions)?;
        for (index, statement) in statements.iter().enumerate() {
            if header_literals.contains(&index) || self.current.is_none() {
                continue;
            }
            self.compile_statement(statement, functions)?;
        }
        Ok(())
    }

    fn compile_statement(
        &mut self,
        statement: &Statement,
        functions: &[ResolvedFunction],
    ) -> Result<(), LoweringError> {
        match statement {
            Statement::DataLiteral(literal) => {
                let mut ascii_bits = Vec::with_capacity(literal.bits.len());
                for bit in &literal.bits {
                    match bit {
                        0 => ascii_bits.push(b'0'),
                        1 => ascii_bits.push(b'1'),
                        _ => return Err(lowering_error(literal.span, "invalid data literal bit")),
                    }
                }
                let value = BigInt::parse_bytes(&ascii_bits, 2)
                    .ok_or_else(|| lowering_error(literal.span, "invalid integer data literal"))?;
                self.emit(Operation::PushInt(value), literal.span);
            }
            Statement::Instruction(instruction) => {
                let mnemonic = instruction
                    .opcode
                    .mnemonic()
                    .ok_or_else(|| lowering_error(instruction.span, "unassigned source opcode"))?;
                match mnemonic {
                    "RETURN" => self.terminate(Terminator::ReturnTop {
                        unwind_to_frame_base: self.scope_depth,
                        span: instruction.span,
                    }),
                    "BREAK" | "CONTINUE" => {
                        if mnemonic == "BREAK" {
                            if let Some(targets) = self.loops.last_mut() {
                                targets.has_break = true;
                            }
                        }
                        let targets = self.loops.last().copied().ok_or_else(|| {
                            lowering_error(instruction.span, "loop transfer has no enclosing loop")
                        })?;
                        let (target, target_depth) = if mnemonic == "BREAK" {
                            (targets.break_target, targets.break_scope_depth)
                        } else {
                            (targets.continue_target, targets.continue_scope_depth)
                        };
                        let unwind_scopes =
                            self.scope_depth.checked_sub(target_depth).ok_or_else(|| {
                                lowering_error(
                                    instruction.span,
                                    "loop transfer crosses an invalid scope depth",
                                )
                            })?;
                        self.terminate(Terminator::Jump {
                            target,
                            unwind_scopes,
                            span: instruction.span,
                        });
                    }
                    "RETHROW" => self.terminate(Terminator::Rethrow {
                        span: instruction.span,
                    }),
                    "HALT" => self.terminate(Terminator::Halt {
                        span: instruction.span,
                    }),
                    _ => {
                        let operation = IrOp::from_mnemonic(mnemonic).ok_or_else(|| {
                            lowering_error(
                                instruction.span,
                                format!("source instruction {mnemonic} has no IR mapping"),
                            )
                        })?;
                        self.emit(Operation::Source(operation), instruction.span);
                    }
                }
            }
            Statement::If(block) => self.compile_if(block, functions)?,
            Statement::Loop(block) => self.compile_loop(block, functions)?,
            Statement::For(block) => self.compile_for(block, functions)?,
            Statement::Scope(block) => {
                let outer_depth = self.scope_depth;
                self.emit(Operation::ScopeEnter, block.span);
                self.scope_depth = outer_depth + 1;
                self.compile_sequence(&block.body, functions)?;
                if self.current.is_some() {
                    self.emit(Operation::ScopeExit, block.span);
                }
                self.scope_depth = outer_depth;
            }
            Statement::Function(block) => {
                if !functions.iter().any(|function| function.span == block.span) {
                    return Err(lowering_error(
                        block.span,
                        "function descriptor was not resolved",
                    ));
                }
            }
            Statement::Try(block) => {
                return Err(lowering_error(
                    block.span,
                    "TRY lowering to bytecode exception regions is not implemented",
                ));
            }
        }
        Ok(())
    }

    fn compile_if(
        &mut self,
        block: &star_parser::IfBlock,
        functions: &[ResolvedFunction],
    ) -> Result<(), LoweringError> {
        let scope_depth = self.scope_depth;
        let true_block = self.new_block(scope_depth, block.span);
        let false_block = self.new_block(scope_depth, block.span);
        let join_block = self.new_block(scope_depth, block.span);
        self.terminate(Terminator::BranchIfFalse {
            false_target: false_block,
            true_target: true_block,
            span: block.span,
        });

        self.select(true_block, scope_depth);
        self.compile_sequence(&block.then_body, functions)?;
        let true_falls_through = self.current.is_some();
        if true_falls_through {
            self.terminate(Terminator::Jump {
                target: join_block,
                unwind_scopes: 0,
                span: block.span,
            });
        }

        self.select(false_block, scope_depth);
        if let Some(else_body) = &block.else_body {
            self.compile_sequence(else_body, functions)?;
        }
        let false_falls_through = self.current.is_some();
        if false_falls_through {
            self.terminate(Terminator::Jump {
                target: join_block,
                unwind_scopes: 0,
                span: block.span,
            });
        }

        if true_falls_through || false_falls_through {
            self.select(join_block, scope_depth);
        } else {
            self.current = None;
            self.scope_depth = scope_depth;
        }
        Ok(())
    }

    fn compile_loop(
        &mut self,
        block: &star_parser::LoopBlock,
        functions: &[ResolvedFunction],
    ) -> Result<(), LoweringError> {
        let scope_depth = self.scope_depth;
        let header = self.new_block(scope_depth, block.span);
        let body = self.new_block(scope_depth, block.span);
        let exit = self.new_block(scope_depth, block.span);
        self.terminate(Terminator::Jump {
            target: header,
            unwind_scopes: 0,
            span: block.span,
        });

        self.select(header, scope_depth);
        let condition_reaches_branch = if let Some(condition) = &block.condition {
            self.compile_sequence(condition, functions)?;
            if self.current.is_some() {
                self.terminate(Terminator::BranchIfFalse {
                    false_target: exit,
                    true_target: body,
                    span: block.span,
                });
                true
            } else {
                false
            }
        } else {
            self.terminate(Terminator::Jump {
                target: body,
                unwind_scopes: 0,
                span: block.span,
            });
            true
        };

        self.loops.push(LoopTargets {
            break_target: exit,
            break_scope_depth: scope_depth,
            continue_target: header,
            continue_scope_depth: scope_depth,
            has_break: false,
        });
        if condition_reaches_branch {
            self.select(body, scope_depth);
            self.compile_sequence(&block.body, functions)?;
            if self.current.is_some() {
                self.terminate(Terminator::Jump {
                    target: header,
                    unwind_scopes: 0,
                    span: block.span,
                });
            }
        }
        let targets = self.loops.pop().expect("loop target was just pushed");
        if condition_reaches_branch && block.condition.is_some() || targets.has_break {
            self.select(exit, scope_depth);
        } else {
            self.current = None;
            self.scope_depth = scope_depth;
        }
        Ok(())
    }

    fn compile_for(
        &mut self,
        block: &star_parser::ForBlock,
        functions: &[ResolvedFunction],
    ) -> Result<(), LoweringError> {
        let outer_depth = self.scope_depth;
        let loop_state = self.next_loop_state;
        self.next_loop_state = self
            .next_loop_state
            .checked_add(1)
            .ok_or_else(|| lowering_error(block.span, "function has too many iterator loops"))?;
        self.emit(Operation::ForInit { loop_state }, block.span);
        let loop_depth = outer_depth + 1;
        let header = self.new_block(loop_depth, block.span);
        let body = self.new_block(loop_depth, block.span);
        let exit = self.new_block(loop_depth, block.span);
        self.terminate(Terminator::Jump {
            target: header,
            unwind_scopes: 0,
            span: block.span,
        });

        self.select(header, loop_depth);
        self.terminate(Terminator::ForNext {
            loop_state,
            body_target: body,
            exit_target: exit,
            span: block.span,
        });

        self.loops.push(LoopTargets {
            break_target: exit,
            break_scope_depth: loop_depth,
            continue_target: header,
            continue_scope_depth: loop_depth,
            has_break: false,
        });
        self.select(body, loop_depth);
        self.compile_sequence(&block.body, functions)?;
        if self.current.is_some() {
            self.terminate(Terminator::Jump {
                target: header,
                unwind_scopes: 0,
                span: block.span,
            });
        }
        self.loops.pop();

        self.select(exit, loop_depth);
        self.emit(Operation::ScopeExit, block.span);
        self.scope_depth = outer_depth;
        Ok(())
    }

    fn finish(mut self, initializer: bool, final_span: Span) -> FunctionIr {
        if self.current.is_some() {
            self.terminate(if initializer {
                Terminator::Halt { span: final_span }
            } else {
                Terminator::ReturnTop {
                    unwind_to_frame_base: self.scope_depth,
                    span: final_span,
                }
            });
        }
        FunctionIr {
            id: None,
            parameter_ids: Vec::new(),
            rest_parameter_code: 0,
            asynchronous: false,
            entry: 0,
            max_stack: self.max_stack,
            blocks: self.blocks,
            exception_regions: Vec::new(),
        }
    }
}

fn lower_function(
    statements: &[Statement],
    descriptor: Option<&ResolvedFunction>,
    functions: &[ResolvedFunction],
    max_stack: u32,
    span: Span,
) -> Result<FunctionIr, LoweringError> {
    let mut builder = FunctionBuilder::new(max_stack, span);
    builder.compile_sequence(statements, functions)?;
    let mut function = builder.finish(descriptor.is_none(), span);
    if let Some(descriptor) = descriptor {
        function.id = Some(descriptor.id.clone());
        function.parameter_ids = descriptor.parameter_ids.clone();
        function.rest_parameter_code = if descriptor.variadic {
            u32::try_from(descriptor.parameter_ids.len())
                .map_err(|_| lowering_error(descriptor.span, "too many function parameters"))?
        } else {
            0
        };
        function.asynchronous = descriptor.kind == FunctionKind::Asynchronous;
    }
    Ok(function)
}

fn function_header_literals(
    statements: &[Statement],
    functions: &[ResolvedFunction],
) -> Result<HashSet<usize>, LoweringError> {
    let mut skipped = HashSet::new();
    for (index, statement) in statements.iter().enumerate() {
        let Statement::Function(block) = statement else {
            continue;
        };
        let descriptor = functions
            .iter()
            .find(|function| function.span == block.span)
            .ok_or_else(|| lowering_error(block.span, "function descriptor was not resolved"))?;
        let header_count = descriptor
            .parameter_ids
            .len()
            .checked_add(3)
            .ok_or_else(|| lowering_error(block.span, "function header size overflow"))?;
        let header_start = index.checked_sub(header_count).ok_or_else(|| {
            lowering_error(block.span, "function header is missing its constant fields")
        })?;
        if statements[header_start..index]
            .iter()
            .any(|statement| !matches!(statement, Statement::DataLiteral(_)))
        {
            return Err(lowering_error(
                block.span,
                "bytecode build requires function headers to use adjacent integer literals",
            ));
        }
        skipped.extend(header_start..index);
    }
    Ok(skipped)
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

fn empty_span() -> Span {
    use star_common::SourcePosition;
    Span {
        start: SourcePosition {
            byte_offset: 0,
            line: 1,
            column: 1,
        },
        end: SourcePosition {
            byte_offset: 0,
            line: 1,
            column: 1,
        },
    }
}

fn lowering_error(span: Span, message: impl Into<String>) -> LoweringError {
    LoweringError {
        span,
        message: message.into(),
    }
}
