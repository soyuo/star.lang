# Instruction Semantics

Status: draft

This document specifies operands, stack effects, side effects, and additional failure cases for every assigned source instruction in `spec/opcodes.toml`. Source spellings and opcode values are in `spec/instructions.md`; this document does not define `.starc` bytecode.

## Notation and common rules

- Stack lists are written bottom-to-top. `A, B -> C` consumes `A` then `B` (so `B` is popped first) and pushes `C`. `-` means no values. `v1..vn, n -> Array` means values are pushed in order, followed by their count on top.
- Unless an entry says otherwise, a runtime instruction can fail with `StackUnderflow` when inputs are missing, `TypeError` when an input has the wrong type, or `OutOfMemoryError` when it cannot allocate. It may also be stopped by the configured execution limit. The failure column lists additional instruction-specific cases.
- A `CompileError` from a structural instruction means the source is rejected before execution. Structural instructions are consumed by the compiler and have no separate runtime bytecode effect. The compiler must verify stack depth and compatible types at control-flow joins.
- Variable, function, field, method, module, and type identifiers are nonnegative `Int` values unless otherwise stated. A source operation that declares a stack effect must leave the remaining stack intact.
- `Null` is used for an absent optional result. `TYPE_OF` returns a canonical type-name `String`; `TYPE_CHECK` and `TYPE_ANNOTATE` take that same name as a `String`.
- The source documents do not yet define every operand layout. Where a row below makes one explicit, it is the draft contract for the reference implementation and must remain consistent with the catalog and this notation.

## Basic instructions

| ID | Mnemonic | Stack (bottom -> top) | Effect and side effects | Additional failures |
|---|---|---|---|---|
| `arith.add` | `ADD` | `left, right -> result` | Adds numeric operands; mixed `Int` and `Float` produces `Float`. | `TypeError` for nonnumeric operands. |
| `arith.sub` | `SUB` | `left, right -> result` | Computes `left - right`; mixed numeric operands produce `Float`. | `TypeError` for nonnumeric operands. |
| `arith.mul` | `MUL` | `left, right -> result` | Multiplies numeric operands; mixed numeric operands produce `Float`. | `TypeError` for nonnumeric operands. |
| `arith.div` | `DIV` | `left, right -> result` | Integer division truncates toward zero; if either operand is `Float`, performs floating-point division. | `DivisionByZero`; `TypeError` for nonnumeric operands. |
| `arith.mod` | `MOD` | `left, right -> result` | Computes the integer remainder, with the quotient truncated toward zero. | `DivisionByZero`; `TypeError` unless both operands are `Int`. |
| `io.print` | `PRINT` | `value -> -` | Formats one value and writes it to standard output. | `IOError` if output fails. |
| `io.input` | `INPUT` | `- -> line` | Reads one line from standard input, removes its line terminator, and pushes a UTF-8 `String`; pushes `Null` at end-of-file. | `IOError` for an input failure or invalid UTF-8. |
| `variable.store` | `STORE` | `value, variable_id -> -` | Stores into the nearest accessible mutable binding; creates the binding in the current scope if none exists. | `ReferenceError` when the binding is immutable or the identifier is invalid. |
| `variable.load` | `LOAD` | `variable_id -> value` | Reads the nearest binding with that identifier. | `ReferenceError` if no binding exists. |
| `compare.eq` | `EQ` | `left, right -> Bool` | Pushes whether the values are equal under Star value equality. | None beyond common failures. |
| `compare.gt` | `GT` | `left, right -> Bool` | Compares numeric operands; mixed numeric operands are compared as `Float`. | `TypeError` for nonnumeric operands. |
| `logic.not` | `NOT` | `Bool -> Bool` | Pushes the logical negation. | `TypeError` unless the operand is `Bool`. |

## Types and values

| ID | Mnemonic | Stack (bottom -> top) | Effect and side effects | Additional failures |
|---|---|---|---|---|
| `type.to_int` | `TO_INT` | `value -> Int` | Converts an `Int`, finite `Float`, `Bool`, or decimal `String` to `Int`; float conversion truncates toward zero. | `TypeError` for unsupported or invalid input. |
| `type.to_float` | `TO_FLOAT` | `value -> Float` | Converts an `Int`, `Float`, `Bool`, or decimal `String` to `Float`. | `TypeError` for unsupported or invalid input. |
| `type.to_bool` | `TO_BOOL` | `value -> Bool` | Returns false for `Null`, false, numeric zero, and empty `String`, `Bytes`, `Array`, `Map`, or `Set`; true otherwise. | None beyond common failures. |
| `type.to_string` | `TO_STRING` | `value -> String` | Produces the canonical user-facing string form of the value. | None beyond common failures. |
| `type.to_bytes` | `TO_BYTES` | `String -> Bytes` | Encodes the string as UTF-8 bytes. | None beyond common failures. |
| `encoding.utf8_decode` | `UTF8_DECODE` | `Bytes -> String` | Decodes UTF-8 bytes to a string. | `TypeError` for malformed UTF-8. |
| `encoding.utf8_encode` | `UTF8_ENCODE` | `String -> Bytes` | Encodes a string as UTF-8 bytes. | None beyond common failures. |
| `type.float_from_bits` | `FLOAT_FROM_BITS` | `bits: Int -> Float` | Reinterprets an unsigned 64-bit integer as IEEE 754 `f64` bits. | `TypeError` if the integer is outside `0..2^64-1`. |
| `literal.push_null` | `PUSH_NULL` | `- -> Null` | Pushes `Null`. | None beyond common failures. |
| `literal.push_true` | `PUSH_TRUE` | `- -> Bool` | Pushes true. | None beyond common failures. |
| `literal.push_false` | `PUSH_FALSE` | `- -> Bool` | Pushes false. | None beyond common failures. |
| `type.of` | `TYPE_OF` | `value -> type_name: String` | Pushes the canonical name of the value's runtime type. | None beyond common failures. |
| `type.check` | `TYPE_CHECK` | `value, type_name: String -> Bool` | Pushes whether the value has the named runtime type; unknown type names are rejected. | `TypeError` for an unknown type name. |
| `type.to_char` | `TO_CHAR` | `Int -> Char` | Converts a Unicode scalar value number to `Char`. | `TypeError` for a negative value, surrogate, or value above `0x10FFFF`. |
| `type.to_byte` | `TO_BYTE` | `Int -> Byte` | Converts an integer in `0..255` to `Byte`. | `TypeError` outside that range. |
| `encoding.int_to_bytes` | `INT_TO_BYTES` | `value: Int, length: Int -> Bytes` | Produces exactly `length` big-endian bytes, preserving leading zero bytes; only nonnegative values are accepted. | `TypeError` for negative length/value or a value that does not fit. |

## Arithmetic, comparison, logical, and bitwise instructions

| ID | Mnemonic | Stack (bottom -> top) | Effect and side effects | Additional failures |
|---|---|---|---|---|
| `arith.pow` | `POW` | `base, exponent -> result` | Raises a numeric base to a numeric exponent; integer results remain `Int` when exact and representable. | `TypeError` for unsupported operands; `DivisionByZero` for zero to a negative exponent. |
| `arith.neg` | `NEG` | `value -> result` | Negates an `Int` or `Float`. | `TypeError` for other types. |
| `arith.abs` | `ABS` | `value -> result` | Returns the absolute value of an `Int` or `Float`. | `TypeError` for other types. |
| `compare.lt` | `LT` | `left, right -> Bool` | Compares numeric operands; mixed numeric operands are compared as `Float`. | `TypeError` for nonnumeric operands. |
| `logic.and` | `AND` | `left: Bool, right: Bool -> Bool` | Computes eager logical AND; both operands are evaluated before the instruction. | `TypeError` unless both operands are `Bool`. |
| `logic.or` | `OR` | `left: Bool, right: Bool -> Bool` | Computes eager logical OR; both operands are evaluated before the instruction. | `TypeError` unless both operands are `Bool`. |
| `logic.xor` | `XOR` | `left: Bool, right: Bool -> Bool` | Computes logical exclusive OR. | `TypeError` unless both operands are `Bool`. |
| `bitwise.and` | `BIT_AND` | `left: Int, right: Int -> Int` | Computes arbitrary-precision bitwise AND using infinite two's-complement sign extension. | `TypeError` unless both operands are `Int`. |
| `bitwise.or` | `BIT_OR` | `left: Int, right: Int -> Int` | Computes arbitrary-precision bitwise OR using infinite two's-complement sign extension. | `TypeError` unless both operands are `Int`. |
| `bitwise.xor` | `BIT_XOR` | `left: Int, right: Int -> Int` | Computes arbitrary-precision bitwise XOR using infinite two's-complement sign extension. | `TypeError` unless both operands are `Int`. |
| `bitwise.not` | `BIT_NOT` | `Int -> Int` | Computes arbitrary-precision two's-complement inversion (`-x - 1`). | `TypeError` unless the operand is `Int`. |
| `bitwise.shift_left` | `SHL` | `value: Int, count: Int -> Int` | Shifts left by `count` bits. | `TypeError` for non-integers or a negative count; resource limit if the result is too large. |
| `bitwise.shift_right` | `SHR` | `value: Int, count: Int -> Int` | Arithmetic right shift with sign extension. | `TypeError` for non-integers or a negative count. |

## Control flow

| ID | Mnemonic | Stack (bottom -> top) | Effect and side effects | Additional failures |
|---|---|---|---|---|
| `control.if` | `IF` | `condition: Bool -> -` | Starts a conditional block; the compiler emits a branch and consumes the condition once. | `TypeError` for a non-Boolean condition at runtime; `CompileError` for malformed nesting or a statically proven violation. |
| `control.else` | `ELSE` | `- -> -` | Starts the alternate branch of the matching `IF`. | `CompileError` without one unmatched `IF` or when duplicated. |
| `control.end` | `END` | `- -> -` | Closes the innermost `IF`, loop, or function block. | `CompileError` when no compatible block is open. |
| `control.loop` | `LOOP` | `- -> -` | Starts a loop; with `WHILE`, condition code runs before each iteration; without it, the loop repeats until control transfer. | `CompileError` for malformed nesting. |
| `control.while` | `WHILE` | `condition: Bool -> -` | Ends the loop condition sequence; false exits, true enters the body. | `CompileError` outside a matching `LOOP`; `TypeError` for a non-Boolean condition. |
| `control.break` | `BREAK` | `- -> -` | Transfers control to the end of the innermost loop. | `CompileError` outside a loop. |
| `control.continue` | `CONTINUE` | `- -> -` | Transfers control to the next condition check or iterator step of the innermost loop. | `CompileError` outside a loop. |
| `control.for` | `FOR` | `iterator, variable_id -> loop body` | Iterates the iterator and binds each yielded value to `variable_id` in the loop scope; the compiler emits iteration and exit control flow. | `CompileError` for malformed nesting; `TypeError` for a non-iterator; `ReferenceError` for an invalid binding. |

## Functions and calls

| ID | Mnemonic | Stack (bottom -> top) | Effect and side effects | Additional failures |
|---|---|---|---|---|
| `function.define` | `FUNC` | `function_id, parameter_ids..., parameter_count -> -` | Defines the following `END`-delimited body; parameters are bound to call-frame slots. The header is consumed at compile time. | `CompileError` for duplicate IDs, mismatched count, or malformed body. |
| `function.call` | `CALL` | `arg1..argN, argc, function_id -> result` | Calls the named function; pushes its return value, using `Null` when it returns no value. The function may mutate state or perform I/O. | `ReferenceError` for an unknown function; `TypeError` for invalid arity; `StackOverflow` at the call-depth limit. |
| `function.return` | `RETURN` | `value? -> caller result` | Ends the current function and returns its top value, or `Null` if its frame has no return value. | `CompileError` when used outside a function. |
| `function.lambda` | `LAMBDA` | `body_id, parameter_count -> Closure` | Creates a closure for a compiler-registered body using the shared cells recorded by its `CAPTURE` entries. | `ReferenceError` for an unknown body; `TypeError` for an invalid count. |
| `function.capture` | `CAPTURE` | `variable_id -> -` | Adds the current lexical binding's shared cell to the capture list associated with the in-progress `LAMBDA`; the closure retains the cell, not a copy of its current value. | `CompileError` outside closure construction; `ReferenceError` if the binding does not exist. |
| `function.tail_call` | `TAIL_CALL` | `arg1..argN, argc, function_id -> caller result` | Calls the named function and returns its result directly from the current frame, without adding a call frame. | `ReferenceError` for an unknown function; `TypeError` for invalid arity; `StackOverflow` if the target call cannot reuse the frame. |
| `function.call_value` | `CALL_VALUE` | `arg1..argN, argc, callable -> result` | Calls a `Function`, `Closure`, or `NativeFunction` value; pushes the result. | `TypeError` for a non-callable or invalid arity; `StackOverflow` at the call-depth limit. |

## Variables and scope

| ID | Mnemonic | Stack (bottom -> top) | Effect and side effects | Additional failures |
|---|---|---|---|---|
| `variable.declare` | `DECLARE` | `value, variable_id -> -` | Creates a mutable binding in the current scope. | `ReferenceError` for an invalid identifier or duplicate declaration in the same scope. |
| `variable.const` | `CONST` | `value, variable_id -> -` | Creates an immutable binding in the current scope. | `ReferenceError` for an invalid identifier or duplicate declaration in the same scope. |
| `variable.delete` | `DELETE` | `variable_id -> -` | Removes the binding with that identifier from the current scope. | `ReferenceError` if it is absent or immutable. |
| `variable.load_global` | `LOAD_GLOBAL` | `variable_id -> value` | Reads a binding from the current module's global scope. | `ReferenceError` if absent. |
| `variable.store_global` | `STORE_GLOBAL` | `value, variable_id -> -` | Updates a mutable current-module global or creates it if absent. | `ReferenceError` if immutable or invalid. |
| `scope.begin` | `SCOPE_BEGIN` | `- -> -` | Starts a lexical block scope; consumed by the compiler. | `CompileError` for malformed nesting. |
| `scope.end` | `SCOPE_END` | `- -> -` | Ends the innermost lexical block scope and releases its bindings; consumed by the compiler. | `CompileError` without a matching scope. |
| `variable.type_annotate` | `TYPE_ANNOTATE` | `type_name: String, variable_id -> -` | Adds a runtime type restriction to the binding; later stores must satisfy it. | `ReferenceError` if the variable is absent; `TypeError` for an unknown type or incompatible existing value. |

## Collections and strings

| ID | Mnemonic | Stack (bottom -> top) | Effect and side effects | Additional failures |
|---|---|---|---|---|
| `collection.array_new` | `ARRAY_NEW` | `item1..itemN, count -> Array` | Creates a mutable array preserving item order. | `TypeError` for a negative or excessive count. |
| `collection.map_new` | `MAP_NEW` | `key1, value1..keyN, valueN, pair_count -> Map` | Creates a hash map; later duplicate keys replace earlier values. | `TypeError` for an invalid key or count. |
| `collection.set_new` | `SET_NEW` | `item1..itemN, count -> Set` | Creates a hash set, discarding duplicate values. | `TypeError` for an unhashable value or count. |
| `collection.get_item` | `GET_ITEM` | `collection, key -> value` | Reads an array/tuple/string/bytes index or a map key; string indexing returns one Unicode scalar as `Char`. | `IndexError` for an invalid index; `ReferenceError` for a missing map key. |
| `collection.set_item` | `SET_ITEM` | `collection, key, value -> -` | Mutates an array or map in place; array indices must already be in range. | `IndexError` for an invalid array index; `TypeError` for an immutable or unsupported collection. |
| `collection.append` | `APPEND` | `array, value -> -` | Appends one value to a mutable array. | `TypeError` for a non-array or immutable value. |
| `collection.remove` | `REMOVE` | `collection, key -> removed_value` | Removes an array element by index, map entry by key, or set member by value; returns `Null` when a map/set member is absent. | `IndexError` for an invalid array index; `TypeError` for an unsupported collection. |
| `collection.length` | `LENGTH` | `collection -> Int` | Returns element count; strings count Unicode scalar values, while `Bytes` counts bytes. | `TypeError` for a value without a length. |
| `collection.contains` | `CONTAINS` | `collection, value -> Bool` | Checks array/tuple/string/bytes membership, map key presence, or set membership. | `TypeError` for an unsupported collection or invalid key. |
| `collection.iter` | `ITER` | `collection -> Iterator` | Creates an iterator over a collection in its defined iteration order. | `TypeError` for a non-iterable value. |
| `collection.next` | `NEXT` | `Iterator -> value, has_next: Bool` | Advances once; when exhausted pushes `Null, false`. | `TypeError` for an invalid iterator. |
| `collection.tuple_new` | `TUPLE_NEW` | `item1..itemN, count -> Tuple` | Creates an immutable tuple preserving item order. | `TypeError` for a negative or excessive count. |
| `collection.slice` | `SLICE` | `collection, start, end -> slice` | Returns a copy of the half-open range `[start, end)` for `Array`, `Tuple`, `String`, or `Bytes`. | `IndexError` for invalid bounds; `TypeError` for an unsupported collection. |
| `collection.concat` | `CONCAT` | `left, right -> combined` | Returns a new concatenation of two values of the same supported type: `Array`, `Tuple`, `String`, or `Bytes`. | `TypeError` for mismatched or unsupported types. |

## Objects and interfaces

| ID | Mnemonic | Stack (bottom -> top) | Effect and side effects | Additional failures |
|---|---|---|---|---|
| `object.struct_def` | `STRUCT_DEF` | `struct_id, field_ids..., field_count -> -` | Registers a struct descriptor and its ordered fields at compile time. | `CompileError` for duplicate IDs, duplicate fields, or a mismatched count. |
| `object.new` | `OBJECT_NEW` | `arg1..argN, argc, struct_id -> Object` | Allocates an instance, initializes declared fields, then calls its constructor if present. | `ReferenceError` for an unknown struct; `TypeError` for invalid constructor arity. |
| `object.field_get` | `FIELD_GET` | `object, field_id -> value` | Reads an instance field. | `ReferenceError` for an unknown field. |
| `object.field_set` | `FIELD_SET` | `object, field_id, value -> -` | Updates a mutable instance field. | `ReferenceError` for an unknown field; `TypeError` for an incompatible or immutable field. |
| `object.method_def` | `METHOD_DEF` | `struct_id, method_id, function_id -> -` | Attaches the named function to the struct as a method at compile time. | `CompileError` for unknown or duplicate IDs. |
| `object.method_call` | `METHOD_CALL` | `arg1..argN, argc, object, method_id -> result` | Calls the method with the object as its receiver and pushes its result. | `ReferenceError` for an unknown method; `TypeError` for invalid receiver or arity; `StackOverflow` at the call-depth limit. |
| `object.interface_def` | `INTERFACE_DEF` | `interface_id, method_ids..., method_count -> -` | Registers the required method identifiers at compile time. | `CompileError` for duplicate IDs or a mismatched count. |
| `object.implement` | `IMPLEMENT` | `struct_id, interface_id -> -` | Declares that a struct implements an interface and validates required methods. | `CompileError` for unknown IDs or missing methods. |
| `object.is` | `OBJECT_IS` | `object, type_id -> Bool` | Checks whether the object is an instance of the struct or interface identified by `type_id`. | `ReferenceError` for an unknown type ID. |
| `object.clone` | `OBJECT_CLONE` | `object -> Object` | Creates a shallow copy with the same struct and field values. | `TypeError` for a non-object. |

## Modules

| ID | Mnemonic | Stack (bottom -> top) | Effect and side effects | Additional failures |
|---|---|---|---|---|
| `module.import` | `IMPORT` | `path: Bytes -> Module` | Decodes the path as UTF-8, resolves and loads the module, initializes it once, and pushes its module handle. | `TypeError` for invalid UTF-8; `ReferenceError` if unresolved or for a circular read before initialization. |
| `module.export` | `EXPORT` | `symbol_id -> -` | Exposes the current module's named binding to importers. | `ReferenceError` if the binding is absent. |
| `module.get` | `MODULE_GET` | `module, symbol_id -> value` | Reads an exported symbol from a module handle. | `ReferenceError` if the module or exported symbol is absent. |
| `module.call` | `MODULE_CALL` | `arg1..argN, argc, module, symbol_id -> result` | Calls an exported function and pushes its result. | `ReferenceError` for a missing module/symbol; `TypeError` for invalid arity; `StackOverflow` at the call-depth limit. |
| `module.init` | `MODULE_INIT` | `module -> -` | Runs module initialization once per execution context; repeated calls have no effect. | `ReferenceError` for an invalid module or initialization cycle. |
| `module.import_dynamic` | `IMPORT_DYNAMIC` | `path: String -> Module` | Resolves, loads, and initializes a module at runtime, then pushes its handle. | `ReferenceError` if unresolved or for a circular read before initialization. |

## Asynchronous and concurrent operations

| ID | Mnemonic | Stack (bottom -> top) | Effect and side effects | Additional failures |
|---|---|---|---|---|
| `async.function` | `ASYNC_FUNC` | `function_id, parameter_ids..., parameter_count -> -` | Defines an asynchronous function body; calling it creates a `Task` instead of executing it inline. Header is consumed at compile time. | `CompileError` for duplicate IDs, mismatched count, or malformed body. |
| `async.await` | `AWAIT` | `Task -> result` | Suspends the current task until completion and pushes the task result. | `TypeError` for a non-task; propagates task failure or cancellation. |
| `async.spawn` | `SPAWN` | `Function -> Task` | Schedules the function as a new task and pushes its handle. | `TypeError` for a non-function; `PermissionError` if task creation is disabled. |
| `async.yield` | `YIELD` | `- -> -` | Returns the execution turn to the scheduler. | `TypeError` when used outside an async task. |
| `async.sleep` | `SLEEP` | `milliseconds: Int -> Null` | Suspends the current task for the specified nonnegative duration. | `TypeError` for a negative duration or use outside an async task. |
| `async.join` | `JOIN` | `Task -> result` | Waits for task completion and pushes its result. | `TypeError` for a non-task; propagates task failure or cancellation. |
| `async.cancel` | `CANCEL` | `Task -> Bool` | Requests task cancellation; returns whether the request was accepted. | `TypeError` for a non-task. |
| `async.channel_new` | `CHANNEL_NEW` | `capacity: Int -> Channel` | Creates a channel; capacity zero creates a rendezvous channel. | `TypeError` for a negative capacity. |
| `async.channel_send` | `CHANNEL_SEND` | `channel, value -> Null` | Sends one value, suspending while a bounded channel is full. | `TypeError` for an invalid channel or use outside an async task. |
| `async.channel_recv` | `CHANNEL_RECV` | `channel -> value` | Receives one value, suspending while the channel is empty. | `TypeError` for an invalid channel or use outside an async task. |
| `async.mutex_new` | `MUTEX_NEW` | `- -> Mutex` | Creates a mutex. | None beyond common failures. |
| `async.mutex_lock` | `MUTEX_LOCK` | `Mutex -> Null` | Acquires the mutex, suspending while another task owns it. | `TypeError` for an invalid mutex or use outside an async task. |
| `async.mutex_unlock` | `MUTEX_UNLOCK` | `Mutex -> Null` | Releases a mutex owned by the current task. | `TypeError` for an invalid mutex; `PermissionError` if the current task is not its owner. |

## Native interface

FFI instructions require the FFI permission to be enabled. Native pointers are represented by managed `Handle` values, never exposed as ordinary Star values. A native signature descriptor contains argument types, return type, calling convention, and ownership rules.

| ID | Mnemonic | Stack (bottom -> top) | Effect and side effects | Additional failures |
|---|---|---|---|---|
| `ffi.native_load` | `NATIVE_LOAD` | `path: String -> library: Handle` | Loads a supported native library (`.dll`, `.so`, or `.dylib`) and returns a managed handle. | `PermissionError` when FFI is disabled; `IOError` if loading fails. |
| `ffi.native_symbol` | `NATIVE_SYMBOL` | `library: Handle, name: String, signature -> function: Handle` | Resolves a symbol and binds its declared C ABI signature and ownership rules. | `PermissionError` when FFI is disabled; `ReferenceError` if the symbol is absent; `TypeError` for an invalid signature. |
| `ffi.native_call` | `NATIVE_CALL` | `arg1..argN, argc, function: Handle -> result` | Invokes the bound native function using its signature and C ABI; converts arguments/result and applies ownership rules. | `PermissionError` when FFI is disabled; `TypeError` for argument mismatch; `IOError` if the call cannot be completed. |
| `ffi.native_close` | `NATIVE_CLOSE` | `library: Handle -> -` | Closes a loaded library and invalidates its symbol handles. | `PermissionError` when FFI is disabled; `TypeError` for an invalid handle. |
| `ffi.native_callback` | `NATIVE_CALLBACK` | `function: Function, signature -> callback: Handle` | Creates a native-callable callback and roots its Star function for the callback lifetime. | `PermissionError` when FFI is disabled; `TypeError` for an incompatible signature. |
| `ffi.native_buffer` | `NATIVE_BUFFER` | `length: Int -> buffer: Handle` | Allocates a zero-initialized managed native buffer of the requested byte length. | `PermissionError` when FFI is disabled; `TypeError` for a negative length. |

## Unassigned exception instructions

The exception-handling candidates `0x80-0x86` are not assigned source opcodes in `spec/opcodes.toml`. Their operand and stack contracts are intentionally not specified here until the source-to-bytecode mapping is resolved. VM exception behavior remains described by the runtime specifications.
