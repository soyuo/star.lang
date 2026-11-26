# Star Language Roadmap

The sequence and design constraints here follow the `star-lang` project knowledge in quick-llm-memory. Complete checkboxes from top to bottom. Keep all 47 items visible in this roadmap before implementation begins.

Progress: **0 / 47**

## Phase 1 — Language specification (12)

- [ ] Define `spec/opcodes.toml` as the canonical source-opcode catalog with stable IDs and instruction metadata.
- [ ] Specify allowed source characters, UTF-8/BOM policy, optional whitespace, and strict-mode behavior.
- [ ] Complete `spec/grammar.ebnf` for valid programs, literals, instructions, and nested blocks.
- [ ] Complete `spec/instructions.md` with source syntax and binary representation for every instruction.
- [ ] Specify each instruction's operands, stack inputs/outputs, side effects, and failure cases.
- [ ] Define integer literal encoding, signed values, arbitrary-precision representation, and byte order.
- [ ] Define string and byte literal encoding, lengths, UTF-8 validity, and round-trip behavior.
- [ ] Define identifier encoding, variable declaration, initialization, and lexical scope rules.
- [ ] Define block boundaries, labels, branch targets, and control-flow instruction semantics.
- [ ] Define primitive/composite types, conversions, truthiness, and type-error behavior.
- [ ] Define arithmetic semantics: overflow, integer division, remainder for negative operands, and mixed numeric operations.
- [ ] Specify function ABI: arguments, return values, call frames, recursion, and closure capture.

## Phase 2 — Reference interpreter (6)

- [ ] Establish the initial Rust workspace crates for common types, lexer, parser, interpreter, and CLI.
- [ ] Implement a lexer that validates source characters and reports source locations.
- [ ] Implement a parser and AST for the finalized grammar, including malformed-input diagnostics.
- [ ] Implement semantic checks for names, scopes, types, and invalid control flow.
- [ ] Implement reference execution for stack operations, arithmetic, variables, and standard input/output.
- [ ] Implement conditionals, loops, functions, recursion, and the calculator/Fibonacci/factorial/Brainfuck examples.

## Phase 3 — Compiler and virtual machine (6)

- [ ] Define IR operations and source-to-IR lowering rules.
- [ ] Define the versioned `.starc` bytecode serialization, sections, constant pool, and function tables.
- [ ] Implement IR-to-bytecode compilation and source-location mapping.
- [ ] Implement a bytecode verifier for opcodes, operands, control flow, stack depth, and resource-sensitive instructions.
- [ ] Implement the stack-based VM for the specified bytecode and runtime errors.
- [ ] Require interpreter and VM results to agree on shared conformance and algorithm fixtures.

## Phase 4 — Runtime expansion (5)

- [ ] Implement strings, bytes, arrays, tuples, maps, sets, and iterators with documented indexing and failure behavior.
- [ ] Implement structs, methods, first-class functions, and closures.
- [ ] Implement tracing garbage collection and safe object lifetime handling.
- [ ] Implement exceptions, propagation, stack traces, and cleanup behavior.
- [ ] Enforce configurable memory, stack, call-depth, instruction, and time limits with sandbox capabilities.

## Phase 5 — General-purpose and algorithm libraries (9)

- [ ] Implement deterministic standard input, token/line readers, EOF handling, buffered output, and exit flushing.
- [ ] Implement core collections: dynamic arrays, stacks, queues, deques, hash maps, and hash sets.
- [ ] Implement min/max priority queues, sorting, stable sorting, binary search, and lower/upper bounds.
- [ ] Implement common integer math: gcd/lcm, modular arithmetic, and modular exponentiation.
- [ ] Implement string/byte processing APIs with explicit byte and Unicode indexing contracts.
- [ ] Implement filesystem, path, environment, and process APIs under capability permissions.
- [ ] Implement JSON plus TCP/UDP and HTTP client/server APIs.
- [ ] Implement async tasks, concurrency primitives, worker execution, and native FFI.
- [ ] Deliver the algorithm-solving profile: stable CLI execution of multi-case solutions and a published suite covering arrays, maps, graphs, dynamic programming, sorting/searching, and strings under documented time/memory limits.

## Phase 6 — Developer ecosystem (6)

- [ ] Implement CLI project lifecycle, check/build/run/test/bench, stable exit codes, and actionable diagnostics.
- [ ] Implement source encoder/decoder, formatter, and minifier with round-trip guarantees.
- [ ] Implement REPL and bytecode disassembler.
- [ ] Implement debugger with breakpoints, stepping, stack inspection, and source locations.
- [ ] Implement LSP and editor syntax support for VS Code, Zed, Neovim, and Tree-sitter.
- [ ] Implement package manifests, dependency resolution, package manager, registry, and SDK integration.

## Phase 7 — Stabilization and release (3)

- [ ] Establish reproducible performance benchmarks and complete optimization, fuzzing, security, and compatibility gates.
- [ ] Produce supported Windows/macOS/Linux and x86_64/ARM64 builds with standalone packaging and installation.
- [ ] Publish the stable language/bytecode compatibility policy, complete documentation, and release v1.0.
