# Changelog

All notable Star releases are recorded here. See [the compatibility policy](spec/compatibility.md) for the versioning rules.

## 1.0.0 - 2026-11-28

First stable release of the Star toolchain, language version 1.0, and `.starc` bytecode format 1.0.

- Stabilized the source grammar, instruction encodings and semantics, type system, arithmetic, function ABI, control flow, exceptions, and module rules.
- Published the Rust reference interpreter, `.starc` compiler and verifier, bytecode VM, resource limits, and capability-gated host APIs.
- Added project/package management, standard I/O, collections, algorithms, JSON and network APIs, async tasks, concurrency, workers, and native FFI.
- Added encoder/decoder, formatter, minifier, REPL, disassembler, debugger, LSP, editor support, algorithm fixtures, reproducible benchmarks, fuzzing, and security gates.
- Published standalone CLI packages for six Windows, macOS, and Linux x86_64/ARM64 targets, with installers and checksums.
- Added host-native application bundling with embedded verified bytecode; this uses the Star interpreter and is not AOT compilation.

There is no earlier stable release. The pre-1.0 development history is available in the repository commit history.
