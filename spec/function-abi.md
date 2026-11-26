# Function ABI

Status: draft

This document defines the logical calling convention for named functions, closures, native functions, and asynchronous functions. It specifies source-level stack inputs and runtime frame behavior, but not `.starc` function-table serialization or physical VM opcodes. Source instruction stack effects remain listed in `spec/instruction-semantics.md`.

## Function identity and definition

Function IDs are nonnegative `Int` values unique within their defining module. Parameter IDs are Variable IDs local to one function, as defined in `spec/identifier-module-semantics.md`; parameter IDs and function IDs are separate namespaces.

`FUNC` consumes a header in this order, bottom to top:

```text
function_id, parameter_id_1, ..., parameter_id_N, parameter_count
```

`parameter_count` must equal the number of listed parameter IDs. The function ID must be unique in the module, and parameter IDs must be distinct within the function signature. Violations are `CompileError`. `FUNC` associates the following matching `END`-delimited body with this function ID; the body is a separate code unit and is not executed while the definition is encountered. The compiler registers module function descriptors before resolving calls so forward references, recursion, and mutual recursion can use the same module-local function table.

A function descriptor contains its module identity, function ID, ordered parameter IDs, body, and optional rest-parameter designation. If variadic, the final listed parameter ID is the rest slot and the descriptor marks it explicitly; it is not inferred from any numeric ID. `parameter_count` still counts all listed parameter IDs. The `FUNC` header itself declares fixed arity; the optional rest designation is descriptor metadata supplied by a declaration form that supports variadic functions. Native variadic functions carry the same designation in their host descriptor. This is a logical ABI record, not a `.starc` serialization rule.

## Arguments and calls

`CALL` receives the following stack, bottom to top:

```text
arg1, arg2, ..., argN, argc, function_id
```

`argc` is a nonnegative `Int` equal to the number of argument values immediately below it. Arguments bind from left to right to the descriptor's ordered parameter IDs. A fixed-arity function requires `argc` to equal its parameter count. A variadic function requires at least the number of fixed parameters; remaining arguments are collected, in order, into an `Array` bound to the rest parameter ID. Invalid counts raise `TypeError`; an unknown function ID in the current module raises `ReferenceError`.

`CALL_VALUE` uses the same argument order, with a `Function`, `Closure`, or `NativeFunction` value in place of `function_id`. A `Function` uses its defining module and function ID. A `Closure` uses its body and captured-cell vector. A `NativeFunction` receives the ordered values through the host-call interface and returns either one Star value or a runtime failure. All callable kinds apply their own fixed or variadic arity descriptor. A non-callable value raises `TypeError`.

`TAIL_CALL` uses the `CALL` operand order and function lookup rules. It is valid only when the current function returns that call result directly. The VM replaces the current frame while preserving its caller's return address and call-depth accounting; it must not add a new frame for the tail-called function. A tail call cannot target a closure or native function because its source operand is a function ID.

## Call frames and bindings

Each call creates a fresh frame containing:

- the defining module and function or closure body ID;
- the current instruction location and the caller's return location;
- a function environment with parameter and local bindings;
- the operand-stack base for this invocation;
- the closure's captured shared-cell references, if any;
- active exception-handler state and the caller-frame link.

The caller's argument setup is consumed before the callee begins. The callee starts with an empty operand stack above its frame base. Parameters are initialized in argument order as mutable bindings. Local bindings use the normal lexical lookup and STORE rules. For a closure, lookup priority is the current function environment, captured cells, the defining module's globals, then the execution-context globals. A named function has no captured cells and uses its defining module's globals. Each recursive invocation has a distinct frame and local environment; it does not overwrite another invocation's parameters or locals.

## Return behavior

`RETURN` ends the current function. Its result is the top operand above the frame's stack base, or `Null` if there is no operand above that base. The frame's other temporary operands are discarded. Falling through the matching `END` has the same result as returning `Null`.

Before resuming the caller, the VM unwinds all lexical scopes owned by the returning frame, releases uncaptured local bindings, restores the caller's instruction location and stack base, and pushes exactly one result value. Captured cells remain alive while a closure references them. A `RETURN` outside a function is a `CompileError`, as specified in `spec/control-flow.md`.

## Recursion and resource limits

Direct and mutual recursion use ordinary calls through the module function table. Every non-tail call consumes a frame and is subject to the configured recursion-depth and call-stack limits; reaching the limit raises `StackOverflow` without corrupting caller frames. Tail calls reuse the current frame as described above. Execution limits still apply to recursive and tail-recursive functions.

## Closure capture

`CAPTURE` consumes one Variable ID and resolves it using the current lexical environment chain. It appends a reference to that binding's shared cell to the pending capture vector; it does not copy the current value. Captures are ordered by their `CAPTURE` instructions. Repeating an ID in one capture vector is a `CompileError`; an unavailable binding is a `ReferenceError`.

`LAMBDA` consumes a compiler-registered body ID and parameter count, then creates a `Closure` from that body and the pending capture vector. The body descriptor's parameter count must match; otherwise construction fails with `TypeError`. The pending vector is consumed and cleared, so each construction produces a closure with its own vector of cell references. With no preceding `CAPTURE`, the closure has an empty capture vector. A nested closure may capture a cell already captured by its enclosing closure.

The compiler identifies each closure-construction sequence and verifies that every `CAPTURE` contributes to exactly one `LAMBDA`, that capture IDs are unique within that sequence, and that all control-flow paths agree on the pending capture vector. No pending captures may remain at a code-unit exit or tail call. A violation is `CompileError`.

Captured cells are shared: stores through the defining scope or any closure observe the same cell. The cell remains alive after its defining scope or frame exits for as long as a closure references it. Garbage collection traces closure-to-cell references. Captured bindings retain their Variable IDs, so ordinary shadowing and nearest-binding lookup rules continue to apply.

## Asynchronous functions

`ASYNC_FUNC` uses the same function descriptor and argument binding as `FUNC`. Calling an asynchronous function creates and returns a `Task` instead of running its body inline; its task owns the initial call state and begins execution under the scheduler. A body return completes the task with that value, and falling through completes it with `Null`. Awaiting and task failure propagation follow the async and exception instruction semantics in `spec/instruction-semantics.md`.
