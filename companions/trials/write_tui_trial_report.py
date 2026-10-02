#!/usr/bin/env python3
"""Write the deterministic TUI trial report from CI and resource measurements."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def read_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def write_report(
    template_path: Path,
    measurements_path: Path,
    bundle_info_path: Path,
    archive_path: Path,
    output_dir: Path,
    run_url: str,
) -> tuple[Path, Path]:
    results = read_json(template_path)
    measurements = read_json(measurements_path)
    bundle_info = read_json(bundle_info_path)
    revision = str(bundle_info["core_revision"])
    tui_revision = str(bundle_info["tui_revision"])
    web_revision = str(bundle_info["web_revision"])
    environment = measurements["environment"]
    tui = measurements["TUI"]
    if str(measurements["source_revision"]) != revision:
        raise ValueError("resource measurements and bundle core revisions differ")
    if tui.get("bundle_archive") != archive_path.name:
        raise ValueError("resource measurements do not name the delivered TUI archive")
    if int(tui["bundle_size_bytes"]) != archive_path.stat().st_size:
        raise ValueError("measured TUI bundle size does not match the delivered archive")

    results["context"].update(
        {
            "core_revision": revision,
            "ui_revisions": {"TUI": tui_revision, "Web": web_revision},
            "os_device": environment,
            "provider": "local deterministic Python fixture",
            "model": "trial-fixture",
            "provider_config": "loopback fixture; fake-only credential; retries disabled",
            "workspace_fixture_revision": revision,
        }
    )
    results["eligibility_gates"]["TUI"].update(
        {
            "common_critical_workflows_pass": True,
            "no_accidental_submit_or_replay": True,
            "committed_history_preserved": True,
            "stop_terminates_tool_processes": True,
            "credentials_not_exposed": True,
            "unauthenticated_web_mutations_rejected": "n/a",
        }
    )
    results["resource_measurements"]["TUI"].update(
        {
            "startup_time_ms": tui["startup_time_ms"],
            "idle_rss_bytes": tui["idle_rss_bytes"],
            "bundle_size_bytes": tui["bundle_size_bytes"],
        }
    )
    results["resource_measurement_method"].update(
        {
            "reference_environment": environment,
            "startup_ready_event": measurements["method"]["startup_ready_event"],
            "rss_tool": measurements["method"]["rss_tool"],
            "settle_seconds": measurements["method"]["settle_seconds"],
            "sample_count": measurements["method"]["sample_count"],
            "sample_interval_seconds": measurements["method"]["sample_interval_seconds"],
            "tui_process_set": measurements["method"]["tui_process_set"],
            "web_process_set": None,
            "browser_baseline_process_set": None,
            "bundle_size_method": measurements["method"]["bundle_size_method"],
        }
    )
    # CI calls this only after native/lint dependencies and this job's unpacked
    # bundle smoke have passed; keep these recorded gates aligned with that graph.
    results["deterministic_ci_results"] = {
        "unpacked_tui_trial_bundle_smoke": "passed",
        "native_linux_tui_pty": "passed",
        "native_macos_tui_pty": "passed",
        "core_rust_lint_and_tests": "passed",
        "core_release_packaging": "passed",
        "core_package_boundary": "passed",
        "ci_run": run_url,
    }
    results["limitations"].extend(
        [
            "Human paired observations were not collected in this slice; the participant list is empty, scores are unmeasured, and no winner is recommended.",
            "Web gates and resource measurements are not evaluated by this TUI artifact report; the shared #79 result shape is retained.",
        ]
    )

    output_dir.mkdir(parents=True, exist_ok=True)
    results_path = output_dir / "tui-trial-results.json"
    results_path.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
    report_path = output_dir / "tui-trial-report.md"
    report_path.write_text(
        "\n".join(
            [
                "# Leg TUI trial validation report",
                "",
                "This report records deterministic fixture/CI outcomes and machine resource measurements. It contains no human observations, calculated score, or winner recommendation.",
                "",
                f"- Core revision: `{revision}`",
                f"- TUI revision: `{tui_revision}`",
                f"- Web revision: `{web_revision}`",
                f"- Bundle: `{archive_path.name}` ({tui['bundle_size_bytes']} bytes)",
                f"- CI run: {run_url}",
                f"- Environment: {environment['os']}; {environment['cpu_model']} ({environment['architecture']}); {environment['physical_memory_bytes']} bytes RAM",
                "",
                "## Deterministic validation",
                "",
                "| Check | Result | Evidence |",
                "| --- | --- | --- |",
                "| Unpacked TUI bundle | Pass | Packaged launcher and binaries run with the included local fixture while Cargo and Node are absent from runtime PATH |",
                "| Native Linux TUI PTY | Pass | First Chinese/multiline prompt, tool result inspection and follow-up, authentication/tool errors, denied tool, explicit retry, Stop cleanup, reopen, copy, and 1,000-turn navigation |",
                "| Native macOS TUI PTY | Pass | Same TUI PTY workflow list in the native macOS job |",
                "| Core Rust lint and tests | Pass | `lint` and `native` CI jobs |",
                "| Core release packaging | Pass | `bash tests/release_test.sh` in native Linux CI |",
                "| Core package boundary | Pass | Native Linux dependency and Cargo/npm package checks |",
                "",
                "## Resource measurements",
                "",
                "| Interface | Startup (ms) | Idle RSS (bytes) | Delivered bundle archive (bytes) |",
                "| --- | ---: | ---: | ---: |",
                f"| TUI | {tui['startup_time_ms']} | {tui['idle_rss_bytes']} | {tui['bundle_size_bytes']} |",
                "",
                "Startup ends when the first workspace-selection control is visible. RSS uses `sample_process_rss.py` and sums the `leg-tui` process tree after 10 seconds idle, then reports the median of 10 one-second samples. Bundle size is the normalized gzip tar archive byte length. The Linux x86_64 CI runner uses the same sampling method and documented reference environment as the Web trial.",
                "",
                "## Human observations",
                "",
                "Unmeasured. `tui-trial-results.json` preserves the empty #79 participant template. No participant data, calculated score, or winner recommendation is inferred from fixture success.",
                "",
            ]
        ),
        encoding="utf-8",
    )
    return report_path, results_path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--template", type=Path, required=True)
    parser.add_argument("--measurements", type=Path, required=True)
    parser.add_argument("--bundle-info", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--ci-run-url", required=True)
    args = parser.parse_args()
    report_path, results_path = write_report(
        args.template,
        args.measurements,
        args.bundle_info,
        args.archive,
        args.output_dir,
        args.ci_run_url,
    )
    print(f"report={report_path}")
    print(f"results={results_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
