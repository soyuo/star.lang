# Star Language and Toolchain Reference

Status: Stable for Star v1.0

This index is the entry point for the normative v1.0 language, bytecode, runtime, and toolchain references. The compatibility promises for these documents are in `compatibility.md`. A document may describe a deliberately unsupported extension or implementation boundary; that description is part of the stable contract and does not promise that the feature is enabled.

## Language and source format

- [Source format and coordinates](source-format.md)
- [Grammar](grammar.ebnf)
- [Source opcode catalog](opcodes.toml)
- [Source instruction encodings](instructions.md)
- [Integer literal encoding](integer-encoding.md)
- [String and byte literal encoding](string-byte-encoding.md)
- [Instruction semantics](instruction-semantics.md)
- [Types and conversions](type-semantics.md)
- [Arithmetic semantics](arithmetic-semantics.md)
- [Collections and strings](collection-semantics.md)
- [Control flow](control-flow.md)
- [Function ABI](function-abi.md)
- [Identifiers, modules, and visibility](identifier-module-semantics.md)
- [Objects, methods, and closures](object-closure-semantics.md)
- [Exceptions](exceptions.md)
- [Packages and module resolution](package-management.md)

## Compiler and bytecode

- [Intermediate representation and lowering](ir.md)
- [`.starc` container format](bytecode-format.md)
- [Bytecode instructions](bytecode-instructions.md)
- [Bytecode verification](bytecode-verification.md)

## Runtime and host APIs

- [Resource limits and capability policy](resource-limits.md)
- [Capability-gated host services](capability-host.md)
- [Garbage collection](gc-semantics.md)
- [Standard input and output](standard-io.md)
- [Core collections](core-collections.md)
- [Integer math](integer-math.md)
- [String and byte processing](string-processing.md)
- [JSON and networking](json-network.md)
- [Concurrency runtime](concurrency-runtime.md)
- [Native FFI](native-ffi.md)

## CLI, development, and release

- [CLI reference](cli.md)
- [Algorithm-solving profile](algorithm-solving-profile.md)
- [Source encoding and formatting tools](source-tools.md)
- [REPL and disassembly](repl-disassembly.md)
- [Editor and language server support](editor-support.md)
- [Standalone applications](standalone-applications.md)
- [Cross-platform distribution and installation](distribution.md)
- [Quality gates](quality-gates.md)
- [Compatibility policy](compatibility.md)

The [change log](../CHANGELOG.md) records release-level additions, fixes, and compatibility notes.
