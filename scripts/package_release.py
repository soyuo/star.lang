#!/usr/bin/env python3
"""Create deterministic standalone CLI archives for supported Star targets."""

from __future__ import annotations

import argparse
import gzip
import io
import os
import shutil
import subprocess
import tarfile
import tomllib
import zipfile
from datetime import datetime, timezone
from pathlib import Path


TARGETS = {
    "x86_64-unknown-linux-gnu": "tar.gz",
    "aarch64-unknown-linux-gnu": "tar.gz",
    "x86_64-pc-windows-msvc": "zip",
    "aarch64-pc-windows-msvc": "zip",
    "x86_64-apple-darwin": "tar.gz",
    "aarch64-apple-darwin": "tar.gz",
}


def tracked_readme(root: Path) -> bytes:
    result = subprocess.run(
        ["git", "show", "HEAD:README.md"],
        cwd=root,
        check=True,
        stdout=subprocess.PIPE,
    )
    return result.stdout


def normalized_zip_time(epoch: int) -> tuple[int, int, int, int, int, int]:
    earliest = 315532800  # ZIP timestamps begin at 1980-01-01 UTC.
    latest = 4354819198  # 2107-12-31 23:59:58 UTC.
    value = min(max(epoch, earliest), latest)
    timestamp = datetime.fromtimestamp(value, timezone.utc)
    return (
        timestamp.year,
        timestamp.month,
        timestamp.day,
        timestamp.hour,
        timestamp.minute,
        timestamp.second - timestamp.second % 2,
    )


def add_zip_file(
    archive: zipfile.ZipFile,
    name: str,
    source: Path | io.BytesIO,
    size: int,
    epoch: int,
    executable: bool,
) -> None:
    info = zipfile.ZipInfo(name, normalized_zip_time(epoch))
    info.compress_type = zipfile.ZIP_DEFLATED
    info.create_system = 3
    info.external_attr = (0o100755 if executable else 0o100644) << 16
    info.file_size = size
    with archive.open(info, "w", force_zip64=True) as target:
        if isinstance(source, Path):
            with source.open("rb") as stream:
                shutil.copyfileobj(stream, target)
        else:
            shutil.copyfileobj(source, target)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=sorted(TARGETS))
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--source-date-epoch", required=True, type=int)
    args = parser.parse_args()

    root = Path(__file__).resolve().parents[1]
    binary_name = "star.exe" if "windows" in args.target else "star"
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f"CLI binary does not exist: {binary}")

    metadata = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    version = metadata["workspace"]["package"]["version"]
    if os.environ.get("GITHUB_REF_TYPE") == "tag":
        expected_tag = f"v{version}"
        actual_tag = os.environ.get("GITHUB_REF_NAME", "")
        if actual_tag != expected_tag:
            parser.error(f"release tag {actual_tag!r} must match workspace version {expected_tag!r}")

    suffix = TARGETS[args.target]
    filename = f"star-{args.target}.{suffix}"
    output_dir = args.output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    output = output_dir / filename

    license_path = root / "LICENSE"
    readme = tracked_readme(root)
    members: list[tuple[str, Path | io.BytesIO, int, bool]] = [
        ("LICENSE", license_path, license_path.stat().st_size, False),
        ("README.md", io.BytesIO(readme), len(readme), False),
        (binary_name, binary, binary.stat().st_size, True),
    ]

    if suffix == "zip":
        with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
            for name, source, size, executable in sorted(members, key=lambda item: item[0]):
                add_zip_file(archive, name, source, size, args.source_date_epoch, executable)
    else:
        with output.open("wb") as raw_archive:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw_archive, mtime=args.source_date_epoch) as compressed:
                with tarfile.open(fileobj=compressed, mode="w|") as archive:
                    for name, source, size, executable in sorted(members, key=lambda item: item[0]):
                        entry = tarfile.TarInfo(name)
                        entry.size = size
                        entry.mtime = args.source_date_epoch
                        entry.mode = 0o755 if executable else 0o644
                        entry.uid = 0
                        entry.gid = 0
                        entry.uname = ""
                        entry.gname = ""
                        if isinstance(source, Path):
                            with source.open("rb") as stream:
                                archive.addfile(entry, stream)
                        else:
                            archive.addfile(entry, source)

    print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
