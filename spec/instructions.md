# Instruction Encodings

Status: draft

This document defines the source spelling and source-level opcode representation for every assigned instruction in `spec/opcodes.toml`. The catalog is authoritative for IDs, names, ordinals, and opcode values; this document makes their encoding explicit.

## Encoding rules

- A basic instruction has ordinal `N` and is spelled as `N` consecutive `*` characters followed by one `'`. Its identity is the catalog ordinal; it has no 8-bit extended opcode payload.
- An extended instruction is spelled as a leading `'`, exactly eight payload bits, and a trailing `"`. Payload bits are most-significant-bit first; `*` means `1` and `'` means `0`. The 8-bit payload is the hexadecimal source opcode shown below.
- Every syntax character is ASCII and therefore one UTF-8 source byte: `*` = `2A`, `'` = `27`, and `"` = `22`. The byte column below is the exact UTF-8 byte sequence of the token spelling, not a `.starc` instruction encoding.
- In normal mode, formatting whitespace is ignored before tokenization, including inside literals. Strict mode rejects it. Character validation and source coordinates are defined in `spec/source-format.md`.
- The source opcode namespace is independent of `.starc` VM bytecode. A compiler may translate a source instruction into a different opcode or multiple bytecode instructions; see the bytecode specification.

## Basic instructions

| Ordinal | ID | Mnemonic | Source spelling | Bit pattern | UTF-8 source bytes (hex) |
|---:|---|---|---|---|---|
| `1` | `arith.add` | `ADD` | `*'` | `10` | `2A 27` |
| `2` | `arith.sub` | `SUB` | `**'` | `110` | `2A 2A 27` |
| `3` | `arith.mul` | `MUL` | `***'` | `1110` | `2A 2A 2A 27` |
| `4` | `arith.div` | `DIV` | `****'` | `11110` | `2A 2A 2A 2A 27` |
| `5` | `arith.mod` | `MOD` | `*****'` | `111110` | `2A 2A 2A 2A 2A 27` |
| `6` | `io.print` | `PRINT` | `******'` | `1111110` | `2A 2A 2A 2A 2A 2A 27` |
| `7` | `io.input` | `INPUT` | `*******'` | `11111110` | `2A 2A 2A 2A 2A 2A 2A 27` |
| `8` | `variable.store` | `STORE` | `********'` | `111111110` | `2A 2A 2A 2A 2A 2A 2A 2A 27` |
| `9` | `variable.load` | `LOAD` | `*********'` | `1111111110` | `2A 2A 2A 2A 2A 2A 2A 2A 2A 27` |
| `10` | `compare.eq` | `EQ` | `**********'` | `11111111110` | `2A 2A 2A 2A 2A 2A 2A 2A 2A 2A 27` |
| `11` | `compare.gt` | `GT` | `***********'` | `111111111110` | `2A 2A 2A 2A 2A 2A 2A 2A 2A 2A 2A 27` |
| `12` | `logic.not` | `NOT` | `************'` | `1111111111110` | `2A 2A 2A 2A 2A 2A 2A 2A 2A 2A 2A 2A 27` |

The bit pattern column for a basic instruction records its source-symbol sequence using `*` = `1` and `'` = `0`; it is a variable-length token representation terminated by the final zero symbol, not a byte-aligned opcode field.

## Extended instructions

| Opcode | Payload bits (MSB first) | ID | Mnemonic | Source spelling | UTF-8 source bytes (hex) |
|---:|---|---|---|---|---|
| `0x10` | `00010000` | `type.to_int` | `TO_INT` | `''''*''''"` | `27 27 27 27 2A 27 27 27 27 22` |
| `0x11` | `00010001` | `type.to_float` | `TO_FLOAT` | `''''*'''*"` | `27 27 27 27 2A 27 27 27 2A 22` |
| `0x12` | `00010010` | `type.to_bool` | `TO_BOOL` | `''''*''*'"` | `27 27 27 27 2A 27 27 2A 27 22` |
| `0x13` | `00010011` | `type.to_string` | `TO_STRING` | `''''*''**"` | `27 27 27 27 2A 27 27 2A 2A 22` |
| `0x14` | `00010100` | `type.to_bytes` | `TO_BYTES` | `''''*'*''"` | `27 27 27 27 2A 27 2A 27 27 22` |
| `0x15` | `00010101` | `encoding.utf8_decode` | `UTF8_DECODE` | `''''*'*'*"` | `27 27 27 27 2A 27 2A 27 2A 22` |
| `0x16` | `00010110` | `encoding.utf8_encode` | `UTF8_ENCODE` | `''''*'**'"` | `27 27 27 27 2A 27 2A 2A 27 22` |
| `0x17` | `00010111` | `type.float_from_bits` | `FLOAT_FROM_BITS` | `''''*'***"` | `27 27 27 27 2A 27 2A 2A 2A 22` |
| `0x18` | `00011000` | `literal.push_null` | `PUSH_NULL` | `''''**'''"` | `27 27 27 27 2A 2A 27 27 27 22` |
| `0x19` | `00011001` | `literal.push_true` | `PUSH_TRUE` | `''''**''*"` | `27 27 27 27 2A 2A 27 27 2A 22` |
| `0x1A` | `00011010` | `literal.push_false` | `PUSH_FALSE` | `''''**'*'"` | `27 27 27 27 2A 2A 27 2A 27 22` |
| `0x1B` | `00011011` | `type.of` | `TYPE_OF` | `''''**'**"` | `27 27 27 27 2A 2A 27 2A 2A 22` |
| `0x1C` | `00011100` | `type.check` | `TYPE_CHECK` | `''''***''"` | `27 27 27 27 2A 2A 2A 27 27 22` |
| `0x1D` | `00011101` | `type.to_char` | `TO_CHAR` | `''''***'*"` | `27 27 27 27 2A 2A 2A 27 2A 22` |
| `0x1E` | `00011110` | `type.to_byte` | `TO_BYTE` | `''''****'"` | `27 27 27 27 2A 2A 2A 2A 27 22` |
| `0x1F` | `00011111` | `encoding.int_to_bytes` | `INT_TO_BYTES` | `''''*****"` | `27 27 27 27 2A 2A 2A 2A 2A 22` |
| `0x20` | `00100000` | `arith.pow` | `POW` | `'''*'''''"` | `27 27 27 2A 27 27 27 27 27 22` |
| `0x21` | `00100001` | `arith.neg` | `NEG` | `'''*''''*"` | `27 27 27 2A 27 27 27 27 2A 22` |
| `0x22` | `00100010` | `arith.abs` | `ABS` | `'''*'''*'"` | `27 27 27 2A 27 27 27 2A 27 22` |
| `0x23` | `00100011` | `compare.lt` | `LT` | `'''*'''**"` | `27 27 27 2A 27 27 27 2A 2A 22` |
| `0x24` | `00100100` | `logic.and` | `AND` | `'''*''*''"` | `27 27 27 2A 27 27 2A 27 27 22` |
| `0x25` | `00100101` | `logic.or` | `OR` | `'''*''*'*"` | `27 27 27 2A 27 27 2A 27 2A 22` |
| `0x26` | `00100110` | `logic.xor` | `XOR` | `'''*''**'"` | `27 27 27 2A 27 27 2A 2A 27 22` |
| `0x27` | `00100111` | `bitwise.and` | `BIT_AND` | `'''*''***"` | `27 27 27 2A 27 27 2A 2A 2A 22` |
| `0x28` | `00101000` | `bitwise.or` | `BIT_OR` | `'''*'*'''"` | `27 27 27 2A 27 2A 27 27 27 22` |
| `0x29` | `00101001` | `bitwise.xor` | `BIT_XOR` | `'''*'*''*"` | `27 27 27 2A 27 2A 27 27 2A 22` |
| `0x2A` | `00101010` | `bitwise.not` | `BIT_NOT` | `'''*'*'*'"` | `27 27 27 2A 27 2A 27 2A 27 22` |
| `0x2B` | `00101011` | `bitwise.shift_left` | `SHL` | `'''*'*'**"` | `27 27 27 2A 27 2A 27 2A 2A 22` |
| `0x2C` | `00101100` | `bitwise.shift_right` | `SHR` | `'''*'**''"` | `27 27 27 2A 27 2A 2A 27 27 22` |
| `0x30` | `00110000` | `control.if` | `IF` | `'''**''''"` | `27 27 27 2A 2A 27 27 27 27 22` |
| `0x31` | `00110001` | `control.else` | `ELSE` | `'''**'''*"` | `27 27 27 2A 2A 27 27 27 2A 22` |
| `0x32` | `00110010` | `control.end` | `END` | `'''**''*'"` | `27 27 27 2A 2A 27 27 2A 27 22` |
| `0x33` | `00110011` | `control.loop` | `LOOP` | `'''**''**"` | `27 27 27 2A 2A 27 27 2A 2A 22` |
| `0x34` | `00110100` | `control.while` | `WHILE` | `'''**'*''"` | `27 27 27 2A 2A 27 2A 27 27 22` |
| `0x35` | `00110101` | `control.break` | `BREAK` | `'''**'*'*"` | `27 27 27 2A 2A 27 2A 27 2A 22` |
| `0x36` | `00110110` | `control.continue` | `CONTINUE` | `'''**'**'"` | `27 27 27 2A 2A 27 2A 2A 27 22` |
| `0x37` | `00110111` | `control.for` | `FOR` | `'''**'***"` | `27 27 27 2A 2A 27 2A 2A 2A 22` |
| `0x40` | `01000000` | `function.define` | `FUNC` | `''*''''''"` | `27 27 2A 27 27 27 27 27 27 22` |
| `0x41` | `01000001` | `function.call` | `CALL` | `''*'''''*"` | `27 27 2A 27 27 27 27 27 2A 22` |
| `0x42` | `01000010` | `function.return` | `RETURN` | `''*''''*'"` | `27 27 2A 27 27 27 27 2A 27 22` |
| `0x43` | `01000011` | `function.lambda` | `LAMBDA` | `''*''''**"` | `27 27 2A 27 27 27 27 2A 2A 22` |
| `0x44` | `01000100` | `function.capture` | `CAPTURE` | `''*'''*''"` | `27 27 2A 27 27 27 2A 27 27 22` |
| `0x45` | `01000101` | `function.tail_call` | `TAIL_CALL` | `''*'''*'*"` | `27 27 2A 27 27 27 2A 27 2A 22` |
| `0x46` | `01000110` | `function.call_value` | `CALL_VALUE` | `''*'''**'"` | `27 27 2A 27 27 27 2A 2A 27 22` |
| `0x50` | `01010000` | `variable.declare` | `DECLARE` | `''*'*''''"` | `27 27 2A 27 2A 27 27 27 27 22` |
| `0x51` | `01010001` | `variable.const` | `CONST` | `''*'*'''*"` | `27 27 2A 27 2A 27 27 27 2A 22` |
| `0x52` | `01010010` | `variable.delete` | `DELETE` | `''*'*''*'"` | `27 27 2A 27 2A 27 27 2A 27 22` |
| `0x53` | `01010011` | `variable.load_global` | `LOAD_GLOBAL` | `''*'*''**"` | `27 27 2A 27 2A 27 27 2A 2A 22` |
| `0x54` | `01010100` | `variable.store_global` | `STORE_GLOBAL` | `''*'*'*''"` | `27 27 2A 27 2A 27 2A 27 27 22` |
| `0x55` | `01010101` | `scope.begin` | `SCOPE_BEGIN` | `''*'*'*'*"` | `27 27 2A 27 2A 27 2A 27 2A 22` |
| `0x56` | `01010110` | `scope.end` | `SCOPE_END` | `''*'*'**'"` | `27 27 2A 27 2A 27 2A 2A 27 22` |
| `0x57` | `01010111` | `variable.type_annotate` | `TYPE_ANNOTATE` | `''*'*'***"` | `27 27 2A 27 2A 27 2A 2A 2A 22` |
| `0x60` | `01100000` | `collection.array_new` | `ARRAY_NEW` | `''**'''''"` | `27 27 2A 2A 27 27 27 27 27 22` |
| `0x61` | `01100001` | `collection.map_new` | `MAP_NEW` | `''**''''*"` | `27 27 2A 2A 27 27 27 27 2A 22` |
| `0x62` | `01100010` | `collection.set_new` | `SET_NEW` | `''**'''*'"` | `27 27 2A 2A 27 27 27 2A 27 22` |
| `0x63` | `01100011` | `collection.get_item` | `GET_ITEM` | `''**'''**"` | `27 27 2A 2A 27 27 27 2A 2A 22` |
| `0x64` | `01100100` | `collection.set_item` | `SET_ITEM` | `''**''*''"` | `27 27 2A 2A 27 27 2A 27 27 22` |
| `0x65` | `01100101` | `collection.append` | `APPEND` | `''**''*'*"` | `27 27 2A 2A 27 27 2A 27 2A 22` |
| `0x66` | `01100110` | `collection.remove` | `REMOVE` | `''**''**'"` | `27 27 2A 2A 27 27 2A 2A 27 22` |
| `0x67` | `01100111` | `collection.length` | `LENGTH` | `''**''***"` | `27 27 2A 2A 27 27 2A 2A 2A 22` |
| `0x68` | `01101000` | `collection.contains` | `CONTAINS` | `''**'*'''"` | `27 27 2A 2A 27 2A 27 27 27 22` |
| `0x69` | `01101001` | `collection.iter` | `ITER` | `''**'*''*"` | `27 27 2A 2A 27 2A 27 27 2A 22` |
| `0x6A` | `01101010` | `collection.next` | `NEXT` | `''**'*'*'"` | `27 27 2A 2A 27 2A 27 2A 27 22` |
| `0x6B` | `01101011` | `collection.tuple_new` | `TUPLE_NEW` | `''**'*'**"` | `27 27 2A 2A 27 2A 27 2A 2A 22` |
| `0x6C` | `01101100` | `collection.slice` | `SLICE` | `''**'**''"` | `27 27 2A 2A 27 2A 2A 27 27 22` |
| `0x6D` | `01101101` | `collection.concat` | `CONCAT` | `''**'**'*"` | `27 27 2A 2A 27 2A 2A 27 2A 22` |
| `0x70` | `01110000` | `object.struct_def` | `STRUCT_DEF` | `''***''''"` | `27 27 2A 2A 2A 27 27 27 27 22` |
| `0x71` | `01110001` | `object.new` | `OBJECT_NEW` | `''***'''*"` | `27 27 2A 2A 2A 27 27 27 2A 22` |
| `0x72` | `01110010` | `object.field_get` | `FIELD_GET` | `''***''*'"` | `27 27 2A 2A 2A 27 27 2A 27 22` |
| `0x73` | `01110011` | `object.field_set` | `FIELD_SET` | `''***''**"` | `27 27 2A 2A 2A 27 27 2A 2A 22` |
| `0x74` | `01110100` | `object.method_def` | `METHOD_DEF` | `''***'*''"` | `27 27 2A 2A 2A 27 2A 27 27 22` |
| `0x75` | `01110101` | `object.method_call` | `METHOD_CALL` | `''***'*'*"` | `27 27 2A 2A 2A 27 2A 27 2A 22` |
| `0x76` | `01110110` | `object.interface_def` | `INTERFACE_DEF` | `''***'**'"` | `27 27 2A 2A 2A 27 2A 2A 27 22` |
| `0x77` | `01110111` | `object.implement` | `IMPLEMENT` | `''***'***"` | `27 27 2A 2A 2A 27 2A 2A 2A 22` |
| `0x78` | `01111000` | `object.is` | `OBJECT_IS` | `''****'''"` | `27 27 2A 2A 2A 2A 27 27 27 22` |
| `0x79` | `01111001` | `object.clone` | `OBJECT_CLONE` | `''****''*"` | `27 27 2A 2A 2A 2A 27 27 2A 22` |
| `0x87` | `10000111` | `exception.try` | `TRY` | `'*''''***"` | `27 2A 27 27 27 27 2A 2A 2A 22` |
| `0x88` | `10001000` | `exception.catch` | `CATCH` | `'*'''*'''"` | `27 2A 27 27 27 2A 27 27 27 22` |
| `0x89` | `10001001` | `exception.finally` | `FINALLY` | `'*'''*''*"` | `27 2A 27 27 27 2A 27 27 2A 22` |
| `0x90` | `10010000` | `module.import` | `IMPORT` | `'*''*''''"` | `27 2A 27 27 2A 27 27 27 27 22` |
| `0x91` | `10010001` | `module.export` | `EXPORT` | `'*''*'''*"` | `27 2A 27 27 2A 27 27 27 2A 22` |
| `0x92` | `10010010` | `module.get` | `MODULE_GET` | `'*''*''*'"` | `27 2A 27 27 2A 27 27 2A 27 22` |
| `0x93` | `10010011` | `module.call` | `MODULE_CALL` | `'*''*''**"` | `27 2A 27 27 2A 27 27 2A 2A 22` |
| `0x94` | `10010100` | `module.init` | `MODULE_INIT` | `'*''*'*''"` | `27 2A 27 27 2A 27 2A 27 27 22` |
| `0x95` | `10010101` | `module.import_dynamic` | `IMPORT_DYNAMIC` | `'*''*'*'*"` | `27 2A 27 27 2A 27 2A 27 2A 22` |
| `0xA0` | `10100000` | `async.function` | `ASYNC_FUNC` | `'*'*'''''"` | `27 2A 27 2A 27 27 27 27 27 22` |
| `0xA1` | `10100001` | `async.await` | `AWAIT` | `'*'*''''*"` | `27 2A 27 2A 27 27 27 27 2A 22` |
| `0xA2` | `10100010` | `async.spawn` | `SPAWN` | `'*'*'''*'"` | `27 2A 27 2A 27 27 27 2A 27 22` |
| `0xA3` | `10100011` | `async.yield` | `YIELD` | `'*'*'''**"` | `27 2A 27 2A 27 27 27 2A 2A 22` |
| `0xA4` | `10100100` | `async.sleep` | `SLEEP` | `'*'*''*''"` | `27 2A 27 2A 27 27 2A 27 27 22` |
| `0xA5` | `10100101` | `async.join` | `JOIN` | `'*'*''*'*"` | `27 2A 27 2A 27 27 2A 27 2A 22` |
| `0xA6` | `10100110` | `async.cancel` | `CANCEL` | `'*'*''**'"` | `27 2A 27 2A 27 27 2A 2A 27 22` |
| `0xA7` | `10100111` | `async.channel_new` | `CHANNEL_NEW` | `'*'*''***"` | `27 2A 27 2A 27 27 2A 2A 2A 22` |
| `0xA8` | `10101000` | `async.channel_send` | `CHANNEL_SEND` | `'*'*'*'''"` | `27 2A 27 2A 27 2A 27 27 27 22` |
| `0xA9` | `10101001` | `async.channel_recv` | `CHANNEL_RECV` | `'*'*'*''*"` | `27 2A 27 2A 27 2A 27 27 2A 22` |
| `0xAA` | `10101010` | `async.mutex_new` | `MUTEX_NEW` | `'*'*'*'*'"` | `27 2A 27 2A 27 2A 27 2A 27 22` |
| `0xAB` | `10101011` | `async.mutex_lock` | `MUTEX_LOCK` | `'*'*'*'**"` | `27 2A 27 2A 27 2A 27 2A 2A 22` |
| `0xAC` | `10101100` | `async.mutex_unlock` | `MUTEX_UNLOCK` | `'*'*'**''"` | `27 2A 27 2A 27 2A 2A 27 27 22` |
| `0xB0` | `10110000` | `ffi.native_load` | `NATIVE_LOAD` | `'*'**''''"` | `27 2A 27 2A 2A 27 27 27 27 22` |
| `0xB1` | `10110001` | `ffi.native_symbol` | `NATIVE_SYMBOL` | `'*'**'''*"` | `27 2A 27 2A 2A 27 27 27 2A 22` |
| `0xB2` | `10110010` | `ffi.native_call` | `NATIVE_CALL` | `'*'**''*'"` | `27 2A 27 2A 2A 27 27 2A 27 22` |
| `0xB3` | `10110011` | `ffi.native_close` | `NATIVE_CLOSE` | `'*'**''**"` | `27 2A 27 2A 2A 27 27 2A 2A 22` |
| `0xB4` | `10110100` | `ffi.native_callback` | `NATIVE_CALLBACK` | `'*'**'*''"` | `27 2A 27 2A 2A 27 2A 27 27 22` |
| `0xB5` | `10110101` | `ffi.native_buffer` | `NATIVE_BUFFER` | `'*'**'*'*"` | `27 2A 27 2A 2A 27 2A 27 2A 22` |
## Unassigned and pending values

Only entries listed in `spec/opcodes.toml` are assigned source instructions. Gaps within an allocation family and ranges marked reserved are invalid as source opcodes until assigned in that catalog. Historical VM candidates `0x80-0x86` remain unassigned in the source namespace. Source `TRY`, `CATCH`, and `FINALLY` use the separate source opcodes `0x87-0x89`. The `0xFF` page prefix is reserved and is not currently an instruction.

## Source and bytecode separation

The spellings and values above describe source tokens only. They do not define `.starc` serialization, bytecode opcode values, operands, or instruction stack effects. Those bytecode details belong to the bytecode format and VM specifications.
