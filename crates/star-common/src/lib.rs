//! Types shared by the Star lexer, parser, interpreter, and command-line tools.

/// A source coordinate using a zero-based byte offset and one-based line and byte-column values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourcePosition {
    /// Zero-based offset from the start of the UTF-8 source file.
    pub byte_offset: usize,
    /// One-based source line.
    pub line: usize,
    /// One-based byte column within the source line.
    pub column: usize,
}

/// A half-open source range from `start` through, but not including, `end`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    /// Inclusive start coordinate.
    pub start: SourcePosition,
    /// Exclusive end coordinate.
    pub end: SourcePosition,
}
