# Algorithm-Solving Profile

Status: draft

This profile defines the supported command-line execution path and the published online-judge fixture format for multi-case Star solutions.

## Run command

`star run <file.star|file.starc>` executes a source program with the reference interpreter or a bytecode file with the VM. Standard input is streamed to the guest program. Standard output is streamed unchanged, including spaces, blank lines, and final-newline behavior. The CLI does not split or normalize input.

The runner accepts these ceilings:

| Option | Default | Meaning |
|---|---:|---|
| `--max-memory` | `256M` | Estimated guest-memory ceiling |
| `--max-stack` | `65536` | Operand values |
| `--max-call-depth` | `512` | Nested calls |
| `--max-instructions` | `10000000` | Executed statements or bytecode instructions |
| `--timeout` | `30s` | Cooperative wall-clock deadline checked between execution steps |
| `--strict` | off | Reject source formatting whitespace |
| `--stats` | off | Write timing, steps, and peak guest-memory estimate to stderr |

Memory sizes accept bytes or `K`, `M`, and `G` binary suffixes. Timeouts accept integer milliseconds or seconds. Capabilities are disabled by default. Source and bytecode inputs larger than 64 MiB or the configured memory ceiling are rejected before reading.

Stable process exit codes are `0` success, `2` usage/options, `3` input-file I/O, `4` lexical, parse, semantic, or bytecode verification diagnostics, `5` runtime failure, and `6` a configured resource ceiling. Guest output remains on stdout; diagnostics and optional statistics go to stderr.

Example:

```sh
star run solution.star --max-memory 256M --timeout 30s --stats < cases.in > actual.out
star run solution.starc --max-memory 256M --timeout 30s --stats < cases.in > vm.out
```

The timeout is cooperative: it is enforced at interpreter statement and VM instruction boundaries. `--stats` reports the elapsed time inside the engine execution call, executed steps, and the peak successfully preflighted guest-memory estimate. Source parsing is outside that interval; bytecode verification is inside the VM call. The memory estimate covers the interpreter's tracked guest allocations; it is not operating-system process RSS. Record the platform and toolchain separately when publishing benchmark results.

## Published fixture suite

`tests/oj/manifest.toml` defines deterministic problem cases and limits. Each problem supplies one or more complete input streams and byte-for-byte expected output. Inputs use one value per line so solutions can use line-based `INPUT`; blank lines, EOF, arbitrary-precision integers, duplicate keys, and UTF-8 text are intentional cases.

The six algorithm problems cover arrays, hash maps, graph traversal, dynamic programming, sorting/searching, and strings. Each accepts multiple test cases in one process. The fixture pack does not ship reference solutions. Given a `.star` solution and its matching `.starc` file for each problem, `tests/oj/run.py` applies every case and the shared empty-input boundary to both engines under identical memory/time ceilings and requires the manifest's exit code, byte-exact expected output, and source/VM agreement. The runner produces per-engine timing and memory measurements; publishing a baseline still requires naming the hardware, Star revision, Rust version, and target.

For a published run, record the Star revision, Rust version, target triple, engine, case ID, exit code, `execution_ms`, and `peak_guest_memory_estimate_bytes` from `--stats`. Compare performance only with a declared hardware and language baseline, and publish its input sizes and measurement method. These records are per-run measurements, not portable performance guarantees.
