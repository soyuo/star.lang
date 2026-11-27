# REPL and Bytecode Disassembly

Status: draft

## Source REPL

`star repl` uses one source `Interpreter` for the session. A complete submission
executes as soon as the parser accepts it. An incomplete instruction, data
literal, or block keeps accumulating input at the continuation prompt. Normal
mode formatting whitespace is applied, so line breaks do not change the source
token stream. Invalid input reports a diagnostic and clears only the pending
submission.

The REPL recognizes the complete input lines `help`, `reset`, `exit`, and
`quit` as host commands when no source submission is pending. Guest `INPUT`
reads from the same buffered standard input and may consume subsequent lines.
The prompt and value echo are written to stderr; guest output remains on stdout.

Global bindings, function and type definitions, and the operand stack persist
between accepted submissions. A new function definition replaces an existing
definition with the same numeric ID. When a submission leaves values on the
operand stack, the value first pushed by that submission is echoed; values stay
on the stack for later instructions. `reset` clears guest bindings, definitions,
stack, tasks, channels, mutexes, and managed heap state while retaining the
host's I/O, capability grants, and resource limits. If an outstanding host
borrow prevents garbage collection, the reset state still takes effect and the
REPL reports that collection was deferred.

Input lines are limited to 1 MiB and one pending submission is limited to 64
MiB. EOF exits successfully when no submission is pending. EOF with incomplete
source reports the parser diagnostic and exits with source-error status.

## Bytecode disassembler

`star disasm <file.starc>` validates the entire file with the normal bytecode
verifier and default CLI file, table, instruction, and stack limits before
printing any listing. For this structural verification only, resource
capabilities are treated as granted: disassembly does not execute guest code or
grant those capabilities to any runtime. Invalid files return the
bytecode/source diagnostic exit code; file access errors use the I/O exit code.
The listing is capped at 64 MiB.

Each function heading includes its function-table index, initializer/function
role, async flag, Code-section range, and declared maximum stack. Instructions
are shown with their function-relative byte offset and decoded operands.
Each instruction prints its raw VM opcode byte and VM name. `PUSH_CONST` also
prints a constant-pool index. `INTRINSIC` prints its stable 16-bit intrinsic ID
and operation name after the `0xE0` VM opcode byte. The VM opcode and intrinsic
ID namespaces are distinct, as specified in `bytecode-instructions.md` and
`bytecode-format.md`.

The command is a read-only inspection tool. It does not execute guest code or
resolve source files and emits the listing only after verification succeeds.
