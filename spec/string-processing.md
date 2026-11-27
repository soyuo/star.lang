# String and Byte Processing

Status: draft

`star_interpreter::runtime::strings` provides zero-copy Rust helpers for the `std.string` and `std.bytes` implementations. Guest-visible `String` values are valid UTF-8; `Bytes` values are arbitrary bytes. String indices count Unicode scalar values, while byte indices count UTF-8 or raw bytes. Neither API counts grapheme clusters.

| API | Index unit and behavior | Cost |
|---|---|---|
| `byte_len(&str)` | UTF-8 bytes | `O(1)` |
| `bytes_len(&[u8])` | Raw bytes | `O(1)` |
| `byte_at(&[u8], index)` | Zero-based byte lookup | `O(1)` |
| `byte_slice(&[u8], start, end)` | Borrowed half-open byte range `[start, end)`; invalid or reversed bounds return `None` | `O(1)` |
| `scalar_len(&str)` | Unicode scalar values | ASCII `O(1)`; non-ASCII `O(n)` |
| `scalar_at(&str, index)` | Zero-based scalar lookup | ASCII `O(1)`; non-ASCII `O(n)` |
| `scalar_slice(&str, start, end)` | Borrowed half-open scalar range `[start, end)`; invalid or reversed bounds return `None` | ASCII `O(1)`; non-ASCII `O(n)` boundary scan |
| `find(&str, needle)` | First match as a zero-based scalar index; empty needle returns `Some(0)` | `O(nm)` worst case, including scalar counting before a non-ASCII match |
| `find_bytes(&[u8], needle)` | First match as a zero-based byte index; empty needle returns `Some(0)` | `O(nm)` worst case for haystack length `n` and needle length `m` |
| `split_str` / `split_bytes` | Exact, nonempty delimiter; lazy borrowed fields; leading, adjacent, and trailing empty fields are preserved; empty input yields one empty field | `O(nm)` worst case over a complete scan; `O(1)` iterator space |

The scalar APIs take a byte-fast path for ASCII. For non-ASCII text, scalar offsets are translated through UTF-8 character boundaries, so returned slices always remain valid `&str`. Byte APIs never decode or normalize bytes. Split delimiters are matched exactly and are not trimmed; an empty delimiter returns `SplitError::EmptyDelimiter`.

The helpers return borrowed slices or iterators and do not allocate. Guest wrappers that materialize a new Star String or Bytes value must copy the result and account for its capacity under `max_memory_bytes`.

## Standard-library boundary

These Rust helpers are available through the interpreter crate. Binding them as `std.string` and `std.bytes` functions depends on later standard-module loading; this document does not claim those imports already work. Existing `GET_ITEM`, `LENGTH`, `SLICE`, `CONTAINS`, `UTF8_ENCODE`, and `UTF8_DECODE` retain the contracts in `spec/collection-semantics.md`.
