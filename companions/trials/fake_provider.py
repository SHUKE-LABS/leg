#!/usr/bin/env python3
"""Deterministic, local Anthropic Messages fixture for matched UI trials."""

from __future__ import annotations

import argparse
import http.server
import json
import os
import shlex
import socketserver
import tempfile
import threading
import time
from pathlib import Path
from typing import Any


SCENARIOS = (
    "trial",
    "first-answer",
    "chinese-multiline",
    "text-tool-text",
    "denied-tool",
    "failed-tool",
    "auth-error",
    "capped-tool-loop",
    "browser",
    "paused-live-text",
    "stalled-bash",
    "reopen-after-failure",
    "reopen-after-interruption",
)
PAUSE_MS = 1200
BROWSER_PAUSE_MS = 5000
TRIAL_LINES = "Line one: keep this first.\n第二行：保留中文。\nLine three: keep this third."
UNTRUSTED_MARKDOWN = (
    "<script>window.__legXss = true</script>\n"
    '<img src="http://leg-web-xss.invalid/pixel" onerror="fetch(\'/api/sessions/invalid/stop\',{method:\'POST\'})">\n'
    "[x](javascript:fetch('/api/sessions/invalid/stop',{method:'POST'}))\n"
    "[data](data:text/html,boom)\n"
    "![remote image](http://leg-web-xss.invalid/pixel)\n"
)


def text_content(value: Any) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "\n".join(
            block.get("text", "")
            for block in value
            if isinstance(block, dict) and block.get("type") == "text"
        )
    return ""


def sse(name: str, data: dict[str, Any]) -> bytes:
    return f"event: {name}\ndata: {json.dumps(data, ensure_ascii=False, separators=(',', ':'))}\n\n".encode()


class Fixture:
    def __init__(self, scenario: str, workspace: Path | None):
        self.scenario = scenario
        self.workspace = workspace
        self.lock = threading.Lock()
        self.counts: dict[str, int] = {}
        self.request_count = 0
        self.provider_waits: list[dict[str, Any]] = []
        self.input_checks: dict[str, bool] = {}
        self.expected_files: dict[str, tuple[str, str | None]] = {}
        self.denial_hook: Path | None = None
        self.tempdir: tempfile.TemporaryDirectory[str] | None = None

    def prepare_hook(self) -> None:
        if self.scenario not in ("trial", "browser", "denied-tool"):
            return
        self.tempdir = tempfile.TemporaryDirectory(prefix="leg-ui-trial-")
        hook = Path(self.tempdir.name) / "trial-deny-hook.py"
        hook.write_text(
            "#!/usr/bin/env python3\n"
            "import json, sys\n"
            "event = json.load(sys.stdin)\n"
            "command = event.get('tool_input', {}).get('command', '')\n"
            "if 'TRIAL-DENY' in command:\n"
            "    print(json.dumps({'decision': 'deny', 'reason': 'deterministic trial denial'}))\n"
            "else:\n"
            "    print(json.dumps({'decision': 'allow'}))\n",
            encoding="utf-8",
        )
        hook.chmod(0o700)
        self.denial_hook = hook

    def record_wait(self, request: int, marker: str, milliseconds: int) -> None:
        with self.lock:
            self.provider_waits.append(
                {"request": request, "marker": marker, "simulated_provider_wait_ms": milliseconds}
            )

    def effect(self, relative: str, kind: str, expected: str | None = None) -> None:
        with self.lock:
            self.expected_files[relative] = (kind, expected)

    def check(self, name: str, value: bool = True) -> None:
        with self.lock:
            self.input_checks[name] = value

    def marker_for(self, payload: dict[str, Any]) -> str:
        messages = payload.get("messages", [])
        markers = (
            "TRIAL-CHINESE",
            "TRIAL-SEED-SESSION",
            "TRIAL-NAV-SEED",
            "TRIAL-NAV-CONTINUE",
            "TRIAL-NAV-LOSER",
            "TRIAL-TOOL-TEXT",
            "TRIAL-DENIED",
            "TRIAL-FAILED",
            "TRIAL-CAP",
            "TRIAL-PAUSE",
            "TRIAL-STOP",
            "TRIAL-REOPEN-FAILURE",
            "TRIAL-REOPEN-INTERRUPTION",
            "TRIAL-REOPEN",
            "TRIAL-AUTH",
            "TRIAL-RETRY",
            "TRIAL-PROVIDER-RETRY",
            "TRIAL-RUNNING",
            "TRIAL-LONG",
            "TRIAL-LARGE-TOOL",
            "TRIAL-UNTRUSTED-MARKDOWN",
            "TRIAL-CONTINUE",
            "TRIAL-COMPOSE",
        )
        for message in reversed(messages if isinstance(messages, list) else []):
            if not isinstance(message, dict):
                continue
            body = text_content(message.get("content"))
            for marker in markers:
                if marker in body:
                    return marker
        return ""

    def advance(self, marker: str) -> tuple[int, int]:
        with self.lock:
            self.request_count += 1
            self.counts[marker] = self.counts.get(marker, 0) + 1
            return self.request_count, self.counts[marker]

    def send_error(self, handler: http.server.BaseHTTPRequestHandler, status: int, kind: str, message: str) -> None:
        body = json.dumps(
            {"type": "error", "error": {"type": kind, "message": message}},
            ensure_ascii=False,
        ).encode()
        handler.send_response(status)
        handler.send_header("Content-Type", "application/json")
        handler.send_header("Content-Length", str(len(body)))
        handler.send_header("Connection", "close")
        handler.end_headers()
        handler.wfile.write(body)
        handler.close_connection = True

    def send_json(self, handler: http.server.BaseHTTPRequestHandler, status: int, value: Any) -> None:
        body = json.dumps(value, ensure_ascii=False, indent=2).encode()
        handler.send_response(status)
        handler.send_header("Content-Type", "application/json; charset=utf-8")
        handler.send_header("Content-Length", str(len(body)))
        handler.send_header("Connection", "close")
        handler.end_headers()
        handler.wfile.write(body)
        handler.close_connection = True

    def send_stream(self, handler: http.server.BaseHTTPRequestHandler, blocks: list[dict[str, Any]], stop_reason: str = "end_turn", pause_ms: int = 0, marker: str = "") -> None:
        request_number = getattr(handler, "fixture_request_number", self.request_count)
        if not getattr(handler, "fixture_streaming", False):
            if pause_ms:
                self.record_wait(request_number, marker, pause_ms)
                time.sleep(pause_ms / 1000)
            content = []
            for index, block in enumerate(blocks):
                if block["type"] == "text":
                    content.append({"type": "text", "text": block["text"]})
                elif block["type"] == "tool_use":
                    content.append(
                        {
                            "type": "tool_use",
                            "id": f"toolu_trial_{request_number}_{index}",
                            "name": block["name"],
                            "input": block["input"],
                        }
                    )
            self.send_json(
                handler,
                200,
                {
                    "id": f"msg_trial_{request_number}",
                    "type": "message",
                    "role": "assistant",
                    "content": content,
                    "model": "trial-fixture",
                    "stop_reason": stop_reason,
                    "usage": {"input_tokens": 1, "output_tokens": 1},
                },
            )
            return
        handler.send_response(200)
        handler.send_header("Content-Type", "text/event-stream; charset=utf-8")
        handler.send_header("Cache-Control", "no-cache")
        handler.send_header("Connection", "close")
        handler.end_headers()
        handler.close_connection = True

        def write(data: bytes) -> None:
            handler.wfile.write(data)
            handler.wfile.flush()

        write(
            sse(
                "message_start",
                {
                    "type": "message_start",
                    "message": {
                        "id": f"msg_trial_{request_number}",
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": "trial-fixture",
                        "stop_reason": None,
                        "usage": {"input_tokens": 1, "output_tokens": 0},
                    },
                },
            )
        )
        for index, block in enumerate(blocks):
            block_type = block["type"]
            if block_type == "text":
                write(
                    sse(
                        "content_block_start",
                        {"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}},
                    )
                )
                value = block["text"]
                midpoint = max(1, len(value) // 2)
                chunks = [value[:midpoint], value[midpoint:]]
                for chunk_index, chunk in enumerate(chunks):
                    if chunk:
                        write(
                            sse(
                                "content_block_delta",
                                {"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": chunk}},
                            )
                        )
                    if pause_ms and chunk_index == 0:
                        self.record_wait(request_number, marker, pause_ms)
                        time.sleep(pause_ms / 1000)
                write(sse("content_block_stop", {"type": "content_block_stop", "index": index}))
            elif block_type == "tool_use":
                write(
                    sse(
                        "content_block_start",
                        {
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {
                                "type": "tool_use",
                                "id": f"toolu_trial_{request_number}_{index}",
                                "name": block["name"],
                                "input": {},
                            },
                        },
                    )
                )
                partial = json.dumps(block["input"], ensure_ascii=False, separators=(",", ":"))
                write(
                    sse(
                        "content_block_delta",
                        {"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": partial}},
                    )
                )
                write(sse("content_block_stop", {"type": "content_block_stop", "index": index}))
        write(
            sse(
                "message_delta",
                {"type": "message_delta", "delta": {"stop_reason": stop_reason, "stop_sequence": None}, "usage": {"output_tokens": 1}},
            )
        )
        write(sse("message_stop", {"type": "message_stop"}))

    def send_partial_then_close(self, handler: http.server.BaseHTTPRequestHandler) -> None:
        if not getattr(handler, "fixture_streaming", False):
            self.send_error(handler, 503, "api_error", "fixture response interrupted before completion")
            return
        handler.send_response(200)
        handler.send_header("Content-Type", "text/event-stream; charset=utf-8")
        handler.send_header("Connection", "close")
        handler.end_headers()
        handler.wfile.write(
            sse(
                "message_start",
                {"type": "message_start", "message": {"id": "msg_interrupted", "usage": {"input_tokens": 1, "output_tokens": 0}}},
            )
        )
        handler.wfile.write(
            sse(
                "content_block_start",
                {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
            )
        )
        handler.wfile.write(
            sse(
                "content_block_delta",
                {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Partial fixture text before the interrupted stream."}},
            )
        )
        handler.wfile.flush()
        handler.close_connection = True

    def handle_messages(self, handler: http.server.BaseHTTPRequestHandler, payload: dict[str, Any]) -> None:
        marker = self.marker_for(payload) if self.scenario in ("trial", "browser") else self.scenario
        request, occurrence = self.advance(marker)
        handler.fixture_request_number = request

        if marker == "TRIAL-PROVIDER-RETRY" and occurrence == 1:
            self.check("provider_retry_first_attempt_failed")
            self.send_error(handler, 503, "overloaded_error", "retry this provider request once")
            return
        if marker in ("auth-error", "TRIAL-AUTH"):
            self.check("auth_error_sent")
            self.send_error(handler, 401, "authentication_error", "deterministic trial authentication error")
            return
        if marker == "reopen-after-failure" and occurrence == 1:
            self.check("reopen_failure_sent")
            self.send_error(handler, 401, "authentication_error", "first request fails; reopen and retry the same prompt")
            return
        if marker == "reopen-after-interruption":
            if occurrence == 1:
                self.check("interrupted_stream_sent")
                self.send_partial_then_close(handler)
                return
            self.check("reopen_after_interruption_succeeded")
            self.send_stream(handler, [{"type": "text", "text": "Reopened after the interrupted fixture response."}])
            return
        if marker == "TRIAL-REOPEN-FAILURE" and occurrence == 1:
            self.check("reopen_failure_sent")
            self.send_error(handler, 401, "authentication_error", "reopen this failed fixture session and retry")
            return
        if marker == "TRIAL-REOPEN-INTERRUPTION":
            if occurrence == 1:
                self.check("interrupted_stream_sent")
                self.send_partial_then_close(handler)
                return
            self.check("reopen_after_interruption_succeeded")
            self.send_stream(handler, [{"type": "text", "text": "Reopened after the interrupted fixture response."}])
            return
        if marker == "TRIAL-RETRY" and occurrence == 1:
            self.check("retry_failure_sent")
            self.send_error(handler, 401, "authentication_error", "press the UI's explicit retry action")
            return
        if marker == "paused-live-text" or marker == "TRIAL-PAUSE":
            pause_ms = BROWSER_PAUSE_MS if self.scenario == "browser" else PAUSE_MS
            self.send_stream(handler, [{"type": "text", "text": "The first live text is visible. The fixture resumes after its fixed pause."}], pause_ms=pause_ms, marker=marker)
            self.check("paused_response_completed")
            return
        if marker == "TRIAL-SEED-SESSION":
            self.send_stream(handler, [{"type": "text", "text": "Fixture seed: blue lantern."}])
            return
        if marker == "TRIAL-NAV-SEED":
            if self.has_tool_result(payload):
                self.check("navigation_seed_tool_result_returned")
                self.send_stream(handler, [{"type": "text", "text": "The navigation seed is complete."}])
            else:
                self.send_stream(
                    handler,
                    [{"type": "tool_use", "name": "bash", "input": {"command": "pwd && printf 'session-rail-tool-history\\n'"}}],
                    stop_reason="tool_use",
                )
            return
        if marker == "TRIAL-NAV-CONTINUE":
            messages = payload.get("messages", [])
            tool_results = [
                block
                for message in messages if isinstance(message, dict) and message.get("role") == "user"
                for block in (message.get("content") if isinstance(message.get("content"), list) else [])
                if isinstance(block, dict) and block.get("type") == "tool_result"
            ]
            history = json.dumps(payload, ensure_ascii=False)
            self.check(
                "navigation_prior_text_and_tool_history_returned",
                "TRIAL-NAV-SEED" in history
                and "The navigation seed is complete." in history
                and any("session-rail-tool-history" in json.dumps(block, ensure_ascii=False) for block in tool_results),
            )
            self.check(
                "navigation_recorded_workspace_returned",
                self.workspace is not None and str(self.workspace.resolve()) in history,
            )
            self.send_stream(handler, [{"type": "text", "text": "The reopened session kept its prior tool history and workspace."}])
            return
        if marker == "TRIAL-CHINESE" or marker == "chinese-multiline":
            all_text = "\n".join(text_content(item.get("content")) for item in payload.get("messages", []) if isinstance(item, dict) and item.get("role") == "user")
            self.check("chinese_multiline_prompt", TRIAL_LINES in all_text)
            self.send_stream(handler, [{"type": "text", "text": "Three lines received, including the Chinese second line."}])
            return
        if marker == "text-tool-text" or marker == "TRIAL-TOOL-TEXT":
            if occurrence == 1:
                self.effect("fixture-write.txt", "equals", "fixture-write-ok\n")
                self.send_stream(
                    handler,
                    [
                        {"type": "text", "text": "First the text, then a verified fixture write."},
                        {"type": "tool_use", "name": "write", "input": {"path": "fixture-write.txt", "content": "fixture-write-ok\n"}},
                    ],
                    stop_reason="tool_use",
                )
            else:
                self.check("tool_result_returned", self.has_tool_result(payload))
                self.send_stream(handler, [{"type": "text", "text": "The write result returned; this is the final text."}])
            return
        if marker == "denied-tool" or marker == "TRIAL-DENIED":
            self.effect("denied-marker.txt", "absent")
            if occurrence > 1 and self.has_error_tool_result(payload):
                self.check("denied_tool_error_returned")
                self.send_stream(handler, [{"type": "text", "text": "The tool was denied and its error result was returned."}])
                return
            self.send_stream(
                handler,
                [{"type": "tool_use", "name": "bash", "input": {"command": "printf denied > denied-marker.txt # TRIAL-DENY"}}],
                stop_reason="tool_use",
            )
            return
        if marker == "failed-tool" or marker == "TRIAL-FAILED":
            self.effect("failed-marker.txt", "absent")
            if occurrence > 1 and self.has_error_tool_result(payload):
                self.check("failed_tool_error_returned")
                self.send_stream(handler, [{"type": "text", "text": "The tool failed and its error result was returned."}])
                return
            self.send_stream(
                handler,
                [{"type": "tool_use", "name": "bash", "input": {"command": "printf failed > failed-marker.txt", "timeout": -1}}],
                stop_reason="tool_use",
            )
            return
        if marker == "capped-tool-loop" or marker == "TRIAL-CAP":
            self.effect("trial-rounds.txt", "equals", "round-1\nround-2\n")
            self.send_stream(
                handler,
                [{"type": "tool_use", "name": "bash", "input": {"command": f"printf 'round-{occurrence}\\n' >> trial-rounds.txt"}}],
                stop_reason="tool_use",
            )
            return
        if marker == "stalled-bash" or marker == "TRIAL-STOP":
            self.effect("trial-stall-finished.txt", "absent")
            self.effect("trial-stalled-child.pid", "pid_stopped")
            command = "sleep 600 & child=$!; printf '%s\\n' \"$child\" > trial-stalled-child.pid; wait \"$child\"; printf unexpected > trial-stall-finished.txt"
            self.send_stream(handler, [{"type": "tool_use", "name": "bash", "input": {"command": command}}], stop_reason="tool_use")
            return
        if marker == "TRIAL-RUNNING":
            self.effect("trial-running-tool-finished.txt", "exists")
            if occurrence > 1 and self.has_tool_result(payload):
                self.check("running_tool_result_returned")
                self.send_stream(handler, [{"type": "text", "text": "The short running tool finished successfully."}])
                return
            command = "sleep 4; printf finished > trial-running-tool-finished.txt"
            self.send_stream(handler, [{"type": "tool_use", "name": "bash", "input": {"command": command}}], stop_reason="tool_use")
            return
        if marker == "TRIAL-LONG":
            long_text = "\n".join(f"Long fixture line {i:03d}: the complete answer remains available to browse and copy." for i in range(1, 181)) + "\nEND OF FIXTURE ANSWER"
            self.send_stream(handler, [{"type": "text", "text": long_text}])
            return
        if marker == "TRIAL-LARGE-TOOL":
            if self.has_tool_result(payload):
                self.check("large_tool_result_returned", True)
                self.send_stream(handler, [{"type": "text", "text": "The large tool result was returned."}])
            else:
                self.send_stream(
                    handler,
                    [{"type": "tool_use", "name": "bash", "input": {"command": "python3 -c 'print(\"L\" * 12000)'"}}],
                    stop_reason="tool_use",
                )
            return
        if marker == "TRIAL-UNTRUSTED-MARKDOWN":
            if self.has_tool_result(payload):
                self.check("untrusted_tool_result_returned", True)
                self.send_stream(handler, [{"type": "text", "text": UNTRUSTED_MARKDOWN}])
            else:
                self.send_stream(
                    handler,
                    [{"type": "tool_use", "name": "read", "input": {"path": "xss-fixture.html"}}],
                    stop_reason="tool_use",
                )
            return
        if marker == "TRIAL-CONTINUE":
            self.send_stream(handler, [{"type": "text", "text": "Continuation response: the earlier fixture answer is still in this session."}])
            return
        if marker == "TRIAL-COMPOSE":
            all_text = "\n".join(text_content(item.get("content")) for item in payload.get("messages", []) if isinstance(item, dict) and item.get("role") == "user")
            self.check("multiline_prompt", "first line" in all_text and "第二行" in all_text and "third line" in all_text)
            self.send_stream(handler, [{"type": "text", "text": "Multiline fixture prompt received."}])
            return
        if marker == "TRIAL-RETRY" or marker == "TRIAL-REOPEN-FAILURE":
            self.check("retry_or_reopen_succeeded")
            self.send_stream(handler, [{"type": "text", "text": "The explicit retry succeeded."}])
            return
        if marker == "TRIAL-REOPEN-INTERRUPTION" or marker == "TRIAL-REOPEN":
            self.check("reopen_succeeded")
            self.send_stream(handler, [{"type": "text", "text": "The reopened session answered successfully."}])
            return

        if marker in ("first-answer", "trial") or not marker:
            self.send_stream(handler, [{"type": "text", "text": "Fixture ready. This answer came from a local fake provider."}])
            return
        self.send_stream(handler, [{"type": "text", "text": f"Deterministic fixture response for {marker}."}])

    @staticmethod
    def has_tool_result(payload: dict[str, Any]) -> bool:
        for message in reversed(payload.get("messages", [])):
            if not isinstance(message, dict) or message.get("role") != "user":
                continue
            content = message.get("content")
            return isinstance(content, list) and any(
                isinstance(block, dict) and block.get("type") == "tool_result"
                for block in content
            )
        return False

    @staticmethod
    def has_error_tool_result(payload: dict[str, Any]) -> bool:
        for message in reversed(payload.get("messages", [])):
            if not isinstance(message, dict) or message.get("role") != "user":
                continue
            content = message.get("content")
            return isinstance(content, list) and any(
                isinstance(block, dict)
                and block.get("type") == "tool_result"
                and block.get("is_error") is True
                for block in content
            )
        return False

    def status(self) -> dict[str, Any]:
        with self.lock:
            expected_files = dict(self.expected_files)
            waits = list(self.provider_waits)
            counts = dict(self.counts)
            input_checks = dict(self.input_checks)
            requests = self.request_count
        checks: list[dict[str, Any]] = []
        for relative, (kind, expected) in sorted(expected_files.items()):
            if self.workspace is None:
                checks.append({"path": relative, "expectation": kind, "ok": None, "note": "start with --workspace to verify workspace effects"})
                continue
            path = self.workspace / relative
            if kind == "exists":
                ok = path.is_file()
            elif kind == "absent":
                ok = not path.exists()
            elif kind == "equals":
                ok = path.is_file() and path.read_text(encoding="utf-8") == expected
            elif kind == "pid_stopped":
                try:
                    raw_pid = path.read_text(encoding="utf-8").strip()
                except FileNotFoundError:
                    ok = False
                else:
                    try:
                        pid = int(raw_pid)
                    except ValueError:
                        ok = False
                    else:
                        try:
                            os.kill(pid, 0)
                        except ProcessLookupError:
                            ok = True
                        except (PermissionError, OSError):
                            ok = False
                        else:
                            ok = False
            else:
                ok = False
            checks.append({"path": relative, "expectation": kind, "ok": ok})
        return {
            "schema": "leg-ui-trial.fixture-status/v1",
            "scenario": self.scenario,
            "requests": requests,
            "scenario_requests": counts,
            "simulated_provider_waits": waits,
            "input_checks": input_checks,
            "workspace_checks": checks,
            "note": "No prompts, provider headers, credentials, or transcript content are retained.",
        }


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    fixture: Fixture

    def log_message(self, _format: str, *_args: Any) -> None:
        return

    def do_GET(self) -> None:
        if self.path == "/__trial/status":
            self.fixture.send_json(self, 200, self.fixture.status())
            return
        self.fixture.send_json(self, 404, {"error": "not found"})

    def do_POST(self) -> None:
        if self.path != "/v1/messages":
            self.fixture.send_json(self, 404, {"error": "not found"})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            payload = json.loads(self.rfile.read(length))
            if not isinstance(payload, dict):
                raise ValueError("request body is not an object")
        except (ValueError, json.JSONDecodeError) as error:
            self.fixture.send_json(self, 400, {"error": f"invalid JSON request: {error}"})
            return
        self.fixture_streaming = payload.get("stream") is True
        # The incomplete-stream fixture closes its first response immediately;
        # repeating the same prompt gets the scripted successful response.
        try:
            self.fixture.handle_messages(self, payload)
        except (BrokenPipeError, ConnectionResetError):
            # A UI may stop a paused stream. That disconnect is a fixture
            # outcome, not a provider-server traceback.
            return


class LoopbackThreadingHTTPServer(http.server.ThreadingHTTPServer):
    def server_bind(self) -> None:
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]


def powershell_quote(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def environment(base_url: str, scenario: str, hook: Path | None) -> dict[str, str]:
    values = {
        "LEG_PROVIDER": "anthropic",
        "ANTHROPIC_BASE_URL": base_url,
        "ANTHROPIC_API_KEY": "trial-only-not-a-secret",
        "LEG_MODEL": "trial-fixture",
        "LEG_MAX_RETRIES": "0",
    }
    if scenario in ("trial", "browser", "capped-tool-loop"):
        values["LEG_MAX_TOOL_ROUNDS"] = "2"
    if scenario in ("trial", "browser", "stalled-bash"):
        values["LEG_BASH_TIMEOUT_SECS"] = "120"
    if hook is not None:
        values["LEG_PRETOOL_HOOK"] = str(hook)
    return values


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", choices=SCENARIOS, default="trial")
    parser.add_argument("--host", default="127.0.0.1", help="bind address (default: loopback only)")
    parser.add_argument("--port", type=int, default=0, help="listen port (default: choose a free port)")
    parser.add_argument("--workspace", type=Path, help="trial workspace root for deterministic effect checks")
    args = parser.parse_args()
    if args.workspace is not None:
        workspace = args.workspace.expanduser().resolve()
        if not workspace.is_dir():
            parser.error(f"workspace is not a directory: {workspace}")
    else:
        workspace = None

    fixture = Fixture(args.scenario, workspace)
    fixture.prepare_hook()
    handler_type = type("FixtureHandler", (Handler,), {"fixture": fixture})
    try:
        server = LoopbackThreadingHTTPServer((args.host, args.port), handler_type)
    except OSError as error:
        parser.error(f"cannot listen on {args.host}:{args.port}: {error}")
    server.daemon_threads = True
    host, port = server.server_address[:2]
    base_url = f"http://{host}:{port}"
    values = environment(base_url, args.scenario, fixture.denial_hook)

    print(f"Fixture scenario: {args.scenario}", flush=True)
    print(f"Listening: {base_url}/v1/messages", flush=True)
    print("Copy the matching environment block to the terminal that launches leg.", flush=True)
    print("\nBash / zsh:", flush=True)
    for key, value in values.items():
        print(f"export {key}={shlex.quote(value)}", flush=True)
    print("\nPowerShell:", flush=True)
    for key, value in values.items():
        print(f"$env:{key} = {powershell_quote(value)}", flush=True)
    print(f"\nStatus: {base_url}/__trial/status", flush=True)
    if workspace is None:
        print("Workspace effects are not checked; pass --workspace PATH to enable checks.", flush=True)
    print("Press Ctrl-C here to stop the fixture.", flush=True)

    try:
        server.serve_forever(poll_interval=0.25)
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        print(json.dumps(fixture.status(), ensure_ascii=False, indent=2), flush=True)
        if fixture.tempdir is not None:
            fixture.tempdir.cleanup()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
