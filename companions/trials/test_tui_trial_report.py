"""Checks that the generated TUI report records evidence without inventing human data."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from companions.trials.write_tui_trial_report import write_report


ROOT = Path(__file__).resolve().parents[2]


class WriteTuiTrialReportTests(unittest.TestCase):
    def test_report_keeps_exact_revisions_and_unmeasured_human_template(self) -> None:
        environment = {
            "os": "Linux test",
            "cpu_model": "Test CPU",
            "architecture": "x86_64",
            "physical_memory_bytes": 123456,
        }
        measurements = {
            "environment": environment,
            "method": {
                "startup_ready_event": "workspace chooser visible",
                "rss_tool": "sample_process_rss.py via ps process trees",
                "settle_seconds": 10,
                "sample_count": 10,
                "sample_interval_seconds": 1,
                "tui_process_set": "leg-tui process tree",
                "bundle_size_method": "normalized .tar.gz archive bytes",
            },
            "source_revision": "a" * 40,
            "TUI": {
                "startup_time_ms": 101,
                "idle_rss_bytes": 202,
                "bundle_size_bytes": 303,
                "bundle_archive": "leg-tui-experimental-linux-x86_64-test.tar.gz",
            },
        }
        revision = "a" * 40
        tui_revision = "b" * 40
        web_revision = "c" * 40

        with tempfile.TemporaryDirectory(prefix="tui-trial-report-test-") as directory:
            temporary = Path(directory)
            measurements_path = temporary / "measurements.json"
            bundle_info_path = temporary / "bundle-info.json"
            archive_path = temporary / "leg-tui-experimental-linux-x86_64-test.tar.gz"
            measurements_path.write_text(json.dumps(measurements), encoding="utf-8")
            bundle_info_path.write_text(
                json.dumps(
                    {
                        "core_revision": revision,
                        "tui_revision": tui_revision,
                        "web_revision": web_revision,
                    }
                ),
                encoding="utf-8",
            )
            archive_path.write_bytes(b"x" * 303)
            report_path, results_path = write_report(
                ROOT / "companions/trials/results-template.json",
                measurements_path,
                bundle_info_path,
                archive_path,
                temporary / "out",
                "https://example.invalid/run/82",
            )

            report = report_path.read_text(encoding="utf-8")
            results = json.loads(results_path.read_text(encoding="utf-8"))

        self.assertIn(f"Core revision: `{revision}`", report)
        self.assertIn(f"TUI revision: `{tui_revision}`", report)
        self.assertIn(f"Web revision: `{web_revision}`", report)
        self.assertIn("| TUI | 101 | 202 | 303 |", report)
        self.assertIn("https://example.invalid/run/82", report)
        self.assertIn("| Native Linux TUI PTY | Pass |", report)
        self.assertIn("| Native macOS TUI PTY | Pass |", report)
        self.assertEqual(
            results["context"]["ui_revisions"],
            {"TUI": tui_revision, "Web": web_revision},
        )
        self.assertEqual(results["participants"], [])
        self.assertIsNone(results["calculated_scores"])
        self.assertEqual(results["eligibility_gates"]["TUI"]["credentials_not_exposed"], True)
        self.assertIsNone(results["eligibility_gates"]["Web"]["common_critical_workflows_pass"])
        self.assertEqual(
            results["deterministic_ci_results"]["unpacked_tui_trial_bundle_smoke"],
            "passed",
        )
        self.assertEqual(results["deterministic_ci_results"]["ci_run"], "https://example.invalid/run/82")
        self.assertEqual(results["resource_measurements"]["TUI"]["bundle_size_bytes"], 303)
        self.assertIsNone(results["resource_measurements"]["Web"]["bundle_size_bytes"])


if __name__ == "__main__":
    unittest.main()
