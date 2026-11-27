use std::fmt;

use crate::{FunctionKind, InstructionOpcode, Program, Statement};

/// An AST element cannot be represented by an assigned source token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatError {
    /// The parser admitted an opcode that is absent from the catalog.
    UnassignedOpcode(InstructionOpcode),
    /// A structural AST node has no matching structural opcode in the catalog.
    MissingStructuralOpcode(&'static str),
}

impl fmt::Display for FormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnassignedOpcode(opcode) => {
                write!(
                    formatter,
                    "cannot format unassigned source opcode {opcode:?}"
                )
            }
            Self::MissingStructuralOpcode(mnemonic) => {
                write!(
                    formatter,
                    "opcode catalog is missing structural token {mnemonic}"
                )
            }
        }
    }
}

impl std::error::Error for FormatError {}

/// Formats an AST with four-space indentation and one token per line.
///
/// The output uses only the three Star source glyphs plus optional formatting
/// whitespace and ends in one line feed when the program is nonempty.
pub fn format_program(program: &Program) -> Result<String, FormatError> {
    let mut output = String::new();
    format_sequence(&program.statements, 0, &mut output)?;
    Ok(output)
}

fn format_sequence(
    statements: &[Statement],
    depth: usize,
    output: &mut String,
) -> Result<(), FormatError> {
    for statement in statements {
        match statement {
            Statement::Instruction(instruction) => {
                let spelling = instruction
                    .opcode
                    .source_spelling()
                    .ok_or(FormatError::UnassignedOpcode(instruction.opcode))?;
                write_line(output, depth, &spelling);
            }
            Statement::DataLiteral(literal) => {
                let payload: String = literal
                    .bits
                    .iter()
                    .map(|bit| if *bit == 1 { '*' } else { '\'' })
                    .collect();
                write_line(output, depth, &format!("\"{payload}\""));
            }
            Statement::If(block) => {
                write_structural(output, depth, "IF")?;
                format_sequence(&block.then_body, depth + 1, output)?;
                if let Some(else_body) = &block.else_body {
                    write_structural(output, depth, "ELSE")?;
                    format_sequence(else_body, depth + 1, output)?;
                }
                write_structural(output, depth, "END")?;
            }
            Statement::Loop(block) => {
                write_structural(output, depth, "LOOP")?;
                if let Some(condition) = &block.condition {
                    format_sequence(condition, depth + 1, output)?;
                    write_structural(output, depth, "WHILE")?;
                }
                format_sequence(&block.body, depth + 1, output)?;
                write_structural(output, depth, "END")?;
            }
            Statement::For(block) => {
                write_structural(output, depth, "FOR")?;
                format_sequence(&block.body, depth + 1, output)?;
                write_structural(output, depth, "END")?;
            }
            Statement::Function(block) => {
                let mnemonic = match block.kind {
                    FunctionKind::Synchronous => "FUNC",
                    FunctionKind::Asynchronous => "ASYNC_FUNC",
                };
                write_structural(output, depth, mnemonic)?;
                format_sequence(&block.body, depth + 1, output)?;
                write_structural(output, depth, "END")?;
            }
            Statement::Scope(block) => {
                write_structural(output, depth, "SCOPE_BEGIN")?;
                format_sequence(&block.body, depth + 1, output)?;
                write_structural(output, depth, "SCOPE_END")?;
            }
            Statement::Try(block) => {
                write_structural(output, depth, "TRY")?;
                format_sequence(&block.try_body, depth + 1, output)?;
                if let Some(catch_body) = &block.catch_body {
                    write_structural(output, depth, "CATCH")?;
                    format_sequence(catch_body, depth + 1, output)?;
                }
                if let Some(finally_body) = &block.finally_body {
                    write_structural(output, depth, "FINALLY")?;
                    format_sequence(finally_body, depth + 1, output)?;
                }
                write_structural(output, depth, "END")?;
            }
        }
    }
    Ok(())
}

fn write_structural(
    output: &mut String,
    depth: usize,
    mnemonic: &'static str,
) -> Result<(), FormatError> {
    let spelling = InstructionOpcode::from_mnemonic(mnemonic)
        .and_then(InstructionOpcode::source_spelling)
        .ok_or(FormatError::MissingStructuralOpcode(mnemonic))?;
    write_line(output, depth, &spelling);
    Ok(())
}

fn write_line(output: &mut String, depth: usize, token: &str) {
    output.push_str(&" ".repeat(depth * 4));
    output.push_str(token);
    output.push('\n');
}
