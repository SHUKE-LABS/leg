#!/usr/bin/env python3
"""Create a normalized gzip tar archive for a local UI trial bundle."""

from __future__ import annotations

import argparse
import gzip
import tarfile
from pathlib import Path


def add_entry(archive: tarfile.TarFile, source: Path, arcname: str) -> None:
    info = archive.gettarinfo(str(source), arcname=arcname)
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    info.mtime = 0
    info.pax_headers = {}
    if info.isdir():
        archive.addfile(info)
    elif info.isfile():
        with source.open("rb") as file:
            archive.addfile(info, file)
    else:
        raise ValueError(f"bundle contains unsupported file type: {source}")


def create_archive(source_dir: Path, output: Path, archive_root: str) -> None:
    if not source_dir.is_dir():
        raise ValueError(f"bundle directory does not exist: {source_dir}")
    if output.exists():
        raise ValueError(f"archive output already exists: {output}")
    if not archive_root or "/" in archive_root or "\\" in archive_root:
        raise ValueError("archive root must be one directory name")
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("wb") as raw_output:
        with gzip.GzipFile(
            fileobj=raw_output, filename="", mode="wb", compresslevel=9, mtime=0
        ) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                add_entry(archive, source_dir, archive_root)
                for path in sorted(source_dir.rglob("*"), key=lambda item: item.as_posix()):
                    relative = path.relative_to(source_dir).as_posix()
                    add_entry(archive, path, f"{archive_root}/{relative}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("archive_root")
    args = parser.parse_args()
    create_archive(args.source.resolve(), args.output.resolve(), args.archive_root)
    print(f"archive_size_bytes={args.output.stat().st_size}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
