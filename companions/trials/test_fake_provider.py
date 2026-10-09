"""Focused standard-library checks for the matched UI trial fixture."""

from __future__ import annotations

import json
import tempfile
import threading
import unittest
from contextlib import contextmanager
from pathlib import Path
from typing import Iterator
from urllib.request import Request, urlopen

from companions.trials.fake_provider import (
    Fixture,
    Handler,
    LoopbackThreadingHTTPServer,
    TRIAL_LINES,
    environment,
)


@contextmanager
def running_fixture(fixture: Fixture) -> Iterator[str]:
    handler_type = type("TestFixtureHandler", (Handler,), {"fixture": fixture})
    server = LoopbackThreadingHTTPServer(("127.0.0.1", 0), handler_type)
    server.daemon_threads = True
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        host, port = server.server_address[:2]
        yield f"http://{host}:{port}"
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=5)


def post_message(base_url: str, messages: list[dict[str, object]]) -> dict[str, object]:
    body = json.dumps({"stream": False, "messages": messages}).encode("utf-8")
    request = Request(
        f"{base_url}/v1/messages",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urlopen(request, timeout=5) as response:
        return json.load(response)


class TrialFixtureChecks(unittest.TestCase):
    def test_timeout_environment_is_limited_to_manual_trial_contract(self) -> None:
        trial = environment("http://127.0.0.1:1234", "trial", None)
        self.assertEqual(trial["LEG_BASH_TIMEOUT_SECS"], "600")
        for scenario in ("browser", "stalled-bash"):
            with self.subTest(scenario=scenario):
                self.assertEqual(
                    environment("http://127.0.0.1:1234", scenario, None)["LEG_BASH_TIMEOUT_SECS"],
                    "120",
                )

    def test_failed_tool_checks_error_result_and_absent_marker(self) -> None:
        with tempfile.TemporaryDirectory(prefix="leg-trial-test-") as temporary:
            workspace = Path(temporary)
            fixture = Fixture("trial", workspace)
            with running_fixture(fixture) as base_url:
                first = post_message(base_url, [{"role": "user", "content": "TRIAL-FAILED"}])
                tool_use = next(
                    block
                    for block in first["content"]
                    if isinstance(block, dict) and block.get("type") == "tool_use"
                )
                self.assertEqual(tool_use["input"]["timeout"], -1)

                post_message(
                    base_url,
                    [
                        {"role": "user", "content": "TRIAL-FAILED"},
                        {"role": "assistant", "content": [tool_use]},
                        {
                            "role": "user",
                            "content": [
                                {
                                    "type": "tool_result",
                                    "tool_use_id": tool_use["id"],
                                    "is_error": True,
                                    "content": "negative timeout rejected before execution",
                                }
                            ],
                        },
                    ],
                )

            status = fixture.status()
            self.assertTrue(status["input_checks"].get("failed_tool_error_returned"))
            marker_check = next(
                check for check in status["workspace_checks"] if check["path"] == "failed-marker.txt"
            )
            self.assertEqual(marker_check["expectation"], "absent")
            self.assertIs(marker_check["ok"], True)

    def test_chinese_marker_checks_the_documented_three_lines(self) -> None:
        fixture = Fixture("trial", None)
        prompt = f"TRIAL-CHINESE\n{TRIAL_LINES}"
        payload = {"messages": [{"role": "user", "content": prompt}]}
        self.assertEqual(fixture.marker_for(payload), "TRIAL-CHINESE")
        self.assertEqual(
            fixture.marker_for({"messages": [{"role": "user", "content": "TRIAL-COMPOSE"}]}),
            "",
        )

        with running_fixture(fixture) as base_url:
            post_message(base_url, payload["messages"])

        self.assertTrue(fixture.status()["input_checks"].get("chinese_multiline_prompt"))

    def test_held_tui_pause_marker_creates_a_gate(self) -> None:
        fixture = Fixture("trial", None, hold_after_first_chunk=True)

        request, occurrence = fixture.advance("TRIAL-PAUSE")

        self.assertEqual((request, occurrence), (1, 1))
        self.assertEqual(fixture.status()["pause_gates"], {"1": "waiting"})


if __name__ == "__main__":
    unittest.main()
