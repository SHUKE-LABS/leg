"""Checks that the generated Web trial report preserves required evidence."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from companions.trials.write_web_trial_report import BROWSER_E2E_WORKFLOWS, write_report


ROOT = Path(__file__).resolve().parents[2]


class WriteWebTrialReportTests(unittest.TestCase):
    def test_report_records_resources_and_each_browser_workflow(self) -> None:
        environment = {
            "os": "Linux test",
            "cpu_model": "Test CPU",
            "architecture": "x86_64",
            "physical_memory_bytes": 123456,
            "browser": "chromium",
            "browser_version": "1.2.3",
        }
        measurements = {
            "environment": environment,
            "method": {
                "startup_ready_event": "workspace control visible",
                "rss_tool": "sample_process_rss.py via ps",
                "settle_seconds": 10,
                "sample_count": 10,
                "sample_interval_seconds": 1,
                "tui_process_set": "leg-tui tree",
                "web_process_set": "host and browser trees",
                "browser_baseline_process_set": "about:blank browser tree",
                "bundle_size_method": "compressed archive bytes",
            },
            "TUI": {"startup_time_ms": 101, "idle_rss_bytes": 202, "bundle_size_bytes": 303},
            "Web": {
                "startup_time_ms": 404,
                "idle_rss_bytes": 505,
                "host_plus_browser_total_rss_bytes": 606,
                "browser_baseline_rss_bytes": 101,
                "bundle_size_bytes": 707,
            },
        }

        with tempfile.TemporaryDirectory(prefix="web-trial-report-test-") as directory:
            temporary = Path(directory)
            measurements_path = temporary / "measurements.json"
            bundle_info_path = temporary / "bundle-info.json"
            archive_path = temporary / "web-bundle.tar.gz"
            measurements_path.write_text(json.dumps(measurements), encoding="utf-8")
            bundle_info_path.write_text(
                json.dumps({"core_revision": "core-sha", "ui_revision": "web-sha"}),
                encoding="utf-8",
            )
            archive_path.touch()
            report_path, results_path = write_report(
                ROOT / "companions/trials/results-template.json",
                measurements_path,
                bundle_info_path,
                archive_path,
                temporary / "out",
                "https://example.invalid/run/1",
            )

            report = report_path.read_text(encoding="utf-8")
            results = json.loads(results_path.read_text(encoding="utf-8"))

        for value in ("101", "202", "303", "404", "505", "606", "101", "707"):
            self.assertIn(value, report)
        self.assertIn("Browser-only `about:blank` baseline: 101 bytes.", report)
        self.assertIn("10 seconds idle, then 10 one-second samples", report)
        for workflow in BROWSER_E2E_WORKFLOWS:
            self.assertIn(workflow["name"], report)
            self.assertIn(workflow["coverage"], report)
        recorded = results["deterministic_ci_results"]["browser_e2e_workflows"]
        self.assertEqual(len(recorded), len(BROWSER_E2E_WORKFLOWS))
        self.assertTrue(all(item["result"] == "passed" for item in recorded))
        self.assertEqual(
            results["resource_measurements"]["Web"]["host_plus_browser_total_rss_bytes"],
            606,
        )


if __name__ == "__main__":
    unittest.main()
