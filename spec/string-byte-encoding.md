# String and Byte Encoding

Status: draft

This document defines how source integer payloads construct byte sequences and UTF-8 strings. `spec/integer-encoding.md` defines the source bit mapping and `Int` values; `spec/source-format.md` defines which characters and formatting whitespace are accepted.

## Source payloads and byte sequences

A quoted bit payload is an unsigned integer literal, not a `Bytes` or `String` value by itself. To construct bytes, place the nonnegative payload integer and the desired byte length on the stack, then execute `INT_TO_BYTES`:

```text
PUSH PAYLOAD_INTEGER
PUSH BYTE_LENGTH
INT_TO_BYTES
```

The integer is interpreted as an unsigned, big-endian byte sequence of exactly `BYTE_LENGTH` bytes. `INT_TO_BYTES` left-pads with zero bytes and never trims leading zeros. For length `N > 0`, the payload must satisfy `0 <= value < 256^N`; for length zero, only value zero is valid and the result is empty. Negative values and invalid lengths raise `TypeError`.

The payload bits themselves are most-significant-bit first, with apostrophe representing zero and asterisk representing one. The explicit byte length supplies any leading zero bits and bytes omitted from the integer's minimal spelling. Formatting spaces, tabs, CR, and LF in normal source mode are discarded before tokenization, including inside quoted payloads; they are not string or byte data.

| Intended bytes | Payload integer | Byte length | Result |
|---|---:|---:|---|
| `00` | 0 | 1 | `00` |
| `00 41` | 65 | 2 | `00 41` |
| `C3 A9` | 50089 (`0xC3A9`) | 2 | `C3 A9` |
| empty | 0 | 0 | empty `Bytes` |

To form the source payload for a byte sequence, interpret its bytes as one unsigned big-endian integer and provide the original byte count as `BYTE_LENGTH`. This pair is lossless, including for empty and leading-zero byte sequences.

## UTF-8 strings

`UTF8_DECODE` consumes `Bytes` and produces a `String` only when the entire byte sequence is valid UTF-8. Decoding is strict: it rejects malformed continuation bytes, truncated sequences, overlong encodings, surrogate code points, and values above U+10FFFF. It does not replace invalid input with U+FFFD or normalize Unicode. Invalid input raises `TypeError`.

`UTF8_ENCODE` consumes a `String` and produces its UTF-8 byte sequence. A source string value can therefore be constructed from payload and length with `INT_TO_BYTES`, followed by `UTF8_DECODE`. An encoded string can be returned to bytes with `UTF8_ENCODE` or `TO_BYTES`.

All Unicode scalar values are supported. A U+0000 scalar is a zero byte and is preserved; U+FEFF inside string data is also an ordinary scalar. The prohibition on a UTF-8 BOM applies to the source file itself, not to bytes decoded as string data. Source line breaks used as formatting are not data; encode a line-feed scalar as byte `0A` in the payload.

## Length and round-trip rules

- `Bytes` length is the number of bytes.
- `String` length is the number of Unicode scalar values. Its UTF-8 byte length is the number of bytes produced by `UTF8_ENCODE`; it may differ from scalar length.
- For every byte sequence `b`, `INT_TO_BYTES(unsigned_big_endian_value(b), length(b))` returns exactly `b`.
- For every valid UTF-8 byte sequence `b`, `UTF8_ENCODE(UTF8_DECODE(b))` returns exactly `b`.
- For every `String` value `s`, `UTF8_DECODE(UTF8_ENCODE(s))` returns exactly `s`, with no normalization or scalar changes.
- Invalid UTF-8 has no `String` round trip: `UTF8_DECODE` fails instead of repairing or truncating the input.

| Input | Operation | Result |
|---|---|---|
| `00 41` | `UTF8_DECODE` | two-scalar string containing U+0000 then `A` |
| `C3 A9` | `UTF8_DECODE` | `é` (one scalar, two bytes) |
| `C3 28` | `UTF8_DECODE` | `TypeError` |
| empty `Bytes` | `UTF8_DECODE` | empty `String` |

The string and byte stack effects for `UTF8_DECODE`, `UTF8_ENCODE`, `TO_BYTES`, `LENGTH`, and `INT_TO_BYTES` are specified in `spec/instruction-semantics.md` and `spec/integer-encoding.md`.
