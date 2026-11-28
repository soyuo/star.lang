# Standalone Applications

Status: Stable for Star v1.0

`star build --target native --release` produces a host-native executable containing the Star runtime and a compiled `.starc` application. This is a bundled interpreter, not ahead-of-time machine-code generation. It uses the same platform and architecture as the `star` executable performing the build; cross-compilation is not provided by this command.

## Build behavior

The command requires a release-compiled `star` executable. It accepts a `.star` source file or the entry file of a Star project. A project build packages the capability grants resolved from its manifest; a standalone source-file build has no ambient capability grants. The output defaults to the source file's base name (`.exe` on Windows); `--output` selects another path. Existing output files are replaced after the complete bundle has been written to a temporary sibling file.

The bytecode payload is limited to 64 MiB and is verified again when the application starts. Runtime resource limits use the v1.0 defaults in `spec/resource-limits.md`. Standard input, output, and error remain the host process streams. The current application launcher does not pass operating-system command-line arguments to the Star program.

## Appended container

The executable consists of the release `star` runtime, the exact compiled `.starc` bytes, and this fixed 24-byte little-endian trailer:

| Offset from trailer start | Size | Value |
|---:|---:|---|
| 0 | 8 | `.starc` payload length as unsigned `u64` |
| 8 | 8 | Capability bit mask as unsigned `u64` |
| 16 | 8 | ASCII magic `STARAPP1` |

The runtime checks the final eight bytes for the magic, bounds the payload length, reads the `.starc` bytes immediately before the trailer, reconstructs the embedded grants, and executes the bytecode through the ordinary verifier and VM. Unknown capability bits or malformed lengths make the executable invalid. `.starc` retains its own checksum and version header.

Capability bits are stable within bundle format 1:

| Bit | Capability |
|---:|---|
| 0 | `fs.read` |
| 1 | `fs.write` |
| 2 | `net.connect` |
| 3 | `net.listen` |
| 4 | `process.spawn` |
| 5 | `env.read` |
| 6 | `ffi` |
| 7 | `clock` |
| 8 | `random` |

Embedded grants are the build-time authorization for the guest program; they do not provide operating-system isolation. A person who can modify the executable can modify its bytecode or grants. Do not distribute an application with grants broader than its intended use. The current runtime has no publisher-signature verification for individual application bundles.

## Output selection

Without `--output`, `main.star` produces `main` on Unix-like systems and `main.exe` on Windows. A project build uses the entry file's base name. `--output <path>` is interpreted as the complete output filename, including any desired extension.
