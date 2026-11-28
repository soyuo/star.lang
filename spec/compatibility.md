# Star 1.0 Compatibility Policy

Status: Stable for Star v1.0

This policy defines the compatibility promises for Star source programs, `.starc` bytecode, packages, and the `star` distribution. The CLI release number, language version, and bytecode format version are independent version axes; a matching number is not implied.

## Version axes

| Surface | Star 1.0 value | Where it is reported or encoded |
|---|---|---|
| CLI, compiler, and runtime distribution | `1.0.0` | Cargo workspace version and `star --version` |
| Source language | `1.0` | `star --version`; source files have no in-band version declaration |
| `.starc` container | major `1`, minor `0` | The `.starc` header |
| `Star.toml` manifest schema | `1` | `schema_version` |
| Package | Package-specific SemVer | `[package].version` in `Star.toml` and registry records |

The compiler selects the language version reported by its own `star --version`. A source file does not select or override that version. The `.starc` version is parsed and checked independently of the source language and package versions.

## Semantic versioning

The `star` toolchain uses Semantic Versioning:

- A **major** release may make an incompatible change to source syntax or semantics, standard-library contracts, CLI behavior, package interfaces, or the toolchain API. A major `.starc` format change is versioned independently and does not follow automatically from a CLI major release.
- A **minor** release may add source features, library APIs, CLI commands, package capabilities, or optional bytecode-format features while preserving existing 1.x behavior.
- A **patch** release fixes defects or security problems without intentionally changing valid-program behavior or public interfaces. A security fix may disable a vulnerable behavior; its release notes must describe the change and any migration needed.

Compatibility guarantees apply to documented public behavior. Undocumented Rust implementation details and internal APIs are not stable interfaces.

## Source compatibility

The v1.0 source grammar, source characters, instruction spellings, typing and conversion rules, instruction effects, and runtime failure behavior in the normative documents listed by `spec/index.md` define language version 1.0. The source reference interpreter accepts valid 1.0 source and preserves its specified observable behavior, including output bytes, exit status, and documented errors. CLI commands that compile source support the subset listed in `spec/cli.md`; valid source outside that lowering subset remains valid language 1.0 and can be run by the reference interpreter, but compilation produces a lowering diagnostic.

A 1.x minor release may add syntax or APIs only when existing 1.0 programs keep their meaning and remain valid. New reserved words, token spellings, or overloads must not reinterpret valid 1.0 programs. Removing or changing a documented 1.x behavior requires a new language major version and a migration note.

Because source files do not carry a version declaration, the selected language profile is reported by `star --version`. `Star.toml` schema and package versions are separate from the language version.

## Bytecode compatibility

`.starc` major and minor versions are independent of the CLI and language versions. Star 1.0 emits `.starc` 1.0. A version 1 reader rejects an unsupported major version. It may accept a newer minor version only when all known section layouts retain their defined meaning, every unknown section is optional, and reserved fields remain valid, as specified in `spec/bytecode-format.md`.

A minor bytecode revision may add optional sections or other explicitly backward-compatible data. It must not change the interpretation of an existing section, opcode, operand, index, or reserved field. An incompatible container layout or instruction contract requires a new bytecode major version. A VM must reject a validly encoded instruction or feature it does not implement; accepting the container does not imply support for every optional feature.

Source instruction IDs, VM opcodes, intrinsic IDs, section IDs, and container versions are distinct namespaces. Changes to one namespace do not implicitly change the others.

## Deprecation and migration

Deprecations are documented in the affected API reference and announced in release notes. A deprecated 1.x API remains supported through the 1.x line unless a documented security issue requires earlier removal or restriction. Removal or incompatible behavior change is reserved for the next applicable major version. Migration notes accompany every such change.

## Star 1.0 release

The initial stable release is CLI/toolchain `1.0.0`, language `1.0`, and `.starc` `1.0`. The supported native CLI targets and installation process are listed in `spec/distribution.md`. `star build --target native --release` embeds verified `.starc` bytecode into the host-native Star runtime; it packages an interpreter and is not ahead-of-time machine-code compilation. The six published runtime targets are listed in the distribution reference.

The compatibility and quality gates required for releases are in `spec/quality-gates.md`. Release artifacts are checksummed; checksums detect accidental corruption and do not authenticate a publisher.
