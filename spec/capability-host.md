# Capability-Gated Host Services

Status: draft

The Rust reference runtime exposes filesystem, path, environment, and process
operations through `runtime::host::HostServices`. The service checks a host-
provided `star_compiler::Capability` grant on every operation and denies all
capabilities by default. These Rust APIs are not yet registered as guest
`std.fs`, `std.path`, `std.env`, or `std.process` functions; guest module
registration belongs to the later module-loading work.

## Configuration and capability ownership

`Interpreter::with_capabilities` supplies grants for source execution.
`BytecodeVm` derives its host-service grants from the same
`VerificationPolicy::granted_capabilities` used by the verifier. The host may
configure filesystem roots through `host_services_mut`; guests cannot configure
or expand those roots.

Read and write roots are separate directory handles. `set_read_root` requires
`fs.read`, and `set_write_root` requires `fs.write`. Opening a root uses host
ambient authority, so these setup methods are for trusted host code only. A
granted capability without its configured root fails with
`FilesystemRootNotConfigured`. Every file operation checks its capability
before touching the filesystem.

## Filesystem

`read_file`, `read_text`, and `read_dir` use the read root. Directory entry names
are returned in sorted order. `write_file`, `create_dir_all`, and `remove_file`
use the separate write root. Text reads require valid UTF-8. File writes replace
the target contents.

Input paths must be relative. The lexical `runtime::path::normalize_relative`
rejects absolute paths and `..` that escapes its relative root. Filesystem
operations then resolve paths through `cap_std::fs::Dir`, which keeps `..`,
absolute paths, and symlink resolution within the configured directory
[(`Dir` API)](https://docs.rs/cap-std/latest/cap_std/fs/struct.Dir.html)
([security design](https://github.com/sunfishcode/cap-std/blob/main/README.md#capability-based-security)).
The host path helpers do not themselves authorize access or resolve symlinks.

## Paths

`runtime::path` provides `join`, `parent`, `file_name`, `extension`,
`is_absolute`, and `normalize_relative`. These are lexical, operating-system
path operations. A path returned by these helpers must still pass through a
configured filesystem capability before use.

## Environment

`get_env_os` and `get_env` require `env.read` for each lookup. The OS-string
variant preserves non-Unicode values; `get_env` returns
`EnvironmentValueNotUnicode` when the value cannot be represented as UTF-8.
Missing variables return `None`. Environment mutation is not included because
the capability set defines read access only.

## Processes

`spawn` requires `process.spawn`, an absolute executable path, and explicit
arguments. It invokes the executable directly without a shell, clears the child
environment, closes child stdin, and pipes stdout/stderr to the host caller.
Callers must drain piped output while the child runs to avoid blocking a child
that fills a pipe.

Granting `process.spawn` allows a child to use the host account's operating-
system authority. It does not confine that process to the VM's filesystem roots
or resource limits. Strong isolation requires an OS-isolated process, as
specified by the sandbox security requirements.

## Native FFI

Native library access and synchronous callbacks are separately gated by the
`ffi` capability; see `spec/native-ffi.md` for signatures and callback lifetime
rules.
