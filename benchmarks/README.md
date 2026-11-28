# Reproducible Star Benchmarks

`suite.toml` is the fixed workload list. Each case runs the same source file in
the reference interpreter and a `.starc` file freshly built for that report.
The suite includes recursion, arbitrary-precision arithmetic, interactive
input, and an interpreter workload.

From the repository root, build the CLI and record a report:

```sh
cargo build --locked --release -p star-cli
python3 scripts/benchmark_suite.py \
  --star-cli target/release/star \
  --iterations 10 \
  --output target/benchmark-results.json
```

On Windows, use `target\release\star.exe` and the equivalent PowerShell line
continuations. The script records the Star revision/version, Cargo lock and
suite hashes, Rust compiler/target, OS, CPU architecture/model, runner image,
input sizes and hashes, build time, process wall time, guest-memory estimate,
and executed steps. It also checks source/VM output equality before timing.

`star bench` includes process startup, file loading, and input delivery; its
wall-time result is not guest-only execution time. The `--stats` calibration
records guest allocation estimates rather than process RSS. Compare reports
from the same runner image and hardware. CI uploads a JSON artifact for each
benchmark run; timing is not a hard pass/fail threshold because hosted-runner
noise makes small regressions unreliable.
