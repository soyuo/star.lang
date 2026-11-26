//! Source-character validation and normalization for Star programs.

use std::fmt;

pub use star_common::{SourcePosition, Span};

/// Controls whether source formatting whitespace is accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LexMode {
    /// Accept and discard ASCII space, tab, carriage return, and line feed.
    Normal,
    /// Accept only `*`, `'`, and `"`.
    Strict,
}

/// The reason source validation failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LexErrorKind {
    /// The source begins with a UTF-8 byte-order mark.
    ByteOrderMark,
    /// The byte sequence at the reported location is not valid UTF-8.
    InvalidUtf8,
    /// A valid but unsupported source character was found.
    InvalidCharacter,
    /// Formatting whitespace was found in strict mode.
    WhitespaceNotAllowed,
}

/// A source validation error located in the original input bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LexError {
    /// Category of source error.
    pub kind: LexErrorKind,
    /// Location of the first byte involved in the error.
    pub position: SourcePosition,
    /// First byte involved in the error.
    pub found_byte: u8,
}

impl fmt::Display for LexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let location = format!(
            "line {}, column {} (byte offset {})",
            self.position.line, self.position.column, self.position.byte_offset
        );

        match self.kind {
            LexErrorKind::ByteOrderMark => {
                write!(formatter, "UTF-8 BOM is not allowed at {location}")
            }
            LexErrorKind::InvalidUtf8 => write!(
                formatter,
                "invalid UTF-8 sequence starting with byte 0x{:02X} at {location}",
                self.found_byte
            ),
            LexErrorKind::InvalidCharacter => write!(
                formatter,
                "invalid source character starting with byte 0x{:02X} at {location}",
                self.found_byte
            ),
            LexErrorKind::WhitespaceNotAllowed => write!(
                formatter,
                "formatting whitespace is not allowed in strict mode at {location}"
            ),
        }
    }
}

impl std::error::Error for LexError {}

/// Validated source bytes after the selected mode's whitespace handling.
///
/// Each normalized byte offset, including the one-past-end offset, maps to a
/// coordinate in the original source. This preserves locations when normal
/// mode discards formatting whitespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedSource {
    bytes: Vec<u8>,
    original_positions: Vec<SourcePosition>,
    end_position: SourcePosition,
}

impl NormalizedSource {
    /// Returns the validated source with normal-mode whitespace removed.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Maps a normalized byte offset to its original location; EOF maps to the
    /// end of the original input, including any discarded trailing whitespace.
    pub fn position_at(&self, normalized_offset: usize) -> Option<SourcePosition> {
        if normalized_offset == self.bytes.len() {
            Some(self.end_position)
        } else {
            self.original_positions.get(normalized_offset).copied()
        }
    }

    /// Maps a normalized half-open byte range to its original source span.
    /// The end coordinate follows the final included byte, excluding trailing
    /// formatting whitespace from the span.
    pub fn span(&self, start: usize, end: usize) -> Option<Span> {
        if start > end {
            return None;
        }

        let start_position = self.position_at(start)?;
        let end_position = if end > start {
            let last_byte = *self.original_positions.get(end - 1)?;
            // Every normalized character is one of the single-byte ASCII source glyphs.
            SourcePosition {
                byte_offset: last_byte.byte_offset + 1,
                line: last_byte.line,
                column: last_byte.column + 1,
            }
        } else {
            start_position
        };

        Some(Span {
            start: start_position,
            end: end_position,
        })
    }
}

/// Validates source encoding and characters, discarding normal-mode whitespace.
///
/// This stage does not parse tokens or validate opcodes. It accepts only `*`,
/// `'`, and `"`, plus ASCII space, tab, CR, and LF in normal mode. Diagnostics
/// always refer to positions in the original byte stream.
pub fn normalize_source(source: &[u8], mode: LexMode) -> Result<NormalizedSource, LexError> {
    if source.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(LexError {
            kind: LexErrorKind::ByteOrderMark,
            position: SourcePosition {
                byte_offset: 0,
                line: 1,
                column: 1,
            },
            found_byte: 0xEF,
        });
    }

    let mut cursor = SourceCursor::default();
    let mut bytes = Vec::with_capacity(source.len());
    let mut original_positions = Vec::with_capacity(source.len());

    while cursor.byte_offset < source.len() {
        let byte = source[cursor.byte_offset];
        let position = cursor.position();

        if is_format_whitespace(byte) {
            if mode == LexMode::Strict {
                return Err(LexError {
                    kind: LexErrorKind::WhitespaceNotAllowed,
                    position,
                    found_byte: byte,
                });
            }

            cursor.advance(byte);
            continue;
        }

        if matches!(byte, b'*' | b'\'' | b'"') {
            bytes.push(byte);
            original_positions.push(position);
            cursor.advance(byte);
            continue;
        }

        let kind = if byte.is_ascii() {
            LexErrorKind::InvalidCharacter
        } else {
            match std::str::from_utf8(&source[cursor.byte_offset..]) {
                Err(error) if error.valid_up_to() == 0 => LexErrorKind::InvalidUtf8,
                Err(_) | Ok(_) => LexErrorKind::InvalidCharacter,
            }
        };

        return Err(LexError {
            kind,
            position,
            found_byte: byte,
        });
    }

    Ok(NormalizedSource {
        bytes,
        original_positions,
        end_position: cursor.position(),
    })
}

fn is_format_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

#[derive(Clone, Copy, Debug)]
struct SourceCursor {
    byte_offset: usize,
    line: usize,
    column: usize,
    previous_was_cr: bool,
}

impl Default for SourceCursor {
    fn default() -> Self {
        Self {
            byte_offset: 0,
            line: 1,
            column: 1,
            previous_was_cr: false,
        }
    }
}

impl SourceCursor {
    fn position(self) -> SourcePosition {
        SourcePosition {
            byte_offset: self.byte_offset,
            line: self.line,
            column: self.column,
        }
    }

    fn advance(&mut self, byte: u8) {
        self.byte_offset += 1;

        match byte {
            b'\r' => {
                self.line += 1;
                self.column = 1;
                self.previous_was_cr = true;
            }
            b'\n' => {
                if !self.previous_was_cr {
                    self.line += 1;
                }
                self.column = 1;
                self.previous_was_cr = false;
            }
            _ => {
                self.column += 1;
                self.previous_was_cr = false;
            }
        }
    }
}
