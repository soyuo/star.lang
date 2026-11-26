# Integer Literal Encoding

Status: draft

This document defines source integer literal values, the signed `Int` domain, and conversion of an `Int` to bytes. `spec/grammar.ebnf` defines token boundaries; `spec/source-format.md` defines source character validation.

## Source integer literals

A source integer literal is a non-empty data literal: a double quote, one or more bit characters, and a closing double quote. Within the payload, apostrophe means bit `0` and asterisk means bit `1`. The first payload bit is the most significant bit. The payload is interpreted as an unsigned binary magnitude and produces an `Int` value.

| Source literal | Payload bits | `Int` value |
|---|---|---:|
| `"'"` | `0` | 0 |
| `"*"` | `1` | 1 |
| `"**"` | `11` | 3 |
| `"'*"` | `01` | 1 |
| `"**''*''"` | `1100100` | 100 |

Leading zero bits are valid and do not change the value. An empty payload is invalid. Literal width is not inferred from the number of source bytes; use `INT_TO_BYTES` with an explicit length when a byte width and leading zero bytes must be preserved.

## Signed and arbitrary-precision values

`Int` is the mathematical domain of arbitrary-precision signed integers. A source integer literal itself is nonnegative; there is no sign character or signed-literal prefix. Produce a negative value by applying `NEG` to a nonnegative `Int`, or by arithmetic. The sign does not change the literal bit order.

Integer operations must preserve their mathematical result and must not wrap or saturate at a machine width. An implementation may store common values in a fixed-width fast representation, but it must promote when needed so that observable behavior remains arbitrary precision. Resource limits may reject an operation that cannot be represented within the configured memory or execution limits; silent truncation is not allowed.

Zero has one semantic value, regardless of leading zero bits. Negative zero is not a distinct `Int`. Bitwise operations use the infinite two's-complement behavior specified in `spec/instruction-semantics.md`; this rule does not make source literals signed bit patterns.

## Integer-to-byte conversion

`INT_TO_BYTES` consumes a nonnegative `Int` value followed by a nonnegative byte length and produces exactly that many bytes in big-endian order: the most significant byte comes first. The integer is zero-extended on the left to fill the requested width. A length of zero is valid only for value zero and produces an empty `Bytes` value. A value that does not fit the requested width, a negative value, or a negative length raises `TypeError`.

| Value | Length | Output bytes (hex) |
|---:|---:|---|
| 0 | 1 | `00` |
| 5 | 3 | `00 00 05` |
| 258 | 2 | `01 02` |
| 256 | 1 | `TypeError` |

This conversion is unsigned and preserves leading zero bytes through its explicit length. Signed `Int` values remain valid runtime values; callers must choose an explicit byte-level representation before converting a negative value.

## Source and `.starc` byte order

Source integer literals record the most significant bit first. `INT_TO_BYTES` emits the most significant byte first. These source-level rules do not change `.starc` file layout: the bytecode format specifies little-endian fixed-width integer fields and ULEB128 or SLEB128 variable-length operands. The serialized constant-pool representation for arbitrary-precision integers belongs to the `.starc` format specification and must preserve this document's signed mathematical value without narrowing it.
