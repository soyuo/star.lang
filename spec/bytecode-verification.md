# Bytecode Verification

Status: stable

`star-compiler::verify_bytecode` validates untrusted `.starc` version 1.0 data before a VM consumes it. Container layout and instruction encoding remain defined by `spec/bytecode-format.md` and `spec/bytecode-instructions.md`.

## Host policy

The caller supplies finite limits for file bytes, aggregate table entries, decoded instructions, and operand-stack depth. Limits are checked before allocating from serialized counts and while decoding code. Limits are host policy and are not embedded in the bytecode. The VM host also supplies runtime ceilings through `ExecutionLimits`; their accounting and cooperative timeout behavior are defined in `spec/resource-limits.md`.

The caller also supplies granted capabilities. Verification rejects a resource-sensitive intrinsic when its required capability is absent:

| Intrinsic | Required capabilities |
|---|---|
| `IMPORT`, `IMPORT_DYNAMIC` | Path-dependent: `fs.read` when a module file is read; `env.read` only when resolution consults `STAR_PATH`. The resolver checks these at the point of access. |
| `SLEEP` | `clock` |
| `NATIVE_LOAD`, `NATIVE_SYMBOL`, `NATIVE_CALL`, `NATIVE_CLOSE`, `NATIVE_CALLBACK`, `NATIVE_BUFFER` | `ffi` |

The VM checks capabilities again when an import executes. Passing verification never grants a capability. The CLI resolves project and package source modules before compilation; reading a module under `STAR_PATH` requires both `env.read` and `fs.read`. At runtime, `IMPORT` and `IMPORT_DYNAMIC` can resolve only modules already present in the linked image and recheck those grants for `starpath/` modules. The VM does not load new files during execution. A name absent from the image raises `ReferenceError`.

The host capability vocabulary is `fs.read`, `fs.write`, `net.connect`, `net.listen`, `process.spawn`, `env.read`, `ffi`, `clock`, and `random`. Runtime and host APIs check the applicable grant at each operation. A grant does not create an API that the current runtime does not expose. The source interpreter has no ambient filesystem, network, process, environment, native-library, clock, or random-source access; trusted hosts may inject capability-gated services, and standard input and output are explicit host-provided streams.

## Container and table validation

The verifier accepts major version 1, minor version 0, with exactly the defined required sections and optional `DebugInfo`. It verifies the zeroed-payload SHA-256 checksum before parsing tables, rejects reserved bits, unknown sections, noncanonical LEB128, invalid UTF-8, malformed constant payloads, invalid references, inconsistent module/function/type ownership, invalid exception ranges, and invalid debug locations. Every offset, length, count, and index is checked with overflow-safe arithmetic and against the caller's limits.

## Instruction and control-flow validation

Only the direct opcodes and intrinsic IDs in `spec/bytecode-instructions.md` are accepted. Each operand is decoded at its specified width. Constant indexes, direct function IDs, loop-state IDs, and branch targets must resolve; every branch target must identify an instruction start in the same function. Instruction streams must be fully decoded and cannot fall through past a function boundary. Function initializers may halt; ordinary functions may return or tail-call. Other terminal instructions are traps.

The verifier propagates abstract operand-stack state from each function entry. It rejects statically provable underflow, incompatible stack heights at joins, inconsistent scope depths, invalid scope unwinds, and `max_stack` values below the statically known requirement. It also computes each function's required depth and applies the host stack ceiling to that requirement; a file's larger declared upper bound alone does not consume the host's runtime stack budget. An empty `RETURN_TOP` stack is valid and produces the ABI-defined implicit `Null` result. At joins, known integer constants are retained only when all incoming values agree. Dynamic-arity instructions can make a lower stack prefix opaque; subsequent underflow checks on that prefix remain runtime checks, while joins must agree on the known suffix and opaque-prefix state. The VM must enforce the verified function stack limit at runtime.

The verifier also tracks the pending closure-capture vector as hidden control-flow state. `CAPTURE` makes it pending, `LAMBDA` consumes it, and all incoming paths at a join must agree whether it is pending. `RETURN_TOP`, `TAIL_CALL`, and `HALT` cannot exit with unconsumed captures.

`FOR_INIT` must define each function-local loop-state ID once, and every reachable `FOR_NEXT` must refer to an initialized loop state. A `RETURN_TOP` must unwind exactly the current explicit scope depth. All reachable paths must end at an explicit control-flow terminator or remain in a cycle; fallthrough is rejected.

## Version boundary

Version 1.0 verification rejects unknown minor versions, required unknown sections, direct VM opcodes, and intrinsic IDs. Catch-all exception regions are verified and executed by the version 1.0 runtime; typed exception matching is unsupported. The verifier does not execute bytecode, prove dynamic Star value types, or replace semantic analysis.
