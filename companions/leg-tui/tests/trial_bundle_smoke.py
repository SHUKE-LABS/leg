#!/usr/bin/env python3
"""Validate the files and metadata in an unpacked TUI trial bundle."""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
from pathlib import Path


def smoke_bundle(bundle_dir: Path) -> dict[str, object]:
    bundle_dir = bundle_dir.resolve()
    launcher = bundle_dir / "start-tui.sh"
    if not launcher.is_file() or not os.access(launcher, os.X_OK):
        raise AssertionError("unpacked bundle is missing an executable start-tui.sh")
    for name in ("leg", "leg-ui-supervisor", "leg-tui"):
        binary = bundle_dir / "bin" / name
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise AssertionError(f"unpacked bundle is missing executable bin/{name}")

    required_files = (
        "LICENSE",
        "THIRD_PARTY_NOTICES.txt",
        "QUICKSTART.md",
        "docs/ui-experiments.md",
        "companions/trials/fake_provider.py",
        "companions/trials/results-template.json",
        "companions/trials/score.py",
        "bundle-info.json",
    )
    for relative in required_files:
        if not (bundle_dir / relative).is_file():
            raise AssertionError(f"unpacked bundle is missing {relative}")

    metadata = json.loads((bundle_dir / "bundle-info.json").read_text(encoding="utf-8"))
    if metadata.get("schema") != "leg-tui-trial.bundle/v1":
        raise AssertionError(f"unexpected bundle schema: {metadata.get('schema')!r}")
    if metadata.get("experimental") is not True:
        raise AssertionError("bundle metadata does not mark this interface experimental")
    if metadata.get("platform") not in ("linux", "macos"):
        raise AssertionError(f"unsupported bundle platform: {metadata.get('platform')!r}")
    if metadata.get("architecture") not in ("x86_64", "aarch64"):
        raise AssertionError(f"unsupported bundle architecture: {metadata.get('architecture')!r}")
    for field in ("rust_target", "core_version", "tui_version"):
        if not isinstance(metadata.get(field), str) or not metadata[field]:
            raise AssertionError(f"bundle metadata is missing {field}")
    for field in ("root_lock_sha256", "companion_lock_sha256"):
        if re.fullmatch(r"[0-9a-f]{64}", str(metadata.get(field, ""))) is None:
            raise AssertionError(f"bundle metadata has an invalid {field}")
    revisions = (
        metadata.get("core_revision"),
        metadata.get("tui_revision"),
        metadata.get("web_revision"),
    )
    if any(not isinstance(value, str) or len(value) != 40 for value in revisions):
        raise AssertionError(f"bundle is missing exact core/TUI/Web revisions: {revisions!r}")
    if len(set(revisions)) != 1:
        raise AssertionError(f"single-repository trial revisions disagree: {revisions!r}")

    runtime_path = os.environ.get("PATH", "")
    for command in ("cargo", "node"):
        if shutil.which(command, path=runtime_path) is not None:
            raise AssertionError(f"bundle smoke runtime PATH unexpectedly includes {command}")
    return metadata


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle-dir", type=Path, required=True)
    args = parser.parse_args()
    metadata = smoke_bundle(args.bundle_dir)
    print(
        "unpacked TUI bundle files passed: "
        f"{metadata['platform']}/{metadata['architecture']} "
        f"{metadata['core_revision']} (Cargo/Node absent from runtime PATH)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
