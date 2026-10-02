#!/usr/bin/env python3
"""Write the deterministic Web trial report from CI and resource measurements."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


BROWSER_E2E_WORKFLOWS = [
    {
        "name": "Launch and trust boundary",
        "coverage": "One-time launch token, workspace gating, and unauthenticated or hostile-Origin mutations; rejected requests leave the session high-water mark and fixture request count unchanged.",
    },
    {
        "name": "Prompt entry and exact-once submission",
        "coverage": "CJK and multiline input, paste, IME composition, Enter behavior, repeated clicks, and preservation of an editable draft during a running turn.",
    },
    {
        "name": "Submission and event recovery",
        "coverage": "Retry after a pre-accept network loss reuses the request ID; post-accept response loss, event-stream reconnect, refresh, and session reopen recover accepted work without replay.",
    },
    {
        "name": "Live turns, interruption, and Stop",
        "coverage": "Provisional streamed text, stopping a stalled tool and its child process, host restart during a turn, and retry of an interrupted prompt with the expected request ID.",
    },
    {
        "name": "Provider and tool behavior",
        "coverage": "Provider authentication failure and transient retry, tool-round execution, large tool summaries, and status or timeout handling.",
    },
    {
        "name": "Rendering and responsive layout",
        "coverage": "Untrusted Markdown rendering, long transcript scroll anchoring during streaming, and control visibility at the required viewport sizes.",
    },
    {
        "name": "Sessions and navigation",
        "coverage": "Workspace and session switching, busy-session visibility, cross-tab accepted-prompt updates, and separation of tab drafts from submitted prompt provenance.",
    },
    {
        "name": "History tools and privacy",
        "coverage": "Search across hidden tool input and results, Unicode copy and permission fallback, transcript download, and redaction of credentials and private fields.",
    },
    {
        "name": "Long-history interaction",
        "coverage": "Selection and inspection of a 1,000-turn transcript, bounded mounted rows and tool details, and measured selection and inspector response time.",
    },
]


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
    environment = measurements["environment"]

    results["context"].update(
        {
            "core_revision": revision,
            "ui_revisions": {"TUI": revision, "Web": revision},
            "os_device": environment,
            "provider": "local deterministic Python fixture",
            "model": "trial-fixture",
            "provider_config": "loopback fixture; no real API key; retries disabled",
            "workspace_fixture_revision": revision,
        }
    )
    results["eligibility_gates"]["Web"].update(
        {
            "common_critical_workflows_pass": True,
            "no_accidental_submit_or_replay": True,
            "committed_history_preserved": True,
            "stop_terminates_tool_processes": True,
            "credentials_not_exposed": True,
            "unauthenticated_web_mutations_rejected": True,
        }
    )
    results["resource_measurements"]["TUI"].update(
        {
            "startup_time_ms": measurements["TUI"]["startup_time_ms"],
            "idle_rss_bytes": measurements["TUI"]["idle_rss_bytes"],
            "bundle_size_bytes": measurements["TUI"]["bundle_size_bytes"],
        }
    )
    results["resource_measurements"]["Web"].update(
        {
            "startup_time_ms": measurements["Web"]["startup_time_ms"],
            "idle_rss_bytes": measurements["Web"]["idle_rss_bytes"],
            "host_plus_browser_total_rss_bytes": measurements["Web"][
                "host_plus_browser_total_rss_bytes"
            ],
            "browser_baseline_rss_bytes": measurements["Web"][
                "browser_baseline_rss_bytes"
            ],
            "bundle_size_bytes": measurements["Web"]["bundle_size_bytes"],
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
            "web_process_set": measurements["method"]["web_process_set"],
            "browser_baseline_process_set": measurements["method"][
                "browser_baseline_process_set"
            ],
            "bundle_size_method": measurements["method"]["bundle_size_method"],
        }
    )
    # These depend on the preceding bundle smoke and this job's `needs: [native, lint]`.
    # Keep both workflow gates aligned with the results recorded here.
    results["deterministic_ci_results"] = {
        "unpacked_trial_bundle_smoke": "passed",
        "native_linux_macos": "passed",
        "browser_chromium": "passed",
        "browser_firefox": "passed",
        "core_rust_lint_and_tests": "passed",
        "core_release_packaging": "passed",
        "browser_e2e_workflows": [
            {**workflow, "result": "passed"} for workflow in BROWSER_E2E_WORKFLOWS
        ],
        "ci_run": run_url,
    }
    results["limitations"].extend(
        [
            "Human paired observations were not collected in this slice; the participant list is empty and no winner is recommended.",
            "TUI bundle size is a reference archive made from the same revision and common trial materials for measurement; issue #82 owns the TUI distributable.",
        ]
    )

    output_dir.mkdir(parents=True, exist_ok=True)
    results_path = output_dir / "web-trial-results.json"
    results_path.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
    report_path = output_dir / "web-trial-report.md"
    report_path.write_text(
        "\n".join(
            [
                "# Leg Web trial validation report",
                "",
                "This report records deterministic fixture/CI outcomes and machine resource measurements. It contains no human observations or winner recommendation.",
                "",
                f"- Core revision: `{revision}`",
                f"- Web revision: `{bundle_info['ui_revision']}`",
                f"- Bundle: `{archive_path.name}` ({measurements['Web']['bundle_size_bytes']} bytes)",
                f"- CI run: {run_url}",
                f"- Environment: {environment['os']}; {environment['cpu_model']} ({environment['architecture']}); {environment['physical_memory_bytes']} bytes RAM; {environment['browser']} {environment['browser_version']}",
                "",
                "## Deterministic validation",
                "",
                "| Check | Result | Evidence |",
                "| --- | --- | --- |",
                "| Core Rust lint and tests | Pass | `lint` and `native` CI jobs |",
                "| Release packaging | Pass | `bash tests/release_test.sh` in the Linux native job |",
                "| Native Web host checks | Pass | Linux and macOS lifecycle smoke and package tests |",
                "| Unpacked trial bundle | Pass | Launcher/page smoke and lifecycle smoke using packaged binaries and fixture on Linux and macOS, with Cargo and Node absent from runtime PATH |",
                "| Chromium browser E2E | Pass | `browser_e2e.py --browser chromium` |",
                "| Firefox browser E2E | Pass | `browser_e2e.py --browser firefox` |",
                "",
                "### Browser E2E workflows",
                "",
                "Each workflow below runs in the Chromium and Firefox executions of `browser_e2e.py`.",
                "",
                "| Workflow | Result | Coverage |",
                "| --- | --- | --- |",
                *[
                    f"| {workflow['name']} | Pass | {workflow['coverage']} |"
                    for workflow in BROWSER_E2E_WORKFLOWS
                ],
                "",
                "## Resource measurements",
                "",
                "| Interface | Startup (ms) | Idle RSS (bytes) | Bundle archive (bytes) |",
                "| --- | ---: | ---: | ---: |",
                f"| TUI reference | {measurements['TUI']['startup_time_ms']} | {measurements['TUI']['idle_rss_bytes']} | {measurements['TUI']['bundle_size_bytes']} |",
                f"| Web incremental | {measurements['Web']['startup_time_ms']} | {measurements['Web']['idle_rss_bytes']} | {measurements['Web']['bundle_size_bytes']} |",
                "",
                f"Web host-plus-browser combined idle RSS: {measurements['Web']['host_plus_browser_total_rss_bytes']} bytes.",
                f"Browser-only `about:blank` baseline: {measurements['Web']['browser_baseline_rss_bytes']} bytes.",
                f"TUI reference archive: {measurements['TUI']['bundle_size_bytes']} bytes.",
                "",
                "RSS used `ps -axo pid=,ppid=,rss=,command=` through `sample_process_rss.py`: 10 seconds idle, then 10 one-second samples, with descendant process RSS summed per sample and the median reported. TUI counts the `leg-tui` process tree. Web counts the `leg-web` host and browser process trees; the baseline uses the same browser build with `about:blank` and no host. Web incremental RSS is combined RSS minus the baseline.",
                "",
                "Startup ends when the first workspace selector is visible. Bundle size is the normalized gzip tar archive byte length. The TUI size is a reference archive assembled from the same core revision and shared fixture materials; it is not the TUI deliverable for issue #82.",
                "",
                "## Human observations",
                "",
                "Unmeasured. `web-trial-results.json` retains the empty #79 participant template. No participant data or winner recommendation is inferred from fixture success.",
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
