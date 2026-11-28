# Native Foreign Function Interface

Status: stable

## Capability and signatures

Native loading, symbol lookup, calls, callbacks, buffers, and library closing
require the `ffi` capability. The capability is denied by default because
native code executes with the host process's authority and can violate memory
safety.

Signatures use `cdecl(type, ...)->result`. Supported scalar types are `i8`,
`u8`, `i16`, `u16`, `i32`, `u32`, `i64`, `u64`, `isize`, `usize`, `f32`,
`f64`, and `bool`; `void` is valid as a result. Native calls also support the
managed `cstring` and `buffer` pointer arguments described by their handle
contracts. Native pointer results are rejected.

## Guest callbacks

`NATIVE_CALLBACK` consumes a Star `Function` or `Closure` and a callback
signature, then returns a managed callback handle. If the callable resolves to
an async function, callback invocation fails with `TypeError`. Its signature may
take scalar integer, float, and boolean arguments and return `void` or one of
those scalar types. Strings, buffers, nested callbacks, and async functions are
not valid callback parameters or results.

The callback trampoline is active only during a `NATIVE_CALL` that passes its
handle. Native code may invoke it synchronously on the same operating-system
thread before returning from that call. It must not retain the callback pointer
afterward or invoke it concurrently. A callback invoked from another thread is
rejected; the native call reports `TypeError` after the native function
returns. Guest callback errors are retained and propagated after the C call
returns. Panics are contained at the callback boundary; neither errors nor
panics unwind through native frames.

The callback handle keeps its Star callable alive. Dropping the handle releases
the trampoline; native code must not keep using its address after the handle's
lifetime. Native libraries and callbacks are trusted host extensions, not a
sandbox boundary.

## Implementation boundary

Source and bytecode execution both dispatch `NATIVE_CALLBACK` and support
synchronous callback invocation from `NATIVE_CALL`. Callback execution shares
the active interpreter state, including captures, host capabilities, and
resource limits. Async scheduling and worker execution are separate operations
and are not entered implicitly by a callback.
