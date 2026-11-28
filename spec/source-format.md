# Star Source Format

Status: stable

This document defines the character-level format of Star source files. It does not define the full token grammar, which is specified separately in `grammar.ebnf`.

## Source alphabet

The only source-language characters are these exact ASCII code points:

| Glyph | Unicode code point | Role |
|---|---|---|
| `*` | U+002A ASTERISK | Instruction character; bit value 1 |
| `'` | U+0027 APOSTROPHE | Instruction delimiter; bit value 0 |
| `"` | U+0022 QUOTATION MARK | Data-literal delimiter |

Unicode lookalikes, typographic quotes, full-width forms, and other visually similar characters are invalid. Source text is not Unicode-normalized before validation.

## File encoding

- A `.star` source file is decoded as UTF-8.
- Invalid UTF-8 and a UTF-8 byte-order mark (BOM) are errors.
- The compiler must report the original byte offset and line/column for invalid input.
- Line endings do not separate instructions and have no language meaning.

## Formatting modes

### Normal mode

Normal mode accepts the three source characters plus ASCII space (U+0020), horizontal tab (U+0009), carriage return (U+000D), and line feed (U+000A). These four formatting characters are ignored before tokenization, both between tokens and within literals, and do not change program meaning.

Formatting whitespace may occur between tokens or inside a data literal and is ignored before tokenization. After whitespace removal, a literal payload must contain only the asterisk and apostrophe characters. Any other character is an error; normal mode does not silently discard arbitrary text.

### Strict mode

Strict mode accepts only `*`, `'`, and `"`. Spaces, tabs, line endings, BOMs, and all other characters are errors.

## Data literals

A data literal is delimited by U+0022 quotation marks. Its payload consists only of `*` and `'`, representing bits 1 and 0 respectively, most-significant bit first. No escape sequence or raw character syntax is defined in this format.

Textual data is represented by encoding the text as UTF-8 bytes and then encoding those bytes as bits in the literal. Decoding the data as a string is a separate operation and must validate UTF-8. The byte length is carried separately where needed so leading zero bits/bytes are preserved.

## Comments

The source grammar defines no comment syntax. A comment written with letters or punctuation is invalid source; whitespace formatting remains the only non-token text accepted in normal mode.

## Source positions

Lexing may ignore normal-mode formatting whitespace, but diagnostics and source maps retain positions in the original UTF-8 file. Report the original byte offset and a one-based line and byte column.

Byte offsets are zero-based from the beginning of the file. Line and byte-column numbers are one-based; the byte column counts UTF-8 bytes since the start of the current line. Treat CRLF as one line break, and treat a standalone CR or LF as one line break. The first byte after a line break is column 1. For invalid UTF-8, report the offset of the first invalid byte and compute its line and byte column from preceding bytes without replacing or normalizing the input. A BOM is reported at byte offset 0, line 1, column 1.

## References

- QLM `star-lang/language/source-format/characters.md`
- QLM `star-lang/language/syntax/grammar.md`
- QLM `star-lang/programming/strings.md`