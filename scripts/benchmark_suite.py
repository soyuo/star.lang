#!/usr/bin/env python3
"""Run paired source-interpreter and bytecode-VM benchmarks from suite.toml."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import subprocess
import sys
import tempfile
import time
import tomllib
from datetime import datetime, timezone
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
BENCH_LINE = re.compile(
    r"star: bench_iterations=(\d+) total_ms=([0-9.]+) mean_ms=([0-9.]+) input_bytes=(\d+)"
)
STATS_LINE = re.compile(
    rb"star: execution_ms=([0-9.]+) peak_guest_memory_estimate_bytes=(\d+) executed_steps=(\d+)"
)


class BenchmarkError(RuntimeError):
    pass


def confined_file(root: Path, relative: str) -> Path:
    path = (root / relative).resolve()
    if path != root and root not in path.parents:
        raise BenchmarkError(f"suite path escapes repository: {relative}")
    if not path.is_file():
        raise BenchmarkError(f"suite file does not exist: {relative}")
    return path


def run(command: list[str], *, input_bytes: bytes | None = None, timeout: float = 45.0):
    try:
        return subprocess.run(
            command,
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise BenchmarkError(f"command failed to complete: {command[0]}: {error}") from error


def require_success(result, command: list[str]) -> None:
    if result.returncode != 0:
        stderr = result.stderr.decode("utf-8", "replace")
        raise BenchmarkError(
            f"command exited {result.returncode}: {' '.join(command)}\n{stderr}"
        )


def rust_metadata() -> tuple[str, str]:
    command = ["rustc", "--version", "--verbose"]
    result = run(command)
    require_success(result, command)
    version = result.stdout.decode("utf-8", "replace").strip()
    host = next(
        (line.partition(":")[2].strip() for line in version.splitlines() if line.startswith("host:")),
        "unknown",
    )
    return version, host


def run_stats(
    cli: str, program: Path, input_bytes: bytes, memory_bytes: int, time_limit_ms: int
) -> dict[str, int | float | str]:
    command = [
        cli,
        "run",
        str(program),
        "--max-memory",
        str(memory_bytes),
        "--timeout",
        f"{time_limit_ms}ms",
        "--stats",
    ]
    result = run(command, input_bytes=input_bytes, timeout=time_limit_ms / 1000 + 10)
    require_success(result, command)
    match = STATS_LINE.search(result.stderr)
    if match is None:
        raise BenchmarkError(f"runtime statistics missing for {program}")
    return {
        "exit_code": result.returncode,
        "output_sha256": hashlib.sha256(result.stdout).hexdigest(),
        "runtime_ms": float(match.group(1)),
        "peak_guest_memory_estimate_bytes": int(match.group(2)),
        "executed_steps": int(match.group(3)),
    }


def run_bench(
    cli: str,
    program: Path,
    input_path: Path,
    iterations: int,
    memory_bytes: int,
    time_limit_ms: int,
) -> dict[str, int | float]:
    command = [
        cli,
        "bench",
        str(program),
        "--iterations",
        str(iterations),
        "--input",
        str(input_path),
        "--max-memory",
        str(memory_bytes),
        "--timeout",
        f"{time_limit_ms}ms",
    ]
    result = run(command, timeout=(time_limit_ms / 1000 + 10) * iterations + 20)
    require_success(result, command)
    match = BENCH_LINE.search(result.stdout.decode("utf-8", "replace"))
    if match is None:
        raise BenchmarkError(f"benchmark summary missing for {program}")
    return {
        "iterations": int(match.group(1)),
        "total_process_wall_ms": float(match.group(2)),
        "mean_process_wall_ms": float(match.group(3)),
        "input_bytes": int(match.group(4)),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", type=Path, default=ROOT / "benchmarks" / "suite.toml")
    parser.add_argument("--star-cli", default="star")
    parser.add_argument("--iterations", type=int)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()

    try:
        suite_path = arguments.suite.resolve()
        suite = tomllib.loads(suite_path.read_text(encoding="utf-8"))
        iterations = (
            arguments.iterations
            if arguments.iterations is not None
            else int(suite["iterations"])
        )
        memory_bytes = int(suite["memory_limit_bytes"])
        time_limit_ms = int(suite["time_limit_ms"])
        cases = suite["case"]
        if suite["schema_version"] != 1 or iterations < 1 or not cases:
            raise BenchmarkError("invalid benchmark suite schema or iteration count")
        if memory_bytes <= 0 or time_limit_ms <= 0:
            raise BenchmarkError("benchmark limits must be positive")

        rust_version, rust_host = rust_metadata()
        manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        star_version = manifest["workspace"]["package"]["version"]
        git_command = ["git", "rev-parse", "HEAD"]
        git_result = run(git_command)
        require_success(git_result, git_command)
        commit = git_result.stdout.decode("ascii").strip()
        suite_hash = hashlib.sha256(suite_path.read_bytes()).hexdigest()
        case_results = []
        case_ids = set()

        with tempfile.TemporaryDirectory(prefix="star-bench-") as temporary:
            temporary_root = Path(temporary)
            for case in cases:
                case_id = case["id"]
                if not re.fullmatch(r"[a-z0-9][a-z0-9-]*", case_id) or case_id in case_ids:
                    raise BenchmarkError(f"invalid or duplicate benchmark case id: {case_id}")
                case_ids.add(case_id)
                source = confined_file(ROOT, case["source"])
                input_bytes = case.get("input", "").encode("utf-8")
                input_path = temporary_root / f"{case_id}.input"
                bytecode_path = temporary_root / f"{case_id}.starc"
                input_path.write_bytes(input_bytes)

                build_command = [
                    arguments.star_cli,
                    "build",
                    str(source),
                    "--output",
                    str(bytecode_path),
                ]
                build_started = time.perf_counter()
                build_result = run(build_command)
                build_ms = (time.perf_counter() - build_started) * 1000
                require_success(build_result, build_command)

                source_stats = run_stats(
                    arguments.star_cli, source, input_bytes, memory_bytes, time_limit_ms
                )
                bytecode_stats = run_stats(
                    arguments.star_cli, bytecode_path, input_bytes, memory_bytes, time_limit_ms
                )
                if source_stats["output_sha256"] != bytecode_stats["output_sha256"]:
                    raise BenchmarkError(f"source/VM output differs for case {case_id}")

                source_bench = run_bench(
                    arguments.star_cli,
                    source,
                    input_path,
                    iterations,
                    memory_bytes,
                    time_limit_ms,
                )
                vm_bench = run_bench(
                    arguments.star_cli,
                    bytecode_path,
                    input_path,
                    iterations,
                    memory_bytes,
                    time_limit_ms,
                )
                source_bench["program_sha256"] = hashlib.sha256(source.read_bytes()).hexdigest()
                vm_bench["program_sha256"] = hashlib.sha256(bytecode_path.read_bytes()).hexdigest()
                case_results.append(
                    {
                        "id": case_id,
                        "source_path": source.relative_to(ROOT).as_posix(),
                        "input_bytes": len(input_bytes),
                        "input_sha256": hashlib.sha256(input_bytes).hexdigest(),
                        "build_ms": round(build_ms, 3),
                        "source_interpreter": {**source_stats, **source_bench},
                        "bytecode_vm": {**bytecode_stats, **vm_bench},
                        "vm_to_interpreter_mean_ratio": round(
                            float(vm_bench["mean_process_wall_ms"])
                            / max(float(source_bench["mean_process_wall_ms"]), 0.001),
                            4,
                        ),
                    }
                )

        report = {
            "schema_version": 1,
            "recorded_at_utc": datetime.now(timezone.utc).isoformat(),
            "star_version": star_version,
            "commit": commit,
            "cargo_lock_sha256": hashlib.sha256((ROOT / "Cargo.lock").read_bytes()).hexdigest(),
            "suite_sha256": suite_hash,
            "rustc": rust_version,
            "target_triple": rust_host,
            "host": {
                "os": platform.platform(),
                "system": platform.system(),
                "release": platform.release(),
                "machine": platform.machine(),
                "processor": platform.processor(),
                "logical_cpu_count": os.cpu_count(),
                "runner_os": os.environ.get("RUNNER_OS"),
                "runner_arch": os.environ.get("RUNNER_ARCH"),
                "image_os": os.environ.get("ImageOS"),
                "image_version": os.environ.get("ImageVersion"),
            },
            "method": {
                "baseline": "Star reference interpreter on the same host and inputs",
                "comparison": "Star bytecode VM against the reference interpreter",
                "wall_time": "star bench mean; includes a fresh process start, file load, and input per iteration",
                "guest_memory": "one star run --stats calibration per engine; tracked guest-memory estimate, not process RSS",
                "iterations": iterations,
                "memory_limit_bytes": memory_bytes,
                "time_limit_ms": time_limit_ms,
            },
            "cases": case_results,
        }
        output = arguments.output.resolve()
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(f"Wrote benchmark report: {output}")
        for case in case_results:
            print(
                f"{case['id']}: source={case['source_interpreter']['mean_process_wall_ms']:.3f}ms "
                f"vm={case['bytecode_vm']['mean_process_wall_ms']:.3f}ms "
                f"ratio={case['vm_to_interpreter_mean_ratio']:.4f}"
            )
        return 0
    except (OSError, KeyError, TypeError, ValueError, tomllib.TOMLDecodeError, BenchmarkError) as error:
        print(f"benchmark error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
