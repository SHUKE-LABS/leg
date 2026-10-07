from __future__ import annotations

import json
import threading
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from urllib.request import Request, urlopen

import responsiveness_harness as harness


class ResponsivenessHarnessTests(unittest.TestCase):
    def test_seeded_dataset_has_required_sizes(self) -> None:
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            workspace = root / "workspace"
            dataset = harness.seed_responsiveness_catalog(root / "state", workspace)
            events = [
                json.loads(line)
                for line in Path(dataset["history_path"]).read_text(encoding="utf-8").splitlines()
            ]

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
        self.assertEqual(len(ordinary_replies), 1000)
        self.assertTrue(all(len(reply.encode("utf-8")) == 4096 for reply in ordinary_replies))
        self.assertEqual(len(long_answer.splitlines()), 10_000)
        self.assertEqual(len(tool_result.encode("utf-8")), 1024 * 1024)

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
                "messages": [{"role": "user", "content": "RESPONSIVENESS-STREAM-A"}],
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
            emissions = fixture.stream_emissions["RESPONSIVENESS-STREAM-A"]
            self.assertEqual(len(chunks), 4)
            self.assertTrue(all(len(chunk.encode("utf-8")) == 32 for chunk in chunks))
            self.assertEqual([event["chunk_index"] for event in emissions], [0, 1, 2, 3])
            self.assertTrue(
                all(left["emitted_ns"] < right["emitted_ns"] for left, right in zip(emissions, emissions[1:]))
            )
            self.assertEqual(status["responsiveness_streams"]["RESPONSIVENESS-STREAM-A"]["chunks_emitted"], 4)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)
            module.RESPONSIVENESS_CHUNK_COUNT = original_count
            module.RESPONSIVENESS_CHUNK_INTERVAL_MS = original_interval


if __name__ == "__main__":
    unittest.main()
