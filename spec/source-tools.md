# Source Encoding and Formatting Tools

Status: stable

This document defines the source encoder, decoder, formatter, and minifier. All tools use the source character, grammar, integer, and string contracts in `spec/source-format.md`, `spec/grammar.ebnf`, `spec/integer-encoding.md`, and `spec/string-byte-encoding.md`.

## Formatter and minifier

`star fmt <file.star>` reads and validates a source file in normal mode, parses it, and prints a canonical source representation. It uses four spaces per nesting level, one token per line, no spaces inside data literals, and one final line feed for a nonempty program. `star fmt <file.star> --write` replaces the input file with that output.

Formatting preserves the parsed statement structure, instruction identities, and every data-literal bit, including leading zero bits. It does not preserve original whitespace or source spans. The parser rejects invalid source before any output or file replacement occurs.

`star minify <file.star>` validates and parses the source, then prints the normalized source with ASCII space, tab, CR, and LF removed. This whitespace is insignificant even inside a data literal. The output has no added final newline. `--write` replaces the input file with the minified form. A minified source file is valid in strict mode because it contains no formatting whitespace.

Parsing either formatter output produces the same syntax tree shape and literal bit sequences as the input. These tools do not evaluate or rewrite instructions.

## Encoder

`star encode` accepts one of these forms and writes a valid source fragment to stdout:

| Form | Output |
|---|---|
| `--int N` | A data literal for the magnitude of the decimal arbitrary-precision integer; a trailing `NEG` for negative values. |
| `--float N` | A 64-bit data literal followed by `FLOAT_FROM_BITS`; the input must parse as a finite decimal binary64 value. |
| `--string TEXT` | A big-endian payload integer, explicit UTF-8 byte length, `INT_TO_BYTES`, and `UTF8_DECODE`. Leading zero bytes are preserved by the length. |
| `--file PATH` | The file’s raw bytes encoded as a big-endian payload integer, explicit byte length, and `INT_TO_BYTES`. |
| `--opcode NAME` | The assigned ordinary instruction token for a case-insensitive mnemonic. Structural tokens such as `IF`, `END`, and `TRY` must be encoded as part of a complete block and are rejected here. |

Generated source is limited to 64 MiB. File input is also limited by the CLI file-size ceiling. A zero-byte input is encoded using a zero payload and zero length.

## Decoder

`star decode <source-file>` accepts a file containing exactly one encoded value or one ordinary instruction token. It prints one of `Int(decimal)`, `Float(bits=0x..., value=...)`, `Bytes(hex bytes)`, `String(escaped text)`, or `Opcode(MNEMONIC)`. A byte payload requires its explicit `INT_TO_BYTES` length. A string payload additionally requires `UTF8_DECODE`; malformed UTF-8 is rejected. Float output includes the exact 64-bit pattern so signed zero and other bit-level details remain visible.

The decoder recognizes the forms emitted by `star encode`; it is not a general Star evaluator. Both encode/decode and formatting use the canonical opcode catalog and reject unassigned source opcodes.
