# Bytecode Instruction Set

Status: draft

This document defines the bytecode instruction subset emitted by `star-compiler` for the IR in `spec/ir.md`. One-byte VM opcodes and their operands are independent of source opcodes, IR names, intrinsic IDs, and `.starc` section IDs. All fixed-width operands are little-endian. Branch targets are absolute byte offsets from the start of the containing function code range and identify instruction starts.

## Emitted instructions

| Opcode | Instruction | Operands |
|---:|---|---|
| `0x01` | `PUSH_CONST` | `constant_pool_index:u32` |
| `0xE0` | `INTRINSIC` | `intrinsic_id:u16` |
| `0xE1` | `JUMP` | `target:u32, unwind_scopes:u32` |
| `0xE2` | `BRANCH_IF_FALSE` | `false_target:u32, true_target:u32` |
| `0xE3` | `FOR_INIT` | `loop_state:u32` |
| `0xE4` | `FOR_NEXT` | `loop_state:u32, body_target:u32, exit_target:u32` |
| `0xE5` | `SCOPE_ENTER` | none |
| `0xE6` | `SCOPE_EXIT` | none |
| `0xE7` | `RETURN_TOP` | `unwind_to_frame_base:u32` |
| `0xE8` | `TRAP` | none |
| `0xFF` | `HALT` | none |

`PUSH_CONST` pushes the value at its constant-pool index. `INTRINSIC` dispatches the IR operation identified below and consumes no encoded operands; it uses the source operation stack contract in `spec/instruction-semantics.md`. `JUMP` unwinds exactly the listed lexical scopes before transferring. `BRANCH_IF_FALSE` pops one condition and selects the false or true target. `FOR_INIT` and `FOR_NEXT` implement the hidden iterator state described in `spec/ir.md`. `RETURN_TOP` returns from the current function. `HALT` ends the module initializer. `TRAP` marks an unreachable block and must fail execution if reached.

Every operand width is fixed, so instruction sizes do not depend on branch distance. Function-relative targets and function-local loop-state indexes are `u32`; the compiler rejects values that do not fit. All target offsets identify the first byte of an instruction in the same function. The verifier in a later roadmap item validates target boundaries, opcode operands, stack effects, and resource limits.

## Stable intrinsic IDs

Intrinsic IDs are a separate `u16` namespace. IDs `0x8000` through `0x8063` identify the nonstructural `IrOp` variants in the exact table below. These values are not source opcode IDs or VM opcode bytes. The source ID column is included only to identify the corresponding source-language operation. Structural source commands lower to the dedicated instructions above or to CFG structure.

| Intrinsic ID | IR operation | Mnemonic | Source ID |
|---:|---|---|---|
| `0x8000` | `Add` | `ADD` | `arith.add` (basic ordinal 1) |
| `0x8001` | `Sub` | `SUB` | `arith.sub` (basic ordinal 2) |
| `0x8002` | `Mul` | `MUL` | `arith.mul` (basic ordinal 3) |
| `0x8003` | `Div` | `DIV` | `arith.div` (basic ordinal 4) |
| `0x8004` | `Mod` | `MOD` | `arith.mod` (basic ordinal 5) |
| `0x8005` | `Print` | `PRINT` | `io.print` (basic ordinal 6) |
| `0x8006` | `Input` | `INPUT` | `io.input` (basic ordinal 7) |
| `0x8007` | `Store` | `STORE` | `variable.store` (basic ordinal 8) |
| `0x8008` | `Load` | `LOAD` | `variable.load` (basic ordinal 9) |
| `0x8009` | `Eq` | `EQ` | `compare.eq` (basic ordinal 10) |
| `0x800A` | `Gt` | `GT` | `compare.gt` (basic ordinal 11) |
| `0x800B` | `Not` | `NOT` | `logic.not` (basic ordinal 12) |
| `0x800C` | `ToInt` | `TO_INT` | `type.to_int` (source opcode 0x10) |
| `0x800D` | `ToFloat` | `TO_FLOAT` | `type.to_float` (source opcode 0x11) |
| `0x800E` | `ToBool` | `TO_BOOL` | `type.to_bool` (source opcode 0x12) |
| `0x800F` | `ToString` | `TO_STRING` | `type.to_string` (source opcode 0x13) |
| `0x8010` | `ToBytes` | `TO_BYTES` | `type.to_bytes` (source opcode 0x14) |
| `0x8011` | `Utf8Decode` | `UTF8_DECODE` | `encoding.utf8_decode` (source opcode 0x15) |
| `0x8012` | `Utf8Encode` | `UTF8_ENCODE` | `encoding.utf8_encode` (source opcode 0x16) |
| `0x8013` | `FloatFromBits` | `FLOAT_FROM_BITS` | `type.float_from_bits` (source opcode 0x17) |
| `0x8014` | `PushNull` | `PUSH_NULL` | `literal.push_null` (source opcode 0x18) |
| `0x8015` | `PushTrue` | `PUSH_TRUE` | `literal.push_true` (source opcode 0x19) |
| `0x8016` | `PushFalse` | `PUSH_FALSE` | `literal.push_false` (source opcode 0x1A) |
| `0x8017` | `TypeOf` | `TYPE_OF` | `type.of` (source opcode 0x1B) |
| `0x8018` | `TypeCheck` | `TYPE_CHECK` | `type.check` (source opcode 0x1C) |
| `0x8019` | `ToChar` | `TO_CHAR` | `type.to_char` (source opcode 0x1D) |
| `0x801A` | `ToByte` | `TO_BYTE` | `type.to_byte` (source opcode 0x1E) |
| `0x801B` | `IntToBytes` | `INT_TO_BYTES` | `encoding.int_to_bytes` (source opcode 0x1F) |
| `0x801C` | `Pow` | `POW` | `arith.pow` (source opcode 0x20) |
| `0x801D` | `Neg` | `NEG` | `arith.neg` (source opcode 0x21) |
| `0x801E` | `Abs` | `ABS` | `arith.abs` (source opcode 0x22) |
| `0x801F` | `Lt` | `LT` | `compare.lt` (source opcode 0x23) |
| `0x8020` | `And` | `AND` | `logic.and` (source opcode 0x24) |
| `0x8021` | `Or` | `OR` | `logic.or` (source opcode 0x25) |
| `0x8022` | `Xor` | `XOR` | `logic.xor` (source opcode 0x26) |
| `0x8023` | `BitAnd` | `BIT_AND` | `bitwise.and` (source opcode 0x27) |
| `0x8024` | `BitOr` | `BIT_OR` | `bitwise.or` (source opcode 0x28) |
| `0x8025` | `BitXor` | `BIT_XOR` | `bitwise.xor` (source opcode 0x29) |
| `0x8026` | `BitNot` | `BIT_NOT` | `bitwise.not` (source opcode 0x2A) |
| `0x8027` | `Shl` | `SHL` | `bitwise.shift_left` (source opcode 0x2B) |
| `0x8028` | `Shr` | `SHR` | `bitwise.shift_right` (source opcode 0x2C) |
| `0x8029` | `Call` | `CALL` | `function.call` (source opcode 0x41) |
| `0x802A` | `Lambda` | `LAMBDA` | `function.lambda` (source opcode 0x43) |
| `0x802B` | `Capture` | `CAPTURE` | `function.capture` (source opcode 0x44) |
| `0x802C` | `TailCall` | `TAIL_CALL` | `function.tail_call` (source opcode 0x45) |
| `0x802D` | `CallValue` | `CALL_VALUE` | `function.call_value` (source opcode 0x46) |
| `0x802E` | `Declare` | `DECLARE` | `variable.declare` (source opcode 0x50) |
| `0x802F` | `Const` | `CONST` | `variable.const` (source opcode 0x51) |
| `0x8030` | `Delete` | `DELETE` | `variable.delete` (source opcode 0x52) |
| `0x8031` | `LoadGlobal` | `LOAD_GLOBAL` | `variable.load_global` (source opcode 0x53) |
| `0x8032` | `StoreGlobal` | `STORE_GLOBAL` | `variable.store_global` (source opcode 0x54) |
| `0x8033` | `TypeAnnotate` | `TYPE_ANNOTATE` | `variable.type_annotate` (source opcode 0x57) |
| `0x8034` | `ArrayNew` | `ARRAY_NEW` | `collection.array_new` (source opcode 0x60) |
| `0x8035` | `MapNew` | `MAP_NEW` | `collection.map_new` (source opcode 0x61) |
| `0x8036` | `SetNew` | `SET_NEW` | `collection.set_new` (source opcode 0x62) |
| `0x8037` | `GetItem` | `GET_ITEM` | `collection.get_item` (source opcode 0x63) |
| `0x8038` | `SetItem` | `SET_ITEM` | `collection.set_item` (source opcode 0x64) |
| `0x8039` | `Append` | `APPEND` | `collection.append` (source opcode 0x65) |
| `0x803A` | `Remove` | `REMOVE` | `collection.remove` (source opcode 0x66) |
| `0x803B` | `Length` | `LENGTH` | `collection.length` (source opcode 0x67) |
| `0x803C` | `Contains` | `CONTAINS` | `collection.contains` (source opcode 0x68) |
| `0x803D` | `Iter` | `ITER` | `collection.iter` (source opcode 0x69) |
| `0x803E` | `Next` | `NEXT` | `collection.next` (source opcode 0x6A) |
| `0x803F` | `TupleNew` | `TUPLE_NEW` | `collection.tuple_new` (source opcode 0x6B) |
| `0x8040` | `Slice` | `SLICE` | `collection.slice` (source opcode 0x6C) |
| `0x8041` | `Concat` | `CONCAT` | `collection.concat` (source opcode 0x6D) |
| `0x8042` | `StructDef` | `STRUCT_DEF` | `object.struct_def` (source opcode 0x70) |
| `0x8043` | `ObjectNew` | `OBJECT_NEW` | `object.new` (source opcode 0x71) |
| `0x8044` | `FieldGet` | `FIELD_GET` | `object.field_get` (source opcode 0x72) |
| `0x8045` | `FieldSet` | `FIELD_SET` | `object.field_set` (source opcode 0x73) |
| `0x8046` | `MethodDef` | `METHOD_DEF` | `object.method_def` (source opcode 0x74) |
| `0x8047` | `MethodCall` | `METHOD_CALL` | `object.method_call` (source opcode 0x75) |
| `0x8048` | `InterfaceDef` | `INTERFACE_DEF` | `object.interface_def` (source opcode 0x76) |
| `0x8049` | `Implement` | `IMPLEMENT` | `object.implement` (source opcode 0x77) |
| `0x804A` | `ObjectIs` | `OBJECT_IS` | `object.is` (source opcode 0x78) |
| `0x804B` | `ObjectClone` | `OBJECT_CLONE` | `object.clone` (source opcode 0x79) |
| `0x804C` | `Import` | `IMPORT` | `module.import` (source opcode 0x90) |
| `0x804D` | `Export` | `EXPORT` | `module.export` (source opcode 0x91) |
| `0x804E` | `ModuleGet` | `MODULE_GET` | `module.get` (source opcode 0x92) |
| `0x804F` | `ModuleCall` | `MODULE_CALL` | `module.call` (source opcode 0x93) |
| `0x8050` | `ModuleInit` | `MODULE_INIT` | `module.init` (source opcode 0x94) |
| `0x8051` | `ImportDynamic` | `IMPORT_DYNAMIC` | `module.import_dynamic` (source opcode 0x95) |
| `0x8052` | `Await` | `AWAIT` | `async.await` (source opcode 0xA1) |
| `0x8053` | `Spawn` | `SPAWN` | `async.spawn` (source opcode 0xA2) |
| `0x8054` | `Yield` | `YIELD` | `async.yield` (source opcode 0xA3) |
| `0x8055` | `Sleep` | `SLEEP` | `async.sleep` (source opcode 0xA4) |
| `0x8056` | `Join` | `JOIN` | `async.join` (source opcode 0xA5) |
| `0x8057` | `Cancel` | `CANCEL` | `async.cancel` (source opcode 0xA6) |
| `0x8058` | `ChannelNew` | `CHANNEL_NEW` | `async.channel_new` (source opcode 0xA7) |
| `0x8059` | `ChannelSend` | `CHANNEL_SEND` | `async.channel_send` (source opcode 0xA8) |
| `0x805A` | `ChannelRecv` | `CHANNEL_RECV` | `async.channel_recv` (source opcode 0xA9) |
| `0x805B` | `MutexNew` | `MUTEX_NEW` | `async.mutex_new` (source opcode 0xAA) |
| `0x805C` | `MutexLock` | `MUTEX_LOCK` | `async.mutex_lock` (source opcode 0xAB) |
| `0x805D` | `MutexUnlock` | `MUTEX_UNLOCK` | `async.mutex_unlock` (source opcode 0xAC) |
| `0x805E` | `NativeLoad` | `NATIVE_LOAD` | `ffi.native_load` (source opcode 0xB0) |
| `0x805F` | `NativeSymbol` | `NATIVE_SYMBOL` | `ffi.native_symbol` (source opcode 0xB1) |
| `0x8060` | `NativeCall` | `NATIVE_CALL` | `ffi.native_call` (source opcode 0xB2) |
| `0x8061` | `NativeClose` | `NATIVE_CLOSE` | `ffi.native_close` (source opcode 0xB3) |
| `0x8062` | `NativeCallback` | `NATIVE_CALLBACK` | `ffi.native_callback` (source opcode 0xB4) |
| `0x8063` | `NativeBuffer` | `NATIVE_BUFFER` | `ffi.native_buffer` (source opcode 0xB5) |

This emitted subset does not assign direct VM opcodes in `0x80-0x86`. For example, `AWAIT` is an `INTRINSIC` with its stable intrinsic ID; it is not VM opcode `0x80`. Exception-region instructions are not represented by the current IR. The older conceptual VM opcode table in QLM does not override this versioned emitted-instruction table.
