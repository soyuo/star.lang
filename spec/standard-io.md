# Standard I/O

This document defines the reusable Rust I/O primitives used by the reference interpreter and bytecode VM. The public implementation is `star_interpreter::runtime::stdio`; module loading later binds these primitives as `std.io` functions for Star programs.

## Input

`StandardInput<R: BufRead>` reads from one byte stream in order. Its read methods take a maximum byte count and return `InputError::LimitExceeded` before appending data beyond that limit.

| Method | Behavior |
|---|---|
| `read_all_bytes(limit)` | Returns all remaining bytes; an already exhausted stream returns an empty byte vector. |
| `read_line_bytes(limit)` | Returns one line without its terminator. LF, CRLF, and lone CR terminate lines. A final nonempty unterminated line is returned once; EOF with no remaining bytes returns `None`. |
| `read_line(limit)` | Same line and EOF behavior, then validates the bytes as UTF-8. Invalid UTF-8 is `InputError::InvalidUtf8`. |
| `read_token_bytes(limit)` | Skips leading ASCII whitespace (`0x09` through `0x0D`, and `0x20`), then returns bytes through the next whitespace. The trailing delimiter remains for the next read to skip. |
| `read_token(limit)` | Same tokenization, then validates UTF-8. |
| `read_int(limit)` | Parses optional `+` or `-` followed by one or more ASCII decimal digits into arbitrary-precision `BigInt`. No prefixes, separators, or decimal points are accepted. |
| `read_i64(limit)` | Parses the same integer syntax, then returns `OutOfRange("i64")` when the value is outside the signed 64-bit range. |
| `read_float(limit)` | Parses decimal notation with optional sign, fraction, and exponent into a finite binary64 value. Malformed syntax is `MalformedInput("float")`; overflow and nonzero underflow are `OutOfRange("float")`. Signed zero is preserved. `NaN` and infinity spellings are not accepted. |

Every `Option` result distinguishes EOF (`None`) from an empty line or empty input token where the line grammar permits one. Input I/O errors remain `InputError::Io`; byte limits are `InputError::LimitExceeded`. The existing `INPUT` instruction uses the same line reader, pushes a UTF-8 `String`, pushes `Null` at EOF, and reports invalid UTF-8 or host read failures as `IOError`.

## Output

`BufferedOutput<W: Write>` uses an 8 KiB buffer and implements `Write`. It forwards bytes unchanged: it adds no newline, trims no spaces, and performs no encoding conversion. `flush()` sends all buffered bytes to the underlying writer and returns host write failures.

Both `Interpreter::execute` and `BytecodeVm::execute` attempt to flush on success and failure. If execution failed, that error is preserved after the flush attempt; if execution succeeded but flushing failed, execution returns `IOError`. `PRINT` formats its value as specified by the instruction semantics and writes those exact bytes through the buffer.

## Scope

These primitives are reusable from Rust and are the implementation foundation for `std.io`. Star-level module names and exports are connected when module loading is implemented; this specification does not define imports or module initialization.
