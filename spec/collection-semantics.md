# Collection and String Semantics

Status: draft

This document defines the runtime behavior shared by the source interpreter and bytecode VM for `Array`, `Tuple`, `Map`, `Set`, `String`, `Bytes`, and `Iterator` values. Opcode stack effects are in `spec/instruction-semantics.md`.

## Values and iteration order

- `Array` is mutable, variable-length, and zero-indexed. `Tuple` is immutable and preserves element order.
- `Map` is a mutable hash map whose observable iteration order is insertion order. Replacing a value for an existing key does not move that key. Removing and reinserting a key appends it at the end.
- `Set` is a mutable hash set whose observable iteration order is the insertion order of distinct members. Inserting a duplicate does not move it. Removing and reinserting a member appends it at the end.
- `String` is immutable UTF-8 text. Character indexing, slicing, and `LENGTH` count Unicode scalar values, not bytes or grapheme clusters. `Bytes` is immutable and indexes individual bytes.
- `ITER` snapshots the collection's sequence when the iterator is created. It yields Array/Tuple elements, String characters, Bytes as `Byte`, Map keys, and Set members in the orders above. Mutations after iterator creation do not change that sequence; referenced mutable elements remain shared values.
- `NEXT` advances once and returns the next value with `true`; when exhausted, it returns `Null, false` and stays exhausted.
- `TO_STRING`/`PRINT` use the display rules in `spec/type-semantics.md`: Maps and Sets display only their lengths, and recursive Array/Tuple display marks a value already on the active path as `<cycle>`.

## Map keys and Set members

Maps and sets use the same immutable key equality and hashing rules. Valid keys are `Null`, `Bool`, `Int`, non-NaN `Float`, `String`, `Char`, `Byte`, `Bytes`, and tuples whose elements are all valid keys. `Array`, `Map`, `Set`, `Iterator`, and tuples containing any invalid key type are rejected with `TypeError`.

Equality is type-specific: for example, `Int(1)` and `Float(1.0)` are different keys. Floats use numeric equality; positive and negative zero are the same key, infinities are valid, and NaN is rejected because it is not equal to itself. Tuples compare recursively by element value. Strings compare by their Unicode scalar sequence and Bytes by their byte sequence. Map and Set equality is independent of insertion order.

`MAP_NEW` and `SET_NEW` preserve first insertion order and the original key/member value for iteration. Later duplicate map keys replace the prior value without changing the key's position; duplicate set members are ignored. A non-hashable map key or set member raises `TypeError`.

## Indexing, mutation, and failure behavior

- Array, Tuple, String, and Bytes indices must be nonnegative `Int` values. A value of another type raises `TypeError`; a negative or out-of-range index raises `IndexError`.
- `GET_ITEM` returns an Array/Tuple element, a String `Char`, or a Bytes `Byte`. A missing Map key raises `ReferenceError`; an invalid Map key raises `TypeError`.
- `SET_ITEM` mutates an existing Array index or inserts/replaces a Map entry. It never extends an Array. Tuple, String, and Bytes mutation raises `TypeError`.
- `APPEND` applies only to Array. `REMOVE` removes an Array element by index and returns it; for Map and Set it removes by key/member and returns the removed value. A missing Map key or Set member returns `Null`.
- `CONTAINS` uses runtime value equality for Array and Tuple; Map and Set use the key rules above. String accepts a `Char` or substring `String`; Bytes accepts a `Byte` or byte-sequence `Bytes`. An empty String/Bytes search sequence is contained in every String/Bytes value, respectively.
- `SLICE` uses a half-open range `[start, end)`, requires nonnegative `Int` bounds and `0 <= start <= end <= length`, and returns a copy of the same type. Invalid bounds raise `IndexError`; non-Int bounds raise `TypeError`.
- `CONCAT` accepts only two values of the same type among Array, Tuple, String, and Bytes. It returns a new value; Array results are mutable and Tuple results remain immutable.
- `LENGTH` counts Array/Tuple elements, Map entries, Set members, Unicode scalar values in String, or bytes in Bytes. It raises `TypeError` for unsupported values. `TO_BOOL` treats empty collections and empty String/Bytes as false, and nonempty ones as true.

## Complexity

Let `n` be the collection size, `k` the slice/search byte or scalar length, and `a`, `b` the concatenated operand sizes.

| Operation | Expected time | Extra space |
|---|---:|---:|
| Array/Tuple index or length | `O(1)` | `O(1)` |
| Array append | amortized `O(1)` | amortized `O(1)` |
| Array remove or Array/Tuple membership | `O(n)` | `O(1)` |
| String index, length, or scalar slice | `O(n)` | slice `O(k)` |
| Bytes index or length | `O(1)` | `O(1)` |
| Map/Set lookup, membership, insertion, or replacement | average `O(1)`, worst `O(n)` | average `O(1)` per new key/member |
| Map/Set removal | average `O(n)` including stable-order maintenance; worst `O(n)` | `O(1)` |
| Map/Set iteration snapshot | `O(n)` | `O(n)` |
| String/Bytes substring membership | worst `O(nk)` | `O(1)` |
| Array/Tuple/Bytes slice | `O(k)` | `O(k)` |
| Array/Tuple/String/Bytes concatenation | `O(a+b)` | `O(a+b)` |

Hash-based operations have worst-case linear behavior under collisions. The insertion-order index makes Map/Set removal linear even when the hash lookup itself is constant-time on average.
