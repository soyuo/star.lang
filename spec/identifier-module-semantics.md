# Identifiers, Scope, and Modules

Status: draft

This document defines numeric identifiers, variable binding and lookup, module path encoding, exports, resolution, and initialization. Identifier values use `Int` as defined by `spec/integer-encoding.md`; string paths use `spec/string-byte-encoding.md`. Opcode stack effects are in `spec/instruction-semantics.md`; function calls, frames, and closure capture are in `spec/function-abi.md`.

## Numeric identifier encoding

Star source has no alphabetic identifier tokens. An identifier operand is a nonnegative integer literal, encoded MSB-first as specified in `spec/integer-encoding.md`. The instruction or declaration context determines its kind; the same number can be used in different identifier namespaces without making the entities identical.

| Identifier kind | Uniqueness and lookup scope |
|---|---|
| Variable ID | A binding key within one lexical environment; may be shadowed by a declaration in a nested scope. |
| Function ID | Unique among functions in its defining module. |
| Parameter ID | A variable ID local to the function's parameter environment. |
| Module symbol ID | The numeric ID of one top-level binding selected for export; there is no separate alias ID. |
| Struct and interface ID | Unique by kind within the defining module. |
| Field ID | Unique within one struct. |
| Method ID | Unique within one struct or interface. |

IDs are semantic integers, not array offsets. A compiler or VM may map them to compact internal slots, but that mapping must preserve equality and scope and must not narrow a valid nonnegative `Int`. An operand that is not an `Int` raises `TypeError`; a negative or otherwise invalid identifier raises `ReferenceError`.

Standard-library modules and public functions have stable numeric IDs. Human-readable aliases such as `std.core` are documentation and tooling names; a source import path is encoded as UTF-8 data, and its resolution maps it to the stable module identity. No particular numeric IDs are assigned by this document.

## Variables and lexical scope

The lexical environment chain, from innermost to outermost, is block, function locals, captured closure cells, the defining module's global scope, then the execution context's global environment. `SCOPE_BEGIN` and `SCOPE_END` delimit block environments; a function call creates a function environment, and each loaded module has its own module environment. The execution-context global environment contains runtime-provided bindings; one module cannot see another module's private top-level bindings through it.

- `DECLARE` creates a mutable binding in the current environment. A second declaration of the same Variable ID in that same environment raises `ReferenceError`.
- `CONST` creates an immutable binding in the current environment and follows the same duplicate-declaration rule.
- A nested declaration may reuse an outer Variable ID and shadows that binding until the nested environment ends.
- `LOAD` searches from the current environment outward and returns the first matching binding. It raises `ReferenceError` if no binding exists.
- `STORE` searches for the nearest binding with that Variable ID. It updates that binding if mutable, raises `ReferenceError` if immutable, and creates a mutable binding in the current environment only when the ID is absent from the entire chain.
- `DELETE` removes a mutable binding from the current environment only; it does not delete an outer binding found by lookup. Deleting an absent or immutable binding raises `ReferenceError`.
- `LOAD_GLOBAL` reads from the current module's global scope. `STORE_GLOBAL` updates a mutable binding there or creates one if absent; neither searches another module's private environment. An absent load or a store to an immutable binding raises `ReferenceError`.
- `TYPE_ANNOTATE` associates a runtime type restriction with a binding. It raises `ReferenceError` if the binding is absent and `TypeError` for an unknown type or a value that violates the restriction; later stores must also satisfy it.

A closure captures shared binding cells, not snapshots of their values. A captured cell remains alive and mutable for the closure's lifetime even if the defining block has ended. `SCOPE_END` ends lookup visibility for uncaptured local bindings; it does not invalidate a captured cell.

## Module path encoding and resolution

Module paths are Unicode strings transported as UTF-8 bytes. To author a path in Star source, encode its UTF-8 bytes as the unsigned big-endian integer plus explicit byte length described in `spec/string-byte-encoding.md`; apply `UTF8_DECODE` when a `String` is required. `IMPORT` accepts the byte path and decodes it as UTF-8. `IMPORT_DYNAMIC` accepts a `String`. Invalid UTF-8 raises `TypeError`.

The logical path uses `/` as its separator on every host. Resolution searches in this order:

1. Relative to the importing source file.
2. The current project's module directory.
3. Installed project dependencies.
4. The Star standard library, including registered aliases such as `std.core`.
5. Directories registered in `STAR_PATH`.

The final step reads the `STAR_PATH` environment variable and requires the `env.read` capability. If resolution reaches this step without that permission, import raises `PermissionError` without reading the variable. Earlier resolution steps remain available without `env.read`.

Path matching is exact and case-sensitive at the language level. On a case-insensitive host filesystem, the resolver must verify the exact component spelling before accepting a match. `.` path components are removed. `..` may move within a permitted root but must not escape it. Embedded NUL is invalid and raises `TypeError`. Absolute paths and attempts to escape a permitted root are rejected with `PermissionError`; symlink resolution must not bypass the same root restriction. Filesystem capability checks still apply. If no module resolves, import raises `ReferenceError`.

The resolved canonical module identity, rather than the spelling of the path used to find it, keys module state. Two paths that resolve to the same module therefore share one module instance within an execution context.

## Visibility and module initialization

Top-level variables, functions, structs, and interfaces are private to their module by default. `EXPORT n` selects the top-level binding whose existing numeric identifier is `n`; it does not create a second ID or alias. Because `EXPORT` has no kind operand, exactly one top-level binding of any kind may have that numeric ID in a module's exportable namespace. No match, multiple cross-kind matches, or a repeated export raises `ReferenceError`. `MODULE_GET` and `MODULE_CALL` use the same numeric ID and can access only exported symbols; private symbols remain inaccessible across module boundaries.

Each module has one state per execution context: `Unloaded`, `Loading`, `Initializing`, `Ready`, or `Failed`.

- Import resolves and loads the module, initializes its dependencies, runs its initializer once, and returns its module handle after it reaches `Ready`.
- `MODULE_INIT` on a `Ready` module has no effect. It does not execute the initializer a second time.
- A circular import may receive a provisional handle while a module is `Loading` or `Initializing`. Reading an export before that binding has been initialized raises `ReferenceError`.
- If initialization fails, the module becomes `Failed`; later imports rethrow the stored failure and do not execute the initializer again in that context.
- `IMPORT_DYNAMIC` uses the same identity, resolution, visibility, and initialization rules as `IMPORT`, but performs the resolution at runtime.

Initialization order follows the dependency traversal: each dependency is initialized before the importing module continues, except for a back-edge to a module already in `Loading` or `Initializing`, which uses the provisional handle described above. This makes cycle behavior deterministic while preserving the error for reads that occur too early.
