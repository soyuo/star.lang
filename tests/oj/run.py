#!/usr/bin/env python3
"""Compare paired Star source/bytecode solutions against the OJ fixture pack."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from pathlib import Path


STATS = re.compile(
    rb"star: execution_ms=([0-9.]+) "
    rb"peak_guest_memory_estimate_bytes=(\d+) executed_steps=(\d+)"
)


def confined_file(root: Path, relative: str) -> Path:
    path = (root / relative).resolve()
    if path != root and root not in path.parents:
        raise ValueError(f"manifest path escapes suite root: {relative}")
    if not path.is_file():
        raise ValueError(f"manifest file does not exist: {relative}")
    return path


def run_engine(
    cli: str,
    program: Path,
    input_bytes: bytes,
    memory_bytes: int,
    time_limit_ms: int,
) -> subprocess.CompletedProcess[bytes] | None:
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
    try:
        return subprocess.run(
            command,
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=(time_limit_ms / 1000) + 5,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return None
    except OSError as error:
        message = f"could not start Star CLI '{cli}': {error}".encode("utf-8")
        return subprocess.CompletedProcess(command, 127, b"", message)


def stats_text(result: subprocess.CompletedProcess[bytes] | None) -> str:
    if result is None:
        return "timeout"
    match = STATS.search(result.stderr)
    if not match:
        return "no-stats"
    elapsed, memory, steps = (value.decode("ascii") for value in match.groups())
    return f"{elapsed}ms/{memory}B/{steps} steps"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--solutions", type=Path, required=True)
    parser.add_argument("--star-cli", default="star")
    arguments = parser.parse_args()

    root = Path(__file__).resolve().parent
    solution_root = arguments.solutions.resolve()
    try:
        manifest = tomllib.loads((root / "manifest.toml").read_text(encoding="utf-8"))
        memory_bytes = int(manifest["memory_limit_bytes"])
        time_limit_ms = int(manifest["time_limit_ms"])
        expected_exit = int(manifest["expected_exit_code"])
        cases = manifest["case"]
        empty_case = manifest["empty_input"]
        if not cases:
            raise ValueError("manifest contains no cases")
        for case in cases:
            confined_file(root, case["input"])
            confined_file(root, case["expected_output"])
        confined_file(root, empty_case["input"])
        confined_file(root, empty_case["expected_output"])
        pairs = {
            case["id"]: (
                confined_file(solution_root, f"{case['id']}.star"),
                confined_file(solution_root, f"{case['id']}.starc"),
            )
            for case in cases
        }
    except (OSError, KeyError, TypeError, ValueError, tomllib.TOMLDecodeError) as error:
        print(f"OJ suite configuration error: {error}", file=sys.stderr)
        return 2

    failed = False
    empty_input = (root / empty_case["input"]).read_bytes()
    empty_expected = (root / empty_case["expected_output"]).read_bytes()
    for case in cases:
        case_input = (root / case["input"]).read_bytes()
        expected = (root / case["expected_output"]).read_bytes()
        source_path, bytecode_path = pairs[case["id"]]
        source = run_engine(
            arguments.star_cli, source_path, case_input, memory_bytes, time_limit_ms
        )
        bytecode = run_engine(
            arguments.star_cli, bytecode_path, case_input, memory_bytes, time_limit_ms
        )
        source_empty = run_engine(
            arguments.star_cli, source_path, empty_input, memory_bytes, time_limit_ms
        )
        bytecode_empty = run_engine(
            arguments.star_cli, bytecode_path, empty_input, memory_bytes, time_limit_ms
        )
        outcomes = (source, bytecode, source_empty, bytecode_empty)
        passed = all(result is not None for result in outcomes)
        if passed:
            passed = all(
                result.returncode == expected_exit and result.stdout == expected
                for result in (source, bytecode)
            )
            passed = passed and all(
                result.returncode == int(empty_case["expected_exit_code"])
                and result.stdout == empty_expected
                for result in (source_empty, bytecode_empty)
            )
            passed = passed and source.returncode == bytecode.returncode
            passed = passed and source.stdout == bytecode.stdout
            passed = passed and source_empty.returncode == bytecode_empty.returncode
            passed = passed and source_empty.stdout == bytecode_empty.stdout
        status = "PASS" if passed else "FAIL"
        print(
            f"{status} {case['id']} "
            f"source={source.returncode if source else 'timeout'} ({stats_text(source)}) "
            f"vm={bytecode.returncode if bytecode else 'timeout'} ({stats_text(bytecode)}); "
            f"empty-source={source_empty.returncode if source_empty else 'timeout'} "
            f"({stats_text(source_empty)}), empty-vm="
            f"{bytecode_empty.returncode if bytecode_empty else 'timeout'} "
            f"({stats_text(bytecode_empty)})"
        )
        if not passed:
            failed = True
            for engine, result in (
                ("source", source),
                ("vm", bytecode),
                ("empty-source", source_empty),
                ("empty-vm", bytecode_empty),
            ):
                if result is not None and result.stderr:
                    details = result.stderr.decode("utf-8", "replace")
                    print(f"  {engine} stderr: {details}", file=sys.stderr)
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
