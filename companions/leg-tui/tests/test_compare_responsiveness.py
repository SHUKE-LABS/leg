from __future__ import annotations

import io
import json
import unittest
from argparse import Namespace
from contextlib import redirect_stdout
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

import compare_responsiveness as comparison


class ResponsivenessComparisonTests(unittest.TestCase):
    def run_comparison(
        self,
        host: str,
        baseline_status: str,
        final_status: str,
        behavior_status: str = "passed",
        pull_request_head_revision: str | None = None,
    ) -> tuple[int, dict[str, object]]:
        with TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            args = Namespace(
                host=host,
                output_dir=output_dir,
                baseline_revision="baseline-revision",
                final_revision="final-revision",
                pull_request_head_revision=pull_request_head_revision,
                tui_bin=Path("leg-tui"),
                leg_bin=Path("leg"),
                supervisor_bin=Path("leg-ui-supervisor"),
            )

            def measurement(
                _bins: dict[str, str],
                _revision: str,
                role: str,
                output_path: Path,
                _log_dir: Path,
            ) -> dict[str, object]:
                status = baseline_status if role == "baseline" else final_status
                return {
                    "status": status,
                    "report": output_path.name,
                    "gates": {"status": "passed" if status == "passed" else "failed"},
                }

            with (
                patch.object(comparison, "_git_revision", return_value="final-revision"),
                patch.object(comparison, "invoke", return_value={"status": "passed"}),
                patch.object(comparison, "build_revision", return_value={"status": "passed"}),
                patch.object(comparison, "run_measurement", side_effect=measurement),
                patch.object(
                    comparison,
                    "run_windows_behavior",
                    return_value={"status": behavior_status},
                ),
                redirect_stdout(io.StringIO()),
            ):
                exit_code = comparison.compare(args)

            report = json.loads((output_dir / "comparison.json").read_text(encoding="utf-8"))
            return exit_code, report

    def test_windows_baseline_failure_is_recorded_but_final_gates_still_pass(self) -> None:
        exit_code, report = self.run_comparison("windows", "failed", "passed")

        self.assertEqual(exit_code, 0)
        self.assertEqual(report["status"], "passed_with_windows_baseline_failure")
        self.assertTrue(report["baseline"]["allowed_failure"])
        self.assertTrue(report["baseline"]["exact_failure_recorded"])

    def test_linux_baseline_failure_fails_the_comparison(self) -> None:
        exit_code, report = self.run_comparison("linux", "failed", "passed")

        self.assertEqual(exit_code, 1)
        self.assertEqual(report["status"], "failed")
        self.assertFalse(report["baseline"]["allowed_failure"])
        self.assertTrue(report["baseline"]["exact_failure_recorded"])

    def test_final_failure_fails_even_when_windows_baseline_is_unavailable(self) -> None:
        exit_code, report = self.run_comparison("windows", "failed", "failed")

        self.assertEqual(exit_code, 1)
        self.assertEqual(report["status"], "failed")
        self.assertFalse(report["final"]["measurement"]["status"] == "passed")

    def test_windows_behavior_failure_fails_the_comparison(self) -> None:
        exit_code, report = self.run_comparison("windows", "passed", "passed", "failed")

        self.assertEqual(exit_code, 1)
        self.assertEqual(report["windows_behavior"]["status"], "failed")

    def test_windows_behavior_uses_configured_python(self) -> None:
        with TemporaryDirectory() as temporary:
            output_dir = Path(temporary)
            output_path = output_dir / "windows-behavior.json"

            def invoke(command, _cwd, _env, _log_path, _timeout):
                output_path.write_text(
                    json.dumps({"checks": {}, "status": "passed"}),
                    encoding="utf-8",
                )
                return {"status": "passed"}

            with (
                patch.dict(comparison.os.environ, {"TUI_WINDOWS_BEHAVIOR_PYTHON": "behavior-python"}),
                patch.object(comparison, "invoke", side_effect=invoke) as mocked_invoke,
            ):
                result = comparison.run_windows_behavior(
                    {"tui_bin": "tui.exe", "leg_bin": "leg.exe", "supervisor_bin": "supervisor.exe"},
                    "source-revision",
                    output_path,
                    output_dir,
                )

        self.assertEqual(mocked_invoke.call_args.args[0][0], "behavior-python")
        self.assertEqual(result["status"], "passed")

    def test_comparison_report_records_pull_request_head_revision(self) -> None:
        exit_code, report = self.run_comparison(
            "linux",
            "passed",
            "passed",
            pull_request_head_revision="pr-head-revision",
        )

        self.assertEqual(exit_code, 0)
        self.assertEqual(report["final_revision"], "final-revision")
        self.assertEqual(report["pull_request_head_revision"], "pr-head-revision")


if __name__ == "__main__":
    unittest.main()
