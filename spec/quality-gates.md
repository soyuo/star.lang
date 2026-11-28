# Stabilization Quality Gates

The gates run from `.github/workflows/quality-gates.yml` on pull requests and
pushes to `develop`:

- Formatting, Clippy correctness/suspicious/performance lints as errors, and
  the complete Cargo workspace test suite. Other Clippy style lints remain
  advisory so existing style debt does not block correctness or performance
  fixes.
- Native test runs on Windows, macOS, and Linux across x86_64 and ARM64.
- `cargo audit` against the locked dependency graph.
- Bounded `cargo-fuzz` smoke runs for source normalization/parsing and
  untrusted `.starc` verification. Scheduled or manual longer fuzzing can use
  the same targets and seeds.

The separate performance workflow builds the CLI in release mode, runs the
fixed suite in `benchmarks/suite.toml`, and uploads a JSON report. It compares
the reference interpreter and bytecode VM using identical inputs on the same
host. Reports include input size/hash, revision, Rust toolchain, target triple,
host CPU/OS, build time, process wall time, guest-memory estimate, and steps.
Wall-time samples include process startup; guest memory is tracked allocation,
not process RSS. Timing reports are measurements for optimization review, not a
fixed threshold gate.

`cargo audit` and fuzzing supplement the workspace tests; neither replaces
the runtime capability checks or adversarial bytecode verifier checks in the
test suite.
