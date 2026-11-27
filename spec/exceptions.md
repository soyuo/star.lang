# Exception Propagation and Runtime Cleanup

Status: draft

## Runtime errors

An unhandled execution error is returned as a `RuntimeError` with a primary `RuntimeErrorKind`, a source `Span`, and a `stack_trace`. The primary span identifies the failing instruction or construct. Each stack-trace frame records the called function's stable ID and the source span of the call that entered it.

Frames are stored outermost first. Displayed diagnostics print the innermost frame first, followed by callers, after the primary error and location. Errors retain the trace captured at the failure site while propagating; propagation does not duplicate frames or replace the primary span.

Errors that occur before entering a callee, such as a bad argument count or unknown function ID, use the call instruction as the primary span and include only frames that were already active. Static verification errors have no runtime call frames.

## Propagation and cleanup

The source interpreter and bytecode VM propagate an unhandled runtime error to their public execution boundary. Before returning it, they discard transient execution state created by that run: operand values above the caller's entry stack, active function frames, lexical scopes, pending capture vectors, and call-depth bookkeeping. The error owns its captured stack trace, so cleaning the live frame state does not erase the diagnostic.

Cleanup is not a transaction. Global bindings, output already written, input already consumed, and other completed side effects remain visible after an error. The source interpreter preserves stack values that existed before the failing `execute` call. The bytecode VM starts each `execute` with fresh module state and leaves its partially initialized module globals inspectable after failure.

Normal function return unwinds that frame's scopes, captures, and operand values before transferring the result to its caller. A runtime failure follows the same cleanup path for every active frame before control returns to the host.

## Structured handlers and fatal failures

`TRY`, `CATCH`, and `FINALLY` are source-level structural instructions with source opcode aliases `0x87`, `0x88`, and `0x89`. The historical VM candidates `0x80-0x86` remain unassigned and are not reused. A `TRY` requires at least one direct `CATCH` or `FINALLY`; it may contain one of each, with `CATCH` before `FINALLY`. `CATCH` handles any non-fatal runtime error and does not bind an error value. Catch handlers and finalizers execute in the surrounding lexical scope.

The source interpreter and bytecode VM both search active exception regions from the innermost protected range outward and then across caller frames, using the failing instruction or call-site offset. Entering a handler restores the region-entry operand stack, scopes, pending captures, and frame bookkeeping. `FINALLY` runs on normal completion and on `RETURN`, `BREAK`, `CONTINUE`, and non-fatal error paths. If a finalizer transfers control or fails, that transfer or failure replaces the pending transfer or error. Fatal execution-limit, compile-integrity, unsupported-runtime, allocator, and VM-integrity failures bypass handlers.

In `.starc` version 1.0, ExceptionTable catch-all records identify function-local half-open protected and handler ranges, plus the stack depth retained from the region-entry snapshot. The runtime expires pending errors when execution leaves their handler range. The verifier checks range boundaries and nesting, requires each `RETHROW` to be inside a handler range, and confirms each reachable protected entry has the declared stack depth. The bytecode handler ends in `RETHROW` when the handled error must continue outward. Typed exception matches remain unsupported in this runtime slice. The compiler IR represents catch-all ranges as function-local `ExceptionRegionIR` records; lowering of `FINALLY` uses explicit control-flow paths so every normal and abrupt exit executes it exactly as specified in `spec/ir.md`.

Allocator aborts and VM-integrity failures are outside the recoverable `RuntimeError` path. No user-level recovery guarantee is made for those failures.
