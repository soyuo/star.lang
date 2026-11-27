//! Deterministic standard input and buffered output primitives.

use std::fmt;
use std::io::{self, BufRead, BufWriter, Write};

use num_bigint::BigInt;
use num_traits::ToPrimitive;

const OUTPUT_BUFFER_CAPACITY: usize = 8 * 1024;

/// Errors returned by standard input parsing.
#[derive(Debug)]
pub enum InputError {
    /// The host input reader failed.
    Io(io::Error),
    /// A line or token expected to be UTF-8 was not valid UTF-8.
    InvalidUtf8,
    /// A token did not match the requested numeric syntax.
    MalformedInput(&'static str),
    /// A valid numeric token cannot be represented by the requested type.
    OutOfRange(&'static str),
    /// Reading more bytes would exceed the caller-provided limit.
    LimitExceeded,
}

impl fmt::Display for InputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "input I/O error: {error}"),
            Self::InvalidUtf8 => formatter.write_str("input is not valid UTF-8"),
            Self::MalformedInput(kind) => write!(formatter, "malformed {kind} input"),
            Self::OutOfRange(kind) => write!(formatter, "{kind} input is out of range"),
            Self::LimitExceeded => formatter.write_str("input exceeds the configured byte limit"),
        }
    }
}

impl std::error::Error for InputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// Buffered, byte-oriented standard input with deterministic EOF handling.
pub struct StandardInput<R: BufRead> {
    reader: R,
}

impl<R: BufRead> StandardInput<R> {
    /// Wraps an input reader.
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    /// Reads the remaining input bytes, failing before appending beyond `max_bytes`.
    pub fn read_all_bytes(&mut self, max_bytes: usize) -> Result<Vec<u8>, InputError> {
        let mut bytes = Vec::new();
        loop {
            let chunk = self.reader.fill_buf().map_err(InputError::Io)?;
            if chunk.is_empty() {
                return Ok(bytes);
            }
            append_limited(&mut bytes, chunk, max_bytes)?;
            let consumed = chunk.len();
            self.reader.consume(consumed);
        }
    }

    /// Reads one line, strips LF, CRLF, or CR, and returns `None` only at EOF.
    /// A final non-terminated line is returned as a line.
    pub fn read_line_bytes(&mut self, max_bytes: usize) -> Result<Option<Vec<u8>>, InputError> {
        let mut line = Vec::new();
        loop {
            let buffer = self.reader.fill_buf().map_err(InputError::Io)?;
            if buffer.is_empty() {
                return Ok((!line.is_empty()).then_some(line));
            }
            if let Some(index) = buffer
                .iter()
                .position(|byte| *byte == b'\n' || *byte == b'\r')
            {
                let terminator = buffer[index];
                append_limited(&mut line, &buffer[..index], max_bytes)?;
                self.reader.consume(index + 1);
                if terminator == b'\r'
                    && self.reader.fill_buf().map_err(InputError::Io)?.first() == Some(&b'\n')
                {
                    self.reader.consume(1);
                }
                return Ok(Some(line));
            }
            let consumed = buffer.len();
            append_limited(&mut line, buffer, max_bytes)?;
            self.reader.consume(consumed);
        }
    }

    /// Reads one UTF-8 line with the same terminator and EOF rules as `read_line_bytes`.
    pub fn read_line(&mut self, max_bytes: usize) -> Result<Option<String>, InputError> {
        self.read_line_bytes(max_bytes)?
            .map(|line| String::from_utf8(line).map_err(|_| InputError::InvalidUtf8))
            .transpose()
    }

    /// Reads the next token, separated by ASCII whitespace, without consuming its trailing delimiter.
    pub fn read_token_bytes(&mut self, max_bytes: usize) -> Result<Option<Vec<u8>>, InputError> {
        let mut token = Vec::new();
        let mut started = false;
        loop {
            let buffer = self.reader.fill_buf().map_err(InputError::Io)?;
            if buffer.is_empty() {
                return Ok((!token.is_empty()).then_some(token));
            }
            if !started {
                let first_non_whitespace =
                    buffer.iter().position(|byte| !byte.is_ascii_whitespace());
                let Some(start) = first_non_whitespace else {
                    let consumed = buffer.len();
                    self.reader.consume(consumed);
                    continue;
                };
                if start > 0 {
                    self.reader.consume(start);
                    continue;
                }
                started = true;
            }
            let token_end = buffer
                .iter()
                .position(u8::is_ascii_whitespace)
                .unwrap_or(buffer.len());
            if token_end == 0 {
                return Ok(Some(token));
            }
            append_limited(&mut token, &buffer[..token_end], max_bytes)?;
            let reached_delimiter = token_end < buffer.len();
            self.reader.consume(token_end);
            if reached_delimiter {
                return Ok(Some(token));
            }
        }
    }

    /// Reads a UTF-8 token; EOF before another token returns `None`.
    pub fn read_token(&mut self, max_bytes: usize) -> Result<Option<String>, InputError> {
        self.read_token_bytes(max_bytes)?
            .map(|token| String::from_utf8(token).map_err(|_| InputError::InvalidUtf8))
            .transpose()
    }

    /// Parses a signed, arbitrary-precision base-10 integer token.
    pub fn read_int(&mut self, max_bytes: usize) -> Result<Option<BigInt>, InputError> {
        let Some(token) = self.read_token_bytes(max_bytes)? else {
            return Ok(None);
        };
        let (negative, digits) = signed_digits(&token, "integer")?;
        let value = BigInt::parse_bytes(digits, 10).ok_or(InputError::MalformedInput("integer"))?;
        Ok(Some(if negative { -value } else { value }))
    }

    /// Parses a signed 64-bit decimal integer, returning an explicit range error on overflow.
    pub fn read_i64(&mut self, max_bytes: usize) -> Result<Option<i64>, InputError> {
        self.read_int(max_bytes)?
            .map(|value| value.to_i64().ok_or(InputError::OutOfRange("i64")))
            .transpose()
    }

    /// Parses a finite decimal floating-point token; overflow and nonzero underflow are range errors.
    pub fn read_float(&mut self, max_bytes: usize) -> Result<Option<f64>, InputError> {
        let Some(token) = self.read_token_bytes(max_bytes)? else {
            return Ok(None);
        };
        if !is_decimal_float(&token) {
            return Err(InputError::MalformedInput("float"));
        }
        let text = std::str::from_utf8(&token).map_err(|_| InputError::MalformedInput("float"))?;
        let value = text
            .parse::<f64>()
            .map_err(|_| InputError::OutOfRange("float"))?;
        if !value.is_finite()
            || (value == 0.0 && token.iter().any(|byte| matches!(byte, b'1'..=b'9')))
        {
            return Err(InputError::OutOfRange("float"));
        }
        Ok(Some(value))
    }

    /// Returns the wrapped reader.
    pub fn into_inner(self) -> R {
        self.reader
    }
}

/// Buffered output that preserves every byte written and has an explicit flush point.
pub struct BufferedOutput<W: Write> {
    writer: BufWriter<W>,
}

impl<W: Write> BufferedOutput<W> {
    /// Wraps a writer with a fixed 8 KiB buffer.
    pub fn new(writer: W) -> Self {
        Self {
            writer: BufWriter::with_capacity(OUTPUT_BUFFER_CAPACITY, writer),
        }
    }

    /// Flushes buffered bytes and returns the underlying writer.
    pub fn into_inner(self) -> Result<W, io::Error> {
        self.writer.into_inner().map_err(|error| error.into_error())
    }

    pub(crate) fn into_inner_after_execution(self) -> W {
        match self.writer.into_inner() {
            Ok(writer) => writer,
            Err(error) => error.into_inner().into_parts().0,
        }
    }
}

impl<W: Write> Write for BufferedOutput<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writer.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

fn append_limited(target: &mut Vec<u8>, bytes: &[u8], limit: usize) -> Result<(), InputError> {
    let new_len = target
        .len()
        .checked_add(bytes.len())
        .filter(|length| *length <= limit)
        .ok_or(InputError::LimitExceeded)?;
    target
        .try_reserve_exact(new_len.saturating_sub(target.len()))
        .map_err(|_| InputError::LimitExceeded)?;
    target.extend_from_slice(bytes);
    Ok(())
}

fn signed_digits<'a>(token: &'a [u8], kind: &'static str) -> Result<(bool, &'a [u8]), InputError> {
    let (negative, digits) = match token.first() {
        Some(b'-') => (true, &token[1..]),
        Some(b'+') => (false, &token[1..]),
        _ => (false, token),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(InputError::MalformedInput(kind));
    }
    Ok((negative, digits))
}

fn is_decimal_float(token: &[u8]) -> bool {
    let mut index = usize::from(matches!(token.first(), Some(b'+' | b'-')));
    let mut integer_digits = 0;
    while token.get(index).is_some_and(u8::is_ascii_digit) {
        integer_digits += 1;
        index += 1;
    }
    let mut fractional_digits = 0;
    if token.get(index) == Some(&b'.') {
        index += 1;
        while token.get(index).is_some_and(u8::is_ascii_digit) {
            fractional_digits += 1;
            index += 1;
        }
    }
    if integer_digits + fractional_digits == 0 {
        return false;
    }
    if matches!(token.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(token.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_start = index;
        while token.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if exponent_start == index {
            return false;
        }
    }
    index == token.len()
}
