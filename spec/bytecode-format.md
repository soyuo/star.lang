# `.starc` Bytecode File Format

Status: draft

This document defines version 1.0 of the serialized `.starc` container, its sections, constant pool, module/type/function tables, and code ranges. The container's numeric fields are independent of source instruction encodings in `spec/opcodes.toml`. IR lowering is defined in `spec/ir.md`; source-language stack behavior remains in `spec/instruction-semantics.md`.

## Encoding primitives and versioning

- Fixed-width integers are unsigned unless marked signed, and are little-endian.
- `ULEB128` and `SLEB128` operands use the shortest valid encoding. `Int` values may use an arbitrary number of LEB128 bytes, subject to file and resource limits.
- Counts and table indexes are `u32`; byte offsets and section lengths are `u64`. Indexes are zero-based. `0xFFFFFFFF` is reserved as the absent-index sentinel and cannot be a valid table index.
- Text in the file is UTF-8. String constants must be valid UTF-8 and are not normalized. Byte constants may contain any byte sequence.

Version 1.0 uses major version `1` and minor version `0`. A reader rejects an unsupported major version. A reader may accept a newer minor version only when all unknown sections are optional and all defined reserved fields remain zero; otherwise it rejects the file. Writers set the file flags to zero in version 1.0.

## File header and section directory

The 16-byte header is followed immediately by `section_count` directory entries. Each directory entry is 24 bytes. The directory is sorted by increasing section ID; section payloads follow in the same order with no padding or gaps.

| Header offset | Field | Encoding |
|---:|---|---|
| 0 | Magic | Four ASCII bytes `STAR` |
| 4 | Major version | `u16` |
| 6 | Minor version | `u16` |
| 8 | File flags | `u32`, zero in version 1.0 |
| 12 | Section count | `u32` |

| Directory offset | Field | Encoding |
|---:|---|---|
| 0 | Section ID | `u32` |
| 4 | Section flags | `u32`; bit 0 means required, all other bits are reserved zero |
| 8 | Payload offset | `u64`, absolute from the first byte of the file |
| 16 | Payload length | `u64` bytes |

Each payload offset must equal the end of the preceding payload; the first payload begins at `16 + 24 * section_count`. Every payload must be within the file, section IDs must be unique, and the final payload must end at EOF. Arithmetic used to check offsets and lengths must be overflow-checked.

| ID | Section | Requirement |
|---:|---|---|
| `1` | ConstantPool | Required |
| `2` | ModuleTable | Required |
| `3` | FunctionTable | Required |
| `4` | Code | Required |
| `5` | TypeTable | Required; may be empty |
| `6` | ExceptionTable | Required; may be empty |
| `7` | DebugInfo | Optional |
| `0xFFFFFFFF` | Checksum | Required and last |

Version 1.0 files therefore contain seven sections, or eight when `DebugInfo` is present. A reader rejects a missing required section, a duplicate section, an unknown section marked required, or an unknown section placed after `Checksum`. Unknown optional section IDs below `0xFFFFFFFF` are skipped. The required bit is set for IDs 1-6 and `0xFFFFFFFF`; it is clear for ID 7.

## Constant pool

The `ConstantPool` payload begins with `count:u32`, followed by exactly `count` entries in index order. Each entry is `tag:u8`, `payload_length:ULEB128`, then that many payload bytes. An index is the entry's zero-based position. Duplicate constants are allowed; an emitter may deduplicate them but is not required to.

| Tag | Kind | Payload |
|---:|---|---|
| `0x00` | Null | Empty |
| `0x01` | Bool | One byte: `0` false or `1` true |
| `0x02` | Int | One canonical SLEB128 integer, signed and arbitrary precision |
| `0x03` | Float | Exactly 8 bytes, IEEE 754 binary64 bits in little-endian order |
| `0x04` | String | Exact UTF-8 bytes; the entry length is the byte length |
| `0x05` | Bytes | Exact uninterpreted bytes |
| `0x06` | Char | One canonical ULEB128 Unicode scalar value |
| `0x07` | FunctionRef | Exactly one `u32` FunctionTable index |
| `0x08` | TypeRef | Exactly one `u32` TypeTable index |

The reader rejects an unknown tag, an invalid tag payload length, noncanonical LEB128, an invalid UTF-8 `String`, a `Bool` other than `0` or `1`, or a `Char` that is not a Unicode scalar. Float bits are preserved, including signed zero and NaN payloads. Every reference index must resolve after all sections have been read.

## Module table

The `ModuleTable` payload is `module_count:u32`, `entry_module_index:u32`, then `module_count` records in index order. The entry index must identify one module. Each module record has this fixed prefix followed by its arrays:

```text
name_string_index:u32
initializer_function_index:u32
first_function_index:u32
function_count:u32
flags:u32
global_count:u32
globals[global_count]
import_count:u32
imports[import_count]
export_count:u32
exports[export_count]
```

Module `name_string_index` refers to a String constant. Function ranges are contiguous in the FunctionTable and must agree with each function's `module_index`. `flags` is zero in version 1.0.

Each global record is `{ variable_id_int_constant_index:u32, flags:u32 }`. The ID must refer to a nonnegative Int constant and be unique within that module. Global flag bit 0 means immutable; other bits are reserved zero.

Each import record is `{ module_name_string_index:u32, symbol_name_string_index:u32, local_id_int_constant_index:u32, kind:u8, reserved:[u8;3] }`. Each export record is `{ name_string_index:u32, id_constant_index:u32, kind:u8, reserved:[u8;3] }`. `kind` is `0` variable, `1` function, or `2` type. Import module/symbol names must refer to String constants; local IDs and exported IDs must refer to nonnegative Int constants. Reserved bytes are zero. Dynamic imports remain runtime instructions and do not require a static import record.

## Function table

The `FunctionTable` payload is `function_count:u32`, followed by records in module order. Each record is:

```text
module_index:u32
function_id_int_constant_index:u32
name_string_index:u32
flags:u32
parameter_count:u32
rest_parameter_code:u32
parameter_id_int_constant_indices[parameter_count]:u32
max_stack:u32
code_offset:u64
code_length:u64
```

Function and parameter IDs refer to nonnegative Int constants. `name_string_index` refers to a String constant or is `0xFFFFFFFF` for an unnamed function. `function_id_int_constant_index` is `0xFFFFFFFF` only for a module initializer. Function records are ordered by module index, with a module initializer first and remaining functions ordered by numeric function ID. A module's `first_function_index` and `function_count` identify its contiguous range.

Function flag bit 0 marks `ASYNC_FUNC`; bit 1 marks the module initializer; all other bits are reserved zero. A module initializer has no function ID, no parameters, `rest_parameter_code=0`, and is not asynchronous. Every module has exactly one initializer, and the ModuleTable reference points to it. A non-initializer has a function ID unique within its module. Parameter IDs are distinct within a function. `rest_parameter_code` is `0` for fixed arity or equals `parameter_count` for a variadic function, following `spec/function-abi.md`.

`max_stack` is the maximum number of operand values required by the function, including values live at a control-flow join. `code_offset` is relative to the beginning of the Code section; `code_length` is the function's byte length. Function ranges must be in bounds, nonoverlapping, and ordered by function-table index. Each function range contains whole instructions and terminates according to its function kind. The Code section contains no unreferenced bytes.

## Type table

The `TypeTable` payload begins with `type_count:u32`, followed by records ordered by module index and numeric type ID. A type record is:

```text
module_index:u32
type_id_int_constant_index:u32
name_string_index:u32
kind:u8                 # 0 Struct, 1 Interface
flags:u8                # zero in version 1.0
reserved:u16            # zero
field_count:u32
fields[field_count]
method_count:u32
methods[method_count]
implemented_interface_count:u32
implemented_interface_type_indices[implemented_interface_count]:u32
```

Each field record is `{ field_id_int_constant_index:u32, name_string_index:u32 }`. Each method record is `{ method_id_int_constant_index:u32, name_string_index:u32, function_index:u32 }`. A struct method references its FunctionTable entry; an interface requirement uses `0xFFFFFFFF`. Interface implementation indexes refer to Interface records in this table. IDs are nonnegative Int constants, names are String constants, and member IDs are unique within a type. An Interface has no fields and no implemented-interface list. A Struct may have fields, methods, and implemented interfaces.

## Code and exception tables

`Code` is the raw concatenation of function byte ranges in FunctionTable order, with no section-level prefix or padding. `code_offset` and `code_length` measure bytes. The bytes are decoded using the VM instruction set; its opcode values, per-instruction operand encodings, and mapping from IR operations are separate from the container layout and are defined by the bytecode compiler/VM work. The container does not equate source opcode numbers with VM opcode numbers. This separation also leaves the VM instruction namespace independent of this file's section IDs.

The `ExceptionTable` payload is `handler_count:u32`, followed by fixed records:

```text
function_index:u32
protected_start:u64
protected_end:u64
handler_offset:u64
stack_depth:u32
exception_type_constant_index:u32  # 0xFFFFFFFF means catch-all
flags:u32                          # zero in version 1.0
```

Offsets are relative to the start of the referenced function's code range. Protected ranges are half-open. Start, end, and handler offsets must be instruction boundaries; `protected_start < protected_end`. Handler records for a function are ordered by start then end. The verifier rejects overlapping ranges unless one range is properly nested in the other. A catch-all uses the sentinel; otherwise the type index refers to a TypeRef constant.

## Debug information and checksum

`DebugInfo` is optional. Its payload is `source_file_count:u32`, followed by String constant indexes, then `location_count:u32` and records:

```text
function_index:u32
code_offset:u64                 # relative to the Code section
source_file_index:u32
start_byte_offset:u64
end_byte_offset:u64              # half-open source span
start_line:u32
start_byte_column:u32
end_line:u32
end_byte_column:u32
```

Records are ordered by function index and code offset. Source offsets are zero-based byte offsets; line and byte-column values follow `spec/source-format.md` (one-based lines and columns). Each code offset identifies an instruction start. A file without DebugInfo remains executable but has no source-location mapping.

`Checksum` is exactly 32 bytes containing SHA-256. Compute the digest over the complete file while treating the Checksum payload's 32 bytes as zero. This includes the header, directory, and all other section payloads. Readers verify it before loading constants or executing code and reject any mismatch.

## Validation and compatibility

A loader validates the checksum, version, header flags, directory ordering and bounds, required sections, section payload lengths, constant tags and references, table references, module/function ownership, function code ranges, type descriptors, exception ranges, and DebugInfo references before execution. The bytecode verifier validates instruction boundaries and branch targets before execution. Loaders and verifiers reject integer overflow, out-of-bounds references, invalid UTF-8, noncanonical LEB128, invalid reserved bits, malformed records, and code bytes not covered by exactly one function range.

Version 1.0 uses no implicit padding and no native pointers. A later incompatible container change increments the major version. A backward-compatible addition increments the minor version and uses an optional section or a newly defined flag bit; readers of version 1.0 skip unknown optional sections and reject unknown required ones. Bytecode opcode additions require an available assigned opcode and a reader that understands the instruction; an unknown opcode is always rejected.
