# Intermediate Representation and Lowering

Status: draft

This document defines the target-independent IR and the lowering from parsed, semantically valid Star programs. It does not define `.starc` serialization, bytecode opcode values, or VM instruction selection; those belong to the bytecode and compiler roadmap items. Source opcode assignments remain in `spec/opcodes.toml`, while stack effects and failures remain in `spec/instruction-semantics.md`.

## IR model

The initial IR is a stack-based control-flow graph. It keeps Star's operand-stack behavior while replacing structured blocks with explicit basic blocks and branches. IR operation names are symbolic and independent of the numeric source-opcode and `.starc` bytecode namespaces.

```text
ModuleIR {
    initializer: FunctionIR,
    functions: [FunctionIR],
}

FunctionIR {
    id: Int?,
    parameter_ids: [Int],
    rest_parameter_code: Int,
    asynchronous: Bool,
    entry: BlockId,
    blocks: [BasicBlock],
}

BasicBlock {
    id: BlockId,
    entry_stack: [TypeHint],
    entry_scope_depth: Int,
    operations: [IrInstruction],
    terminator: Terminator,
}

IrInstruction { op: IrOp, immediate: Immediate?, span: Span }
```

`BlockId` and compiler-generated loop-state IDs are function-local, nonserialized identifiers. `TypeHint` may be a known source type or `Any`; it guides verification and lowering but never removes a runtime check required for a dynamic value. Every block has exactly one terminator, and no operation follows it. The entry stack and scope depth are the states established by all incoming edges.

The evaluation stack is not converted to SSA in this IR. A stack slot has the same position on every incoming edge; the semantic analyzer's height and type-compatibility rules apply at joins and loop back-edges. This avoids implicit value merging and leaves the later bytecode verifier an explicit stack contract. Side-effecting operations remain in source evaluation order.

## IR operations

`IrOp` contains one symbolic operation for each nonstructural assigned source mnemonic, using its PascalCase spelling. For example, source `arith.add` / `ADD` lowers to `IrOp::Add`, `type.to_int` / `TO_INT` to `IrOp::ToInt`, and `collection.get_item` / `GET_ITEM` to `IrOp::GetItem`. These names are not numeric opcodes. Their operand-stack inputs, outputs, type rules, side effects, and failures are exactly those in `spec/instruction-semantics.md`.

The complete assigned operation inventory is grouped below. Every mnemonic in these groups has the corresponding `IrOp` variant unless the structural-lowering section explicitly assigns it to a terminator, scope operation, or function descriptor.

The structural/control mnemonics are `IF`, `ELSE`, `END`, `LOOP`, `WHILE`, `FOR`, `BREAK`, `CONTINUE`, `FUNC`, `ASYNC_FUNC`, `RETURN`, `SCOPE_BEGIN`, and `SCOPE_END`. They lower to CFG, scope, loop-state, or function-descriptor constructs rather than `IrOp` variants.

| Source family | Assigned mnemonics |
|---|---|
| Basic arithmetic | `ADD`, `SUB`, `MUL`, `DIV`, `MOD` |
| Basic I/O, variables, comparison, logic | `PRINT`, `INPUT`, `STORE`, `LOAD`, `EQ`, `GT`, `NOT` |
| Conversion and literal | `TO_INT`, `TO_FLOAT`, `TO_BOOL`, `TO_STRING`, `TO_BYTES`, `UTF8_DECODE`, `UTF8_ENCODE`, `FLOAT_FROM_BITS`, `PUSH_NULL`, `PUSH_TRUE`, `PUSH_FALSE`, `TYPE_OF`, `TYPE_CHECK`, `TO_CHAR`, `TO_BYTE`, `INT_TO_BYTES` |
| Arithmetic, comparison, logic, bitwise | `POW`, `NEG`, `ABS`, `LT`, `AND`, `OR`, `XOR`, `BIT_AND`, `BIT_OR`, `BIT_XOR`, `BIT_NOT`, `SHL`, `SHR` |
| Direct and first-class functions | `CALL`, `LAMBDA`, `CAPTURE`, `TAIL_CALL`, `CALL_VALUE` |
| Variables and annotations | `DECLARE`, `CONST`, `DELETE`, `LOAD_GLOBAL`, `STORE_GLOBAL`, `TYPE_ANNOTATE` |
| Collections | `ARRAY_NEW`, `MAP_NEW`, `SET_NEW`, `GET_ITEM`, `SET_ITEM`, `APPEND`, `REMOVE`, `LENGTH`, `CONTAINS`, `ITER`, `NEXT`, `TUPLE_NEW`, `SLICE`, `CONCAT` |
| Objects and interfaces | `STRUCT_DEF`, `OBJECT_NEW`, `FIELD_GET`, `FIELD_SET`, `METHOD_DEF`, `METHOD_CALL`, `INTERFACE_DEF`, `IMPLEMENT`, `OBJECT_IS`, `OBJECT_CLONE` |
| Modules | `IMPORT`, `EXPORT`, `MODULE_GET`, `MODULE_CALL`, `MODULE_INIT`, `IMPORT_DYNAMIC` |
| Asynchronous operations | `AWAIT`, `SPAWN`, `YIELD`, `SLEEP`, `JOIN`, `CANCEL`, `CHANNEL_NEW`, `CHANNEL_SEND`, `CHANNEL_RECV`, `MUTEX_NEW`, `MUTEX_LOCK`, `MUTEX_UNLOCK` |
| Native interface | `NATIVE_LOAD`, `NATIVE_SYMBOL`, `NATIVE_CALL`, `NATIVE_CLOSE`, `NATIVE_CALLBACK`, `NATIVE_BUFFER` |

`DataLiteral` lowers to `IrOp::PushInt` with its unsigned, MSB-first bit payload preserved exactly; no fixed-width truncation is permitted. Constants introduced for compiler-generated control flow use the same arbitrary-precision `Int` semantics. A lowering may use a more specific internal operation when it proves equivalence, but the required stack and observable behavior remain unchanged.

## Terminators and generated operations

The terminator set is:

| Terminator | Behavior |
|---|---|
| `Jump(target, unwind_scopes)` | Transfers to one block and exits the indicated lexical scopes first. |
| `BranchIfFalse(false_target, true_target)` | Pops one condition; branches on `Bool`. A dynamic non-`Bool` value keeps the specified runtime `TypeError`. |
| `ForNext(loop_state, body_target, exit_target)` | Requests one iterator item; on success assigns it to the loop binding and enters the body, otherwise exits the loop. Iterator failures propagate. |
| `ReturnTop(unwind_to_frame_base)` | Returns the top operand above the function frame base, or `Null` if there is none; discards other frame operands and unwinds function scopes. |
| `Halt` | Ends the module initializer normally. |
| `Unreachable` | Marks a block with no valid incoming execution path after semantic analysis. |

Compiler-generated operations are `ScopeEnter`, `ScopeExit`, and `ForInit(loop_state)`. `ForInit` consumes the source `Iterator` and variable ID, creates the loop's lexical binding, and records the iterator in hidden IR state. This state is not a Star value and cannot be observed by source instructions. `ScopeExit` ends lookup visibility while preserving any cells retained by closures.

Each instruction and terminator carries the source span that caused it. A generated branch uses the span of its controlling block or `BREAK`/`CONTINUE`; generated scope operations use the span of the corresponding scope or loop. The bytecode emitter must preserve these spans in the later source-location table.

## Source-to-IR lowering

Lowering runs after parsing and successful semantic analysis. It uses the function, type, scope, and control-flow information from semantic analysis; it does not repeat name resolution or reinterpret source opcode encodings.

1. Create one module initializer and one `FunctionIR` for each resolved function descriptor. Register all function descriptors before lowering calls so forward, recursive, and mutual calls resolve. A function header's statically known ID, ordered parameter IDs, rest code, and kind populate its descriptor; header metadata is not emitted as initializer instructions, and the function body is lowered only into its own function.
2. Lower a data literal to `PushInt`. Lower each ordinary instruction to the `IrOp` named above. Apply the source instruction's documented stack effect, preserving operand order and failure point. For a binary operation, the lowerer takes operands in their documented left-to-right semantic order even though the right operand is on top of the source stack. Preserve dynamic checks whenever static analysis cannot prove the value type or identifier.
3. Keep operations in their source order within each block. Do not fold, reorder, or remove a potentially failing or side-effecting operation in the initial lowering. Each IR operation retains the source instruction span.
4. Lower `SCOPE_BEGIN` / `SCOPE_END` blocks to `ScopeEnter` / `ScopeExit`. Ordinary `LOAD`, `STORE`, `DECLARE`, `CONST`, `DELETE`, `LOAD_GLOBAL`, and `STORE_GLOBAL` lower to their corresponding IR operations without merging local and module-global lookup rules.
5. Lower a function's `CALL` to `IrOp::Call` with its existing stack argument order and function-ID operand. The operand selects a function in the current module; resolve it statically when known and preserve the runtime lookup/check when it is dynamic. Lower `CALL_VALUE`, `LAMBDA`, and `CAPTURE` to their corresponding IR operations; closure cells and call-frame rules remain those in `spec/function-abi.md`. An `IrOp::Call` to an `ASYNC_FUNC` produces a `Task` per the ABI; scheduler execution is a later runtime concern.

### Structured control flow

- **IF / ELSE / END:** the condition-producing source instructions stay in the current block. `IF` ends it with `BranchIfFalse`. Lower the then and optional else arms into separate blocks and join their reachable exits. A missing `ELSE` uses the false target as a direct edge to the join. The condition is consumed once. The join stack must satisfy the semantic analyzer's height and compatible-type rules.
- **LOOP with WHILE:** create a condition header, body, and exit. The pre-loop edge enters the header. Lower the condition sequence into that header on every iteration, then use `BranchIfFalse` to choose exit or body. The body back-edge targets the condition header; the condition is never cached.
- **Condition-free `LOOP`:** create a body header and exit. The entry and each normal body completion jump to the header. Normal source exits are `BREAK` and `RETURN`; runtime failures and execution limits may also terminate it.
- **`FOR`:** lower the source iterator and variable ID already on the evaluation stack with `ForInit`. The source stack order is iterator then variable ID, bottom to top. The loop header ends with `ForNext`; its success edge enters the body and its exhaustion edge enters the exit. Normal completion and `CONTINUE` target the header; `BREAK` targets the exit. The exit block emits `ScopeExit` for the loop binding.
- **BREAK / CONTINUE:** emit `Jump` to the nearest enclosing loop's exit or continuation target. `unwind_scopes` is the number of lexical scopes crossed, stopping at the current function boundary.
- **RETURN:** terminate the current function with `ReturnTop`; no return edge may cross a function boundary. A source return inside a nested block or loop still exits the function.

Control-flow targets are function-local `BlockId`s, not source opcode values or bytecode addresses. Branch targets are allocated once and every incoming edge is recorded before the function is finalized. The lowerer rejects an unresolved target or a block without exactly one terminator as an internal compiler error.

## Invariants and later passes

- IR preserves source evaluation order, stack effects, lexical/global binding behavior, call ABI, runtime failures, and source spans.
- Every reachable join has a compatible operand-stack state; every loop back-edge restores its header stack state.
- Function IDs and parameter IDs remain arbitrary-precision nonnegative integers in IR metadata.
- Reserved or pending source opcode values have no IR operation. `.starc` opcodes are assigned independently by the bytecode format.
- Future optimization passes may add constants, simplify branches, or remove dead blocks only when they preserve output, I/O, bindings, errors and their order, function behavior, and source attribution.
- IR is an in-memory compiler representation in this item. Versioning, serialization, constant-pool layout, bytecode instruction selection, and VM verification are specified by later roadmap items.
