#!/usr/bin/env python3
"""Build and measure baseline/final leg-tui binaries on one native host."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[3]
HARNESS = ROOT / "companions" / "leg-tui" / "tests" / "responsiveness_harness.py"
WINDOWS_BEHAVIOR = ROOT / "companions" / "leg-tui" / "tests" / "windows_behavior_harness.py"
DEFAULT_BASELINE = "b5444f9f409a6edbab7cd02ed776a7df36e6e412"
BUILD_COMMANDS = (
    ("core", "cargo", "build", "--locked", "--release", "--bin", "leg"),
    (
        "supervisor",
        "cargo",
        "build",
        "--locked",
        "--manifest-path",
        "companions/Cargo.toml",
        "--release",
        "-p",
        "leg-ui-client",
        "--bin",
        "leg-ui-supervisor",
    ),
    (
        "tui",
        "cargo",
        "build",
        "--locked",
        "--manifest-path",
        "companions/Cargo.toml",
        "--release",
        "-p",
        "leg-tui",
    ),
)
BUILD_TIMEOUT_SECONDS = 2700
HARNESS_TIMEOUT_SECONDS = 2700


def write_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    temporary.replace(path)


def invoke(
    command: list[str],
    cwd: Path,
    env: dict[str, str],
    log_path: Path,
    timeout_seconds: int,
) -> dict[str, Any]:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    timed_out = False
    exit_code: int | None
    try:
        with log_path.open("wb") as log_file:
            completed = subprocess.run(
                command,
                cwd=cwd,
                env=env,
                stdout=log_file,
                stderr=subprocess.STDOUT,
                timeout=timeout_seconds,
                check=False,
            )
        exit_code = completed.returncode
    except subprocess.TimeoutExpired:
        timed_out = True
        exit_code = None
    except OSError as error:
        log_path.write_text(f"could not start command: {error}\n", encoding="utf-8")
        exit_code = None

    try:
        log_tail = log_path.read_text(encoding="utf-8", errors="replace")[-12000:]
    except OSError:
        log_tail = ""
    return {
        "command": command,
        "cwd": str(cwd),
        "status": "timeout" if timed_out else ("passed" if exit_code == 0 else "failed"),
        "exit_code": exit_code,
        "timeout_seconds": timeout_seconds,
        "log": log_path.name,
        "log_tail": log_tail,
    }


def binary_paths(target_dir: Path) -> tuple[Path, Path, Path]:
    suffix = ".exe" if os.name == "nt" else ""
    return (
        target_dir / "release" / f"leg-tui{suffix}",
        target_dir / "release" / f"leg{suffix}",
        target_dir / "release" / f"leg-ui-supervisor{suffix}",
    )


def build_revision(
    source_root: Path,
    target_dir: Path,
    output_dir: Path,
    role: str,
) -> dict[str, Any]:
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir.resolve())
    commands: list[dict[str, Any]] = []
    for name, *arguments in BUILD_COMMANDS:
        result = invoke(
            arguments,
            source_root,
            env,
            output_dir / f"{role}-build-{name}.log",
            BUILD_TIMEOUT_SECONDS,
        )
        commands.append(result)
        if result["status"] != "passed":
            return {"status": "failed", "commands": commands}
    tui_bin, leg_bin, supervisor_bin = binary_paths(target_dir)
    missing = [str(path) for path in (tui_bin, leg_bin, supervisor_bin) if not path.is_file()]
    if missing:
        return {"status": "failed", "commands": commands, "missing_binaries": missing}
    return {
        "status": "passed",
        "commands": commands,
        "tui_bin": str(tui_bin),
        "leg_bin": str(leg_bin),
        "supervisor_bin": str(supervisor_bin),
    }


def run_measurement(
    bins: dict[str, str],
    source_revision: str,
    role: str,
    output_path: Path,
    output_dir: Path,
) -> dict[str, Any]:
    command = [
        sys.executable,
        str(HARNESS),
        "--tui-bin",
        bins["tui_bin"],
        "--leg-bin",
        bins["leg_bin"],
        "--supervisor-bin",
        bins["supervisor_bin"],
        "--output",
        str(output_path),
        "--build-profile",
        "release",
        "--source-revision",
        source_revision,
        "--comparison-role",
        role,
    ]
    result = invoke(
        command,
        ROOT,
        os.environ.copy(),
        output_dir / f"{role}-measurement.log",
        HARNESS_TIMEOUT_SECONDS,
    )
    report: dict[str, Any] | None = None
    if output_path.is_file():
        try:
            report = json.loads(output_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            report = None
    result["report"] = output_path.name if report is not None else None
    result["gates"] = report.get("gates") if report else None
    if role == "final" and report and report.get("gates", {}).get("status") != "passed":
        result["status"] = "failed"
        result["gate_failure"] = report.get("gates")
    return result


def run_windows_behavior(
    bins: dict[str, str],
    source_revision: str,
    output_path: Path,
    output_dir: Path,
) -> dict[str, Any]:
    command = [
        sys.executable,
        str(WINDOWS_BEHAVIOR),
        "--tui-bin",
        bins["tui_bin"],
        "--leg-bin",
        bins["leg_bin"],
        "--supervisor-bin",
        bins["supervisor_bin"],
        "--source-revision",
        source_revision,
        "--output",
        str(output_path),
    ]
    result = invoke(
        command,
        ROOT,
        os.environ.copy(),
        output_dir / "windows-behavior.log",
        HARNESS_TIMEOUT_SECONDS,
    )
    report: dict[str, Any] | None = None
    if output_path.is_file():
        try:
            report = json.loads(output_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            report = None
    result["report"] = output_path.name if report is not None else None
    result["checks"] = report.get("checks") if report else None
    return result


def _git_revision() -> str:
    completed = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return completed.stdout.strip()


def compare(args: argparse.Namespace) -> int:
    output_dir = args.output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    final_revision = args.final_revision or _git_revision()
    comparison: dict[str, Any] = {
        "schema": "leg-tui.responsiveness-comparison/v1",
        "host": args.host,
        "baseline_revision": args.baseline_revision,
        "final_revision": final_revision,
        "terminal": {
            "backend": "Windows ConPTY via pywinpty" if args.host == "windows" else "native Linux PTY",
            "frontend": "none; pywinpty drove the ConPTY API directly" if args.host == "windows" else "none; harness drove the native PTY directly",
        },
        "baseline": {},
        "final": {},
    }
    failed = False

    with tempfile.TemporaryDirectory(prefix="leg-tui-183-baseline-") as temporary:
        temp_root = Path(temporary)
        baseline_source = temp_root / "baseline-source"
        baseline_target = temp_root / "baseline-target"
        worktree_result = invoke(
            ["git", "worktree", "add", "--detach", str(baseline_source), args.baseline_revision],
            ROOT,
            os.environ.copy(),
            output_dir / "baseline-worktree.log",
            300,
        )
        baseline_status = {"worktree": worktree_result}
        if worktree_result["status"] == "passed":
            built = build_revision(baseline_source, baseline_target, output_dir, "baseline")
            baseline_status["build"] = built
            if built["status"] == "passed":
                measured = run_measurement(
                    built,
                    args.baseline_revision,
                    "baseline",
                    output_dir / "responsiveness-baseline.json",
                    output_dir,
                )
                baseline_status["measurement"] = measured
                baseline_status["status"] = measured["status"]
            else:
                baseline_status["status"] = "failed"
        else:
            baseline_status["status"] = "failed"
        if baseline_status["status"] != "passed":
            baseline_status["allowed_failure"] = args.host == "windows"
            if args.host != "windows":
                failed = True
        comparison["baseline"] = baseline_status

        if worktree_result["status"] == "passed":
            remove_result = invoke(
                ["git", "worktree", "remove", "--force", str(baseline_source)],
                ROOT,
                os.environ.copy(),
                output_dir / "baseline-worktree-remove.log",
                300,
            )
            baseline_status["worktree_remove"] = remove_result

    final_bins = {
        "tui_bin": str(args.tui_bin.resolve()),
        "leg_bin": str(args.leg_bin.resolve()),
        "supervisor_bin": str(args.supervisor_bin.resolve()),
    }
    final_result = run_measurement(
        final_bins,
        final_revision,
        "final",
        output_dir / "responsiveness-final.json",
        output_dir,
    )
    comparison["final"]["measurement"] = final_result
    if final_result["status"] != "passed":
        failed = True

    if args.host == "windows":
        behavior = run_windows_behavior(
            final_bins,
            final_revision,
            output_dir / "windows-behavior.json",
            output_dir,
        )
        comparison["windows_behavior"] = behavior
        if behavior["status"] != "passed":
            failed = True

    baseline_failed = comparison["baseline"]["status"] != "passed"
    comparison["status"] = (
        "failed"
        if failed
        else "passed_with_windows_baseline_failure"
        if baseline_failed
        else "passed"
    )
    comparison["baseline"]["exact_failure_recorded"] = baseline_failed and (
        args.host == "windows" or failed
    )
    write_json(output_dir / "comparison.json", comparison)
    print(f"comparison_report={output_dir / 'comparison.json'}")
    print(json.dumps(comparison, ensure_ascii=True, indent=2))
    return 1 if failed else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", choices=("windows", "linux"), required=True)
    parser.add_argument("--baseline-revision", default=DEFAULT_BASELINE)
    parser.add_argument("--final-revision")
    parser.add_argument("--tui-bin", required=True, type=Path)
    parser.add_argument("--leg-bin", required=True, type=Path)
    parser.add_argument("--supervisor-bin", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    args = parser.parse_args()
    for binary in (args.tui_bin, args.leg_bin, args.supervisor_bin):
        if not binary.is_file():
            parser.error(f"final binary does not exist: {binary}")
    if not HARNESS.is_file():
        parser.error(f"responsiveness harness is missing: {HARNESS}")
    if args.host == "windows" and not WINDOWS_BEHAVIOR.is_file():
        parser.error(f"Windows behavior harness is missing: {WINDOWS_BEHAVIOR}")
    return compare(args)


if __name__ == "__main__":
    raise SystemExit(main())
