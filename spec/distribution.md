# Standalone Distribution and Installation

Status: stable

The `star` CLI release is a standalone toolchain package. It contains the Star
executable, license, and README; users do not need Rust or Cargo to run Star
source files, verified `.starc` programs, or bundle a host-native application.
`star build --target native --release` combines the installed runtime with
verified application bytecode. It is interpreter bundling, not AOT compilation;
see `spec/standalone-applications.md`.

## Supported release targets

| Operating system | Architecture | Rust target | Archive |
|---|---|---|---|
| Linux (glibc) | x86_64 | `x86_64-unknown-linux-gnu` | `star-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (glibc) | ARM64 | `aarch64-unknown-linux-gnu` | `star-aarch64-unknown-linux-gnu.tar.gz` |
| Windows | x86_64 | `x86_64-pc-windows-msvc` | `star-x86_64-pc-windows-msvc.zip` |
| Windows | ARM64 | `aarch64-pc-windows-msvc` | `star-aarch64-pc-windows-msvc.zip` |
| macOS | Intel x86_64 | `x86_64-apple-darwin` | `star-x86_64-apple-darwin.tar.gz` |
| macOS | Apple silicon ARM64 | `aarch64-apple-darwin` | `star-aarch64-apple-darwin.tar.gz` |

Release archives contain exactly `star` (or `star.exe`), `LICENSE`, and
`README.md` at their root. Archive member timestamps and ownership metadata are
normalized by `scripts/package_release.py`. The tagged release also publishes
`SHA256SUMS`, `install.sh`, and `install.ps1`. The checksum file covers the six
platform archives.

## Creating a release

`.github/workflows/release.yml` builds the CLI and its archive on a native
GitHub-hosted runner for each target above. It runs for pull requests that
touch release inputs and pushes to `develop`; those runs provide one archive
artifact per target without publishing a release. A version tag such as
`v1.0.0` builds the same packages and publishes them as a GitHub release. The
tag must match the workspace version in `Cargo.toml`. The release job requires
the workflow token to have `contents: write` permission.

## Installing

On Linux or macOS, download and inspect `install.sh` from the chosen release,
then run it with an optional release tag and install directory:

```sh
curl --fail --location \
  https://github.com/soyuo/star.lang/releases/latest/download/install.sh \
  --output install.sh
sh install.sh
```

For a specific release, pass its tag as the first argument. The default
destination is `$HOME/.local/bin`; pass a second argument or set
`STAR_INSTALL_DIR` to choose another directory. Add that directory to `PATH`
if it is not already present.

On Windows, download and inspect `install.ps1`, then run it in PowerShell:

```powershell
Invoke-WebRequest `
  -Uri https://github.com/soyuo/star.lang/releases/latest/download/install.ps1 `
  -OutFile install.ps1
./install.ps1
```

Use `-Version v1.0.0` to select a release, `-InstallDir <path>` to choose a
destination, and `-AddToPath` to add the destination to the current user's
PATH. The default destination is `%LOCALAPPDATA%\Programs\Star\bin`.

Both installers select the archive for the current OS and architecture,
download `SHA256SUMS`, verify the archive before extraction, reject unexpected
archive members, and install only the executable. They do not require
administrator privileges by default. Linux ARM64 packages target glibc.
