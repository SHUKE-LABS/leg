from __future__ import annotations

import io
import json
import math
import queue
import re
import sys
import threading
import time
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from unittest.mock import Mock, patch
from urllib.request import Request, urlopen

import responsiveness_harness as harness


class ResponsivenessHarnessTests(unittest.TestCase):
    def test_session_title_visibility_handles_narrow_header_truncation(self) -> None:
        history_screen = "status: Idle  |  History fixtu\ufffd  |  model: trial-fixture"
        legacy_history_screen = "│leg-tui  |  status: Idle  |  History fixture  |  model: trial-fixture"
        stream_screen = "status: Idle  |  Stream 1 A  |  model: trial-fixture"
        running_screen = "status: Running  |  Stream 1 A  |  model: trial-fixture"
        legacy_running_screen = "│leg-tui  |  status: Running  |  Stream 1 A  |  model: trial-fixture"
        completed_screen = "status: Succeeded  |  Stream 1 A  |  model: trial-fixture"

        self.assertTrue(harness._session_title_visible(history_screen, "History fixture"))
        self.assertTrue(harness._session_title_visible(legacy_history_screen, "History fixture"))
        self.assertTrue(harness._session_title_visible(stream_screen, "Stream 1 A"))
        self.assertFalse(harness._session_title_visible(stream_screen, "Stream 1 B"))
        self.assertFalse(
            harness._session_title_visible("transcript mentions History fixtu", "History fixture")
        )
        self.assertFalse(
            harness._session_title_visible(
                "transcript status: Idle  |  History fixture", "History fixture"
            )
        )
        self.assertFalse(harness._session_is_inactive(running_screen, "Stream 1 A"))
        self.assertFalse(harness._session_is_inactive(legacy_running_screen, "Stream 1 A"))
        self.assertTrue(harness._session_is_inactive(legacy_history_screen, "History fixture"))
        self.assertTrue(harness._session_is_inactive(completed_screen, "Stream 1 A"))

    def test_windows_terminal_write_completes_partial_writes(self) -> None:
        class PartialWriter:
            def __init__(self) -> None:
                self.writes: list[str] = []
                self.requests: list[str] = []

            def write(self, value: str) -> int:
                self.requests.append(value)
                written = min(2, len(value.encode("utf-8")))
                self.writes.append(value[:written])
                return written

        terminal = harness.TerminalProcess.__new__(harness.TerminalProcess)
        terminal.process = PartialWriter()
        original_name = harness.os.name
        try:
            harness.os.name = "nt"
            terminal.write("draft010")
        finally:
            harness.os.name = original_name

        self.assertEqual("".join(terminal.process.writes), "draft010")
        self.assertEqual(terminal.process.requests[0], "draft010")
        self.assertEqual(len(terminal.process.requests), 4)

    def test_windows_terminal_write_rejects_zero_progress(self) -> None:
        class StalledWriter:
            def write(self, value: str) -> int:
                return 0

        terminal = harness.TerminalProcess.__new__(harness.TerminalProcess)
        terminal.process = StalledWriter()
        original_name = harness.os.name
        try:
            harness.os.name = "nt"
            with self.assertRaisesRegex(RuntimeError, "invalid progress"):
                terminal.write("draft010")
        finally:
            harness.os.name = original_name

    @unittest.skipUnless(harness.os.name == "nt", "requires Windows ConPTY")
    def test_windows_conpty_reader_does_not_batch_output_for_100ms(self) -> None:
        sample_count = 20
        child_code = (
            "import time\n"
            f"for index in range({sample_count}):\n"
            "    print(f'CAPTURE:{index}:{time.perf_counter_ns()}', flush=True)\n"
            "    time.sleep(0.01)\n"
        )
        environment = {
            name: harness.os.environ[name]
            for name in ("PATH", "SystemRoot", "TEMP", "TMP")
            if name in harness.os.environ
        }
        process = harness.PtyProcess.spawn(
            [sys.executable, "-u", "-c", child_code],
            env=environment,
            dimensions=(24, 80),
            backend=harness.Backend.ConPTY,
        )
        chunks: queue.Queue[tuple[int, str]] = queue.Queue()
        stop_reader = threading.Event()

        def read_output() -> None:
            while not stop_reader.is_set():
                try:
                    chunk = process.read(4096)
                except (EOFError, OSError, ValueError):
                    return
                if chunk:
                    chunks.put((time.perf_counter_ns(), chunk))
                elif not process.isalive():
                    return

        reader = threading.Thread(target=read_output, daemon=True)
        reader.start()
        try:
            latencies_ms: list[float] = []
            output = ""
            seen: set[int] = set()
            marker_pattern = re.compile(r"CAPTURE:(\d+):(\d+)")
            deadline = time.monotonic() + 5.0
            while len(seen) < sample_count and time.monotonic() < deadline:
                try:
                    observed_ns, chunk = chunks.get(timeout=0.25)
                except queue.Empty:
                    if not process.isalive():
                        break
                    continue
                output += chunk
                for match in marker_pattern.finditer(output):
                    index = int(match.group(1))
                    if index in seen:
                        continue
                    seen.add(index)
                    generated_ns = int(match.group(2))
                    latencies_ms.append((observed_ns - generated_ns) / 1_000_000)

            self.assertEqual(
                seen,
                set(range(sample_count)),
                f"ConPTY output markers were missing: {sorted(seen)!r}; output={output!r}",
            )

            p95_index = math.ceil(0.95 * len(latencies_ms)) - 1
            p95_ms = sorted(latencies_ms)[p95_index]
            self.assertLess(
                p95_ms,
                50.0,
                f"ConPTY output reader added a polling delay: {latencies_ms!r}",
            )
        finally:
            stop_reader.set()
            try:
                process.close(force=True)
            finally:
                reader.join(timeout=1.0)

    def test_harness_environment_removes_ambient_credentials(self) -> None:
        ambient = {name: f"ambient-{name}" for name in harness._CREDENTIAL_ENV_VARS}
        ambient["PATH"] = "fixture-path"

        environment = harness._build_harness_environment(
            ambient,
            {
                "LEG_PROVIDER": "anthropic",
                "ANTHROPIC_API_KEY": "trial-only-not-a-secret",
            },
            Path("state"),
            Path("leg-ui-supervisor.exe"),
        )

        self.assertEqual(environment["PATH"], "fixture-path")
        self.assertEqual(environment["ANTHROPIC_API_KEY"], "trial-only-not-a-secret")
        for name in harness._CREDENTIAL_ENV_VARS[1:]:
            with self.subTest(name=name):
                self.assertNotIn(name, environment)

    def test_seeded_dataset_has_required_sizes(self) -> None:
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            workspace = root / "workspace"
            state_dir = root / "state"
            dataset = harness.seed_responsiveness_catalog(state_dir, workspace)
            events = [
                json.loads(line)
                for line in Path(dataset["history_path"]).read_text(encoding="utf-8").splitlines()
            ]
            seeded_response_events = [event for event in events if event["event"] == "response_ok"]
            for session_id in dataset["stream_session_ids"].values():
                stream_path = state_dir / "sessions" / f"{session_id}.jsonl"
                stream_events = [
                    json.loads(line)
                    for line in stream_path.read_text(encoding="utf-8").splitlines()
                ]
                seeded_response_events.extend(
                    event for event in stream_events if event["event"] == "response_ok"
                )

        ordinary_replies = [
            event["reply"]
            for event in events
            if event["event"] == "response_ok" and event.get("turn_index", 0) < 1000
        ]
        long_answer = next(
            event["reply"]
            for event in events
            if event["event"] == "response_ok" and event.get("turn_index") == 1000
        )
        tool_result = next(event["result"] for event in events if event["event"] == "tool_result")
        self.assertEqual(dataset["base_turns"], 1000)
        self.assertEqual(dataset["total_history_turns"], 1002)
        self.assertEqual(len(dataset["stream_pair_pool"]), 2)
        self.assertEqual(len(ordinary_replies), 1000)
        self.assertTrue(all(len(reply.encode("utf-8")) == 4096 for reply in ordinary_replies))
        self.assertEqual(len(long_answer.splitlines()), 10_000)
        self.assertEqual(len(tool_result.encode("utf-8")), 1024 * 1024)
        self.assertTrue(seeded_response_events)
        self.assertTrue(
            all(isinstance(event.get("duration_ms"), int) for event in seeded_response_events)
        )

    def test_latency_summary_and_provider_context_use_actual_timestamps(self) -> None:
        samples = [{"latency_ms": float(value)} for value in range(1, 101)]
        summary = harness.summarize_latencies(samples)
        self.assertEqual(summary, {"sample_count": 100, "p95_ms": 95.0, "max_ms": 100.0})

        context = harness.provider_context(
            [
                {"chunk_index": 0, "emitted_ns": 1_000_000_000},
                {"chunk_index": 1, "emitted_ns": 1_005_000_000},
            ],
            sample_ns=1_003_000_000,
            run_started_ns=1_000_000_000,
        )
        self.assertEqual(context["previous_chunk_index"], 0)
        self.assertEqual(context["next_chunk_index"], 1)
        self.assertEqual(context["emit_age_before_ms"], 3.0)
        self.assertEqual(context["emit_wait_after_ms"], 2.0)
        self.assertEqual(context["inter_emit_gap_ms"], 5.0)

    def test_idle_cpu_is_normalized_to_one_logical_cpu_and_keeps_raw_counters(self) -> None:
        process = SimpleNamespace(
            cpu_times=Mock(
                side_effect=[
                    SimpleNamespace(user=10.0, system=2.0),
                    SimpleNamespace(user=10.20012, system=2.1),
                ]
            )
        )
        with (
            patch.object(harness.psutil, "Process", return_value=process),
            patch.object(harness.time, "perf_counter_ns", side_effect=[1_000, 30_000_001_000]),
            patch.object(harness.time, "sleep") as sleep,
        ):
            measurement = harness.measure_idle_cpu(1234, 30.0)

        sleep.assert_called_once_with(30.0)
        self.assertEqual(measurement["scope"], "leg-tui process only, identified by the spawned root PID")
        self.assertEqual(measurement["raw_counters"]["start"]["user_seconds"], 10.0)
        self.assertEqual(measurement["raw_counters"]["end"]["system_seconds"], 2.1)
        self.assertAlmostEqual(measurement["average_percent_of_one_logical_cpu"], 1.0004)

    def test_responsiveness_gates_enforce_each_group_opening_bound_and_idle_cpu(self) -> None:
        groups = {
            f"{action}@{columns}x{rows}": {
                "sample_count": harness.SAMPLE_COUNT,
                "p95_ms": 100.0,
            }
            for action in harness.RESPONSIVENESS_ACTIONS
            for rows, columns in harness.DIMENSIONS
        }
        timings = {"cached_history_open_ms": 200.0, "inspector_open_ms": 200.0}

        passed = harness.responsiveness_gates(groups, timings, 1.0)

        self.assertEqual(passed["status"], "passed")
        self.assertEqual(
            harness.summarize_latencies(
                [{"latency_ms": 100.0004}] * harness.SAMPLE_COUNT
            )["p95_ms"],
            100.0004,
        )
        groups["inspection@120x40"]["p95_ms"] = 100.0004
        failed = harness.responsiveness_gates(groups, timings, 1.0)
        self.assertEqual(failed["status"], "failed")
        self.assertFalse(failed["checks"]["input_to_visible_output"]["groups"]["inspection@120x40"]["passed"])
        over_cpu_limit = harness.responsiveness_gates(groups, timings, 1.0004)
        self.assertFalse(over_cpu_limit["checks"]["idle_cpu"]["passed"])
        over_open_limit = harness.responsiveness_gates(
            groups,
            {"cached_history_open_ms": 200.0004, "inspector_open_ms": 200.0},
            1.0,
        )
        self.assertFalse(over_open_limit["checks"]["cached_history_open"]["passed"])
        del groups["history_scrolling@80x24"]
        missing = harness.responsiveness_gates(groups, timings, 1.0)
        self.assertFalse(missing["checks"]["input_to_visible_output"]["groups"]["history_scrolling@80x24"]["passed"])
        self.assertEqual(harness.percent_of_one_logical_cpu(0.3, 30.0), 1.0)
        with self.assertRaisesRegex(ValueError, "elapsed_seconds"):
            harness.percent_of_one_logical_cpu(0.0, 0.0)

    def test_only_final_comparison_role_fails_on_unmet_gates(self) -> None:
        report = {
            "source_revision": "measured",
            "harness_revision": "harness",
            "comparison_role": "observation",
            "terminal_transport": "native PTY",
            "terminal": {},
            "dimensions": [],
            "groups": {},
            "timings": {},
            "idle_cpu": {},
            "gates": {"status": "failed"},
            "memory": {},
        }
        with TemporaryDirectory() as temporary:
            binary_paths = {}
            for name in ("tui", "leg", "supervisor"):
                binary_path = Path(temporary) / name
                binary_path.write_text("test binary", encoding="utf-8")
                binary_paths[name] = str(binary_path)
            for role, expected_status in (("observation", 0), ("final", 1)):
                report["comparison_role"] = role
                output = Path(temporary) / f"{role}.json"
                with (
                    patch.object(
                        sys,
                        "argv",
                        [
                            "responsiveness_harness.py",
                            "--tui-bin", binary_paths["tui"],
                            "--leg-bin", binary_paths["leg"],
                            "--supervisor-bin", binary_paths["supervisor"],
                            "--output", str(output),
                            "--comparison-role", role,
                        ],
                    ),
                    patch.object(harness, "run_harness", return_value=report),
                    redirect_stdout(io.StringIO()),
                ):
                    self.assertEqual(harness.main(), expected_status)

    def test_provider_summary_reads_module_from_fixture_server(self) -> None:
        stream_id = "RESPONSIVENESS-STREAM-A-PAIR-001"
        fixture_module = harness.load_fake_provider()

        class Fixture:
            stream_emissions = {
                stream_id: [{"emitted_ns": 10}, {"emitted_ns": 20}]
            }

            def status(self) -> dict[str, object]:
                return {
                    "responsiveness_streams": {
                        stream_id: {
                            "request": 1,
                            "started_ns": 5,
                            "ended_ns": 25,
                        }
                    }
                }

        class FixtureServer:
            fixture = Fixture()
            module = fixture_module

        summary = harness._provider_summary(FixtureServer(), run_started_ns=0)

        self.assertEqual(summary["stream_count"], 1)
        self.assertEqual(
            summary["streams"][stream_id]["chunk_bytes"],
            fixture_module.RESPONSIVENESS_CHUNK_BYTES,
        )

    def test_workload_overlap_requires_each_sample_inside_its_stream_pair(self) -> None:
        fixture_module = harness.load_fake_provider()

        class Fixture:
            def status(self) -> dict[str, object]:
                return {
                    "responsiveness_streams": {
                        "RESPONSIVENESS-STREAM-A-PAIR-001": {
                            "started_ns": 10,
                            "ended_ns": 35,
                            "chunks_emitted": fixture_module.RESPONSIVENESS_CHUNK_COUNT,
                        },
                        "RESPONSIVENESS-STREAM-B-PAIR-001": {
                            "started_ns": 12,
                            "ended_ns": 30,
                            "chunks_emitted": fixture_module.RESPONSIVENESS_CHUNK_COUNT,
                        },
                    }
                }

        class FixtureServer:
            fixture = Fixture()
            module = fixture_module

        samples = [
            {
                "action": "draft_editing",
                "sample_index": 0,
                "input_monotonic_ns": 12,
                "visible_monotonic_ns": 18,
                "latency_ms": 3.0,
                "stream_pair_id": "pair-001",
            },
            {
                "action": "draft_editing",
                "sample_index": 1,
                "input_monotonic_ns": 16,
                "visible_monotonic_ns": 25,
                "latency_ms": 8.0,
                "stream_pair_id": "pair-001",
            },
        ]
        groups = {"draft_editing@80x24": samples}
        pairs = [
            {
                "pair_id": "pair-001",
                "stream_ids": [
                    "RESPONSIVENESS-STREAM-A-PAIR-001",
                    "RESPONSIVENESS-STREAM-B-PAIR-001",
                ],
            }
        ]

        harness._check_workload_overlap(groups, FixtureServer(), pairs)

        self.assertEqual((pairs[0]["overlap_start_ns"], pairs[0]["overlap_end_ns"]), (12, 30))

        samples[1]["visible_monotonic_ns"] = 31
        with self.assertRaisesRegex(RuntimeError, "sample 1 was outside stream pair pair-001"):
            harness._check_workload_overlap(groups, FixtureServer(), pairs)

    def test_memory_window_uses_only_idle_window_samples(self) -> None:
        sampler = harness.ProcessTreeRssSampler(1)
        sampler.samples = [(99, 100), (100, 200), (199, 300), (200, 400), (150, 0)]

        self.assertEqual(sampler.values_between(100, 200), [200, 300])

    def test_responsiveness_fixture_emits_timestamped_fixed_size_chunks(self) -> None:
        module = harness.load_fake_provider()
        self.assertEqual(module.RESPONSIVENESS_CHUNKS_PER_SECOND, 200)
        self.assertEqual(module.RESPONSIVENESS_CHUNK_BYTES, 32)
        self.assertEqual(module.RESPONSIVENESS_DURATION_SECONDS, 30)
        self.assertEqual(module.RESPONSIVENESS_CHUNK_COUNT, 6000)
        self.assertEqual(module.RESPONSIVENESS_CHUNK_INTERVAL_MS, 5)
        original_count = module.RESPONSIVENESS_CHUNK_COUNT
        original_interval = module.RESPONSIVENESS_CHUNK_INTERVAL_MS
        module.RESPONSIVENESS_CHUNK_COUNT = 4
        module.RESPONSIVENESS_CHUNK_INTERVAL_MS = 1
        fixture = module.Fixture("responsiveness", None)
        handler_type = type("TestResponsivenessHandler", (module.Handler,), {"fixture": fixture})
        server = module.LoopbackThreadingHTTPServer(("127.0.0.1", 0), handler_type)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            url = f"http://127.0.0.1:{server.server_address[1]}/v1/messages"
            payload = {
                "stream": True,
                "messages": [{"role": "user", "content": "RESPONSIVENESS-STREAM-A-PAIR-003"}],
            }
            request = Request(
                url,
                data=json.dumps(payload).encode("utf-8"),
                headers={"Content-Type": "application/json"},
            )
            with urlopen(request, timeout=3) as response:
                body = response.read().decode("utf-8")

            chunks = [
                json.loads(line.removeprefix("data: "))["delta"]["text"]
                for line in body.splitlines()
                if line.startswith("data: ")
                and '"type":"text_delta"' in line
            ]
            status = fixture.status()
            emissions = fixture.stream_emissions["RESPONSIVENESS-STREAM-A-PAIR-003"]
            self.assertEqual(len(chunks), 4)
            self.assertTrue(all(len(chunk.encode("utf-8")) == 32 for chunk in chunks))
            self.assertEqual([event["chunk_index"] for event in emissions], [0, 1, 2, 3])
            self.assertTrue(
                all(left["emitted_ns"] < right["emitted_ns"] for left, right in zip(emissions, emissions[1:]))
            )
            self.assertEqual(
                status["responsiveness_streams"]["RESPONSIVENESS-STREAM-A-PAIR-003"]["chunks_emitted"],
                4,
            )
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)
            module.RESPONSIVENESS_CHUNK_COUNT = original_count
            module.RESPONSIVENESS_CHUNK_INTERVAL_MS = original_interval


if __name__ == "__main__":
    unittest.main()
