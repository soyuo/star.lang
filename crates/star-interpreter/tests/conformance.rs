use std::collections::BTreeSet;
use std::io::Cursor;

use star_common::{SourcePosition, Span};
use star_compiler::{
    compile, BasicBlock, FunctionIr, IrInstruction, IrOp, ModuleIr, Operation, Terminator,
    VerificationLimits, VerificationPolicy,
};
use star_interpreter::{
    runtime::{BytecodeVm, Interpreter, RuntimeErrorKind},
    value::Value,
    BigInt,
};
use star_lexer::{normalize_source, LexMode};
use star_parser::parse;

fn point_span() -> Span {
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

fn span_for_token(source: &str, token: &str) -> Span {
    let start = source.find(token).expect("fixture token exists");
    let end = start + token.len();
    Span {
        start: SourcePosition {
            byte_offset: start,
            line: 1,
            column: start + 1,
        },
        end: SourcePosition {
            byte_offset: end,
            line: 1,
            column: end + 1,
        },
    }
}

fn push(value: i64) -> IrInstruction {
    IrInstruction {
        operation: Operation::PushInt(BigInt::from(value)),
        span: point_span(),
    }
}

fn source(operation: IrOp) -> IrInstruction {
    IrInstruction {
        operation: Operation::Source(operation),
        span: point_span(),
    }
}

fn block(
    id: u32,
    entry_scope_depth: u32,
    operations: Vec<IrInstruction>,
    terminator: Terminator,
) -> BasicBlock {
    BasicBlock {
        id,
        entry_scope_depth,
        operations,
        terminator,
    }
}

fn module(blocks: Vec<BasicBlock>, max_stack: u32) -> ModuleIr {
    ModuleIr {
        initializer: FunctionIr {
            id: None,
            parameter_ids: Vec::new(),
            rest_parameter_code: 0,
            asynchronous: false,
            entry: 0,
            max_stack,
            blocks,
            exception_regions: Vec::new(),
        },
        functions: Vec::new(),
    }
}

fn policy() -> VerificationPolicy {
    VerificationPolicy {
        limits: VerificationLimits {
            max_file_bytes: 1_048_576,
            max_table_entries: 1_024,
            max_instructions: 100_000,
            max_stack_depth: 4_096,
        },
        granted_capabilities: BTreeSet::new(),
    }
}

fn assert_conformance(
    source_text: &str,
    module_ir: ModuleIr,
    expected_output: &[u8],
    fixture_name: &str,
) {
    // This conformance layer pairs each shared source fixture with an
    // equivalent hand-built IR module, then compares both engines.
    let normalized = normalize_source(source_text.as_bytes(), LexMode::Normal).unwrap();
    let program = parse(&normalized).unwrap();
    let mut reference = Interpreter::new(Cursor::new(Vec::<u8>::new()), Vec::<u8>::new());
    reference.execute(&program).unwrap();
    let reference_stack = reference.stack().to_vec();
    let (_, reference_output) = reference.into_io();

    let bytecode = compile(&module_ir, "conformance", fixture_name).unwrap();
    let mut vm = BytecodeVm::new(Cursor::new(Vec::<u8>::new()), Vec::<u8>::new(), policy());
    assert_eq!(
        vm.execute(&bytecode).unwrap(),
        Value::Null,
        "{fixture_name}"
    );
    let vm_stack = vm.stack().to_vec();
    let (_, vm_output) = vm.into_io();

    assert_eq!(
        reference_output, expected_output,
        "reference: {fixture_name}"
    );
    assert_eq!(vm_output, expected_output, "VM: {fixture_name}");
    assert_eq!(reference_stack, vm_stack, "final stack: {fixture_name}");
}

#[test]
fn arithmetic_fixture_agrees() {
    let source_text = include_str!("fixtures/arithmetic.star");
    assert_conformance(
        source_text,
        module(
            vec![block(
                0,
                0,
                vec![push(10), push(11), source(IrOp::Add), source(IrOp::Print)],
                Terminator::Halt { span: point_span() },
            )],
            2,
        ),
        b"21",
        "arithmetic.star",
    );
}

#[test]
fn variable_fixture_agrees() {
    let source_text = include_str!("fixtures/variables.star");
    assert_conformance(
        source_text,
        module(
            vec![block(
                0,
                0,
                vec![
                    push(9),
                    push(7),
                    source(IrOp::Store),
                    push(7),
                    source(IrOp::Load),
                    source(IrOp::Print),
                ],
                Terminator::Halt { span: point_span() },
            )],
            2,
        ),
        b"9",
        "variables.star",
    );
}

#[test]
fn conditional_fixture_agrees() {
    let source_text = include_str!("fixtures/conditional.star");
    assert_conformance(
        source_text,
        module(
            vec![
                block(
                    0,
                    0,
                    vec![push(2), push(1), source(IrOp::Gt)],
                    Terminator::BranchIfFalse {
                        false_target: 2,
                        true_target: 1,
                        span: point_span(),
                    },
                ),
                block(
                    1,
                    0,
                    vec![push(1)],
                    Terminator::Jump {
                        target: 3,
                        unwind_scopes: 0,
                        span: point_span(),
                    },
                ),
                block(
                    2,
                    0,
                    vec![push(0)],
                    Terminator::Jump {
                        target: 3,
                        unwind_scopes: 0,
                        span: point_span(),
                    },
                ),
                block(
                    3,
                    0,
                    vec![source(IrOp::Print)],
                    Terminator::Halt { span: point_span() },
                ),
            ],
            2,
        ),
        b"1",
        "conditional.star",
    );
}

#[test]
fn array_sum_algorithm_fixture_agrees() {
    let source_text = include_str!("fixtures/array_sum.star");
    assert_conformance(
        source_text,
        module(
            vec![
                block(
                    0,
                    0,
                    vec![
                        push(0),
                        push(1),
                        source(IrOp::Store),
                        push(1),
                        push(2),
                        push(3),
                        push(3),
                        source(IrOp::ArrayNew),
                        source(IrOp::Iter),
                        push(2),
                        IrInstruction {
                            operation: Operation::ForInit { loop_state: 0 },
                            span: point_span(),
                        },
                    ],
                    Terminator::ForNext {
                        loop_state: 0,
                        body_target: 1,
                        exit_target: 3,
                        span: point_span(),
                    },
                ),
                block(
                    1,
                    1,
                    vec![
                        push(1),
                        source(IrOp::Load),
                        push(2),
                        source(IrOp::Load),
                        source(IrOp::Add),
                        push(1),
                        source(IrOp::Store),
                    ],
                    Terminator::Jump {
                        target: 2,
                        unwind_scopes: 0,
                        span: point_span(),
                    },
                ),
                block(
                    2,
                    1,
                    Vec::new(),
                    Terminator::ForNext {
                        loop_state: 0,
                        body_target: 1,
                        exit_target: 3,
                        span: point_span(),
                    },
                ),
                block(
                    3,
                    1,
                    vec![
                        IrInstruction {
                            operation: Operation::ScopeExit,
                            span: point_span(),
                        },
                        push(1),
                        source(IrOp::Load),
                        source(IrOp::Print),
                    ],
                    Terminator::Halt { span: point_span() },
                ),
            ],
            4,
        ),
        b"6",
        "array_sum.star",
    );
}

#[test]
fn division_by_zero_fixture_agrees_on_error_kind_and_span() {
    let source_text = include_str!("fixtures/division_by_zero.star");
    let division_token = "****'";
    let division_span = span_for_token(source_text, division_token);
    let normalized = normalize_source(source_text.as_bytes(), LexMode::Normal).unwrap();
    let program = parse(&normalized).unwrap();
    let mut reference = Interpreter::new(Cursor::new(Vec::<u8>::new()), Vec::<u8>::new());
    let reference_error = reference.execute(&program).unwrap_err();

    let mut division = source(IrOp::Div);
    division.span = division_span;
    let module_ir = module(
        vec![block(
            0,
            0,
            vec![push(8), push(0), division],
            Terminator::Halt { span: point_span() },
        )],
        2,
    );
    let bytecode = compile(&module_ir, "conformance", "division_by_zero.star").unwrap();
    let mut vm = BytecodeVm::new(Cursor::new(Vec::<u8>::new()), Vec::<u8>::new(), policy());
    let vm_error = vm.execute(&bytecode).unwrap_err();

    assert_eq!(reference_error.kind, RuntimeErrorKind::DivisionByZero);
    assert_eq!(vm_error.kind, reference_error.kind);
    assert_eq!(vm_error.span, reference_error.span);
}
