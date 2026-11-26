# Runtime Types, Conversions, and Type Errors

Status: draft

This document defines runtime type tags, explicit conversions, truthiness, and type-error behavior. Instruction stack effects are in `spec/instruction-semantics.md`; integer and string encodings are in `spec/integer-encoding.md` and `spec/string-byte-encoding.md`. Arithmetic promotion and overflow rules are completed separately in `spec/arithmetic-semantics.md`.

## Runtime types

Every runtime value has exactly one canonical type tag. Type names are case-sensitive.

| Type | Runtime contract |
|---|---|
| `Int` | Signed arbitrary-precision integer; observable arithmetic does not silently wrap. |
| `Float` | IEEE 754 binary64 value, including signed zero, infinities, and NaN values. |
| `Bool` | Exactly `true` or `false`. |
| `Null` | The single absence-of-value value. |
| `Byte` | Unsigned integer in `0..255`. It is a distinct type from `Int`. |
| `Char` | One Unicode scalar value; surrogate code points are excluded. |
| `String` | Immutable sequence of Unicode scalar values, stored as valid UTF-8. |
| `Bytes` | Sequence of arbitrary byte values; it is not required to be valid UTF-8. |
| `Array` | Mutable, ordered, variable-length sequence of values. |
| `Tuple` | Ordered, fixed-length, immutable sequence of values. |
| `Map` | Mutable hash-based collection of key/value pairs. |
| `Set` | Mutable hash-based collection of distinct values. |
| `Struct` | User-defined nominal object with a runtime type ID and numeric field IDs. |
| `Function` | Named Star function value. |
| `Closure` | Callable value containing a function body and captured shared cells. |
| `NativeFunction` | Host-provided callable value. |
| `Iterator` | Stateful value that produces a sequence of values. |
| `Task` | Handle to an asynchronous computation. |
| `Error` | Runtime error value with an error type and message. |
| `Handle` | Opaque reference to a host resource. |
| `Module` | Handle to a loaded module and its exported symbols. |

Functions, closures, and native functions are separate runtime types; all three are callable. An interface is a nominal contract implemented by a `Struct`, not a separate value tag. `OBJECT_IS` checks a struct type ID or interface implementation. `TYPE_OF` reports the canonical tag above, and `TYPE_CHECK` tests one of those exact type names; use `OBJECT_IS` for a specific struct or interface.

`Map` and `Set` key hashability and equality are defined with collection operations; these type tags alone impose no additional key restrictions. Numeric promotion for arithmetic follows `spec/arithmetic-semantics.md`. No conversion happens implicitly except the explicitly specified mixed `Int`/`Float` arithmetic.

## Explicit conversions

Conversions are performed only by their conversion instruction. Unsupported source types, malformed numeric strings, and values outside the target domain raise `TypeError` unless a row states otherwise. The compiler may reject a conversion with `CompileError` when it can prove the failure statically; dynamic values are checked at runtime.

| Conversion | Accepted input and result |
|---|---|
| `TO_INT` | `Int` unchanged; `Byte` to its integer value; `Bool` to `0` or `1`; finite `Float` truncated toward zero; or an ASCII decimal `String` matching `[+-]?[0-9]+`, parsed as arbitrary-precision `Int`. |
| `TO_FLOAT` | `Float` unchanged; `Int` or `Byte` rounded to binary64 using round-to-nearest, ties-to-even; `Bool` to `0.0` or `1.0`; or an ASCII decimal `String` matching `[+-]?([0-9]+(\.[0-9]*)?|\.[0-9]+)([eE][+-]?[0-9]+)?`, parsed with the same rounding rule. Overflow produces the corresponding infinity; underflow may produce signed zero. |
| `TO_BOOL` | Applies the truthiness rule below and returns a `Bool`. |
| `TO_STRING` | Returns the canonical display form defined below. It is not a serialization and has no round-trip guarantee. |
| `TO_BYTES` / `UTF8_ENCODE` | Encode a `String` as its exact UTF-8 bytes. |
| `UTF8_DECODE` | Decode valid UTF-8 bytes to a `String`; malformed, overlong, truncated, surrogate, or out-of-range encodings raise `TypeError`. No replacement, normalization, or BOM removal occurs. |
| `FLOAT_FROM_BITS` | Reinterpret an unsigned 64-bit `Int` as the exact IEEE 754 binary64 bit pattern, including infinities and NaNs. |
| `TO_CHAR` | Convert an `Int` in `0..0x10FFFF` to its Unicode scalar value; surrogate code points raise `TypeError`. |
| `TO_BYTE` | Convert an `Int` in `0..255` to `Byte`; values outside that range raise `TypeError`. |
| `INT_TO_BYTES` | Produce exactly the requested number of big-endian bytes for a nonnegative `Int`; negative values, negative lengths, or insufficient width raise `TypeError`. See `spec/integer-encoding.md`. |

The decimal string grammars use ASCII digits only. They accept an optional leading sign, accept leading zeroes, and do not trim whitespace or accept separators, radix prefixes, `NaN`, or infinity spellings. A decimal `String` that overflows `TO_FLOAT` becomes infinity; `TO_INT` has no precision limit other than available resources.

## Truthiness

`TO_BOOL` returns `false` for `Null`, `false`, numeric zero (`Int`, `Byte`, positive or negative `Float` zero), and empty `String`, `Bytes`, `Array`, `Tuple`, `Map`, or `Set`. It returns `true` for every other value, including `Char` U+0000, nonempty collections, `NaN`, infinities, functions, structs, errors, tasks, handles, and modules. No user-defined conversion hook is called.

`IF` and `WHILE` require a `Bool` directly and never apply truthiness. Use `TO_BOOL` explicitly when a value should control a branch.

## Canonical type names and `TO_STRING`

The exact `TYPE_OF`/`TYPE_CHECK` names are `Int`, `Float`, `Bool`, `Null`, `Byte`, `Char`, `String`, `Bytes`, `Array`, `Tuple`, `Map`, `Set`, `Struct`, `Function`, `Closure`, `NativeFunction`, `Iterator`, `Task`, `Error`, `Handle`, and `Module`. `TYPE_CHECK` with any other name raises `TypeError`; matching is exact and case-sensitive.

`TO_STRING` uses these display forms:

- `Int` and `Byte` use base-10 digits; `Bool` is `true` or `false`; `Null` is `null`; `Char` is its scalar value; `String` is returned unchanged.
- Finite `Float` uses the shortest decimal that round-trips to the same binary64 value, with a decimal point or exponent so it is visibly floating-point. Positive and negative zero are `0.0` and `-0.0`; infinities are `Infinity` and `-Infinity`; NaN is `NaN`.
- `Bytes` is `0x` followed by two lowercase hexadecimal digits per byte; empty bytes display as `0x`.
- `Array` displays its values in order as `[value, ...]`, with an empty array as `[]`. `Tuple` displays `(value, ...)`, with an empty tuple as `()` and a trailing comma for a one-element tuple. Recursive display replaces a reference already on the active display path with `<cycle>`.
- `Map` and `Set` display as `Map(len=N)` and `Set(len=N)` so their output does not depend on hash iteration order. A `Struct` displays as `Struct#ID`; field contents are not serialized by this conversion.
- A named `Function` displays as `Function#ID`; a `Closure` as `Closure#BODY_ID`; a `NativeFunction` as `NativeFunction#ID`. `Iterator`, `Task`, `Handle`, and `Module` display as their type names without exposing host addresses or resource contents.
- An `Error` displays as `ErrorType: message`.

## Type errors

An operation that receives a value outside its accepted type set raises `TypeError` at runtime. A statically provable type violation is a `CompileError`. `TypeError` is not used for absent bindings (`ReferenceError`), invalid indices (`IndexError`), or unauthorized resources (`PermissionError`). Arithmetic-specific operand, division, remainder, and mixed-number behavior is defined by `spec/arithmetic-semantics.md`.
