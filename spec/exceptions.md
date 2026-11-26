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

`TRY`, `CATCH`, and `FINALLY` describe the intended structured-handler semantics in the QLM design, but they are not assigned source opcodes, represented by the current IR, or emitted in `.starc` version 1.0. This implementation therefore does not catch errors in Star code or execute `FINALLY` blocks. The `.starc` ExceptionTable is validated structurally but is not used to dispatch handlers. Any handler implementation must first define source opcodes, IR regions, bytecode records, and verifier/runtime behavior without reusing unresolved historical opcode assignments.

Allocator aborts and VM-integrity failures are outside the recoverable `RuntimeError` path. No user-level recovery guarantee is made for those failures.
