//! Zero-copy string and byte processing helpers with explicit index units.

use std::iter::FusedIterator;

/// A delimiter error returned by the split helpers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SplitError {
    /// Empty delimiters are rejected to avoid ambiguous split behavior.
    EmptyDelimiter,
}

impl std::fmt::Display for SplitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyDelimiter => formatter.write_str("split delimiter must not be empty"),
        }
    }
}

impl std::error::Error for SplitError {}

/// Counts UTF-8 bytes in a string.
pub fn byte_len(value: &str) -> usize {
    value.len()
}

/// Counts bytes in an arbitrary byte sequence.
pub fn bytes_len(value: &[u8]) -> usize {
    value.len()
}

/// Returns the zero-based byte at `index`, or `None` out of range.
pub fn byte_at(value: &[u8], index: usize) -> Option<u8> {
    value.get(index).copied()
}

/// Returns a borrowed half-open byte slice `[start, end)`.
pub fn byte_slice(value: &[u8], start: usize, end: usize) -> Option<&[u8]> {
    if start > end {
        return None;
    }
    value.get(start..end)
}

/// Counts Unicode scalar values, using the byte length directly for ASCII text.
pub fn scalar_len(value: &str) -> usize {
    if value.is_ascii() {
        value.len()
    } else {
        value.chars().count()
    }
}

/// Returns the zero-based Unicode scalar at `index`, or `None` out of range.
pub fn scalar_at(value: &str, index: usize) -> Option<char> {
    if value.is_ascii() {
        value.as_bytes().get(index).copied().map(char::from)
    } else {
        value.chars().nth(index)
    }
}

/// Returns a borrowed half-open Unicode-scalar slice `[start, end)`.
pub fn scalar_slice(value: &str, start: usize, end: usize) -> Option<&str> {
    if start > end {
        return None;
    }
    if value.is_ascii() {
        return value.get(start..end);
    }

    let mut start_byte = (start == 0).then_some(0);
    let mut end_byte = None;
    let mut scalar_count = 0;
    for (byte, _) in value.char_indices() {
        if scalar_count == start {
            start_byte = Some(byte);
        }
        if scalar_count == end {
            end_byte = Some(byte);
        }
        scalar_count += 1;
    }
    if start == scalar_count {
        start_byte = Some(value.len());
    }
    if end == scalar_count {
        end_byte = Some(value.len());
    }

    value.get(start_byte?..end_byte?)
}

/// Finds `needle` and returns its zero-based Unicode-scalar start index.
/// An empty needle matches at index zero.
pub fn find(value: &str, needle: &str) -> Option<usize> {
    let byte_index = value.find(needle)?;
    if value.is_ascii() {
        Some(byte_index)
    } else {
        Some(value[..byte_index].chars().count())
    }
}

/// Finds a byte sequence and returns its zero-based byte start index.
/// An empty needle matches at byte index zero.
pub fn find_bytes(value: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > value.len() {
        return None;
    }
    value
        .windows(needle.len())
        .position(|window| window == needle)
}

/// An iterator over fields separated by an exact nonempty string delimiter.
pub struct StringSplit<'a> {
    remainder: Option<&'a str>,
    delimiter: &'a str,
}

impl<'a> Iterator for StringSplit<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        let remainder = self.remainder.take()?;
        match remainder.find(self.delimiter) {
            Some(index) => {
                self.remainder = Some(&remainder[index + self.delimiter.len()..]);
                Some(&remainder[..index])
            }
            None => Some(remainder),
        }
    }
}

impl FusedIterator for StringSplit<'_> {}

/// Splits on an exact nonempty string delimiter, preserving empty fields.
pub fn split_str<'a>(value: &'a str, delimiter: &'a str) -> Result<StringSplit<'a>, SplitError> {
    if delimiter.is_empty() {
        return Err(SplitError::EmptyDelimiter);
    }
    Ok(StringSplit {
        remainder: Some(value),
        delimiter,
    })
}

/// An iterator over byte slices separated by an exact nonempty byte delimiter.
pub struct BytesSplit<'a> {
    remainder: Option<&'a [u8]>,
    delimiter: &'a [u8],
}

impl<'a> Iterator for BytesSplit<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let remainder = self.remainder.take()?;
        match find_bytes(remainder, self.delimiter) {
            Some(index) => {
                self.remainder = Some(&remainder[index + self.delimiter.len()..]);
                Some(&remainder[..index])
            }
            None => Some(remainder),
        }
    }
}

impl FusedIterator for BytesSplit<'_> {}

/// Splits on an exact nonempty byte delimiter, preserving empty fields.
pub fn split_bytes<'a>(value: &'a [u8], delimiter: &'a [u8]) -> Result<BytesSplit<'a>, SplitError> {
    if delimiter.is_empty() {
        return Err(SplitError::EmptyDelimiter);
    }
    Ok(BytesSplit {
        remainder: Some(value),
        delimiter,
    })
}
