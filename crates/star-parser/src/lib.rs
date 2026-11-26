//! Parser and abstract syntax tree for Star source programs.

use std::fmt;

pub use star_common::{SourcePosition, Span};
use star_lexer::NormalizedSource;

mod catalog {
    include!(concat!(env!("OUT_DIR"), "/assigned_opcodes.rs"));
}

/// A parsed Star program. Statements retain source spans in the original input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Program {
    /// Top-level statements in source order.
    pub statements: Vec<Statement>,
}

/// A source-level statement or structured block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Statement {
    /// An ordinary basic or extended instruction.
    Instruction(Instruction),
    /// A non-empty integer or byte payload encoded as bits, most-significant bit first.
    DataLiteral(DataLiteral),
    /// A conditional with an optional alternate branch.
    If(IfBlock),
    /// A condition-free or `WHILE`-separated loop.
    Loop(LoopBlock),
    /// An iterator loop.
    For(ForBlock),
    /// A synchronous or asynchronous function definition.
    Function(FunctionBlock),
    /// A lexical scope delimited by `SCOPE_BEGIN` and `SCOPE_END`.
    Scope(ScopeBlock),
}

/// An ordinary instruction. Structural opcodes are represented by block nodes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Instruction {
    /// The basic ordinal or extended source opcode.
    pub opcode: InstructionOpcode,
    /// Original source range of the instruction token.
    pub span: Span,
}

/// The source encoding family and value of an ordinary instruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstructionOpcode {
    /// A basic instruction identified by its number of leading asterisks.
    Basic(u8),
    /// An assigned eight-bit extended source opcode.
    Extended(u8),
}

/// A quoted, non-empty bit payload. Each element is `0` or `1`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataLiteral {
    /// Payload bits in source order; leading zero bits are preserved.
    pub bits: Vec<u8>,
    /// Original source range including both quotes.
    pub span: Span,
}

/// The two arms of an `IF` block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IfBlock {
    /// Statements executed when the condition is true.
    pub then_body: Vec<Statement>,
    /// Statements executed when the condition is false, if `ELSE` is present.
    pub else_body: Option<Vec<Statement>>,
    /// Original source range including `IF` and its matching `END`.
    pub span: Span,
}

/// A `LOOP` block, optionally split into condition and body by `WHILE`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoopBlock {
    /// Non-empty condition-producing statements before `WHILE`, when present.
    pub condition: Option<Vec<Statement>>,
    /// Loop body; for a condition-free loop this contains the full body.
    pub body: Vec<Statement>,
    /// Original source range including `LOOP` and its matching `END`.
    pub span: Span,
}

/// An iterator loop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForBlock {
    /// Statements executed for each iterator item.
    pub body: Vec<Statement>,
    /// Original source range including `FOR` and its matching `END`.
    pub span: Span,
}

/// The kind of function definition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FunctionKind {
    /// A `FUNC` definition.
    Synchronous,
    /// An `ASYNC_FUNC` definition.
    Asynchronous,
}

/// A function definition block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionBlock {
    /// Whether this definition uses `FUNC` or `ASYNC_FUNC`.
    pub kind: FunctionKind,
    /// Statements in the function body.
    pub body: Vec<Statement>,
    /// Original source range including the opener and matching `END`.
    pub span: Span,
}

/// A lexical scope block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopeBlock {
    /// Statements in the scope.
    pub body: Vec<Statement>,
    /// Original source range including `SCOPE_BEGIN` and `SCOPE_END`.
    pub span: Span,
}

/// A block closer expected by the grammar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockCloser {
    /// `END`, used by conditionals, loops, iterator loops, and functions.
    End,
    /// `SCOPE_END`, used only by lexical scopes.
    ScopeEnd,
}

impl fmt::Display for BlockCloser {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::End => formatter.write_str("END"),
            Self::ScopeEnd => formatter.write_str("SCOPE_END"),
        }
    }
}

/// A syntax error found while parsing normalized Star source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseError {
    /// Category and details of the malformed input.
    pub kind: ParseErrorKind,
    /// Original source range associated with the error.
    pub span: Span,
}

/// The reason parsing failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseErrorKind {
    /// A run of asterisks ended before its required apostrophe.
    IncompleteBasicInstruction,
    /// A basic instruction used more than the allowed 12 asterisks.
    BasicOrdinalOutOfRange { ordinal: usize },
    /// An extended instruction ended before its eight payload bits were complete.
    IncompleteExtendedPayload { bits_found: usize },
    /// An extended instruction did not end with its required double quote.
    MissingExtendedTerminator,
    /// A data literal had no payload bits.
    EmptyDataLiteral,
    /// A data literal reached end of input before its closing quote.
    UnterminatedDataLiteral,
    /// An extended opcode is absent from the canonical source opcode catalog.
    UnassignedExtendedOpcode { opcode: u8 },
    /// A block separator or closer appeared where no block can consume it.
    UnexpectedStructuralOpcode { opcode: u8 },
    /// `ELSE` appeared without an open conditional that accepts it.
    UnmatchedElse,
    /// An `IF` contained more than one direct `ELSE` separator.
    DuplicateElse,
    /// `WHILE` appeared without an open loop that accepts it.
    UnmatchedWhile,
    /// A loop contained more than one direct `WHILE` separator.
    DuplicateWhile,
    /// `WHILE` appeared before a loop condition statement.
    EmptyLoopCondition,
    /// End of input occurred before the block's required closer.
    UnclosedBlock { expected: BlockCloser },
    /// A block was closed with the other block family's closer.
    MismatchedBlockCloser {
        expected: BlockCloser,
        found: BlockCloser,
    },
}

impl fmt::Display for ParseErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IncompleteBasicInstruction => {
                formatter.write_str("basic instruction is missing its trailing apostrophe")
            }
            Self::BasicOrdinalOutOfRange { ordinal } => write!(
                formatter,
                "basic instruction ordinal {ordinal} is outside the range 1 through 12"
            ),
            Self::IncompleteExtendedPayload { bits_found } => write!(
                formatter,
                "extended instruction has {bits_found} of 8 payload bits"
            ),
            Self::MissingExtendedTerminator => {
                formatter.write_str("extended instruction is missing its trailing double quote")
            }
            Self::EmptyDataLiteral => formatter.write_str("data literal payload cannot be empty"),
            Self::UnterminatedDataLiteral => {
                formatter.write_str("data literal is missing its closing double quote")
            }
            Self::UnassignedExtendedOpcode { opcode } => {
                write!(
                    formatter,
                    "extended source opcode 0x{opcode:02X} is unassigned"
                )
            }
            Self::UnexpectedStructuralOpcode { opcode } => write!(
                formatter,
                "structural source opcode 0x{opcode:02X} is not valid here"
            ),
            Self::UnmatchedElse => formatter.write_str("ELSE has no matching IF"),
            Self::DuplicateElse => formatter.write_str("IF contains more than one ELSE"),
            Self::UnmatchedWhile => formatter.write_str("WHILE has no matching LOOP"),
            Self::DuplicateWhile => formatter.write_str("LOOP contains more than one WHILE"),
            Self::EmptyLoopCondition => {
                formatter.write_str("WHILE requires at least one condition statement")
            }
            Self::UnclosedBlock { expected } => write!(formatter, "block is missing {expected}"),
            Self::MismatchedBlockCloser { expected, found } => {
                write!(formatter, "expected {expected}, found {found}")
            }
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at line {}, column {} (byte offset {})",
            self.kind, self.span.start.line, self.span.start.column, self.span.start.byte_offset
        )
    }
}

impl std::error::Error for ParseError {}

/// Parses a previously validated and normalized source stream into an AST.
///
/// Opcode assignments are loaded from `spec/opcodes.toml` at build time so an
/// unassigned extended opcode cannot enter a valid syntax tree.
pub fn parse(source: &NormalizedSource) -> Result<Program, ParseError> {
    let mut parser = Parser::new(source);
    let sequence = parser.parse_sequence()?;

    if let Some(token) = sequence.stop {
        return Err(parser.unexpected_structure(token));
    }

    Ok(Program {
        statements: sequence.statements,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StructuralOpcode {
    If,
    Else,
    End,
    Loop,
    While,
    For,
    Function,
    ScopeBegin,
    ScopeEnd,
    AsyncFunction,
}

impl StructuralOpcode {
    fn source_opcode(self) -> u8 {
        match self {
            Self::If => 0x30,
            Self::Else => 0x31,
            Self::End => 0x32,
            Self::Loop => 0x33,
            Self::While => 0x34,
            Self::For => 0x37,
            Self::Function => 0x40,
            Self::ScopeBegin => 0x55,
            Self::ScopeEnd => 0x56,
            Self::AsyncFunction => 0xA0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StructuralToken {
    kind: StructuralOpcode,
    span: Span,
}

enum ParsedToken {
    Statement(Statement),
    Structural(StructuralToken),
}

struct Sequence {
    statements: Vec<Statement>,
    stop: Option<StructuralToken>,
}

struct Parser<'a> {
    source: &'a NormalizedSource,
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &'a NormalizedSource) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            offset: 0,
        }
    }

    fn parse_sequence(&mut self) -> Result<Sequence, ParseError> {
        let mut statements = Vec::new();

        while self.offset < self.bytes.len() {
            match self.parse_token()? {
                ParsedToken::Statement(statement) => statements.push(statement),
                ParsedToken::Structural(token) => match token.kind {
                    StructuralOpcode::If => statements.push(self.parse_if(token)?),
                    StructuralOpcode::Loop => statements.push(self.parse_loop(token)?),
                    StructuralOpcode::For => statements.push(self.parse_for(token)?),
                    StructuralOpcode::Function => {
                        statements.push(self.parse_function(token, FunctionKind::Synchronous)?)
                    }
                    StructuralOpcode::AsyncFunction => {
                        statements.push(self.parse_function(token, FunctionKind::Asynchronous)?)
                    }
                    StructuralOpcode::ScopeBegin => statements.push(self.parse_scope(token)?),
                    StructuralOpcode::Else
                    | StructuralOpcode::End
                    | StructuralOpcode::While
                    | StructuralOpcode::ScopeEnd => {
                        return Ok(Sequence {
                            statements,
                            stop: Some(token),
                        });
                    }
                },
            }
        }

        Ok(Sequence {
            statements,
            stop: None,
        })
    }

    fn parse_token(&mut self) -> Result<ParsedToken, ParseError> {
        match self.bytes[self.offset] {
            b'*' => self.parse_basic_instruction(),
            b'\'' => self.parse_extended_instruction(),
            b'"' => self.parse_data_literal(),
            _ => unreachable!("NormalizedSource contains only Star source characters"),
        }
    }

    fn parse_basic_instruction(&mut self) -> Result<ParsedToken, ParseError> {
        let start = self.offset;
        while self.bytes.get(self.offset) == Some(&b'*') {
            self.offset += 1;
        }

        let ordinal = self.offset - start;
        if ordinal > 12 {
            return Err(self.error(
                ParseErrorKind::BasicOrdinalOutOfRange { ordinal },
                self.source_span(start, self.offset),
            ));
        }

        if self.bytes.get(self.offset) != Some(&b'\'') {
            let error_span = if self.offset < self.bytes.len() {
                self.source_span(self.offset, self.offset + 1)
            } else {
                self.point_span(self.offset)
            };
            return Err(self.error(ParseErrorKind::IncompleteBasicInstruction, error_span));
        }

        self.offset += 1;
        let instruction = Instruction {
            opcode: InstructionOpcode::Basic(ordinal as u8),
            span: self.source_span(start, self.offset),
        };
        Ok(ParsedToken::Statement(Statement::Instruction(instruction)))
    }

    fn parse_extended_instruction(&mut self) -> Result<ParsedToken, ParseError> {
        let start = self.offset;
        self.offset += 1;
        let mut opcode = 0_u8;

        for bits_found in 0..8 {
            match self.bytes.get(self.offset) {
                Some(b'*') => {
                    opcode = (opcode << 1) | 1;
                    self.offset += 1;
                }
                Some(b'\'') => {
                    opcode <<= 1;
                    self.offset += 1;
                }
                Some(b'"') | None => {
                    let span = if self.offset < self.bytes.len() {
                        self.source_span(self.offset, self.offset + 1)
                    } else {
                        self.point_span(self.offset)
                    };
                    return Err(self.error(
                        ParseErrorKind::IncompleteExtendedPayload { bits_found },
                        span,
                    ));
                }
                _ => unreachable!("NormalizedSource contains only Star source characters"),
            }
        }

        if self.bytes.get(self.offset) != Some(&b'"') {
            let span = if self.offset < self.bytes.len() {
                self.source_span(self.offset, self.offset + 1)
            } else {
                self.point_span(self.offset)
            };
            return Err(self.error(ParseErrorKind::MissingExtendedTerminator, span));
        }

        self.offset += 1;
        let span = self.source_span(start, self.offset);
        if catalog::ASSIGNED_EXTENDED_OPCODES
            .binary_search(&opcode)
            .is_err()
        {
            return Err(self.error(ParseErrorKind::UnassignedExtendedOpcode { opcode }, span));
        }

        if let Some(kind) = structural_opcode(opcode) {
            Ok(ParsedToken::Structural(StructuralToken { kind, span }))
        } else {
            Ok(ParsedToken::Statement(Statement::Instruction(
                Instruction {
                    opcode: InstructionOpcode::Extended(opcode),
                    span,
                },
            )))
        }
    }

    fn parse_data_literal(&mut self) -> Result<ParsedToken, ParseError> {
        let start = self.offset;
        self.offset += 1;
        let mut bits = Vec::new();

        loop {
            match self.bytes.get(self.offset) {
                Some(b'*') => {
                    bits.push(1);
                    self.offset += 1;
                }
                Some(b'\'') => {
                    bits.push(0);
                    self.offset += 1;
                }
                Some(b'"') => {
                    self.offset += 1;
                    let span = self.source_span(start, self.offset);
                    if bits.is_empty() {
                        return Err(self.error(ParseErrorKind::EmptyDataLiteral, span));
                    }
                    return Ok(ParsedToken::Statement(Statement::DataLiteral(
                        DataLiteral { bits, span },
                    )));
                }
                None => {
                    return Err(self.error(
                        ParseErrorKind::UnterminatedDataLiteral,
                        self.point_span(self.offset),
                    ));
                }
                _ => unreachable!("NormalizedSource contains only Star source characters"),
            }
        }
    }

    fn parse_if(&mut self, opener: StructuralToken) -> Result<Statement, ParseError> {
        let first = self.parse_sequence()?;
        let (then_body, else_body, closer) = match first.stop {
            Some(token) if token.kind == StructuralOpcode::End => (first.statements, None, token),
            Some(token) if token.kind == StructuralOpcode::Else => {
                let alternate = self.parse_sequence()?;
                match alternate.stop {
                    Some(end) if end.kind == StructuralOpcode::End => {
                        (first.statements, Some(alternate.statements), end)
                    }
                    Some(duplicate) if duplicate.kind == StructuralOpcode::Else => {
                        return Err(self.error(ParseErrorKind::DuplicateElse, duplicate.span));
                    }
                    other => {
                        return Err(self.error_for_end_block(other, &opener));
                    }
                }
            }
            other => return Err(self.error_for_end_block(other, &opener)),
        };

        Ok(Statement::If(IfBlock {
            then_body,
            else_body,
            span: joined_span(opener.span, closer.span),
        }))
    }

    fn parse_loop(&mut self, opener: StructuralToken) -> Result<Statement, ParseError> {
        let first = self.parse_sequence()?;
        match first.stop {
            Some(token) if token.kind == StructuralOpcode::End => Ok(Statement::Loop(LoopBlock {
                condition: None,
                body: first.statements,
                span: joined_span(opener.span, token.span),
            })),
            Some(separator) if separator.kind == StructuralOpcode::While => {
                if first.statements.is_empty() {
                    return Err(self.error(ParseErrorKind::EmptyLoopCondition, separator.span));
                }

                let body = self.parse_sequence()?;
                match body.stop {
                    Some(end) if end.kind == StructuralOpcode::End => {
                        Ok(Statement::Loop(LoopBlock {
                            condition: Some(first.statements),
                            body: body.statements,
                            span: joined_span(opener.span, end.span),
                        }))
                    }
                    Some(duplicate) if duplicate.kind == StructuralOpcode::While => {
                        Err(self.error(ParseErrorKind::DuplicateWhile, duplicate.span))
                    }
                    other => Err(self.error_for_end_block(other, &opener)),
                }
            }
            other => Err(self.error_for_end_block(other, &opener)),
        }
    }

    fn parse_for(&mut self, opener: StructuralToken) -> Result<Statement, ParseError> {
        let (body, closer) = self.parse_end_body(opener)?;
        Ok(Statement::For(ForBlock {
            body,
            span: joined_span(opener.span, closer.span),
        }))
    }

    fn parse_function(
        &mut self,
        opener: StructuralToken,
        kind: FunctionKind,
    ) -> Result<Statement, ParseError> {
        let (body, closer) = self.parse_end_body(opener)?;
        Ok(Statement::Function(FunctionBlock {
            kind,
            body,
            span: joined_span(opener.span, closer.span),
        }))
    }

    fn parse_scope(&mut self, opener: StructuralToken) -> Result<Statement, ParseError> {
        let body = self.parse_sequence()?;
        match body.stop {
            Some(closer) if closer.kind == StructuralOpcode::ScopeEnd => {
                Ok(Statement::Scope(ScopeBlock {
                    body: body.statements,
                    span: joined_span(opener.span, closer.span),
                }))
            }
            Some(token) if token.kind == StructuralOpcode::End => Err(self.error(
                ParseErrorKind::MismatchedBlockCloser {
                    expected: BlockCloser::ScopeEnd,
                    found: BlockCloser::End,
                },
                token.span,
            )),
            other => Err(self.error_for_scope_block(other, &opener)),
        }
    }

    fn parse_end_body(
        &mut self,
        opener: StructuralToken,
    ) -> Result<(Vec<Statement>, StructuralToken), ParseError> {
        let body = self.parse_sequence()?;
        match body.stop {
            Some(closer) if closer.kind == StructuralOpcode::End => Ok((body.statements, closer)),
            Some(token) if token.kind == StructuralOpcode::ScopeEnd => Err(self.error(
                ParseErrorKind::MismatchedBlockCloser {
                    expected: BlockCloser::End,
                    found: BlockCloser::ScopeEnd,
                },
                token.span,
            )),
            other => Err(self.error_for_end_block(other, &opener)),
        }
    }

    fn error_for_end_block(
        &self,
        stop: Option<StructuralToken>,
        opener: &StructuralToken,
    ) -> ParseError {
        match stop {
            Some(token) if token.kind == StructuralOpcode::Else => {
                self.error(ParseErrorKind::UnmatchedElse, token.span)
            }
            Some(token) if token.kind == StructuralOpcode::While => {
                self.error(ParseErrorKind::UnmatchedWhile, token.span)
            }
            Some(token) if token.kind == StructuralOpcode::ScopeEnd => self.error(
                ParseErrorKind::MismatchedBlockCloser {
                    expected: BlockCloser::End,
                    found: BlockCloser::ScopeEnd,
                },
                token.span,
            ),
            Some(token) => self.unexpected_structure(token),
            None => self.error(
                ParseErrorKind::UnclosedBlock {
                    expected: BlockCloser::End,
                },
                opener.span,
            ),
        }
    }

    fn error_for_scope_block(
        &self,
        stop: Option<StructuralToken>,
        opener: &StructuralToken,
    ) -> ParseError {
        match stop {
            Some(token) if token.kind == StructuralOpcode::Else => {
                self.error(ParseErrorKind::UnmatchedElse, token.span)
            }
            Some(token) if token.kind == StructuralOpcode::While => {
                self.error(ParseErrorKind::UnmatchedWhile, token.span)
            }
            Some(token) => self.unexpected_structure(token),
            None => self.error(
                ParseErrorKind::UnclosedBlock {
                    expected: BlockCloser::ScopeEnd,
                },
                opener.span,
            ),
        }
    }

    fn unexpected_structure(&self, token: StructuralToken) -> ParseError {
        match token.kind {
            StructuralOpcode::Else => self.error(ParseErrorKind::UnmatchedElse, token.span),
            StructuralOpcode::While => self.error(ParseErrorKind::UnmatchedWhile, token.span),
            _ => self.error(
                ParseErrorKind::UnexpectedStructuralOpcode {
                    opcode: token.kind.source_opcode(),
                },
                token.span,
            ),
        }
    }

    fn error(&self, kind: ParseErrorKind, span: Span) -> ParseError {
        ParseError { kind, span }
    }

    fn source_span(&self, start: usize, end: usize) -> Span {
        self.source
            .span(start, end)
            .expect("parser offsets are within normalized source")
    }

    fn point_span(&self, offset: usize) -> Span {
        self.source_span(offset, offset)
    }
}

fn joined_span(opener: Span, closer: Span) -> Span {
    Span {
        start: opener.start,
        end: closer.end,
    }
}

fn structural_opcode(opcode: u8) -> Option<StructuralOpcode> {
    match opcode {
        0x30 => Some(StructuralOpcode::If),
        0x31 => Some(StructuralOpcode::Else),
        0x32 => Some(StructuralOpcode::End),
        0x33 => Some(StructuralOpcode::Loop),
        0x34 => Some(StructuralOpcode::While),
        0x37 => Some(StructuralOpcode::For),
        0x40 => Some(StructuralOpcode::Function),
        0x55 => Some(StructuralOpcode::ScopeBegin),
        0x56 => Some(StructuralOpcode::ScopeEnd),
        0xA0 => Some(StructuralOpcode::AsyncFunction),
        _ => None,
    }
}
