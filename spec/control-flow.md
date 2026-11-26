# Blocks and Control Flow

Status: draft

This document defines structural block matching, source control-flow behavior, and logical branch targets. `spec/grammar.ebnf` defines the nested syntax, `spec/opcodes.toml` assigns source opcodes, and `spec/instruction-semantics.md` defines their stack effects. `.starc` jump operand serialization is defined by the bytecode-format roadmap item.

## Structural block boundaries

`IF`, `LOOP`, `FOR`, `FUNC`, and `ASYNC_FUNC` open blocks closed by the nearest matching `END`. An `END` cannot close a different block kind. `SCOPE_BEGIN` is closed only by `SCOPE_END`; `END` never closes a lexical scope. A source file must finish with no unclosed structural block.

- `ELSE` separates the two arms of the nearest open `IF` that does not already have an `ELSE`. It cannot cross an intervening block boundary or appear twice for one `IF`.
- `WHILE` separates the condition sequence from the body of the nearest open `LOOP`. It may appear at most once in that loop.
- `FOR` obtains its iterator and variable ID from the stack before the `FOR` token, as specified in the instruction matrix. Its block ends at its matching `END`.
- Function definitions may contain nested blocks. A function boundary resets the loop-control context: `BREAK` and `CONTINUE` in a nested function cannot target a loop in its defining function.

The parser rejects an unmatched `ELSE`, `WHILE`, `END`, or `SCOPE_END`, duplicate separators, a mismatched closer, or end of file inside an open block with `CompileError`.

## Conditionals

The condition-producing instructions execute before `IF` and leave one `Bool` on top of the operand stack. `IF` consumes that value exactly once. It executes the first arm when the value is `true`; otherwise it executes the optional `ELSE` arm, or continues after `END` when no `ELSE` exists. Conditions are Boolean; `IF` does not apply implicit truthiness.

The condition is not part of either arm. Each arm starts with the same operand-stack state after the condition has been consumed. At the join after `END`, every reachable arm must leave the same stack height and compatible types. A statically detectable mismatch is a `CompileError`; a dynamic condition value of a non-Boolean type raises `TypeError` when `IF` executes.

## Loops

### `LOOP` with `WHILE`

In `LOOP condition-code WHILE body END`, the non-empty condition-code sequence runs at the loop header on every iteration. It must restore the loop-header operand-stack state and then leave exactly one `Bool` on top; `WHILE` consumes it. `true` enters the body, and `false` exits after `END`. Reaching the end of the body branches back to the condition-code sequence. The loop condition is not evaluated once before the loop and then cached.

### Condition-free `LOOP`

`LOOP body END` repeats the body without a condition. It exits only through `BREAK`, `RETURN`, an exception, or execution termination. A body may be empty; configured execution limits still apply.

### `FOR`

Before `FOR`, the stack contains an `Iterator` followed by a nonnegative variable ID on top. `FOR` consumes both and creates the loop binding in the loop's lexical environment. It requests the next item before each body execution, assigns that item to the binding, and exits after the iterator is exhausted. Iteration order is the iterator's defined order; an iterator failure propagates as a runtime failure. The loop binding is not visible after its matching `END`, except through a closure that captured its shared cell.

`BREAK` exits the innermost enclosing `LOOP` or `FOR`. `CONTINUE` transfers to the next condition evaluation for a `LOOP`, or to the next iterator step for a `FOR`. Either instruction outside a loop in the current function is a `CompileError`. A loop back-edge must return to a compatible operand-stack state at its header.

`BREAK` and `CONTINUE` unwind lexical scopes that they leave, from innermost outward, before taking the branch. `RETURN` unwinds all scopes in the current function. Uncaptured bindings become inaccessible when their scope is unwound; captured cells remain alive as specified in `spec/identifier-module-semantics.md`.

## Functions and returns

`RETURN` ends the current `FUNC` or `ASYNC_FUNC` invocation and transfers its result to the caller; with no return value it returns `Null`. Falling through the function's `END` also returns `Null`. It is invalid outside a function and raises `CompileError` at compile time. Returning from inside nested conditionals or loops exits the function, not merely the innermost block. Function entry and exit do not permit a branch target from another function.

## Labels and branch targets

Star source has no `LABEL`, `GOTO`, or direct jump-target syntax. The compiler creates opaque labels for basic blocks in its intermediate representation. Each label is defined once and belongs to exactly one code unit: the top-level program or one function. Conditional branches, loop back-edges, loop exits, `BREAK`, and `CONTINUE` refer to these labels. `RETURN` transfers to the caller and has no local branch target.

When emitting `.starc`, the compiler resolves each label to the start of an instruction in the same code unit. A target cannot point into an operand, outside the code unit, or into another function. The compiler rejects an undefined or duplicate IR label; the bytecode verifier rejects an out-of-range or non-instruction-boundary target before execution. The bytecode format may encode the resolved target as an offset; its width, signedness, and relative/absolute base do not change these logical rules.

The compiler lowers structured control flow to basic blocks and explicit branches. `IF` uses a conditional edge to the selected arm; `ELSE` adds an edge around the alternate arm to the join. A conditional `LOOP` branches from its header to the body or exit and back from the body to the header. A condition-free `LOOP` has an unconditional back-edge. `FOR` has an iterator-test block, body block, and exhaustion exit. These are compiler-generated control-flow edges, not source opcodes; VM bytecodes such as `JUMP`, `JUMP_IF_FALSE`, and `JUMP_IF_TRUE` belong to the separate `.starc` namespace.

## Stack-state validation

The compiler builds a control-flow graph and checks every reachable edge. Incoming paths to a join must agree on operand-stack height and have compatible types at each position. A loop establishes its stack state after consuming its condition or iterator inputs; every back-edge and every exit (`BREAK`, a false condition, or iterator exhaustion) must agree with that state. A condition consumes exactly one Boolean value. Violations detectable during compilation are `CompileError`; runtime type checks remain in force for values that cannot be proven statically.
