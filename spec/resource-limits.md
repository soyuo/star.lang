# Runtime Resource Limits and Sandbox Policy

Status: draft

## Configurable limits

The host sets `ExecutionLimits` on the reference interpreter or bytecode VM before execution. The same configuration applies to source statements and bytecode instructions:

| Limit | Default | Enforced quantity |
|---|---:|---|
| `max_memory_bytes` | 268,435,456 | Estimated storage owned by guest values, containers, bindings, closures, and the managed heap |
| `max_stack_values` | 65,536 | Total operand values across active frames |
| `max_call_depth` | 512 | Nested function calls |
| `max_instructions` | 10,000,000 | Executed source statements or bytecode instructions |
| `max_execution_time` | 30 seconds | Elapsed wall-clock time for one execution |

The verifier separately limits serialized file size, table counts, decoded instructions, and per-function stack depth before the VM starts. The runtime total-stack limit remains in force while frames execute.

## Accounting and enforcement

Memory is a deterministic estimate of guest-owned storage, not process RSS. It accounts for dynamic integer, string, and byte payloads; container capacities; binding cells; and registered managed values. It does not account exactly for allocator metadata, Rust runtime state, code pages, native libraries, or other host allocations. The estimate is sampled at execution start and every 32 executed steps. Guest-controlled input, integer literals and arithmetic, shifts, Map insertion, collection construction/cloning/iteration/slicing/appends, object copies, concatenation, and value formatting, plus bytecode `POW` and `INT_TO_BYTES`, also check estimated allocation needs before creating large results. `TO_STRING` and `PRINT` use a bounded formatter. When an estimate exceeds the configured limit, the VM attempts garbage collection and measures again; an estimate still above the limit raises a fatal `ResourceLimit` error.

Stack depth and instruction counts are checked as execution proceeds. The elapsed-time deadline is cooperative and checked between source statements or bytecode instructions. It cannot preempt one long-running host operation or a blocked injected input/output stream. Hosts that need a hard deadline or isolation from native code must additionally run the VM in an OS-isolated process with operating-system resource limits.

Limit errors are fatal to the current execution and bypass Star-level exception handlers. A limit value of zero denies any nonzero use of that resource.

## Capability policy

The bytecode host supplies an explicit `VerificationPolicy::granted_capabilities` allowlist. The verifier checks required capabilities before execution, and the VM checks them again when the resource operation runs. The capability vocabulary and currently supported operations are listed in `spec/bytecode-verification.md`. A grant authorizes only an implemented operation; it does not enable a feature that the current VM reports as unsupported.

The source reference interpreter has no ambient filesystem, network, process, environment, native-library, or random-source access. Standard input and output are injected by the host as explicit streams. Imports are unsupported in the current VM slice; if a future resolver reads a module file, it must require `fs.read` at that access, and it must require `env.read` only if it consults `STAR_PATH`.

VM-level limits and capabilities reduce accidental or bytecode-driven resource access. They are not a substitute for OS process isolation when executing hostile programs.
