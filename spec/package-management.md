# Packages and Module Resolution

Status: stable

`star-package` is the reusable Rust SDK for Star project manifests, lockfiles, module search, and a local source registry. `star` exposes the project operations described below. The public hosted registry is not part of this release; the local registry provides the same version selection and checksum path without a network service.

## Project manifest

The project root contains `star.toml`:

```toml
schema_version = 1

[package]
name = "hello-star"
version = "0.1.0"
edition = "1"
description = "An example Star program"

[build]
entry = "src/main.star"
target = "bytecode"
optimization = 0

[dependencies]

[permissions]
filesystem = false
network = false
process = false
ffi = false
environment = false
```

Package names use ASCII letters, digits, `_`, and `-`; versions use semantic versioning; paths are relative to the declaring package and cannot traverse parent directories. Dependencies are a version requirement (`"^1.2"`), `{ path = "vendor/library" }` when kept inside the project, or `{ git = "https://example.invalid/library.git", rev = "<full commit id>" }`. Git references must use a 40- or 64-digit commit ID. Project `entry` defaults to `src/main.star`.

`star new` and `star init` create `src/main.star`, `src/lib.star`, an example, a manifest, and ignore rules. The manifest and resolver API are also available from the `star-package` Rust crate for editor, build, and SDK integrations.

## Resolution and initialization

`star-package::resolve_module` searches relative to the current source file, the project `src` directory, the configured installed-dependency directory, and the standard-library directory. It consults `STAR_PATH` only after those locations fail, and requires `env.read` at that point. Reading a source or bytecode file requires `fs.read`. Module names cannot be absolute or contain parent traversal; canonicalized candidates must remain beneath their search root, which prevents symlink escapes. `.star` is preferred to `.starc` when no extension is supplied.

`ModuleExecutionContext` tracks `Initializing`, `Initialized`, and `Failed` states per canonical module path. A module initializes once per context. Re-entering an initializing module reports a circular pre-initialization access; a failed initializer stays failed for that context.

The bytecode module table declares each module's name, initializer, global IDs, imports, and exports. Exports are explicit and private-by-default. `MODULE_GET` returns an exported global or callable function; for an exported structure it returns the numeric type ID consumed by structure operations. Static imports bind exported globals by shared cell, functions as module-qualified references, and structures as numeric type IDs. Project `check`, `build`, and `run` compile the entry source, project sources, and lock-verified installed dependency sources into one verified multi-module image. Path dependencies are assigned the same `deps/<name>/<version>/src` module names as installed packages. `STAR_PATH` sources are added only when the manifest grants both `environment` and `filesystem`; their import still requires the corresponding `env.read` and `fs.read` capabilities. Dynamic imports resolve within the linked image. The VM does not read external module files at runtime; standalone source interpretation still executes one parsed `Program`. `star-package` also exposes the canonical file resolver and per-execution initialization-state API for SDK integrations.

## Package operations

The local registry root is `STAR_REGISTRY_HOME`, or `~/.star/registry` when unset. Published package versions are immutable directories at `<registry>/<name>/<version>`. A source checksum is SHA-256 over sorted relative file names and contents, excluding `.git`, `.star`, `target`, `deps`, and symlinks. `star package` writes a deterministic `STARPKG1` source archive under `target/packages/`.

```text
star add <name> [--version <requirement>|--path <directory>|--git <url> --rev <commit>]
star remove <name>
star install
star update
star search <query>
star package
star publish
star login
star audit
```

`install` resolves transitive dependencies and writes sorted `star.lock` records with exact versions, source kinds, and checksums. Registry and Git packages are placed under `deps/<name>/<version>`; path dependencies stay in place. `update` resolves again. `audit` recomputes the graph and verifies lock and installed checksums. `publish` copies a validated project to the configured local registry; registry versions cannot be overwritten. Local registry operations do not require credentials, so `star login` reports that no login is needed.

`star add` and `star remove` change manifest dependencies; run `star install` to refresh the lock and installed packages. The manager does not execute build scripts while resolving or installing packages.
