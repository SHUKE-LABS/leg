#!/usr/bin/env python3
"""Exercise leg-web ownership and reconnect behavior with the local fake provider."""

from __future__ import annotations

import http.client
import json
import os
import queue
import signal
import subprocess
import sys
import tempfile
import time
from collections import deque
from pathlib import Path
from threading import Thread
from urllib.parse import urlsplit


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "trials" / "fake_provider.py"


def read_until(process: subprocess.Popen[str], prefix: str, timeout: float = 15) -> str:
    deadline = time.monotonic() + timeout
    lines: queue.Queue[str | None] = queue.Queue()
    stdout_tail: deque[str] = deque(maxlen=30)
    stderr_tail: deque[str] = deque(maxlen=30)

    def forward_stdout() -> None:
        if process.stdout is not None:
            for line in process.stdout:
                stdout_tail.append(line.rstrip())
                lines.put(line)
        lines.put(None)

    def capture_stderr() -> None:
        if process.stderr is not None:
            for line in process.stderr:
                stderr_tail.append(line.rstrip())

    Thread(target=forward_stdout, daemon=True).start()
    stderr_reader = Thread(target=capture_stderr, daemon=True)
    stderr_reader.start()

    def process_output() -> str:
        stdout = "\n".join(stdout_tail) or "<no stdout output captured>"
        stderr = "\n".join(stderr_tail) or "<no stderr output captured>"
        return f"stdout:\n{stdout}\nstderr:\n{stderr}"

    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(
                f"did not receive {prefix!r} within {timeout}s; fixture/process output:\n{process_output()}"
            )
        try:
            line = lines.get(timeout=remaining)
        except queue.Empty:
            raise TimeoutError(
                f"did not receive {prefix!r} within {timeout}s; fixture/process output:\n{process_output()}"
            ) from None
        if line is None:
            stderr_reader.join(timeout=1)
            raise AssertionError(
                f"process exited before {prefix!r}; fixture/process output:\n{process_output()}"
            )
        if line.startswith(prefix):
            return line.strip()


def start_provider(workspace: Path) -> tuple[subprocess.Popen[str], str]:
    process = subprocess.Popen(
        [sys.executable, str(FIXTURE), "--scenario", "trial", "--workspace", str(workspace)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    try:
        line = read_until(process, "Listening:")
    except BaseException:
        stop_process(process, graceful=False)
        raise
    return process, line.split()[1].removesuffix("/v1/messages")


def start_host(
    web_bin: Path,
    leg_bin: Path,
    supervisor_bin: Path,
    state_dir: Path,
    workspace: Path,
    provider_url: str,
    event_buffer: int = 16,
) -> tuple[subprocess.Popen[str], str, str]:
    env = os.environ.copy()
    env.update(
        {
            "LEG_PROVIDER": "anthropic",
            "ANTHROPIC_BASE_URL": provider_url,
            "ANTHROPIC_API_KEY": "trial-only-not-a-secret",
            "LEG_MODEL": "trial-fixture",
            "LEG_MAX_RETRIES": "0",
            "LEG_BASH_TIMEOUT_SECS": "120",
        }
    )
    process = subprocess.Popen(
        [
            str(web_bin),
            "--no-open",
            "--bind",
            "127.0.0.1:0",
            "--state-dir",
            str(state_dir),
            "--leg-bin",
            str(leg_bin),
            "--supervisor-bin",
            str(supervisor_bin),
            "--event-buffer",
            str(event_buffer),
        ],
        cwd=workspace,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    try:
        line = read_until(process, "Open this one-time launch URL:")
    except BaseException:
        stop_process(process, graceful=False)
        raise
    url = line.split(": ", 1)[1]
    parts = urlsplit(url)
    return process, parts.netloc, parts.fragment


def request(
    authority: str,
    token: str,
    method: str,
    path: str,
    value: object | None = None,
    timeout: float = 8,
) -> tuple[int, dict[str, object] | list[object] | None]:
    connection = http.client.HTTPConnection(authority, timeout=timeout)
    headers = {
        "Host": authority,
        "Origin": f"http://{authority}",
        "Authorization": f"Bearer {token}",
    }
    body = None
    if value is not None:
        body = json.dumps(value, ensure_ascii=False).encode()
        headers["Content-Type"] = "application/json"
    connection.request(method, path, body=body, headers=headers)
    response = connection.getresponse()
    raw = response.read()
    status = response.status
    connection.close()
    if not raw:
        return status, None
    try:
        return status, json.loads(raw)
    except json.JSONDecodeError as error:
        raise AssertionError(f"invalid JSON response ({status}): {raw!r}") from error


def lost_submit(authority: str, token: str, path: str, body: dict[str, object]) -> None:
    connection = http.client.HTTPConnection(authority, timeout=8)
    raw = json.dumps(body).encode()
    connection.request(
        "POST",
        path,
        body=raw,
        headers={
            "Host": authority,
            "Origin": f"http://{authority}",
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
        },
    )
    response = connection.getresponse()
    if response.status != 202:
        message = response.read().decode(errors="replace")
        connection.close()
        raise AssertionError(f"initial submit returned {response.status}: {message}")
    # The browser loses the response body and retries the same request ID.
    connection.close()


def open_events(authority: str, token: str, path: str, timeout: float = 5):
    connection = http.client.HTTPConnection(authority, timeout=timeout)
    connection.request(
        "GET",
        path,
        headers={
            "Host": authority,
            "Origin": f"http://{authority}",
            "Authorization": f"Bearer {token}",
        },
    )
    response = connection.getresponse()
    if response.status != 200:
        body = response.read().decode(errors="replace")
        connection.close()
        raise AssertionError(f"event stream returned {response.status}: {body}")
    return connection, response


def read_sse_events(response: http.client.HTTPResponse, count: int, timeout: float = 6) -> list[dict[str, object]]:
    response.fp.raw._sock.settimeout(timeout)
    buffer = ""
    parsed: list[dict[str, object]] = []
    while len(parsed) < count:
        chunk = response.read1(8192)
        if not chunk:
            break
        buffer += chunk.decode("utf-8", errors="replace")
        while "\n\n" in buffer:
            frame, buffer = buffer.split("\n\n", 1)
            fields: dict[str, str] = {}
            data_lines: list[str] = []
            for line in frame.splitlines():
                if line.startswith(":") or ":" not in line:
                    continue
                key, value = line.split(":", 1)
                value = value[1:] if value.startswith(" ") else value
                if key == "data":
                    data_lines.append(value)
                else:
                    fields[key] = value
            if data_lines:
                parsed.append(
                    {
                        "event": fields.get("event", "message"),
                        "id": int(fields["id"]) if "id" in fields else None,
                        "data": json.loads("\n".join(data_lines)),
                    }
                )
                if len(parsed) >= count:
                    return parsed
    return parsed


def wait_for(predicate, description: str, timeout: float = 15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise TimeoutError(f"timed out waiting for {description}")


def all_sessions(authority: str, token: str) -> list[dict[str, object]]:
    status, body = request(authority, token, "GET", "/api/sessions")
    assert status == 200, body
    assert isinstance(body, dict)
    return body["sessions"]


def find_real_session(authority: str, token: str, name: str) -> str:
    sessions = all_sessions(authority, token)
    for session in sessions:
        if session.get("name") == name and not str(session.get("id", "")).startswith("draft-"):
            return str(session["id"])
    return ""


def snapshot(authority: str, token: str, session_id: str) -> dict[str, object]:
    status, body = request(authority, token, "GET", f"/api/sessions/{session_id}/snapshot")
    assert status == 200, body
    assert isinstance(body, dict)
    return body


def wait_for_receipt(authority: str, token: str, session_id: str, expected_id: int) -> dict[str, object]:
    return wait_for(
        lambda: (
            value
            if (value := snapshot(authority, token, session_id)).get("last_submission", {}).get("request_id") == expected_id
            and value.get("last_submission", {}).get("status") not in ("accepted", "running")
            and value.get("session", {}).get("run_state") == "idle"
            else None
        ),
        f"receipt {expected_id}",
    )


def fixture_request_count(provider_url: str) -> int:
    url = urlsplit(provider_url)
    connection = http.client.HTTPConnection(url.netloc, timeout=5)
    connection.request("GET", "/__trial/status")
    response = connection.getresponse()
    data = json.loads(response.read())
    connection.close()
    return int(data["requests"])


def child_pid(workspace: Path) -> int:
    return int((workspace / "trial-stalled-child.pid").read_text(encoding="utf-8").strip())


def pid_is_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def stop_process(process: subprocess.Popen[str], graceful: bool = True) -> None:
    if process.poll() is not None:
        return
    if graceful:
        process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=15)
            return
        except subprocess.TimeoutExpired:
            pass
    process.kill()
    process.wait(timeout=10)


def run(web_bin: Path, leg_bin: Path, supervisor_bin: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="leg-web-smoke-") as root_string:
        root = Path(root_string)
        workspace = root / "workspace"
        state_dir = root / "state"
        workspace.mkdir()
        state_dir.mkdir()
        provider, provider_url = start_provider(workspace)
        host: subprocess.Popen[str] | None = None
        restarted: subprocess.Popen[str] | None = None
        try:
            host, authority, token = start_host(
                web_bin, leg_bin, supervisor_bin, state_dir, workspace, provider_url
            )
            status, created = request(
                authority,
                token,
                "POST",
                "/api/sessions",
                {"name": "smoke", "cwd": str(workspace)},
            )
            assert status == 201 and isinstance(created, dict), created
            draft_id = str(created["id"])
            for tab_id in ("tab-one", "tab-two"):
                status, selected = request(
                    authority,
                    token,
                    "POST",
                    "/api/sessions/select",
                    {"session_id": draft_id, "tab_id": tab_id},
                )
                assert status == 200 and isinstance(selected, dict), selected

            first_prompt = "TRIAL-CHINESE\nLine one: keep this first.\n第二行：保留中文。\nLine three: keep this third."
            first_body = {"request_id": 1, "prompt": first_prompt}
            submit_path = f"/api/sessions/{draft_id}/submit"
            lost_submit(authority, token, submit_path, first_body)
            status, duplicate = request(authority, token, "POST", submit_path, first_body)
            assert status == 200 and duplicate["duplicate"] is True, duplicate
            session_id = wait_for(
                lambda: find_real_session(authority, token, "smoke"), "first leg session id"
            )
            first_snapshot = wait_for_receipt(authority, token, session_id, 1)
            assert first_snapshot["last_submission"]["status"] == "succeeded", first_snapshot
            assert fixture_request_count(provider_url) == 1, "duplicate submit reached provider twice"

            # Two independent tabs resolve to the same shared catalog session.
            for tab_id in ("tab-one", "tab-two"):
                status, selected = request(
                    authority,
                    token,
                    "POST",
                    "/api/sessions/select",
                    {"session_id": session_id, "tab_id": tab_id},
                )
                assert status == 200 and selected["id"] == session_id, selected

            cursor_before_pause = int(first_snapshot["cursor"])
            status, paused = request(
                authority,
                token,
                "POST",
                f"/api/sessions/{session_id}/submit",
                {"request_id": 2, "prompt": "TRIAL-PAUSE: stream text before the fixture pause."},
            )
            assert status == 202, paused
            connection, response = open_events(
                authority,
                token,
                f"/api/sessions/{session_id}/events?after={cursor_before_pause}",
            )
            first_events = read_sse_events(response, 2)
            assert first_events, "no catch-up event was delivered"
            last_seen = max(int(event["id"]) for event in first_events if event["id"] is not None)
            connection.close()
            live = snapshot(authority, token, session_id)
            assert live["active"] and live["active"]["text"], live
            requests_before_busy = fixture_request_count(provider_url)
            busy_status, busy = request(
                authority,
                token,
                "POST",
                f"/api/sessions/{session_id}/submit",
                {"request_id": 3, "prompt": "this turn must remain queued after a busy response"},
            )
            assert busy_status == 409 and busy["error"] == "session_busy", busy
            after_busy = snapshot(authority, token, session_id)
            assert after_busy["high_water"] == 2 and after_busy["next_request_id"] == 3, after_busy
            assert fixture_request_count(provider_url) == requests_before_busy
            wait_for_receipt(authority, token, session_id, 2)

            # Reconnect from the exact last cursor. Every replayed event is newer.
            connection, response = open_events(
                authority,
                token,
                f"/api/sessions/{session_id}/events?after={last_seen}",
            )
            replay = read_sse_events(response, 1)
            connection.close()
            assert replay and all(int(event["id"]) > last_seen for event in replay), replay

            status, tool_turn = request(
                authority,
                token,
                "POST",
                f"/api/sessions/{session_id}/submit",
                {"request_id": 3, "prompt": "TRIAL-TOOL-TEXT: write and confirm the fixture file."},
            )
            assert status == 202, tool_turn
            tool_snapshot = wait_for_receipt(authority, token, session_id, 3)
            assert tool_snapshot["last_submission"]["status"] == "succeeded", tool_snapshot
            assert (workspace / "fixture-write.txt").read_text(encoding="utf-8") == "fixture-write-ok\n"

            # A cursor older than the bounded buffer receives one authoritative reset.
            connection, response = open_events(
                authority,
                token,
                f"/api/sessions/{session_id}/events?after=0",
            )
            reset = read_sse_events(response, 1)
            connection.close()
            assert reset[0]["event"] == "reset", reset
            assert reset[0]["data"]["session"]["id"] == session_id

            # A provider stream that ends without a correlated turn outcome is never success.
            status, incomplete_turn = request(
                authority,
                token,
                "POST",
                f"/api/sessions/{session_id}/submit",
                {"request_id": 4, "prompt": "TRIAL-REOPEN-INTERRUPTION: close after partial text."},
            )
            assert status == 202, incomplete_turn
            incomplete = wait_for_receipt(authority, token, session_id, 4)
            assert incomplete["last_submission"]["status"] in ("failed", "incomplete"), incomplete
            assert incomplete["last_submission"]["status"] != "succeeded", incomplete
            assert incomplete["session"]["turns"][-1]["outcome"] != "succeeded", incomplete

            # Explicit Stop is repeatable and the owned shell child disappears.
            stop_cursor = int(snapshot(authority, token, session_id)["cursor"])
            status, running = request(
                authority,
                token,
                "POST",
                f"/api/sessions/{session_id}/submit",
                {"request_id": 5, "prompt": "TRIAL-STOP: start a stalled tool."},
            )
            assert status == 202, running
            wait_for(lambda: (workspace / "trial-stalled-child.pid").exists(), "stalled child PID")
            pid = child_pid(workspace)
            connection, response = open_events(
                authority,
                token,
                f"/api/sessions/{session_id}/events?after={stop_cursor}",
            )
            tool_events = read_sse_events(response, 4)
            assert any(
                event["data"].get("data", {}).get("event") == "tool_call"
                for event in tool_events
            ), tool_events
            last_seen = max(int(event["id"]) for event in tool_events if event["id"] is not None)
            connection.close()
            wait_for(
                lambda: snapshot(authority, token, session_id).get("active", {}).get("active_tool"),
                "active tool snapshot",
            )
            # Reconnect while the tool remains owned, then stop and receive only newer cursors.
            connection, response = open_events(
                authority,
                token,
                f"/api/sessions/{session_id}/events?after={last_seen}",
            )
            for _ in range(2):
                status, stopped = request(
                    authority, token, "POST", f"/api/sessions/{session_id}/stop"
                )
                assert status == 200 and stopped["status"] == "stop_requested", stopped
            resumed_events = read_sse_events(response, 1)
            connection.close()
            assert resumed_events and int(resumed_events[0]["id"]) > last_seen, resumed_events
            stopped_snapshot = wait_for_receipt(authority, token, session_id, 5)
            assert stopped_snapshot["last_submission"]["status"] in ("stopped", "incomplete"), stopped_snapshot
            wait_for(lambda: not pid_is_alive(pid), "Stop cleanup")
            assert not (workspace / "trial-stall-finished.txt").exists()

            # A hard controller crash closes the supervisor pipe and never replays.
            status, crash_turn = request(
                authority,
                token,
                "POST",
                f"/api/sessions/{session_id}/submit",
                {"request_id": 6, "prompt": "TRIAL-STOP: start another stalled tool."},
            )
            assert status == 202, crash_turn
            wait_for(lambda: (workspace / "trial-stalled-child.pid").exists(), "crash child PID")
            crash_pid = child_pid(workspace)
            before_restart = fixture_request_count(provider_url)
            stop_process(host, graceful=False)
            host = None
            wait_for(lambda: not pid_is_alive(crash_pid), "controller crash cleanup")

            restarted, new_authority, new_token = start_host(
                web_bin, leg_bin, supervisor_bin, state_dir, workspace, provider_url
            )
            assert new_token != token, "launch token did not rotate on restart"
            recovered = snapshot(new_authority, new_token, session_id)
            assert recovered["high_water"] == 6 and recovered["next_request_id"] == 7, recovered
            assert recovered["recovery_required"] is True, recovered
            assert recovered["last_submission"]["status"] == "incomplete", recovered
            assert fixture_request_count(provider_url) == before_restart, "restart replayed a prompt"
            wait_for(
                lambda: snapshot(new_authority, new_token, session_id)["session"]["run_state"] == "idle",
                "driver cleanup after restart",
            )

            # A new request ID explicitly retries the incomplete prompt; graceful shutdown stops it.
            status, shutdown_turn = request(
                new_authority,
                new_token,
                "POST",
                f"/api/sessions/{session_id}/submit",
                {"request_id": 7, "prompt": "TRIAL-STOP: start another stalled tool."},
            )
            assert status == 202, shutdown_turn
            wait_for(lambda: (workspace / "trial-stalled-child.pid").exists(), "shutdown child PID")
            shutdown_pid = child_pid(workspace)
            stop_process(restarted, graceful=True)
            restarted = None
            wait_for(lambda: not pid_is_alive(shutdown_pid), "graceful host shutdown cleanup")

            restarted, final_authority, final_token = start_host(
                web_bin, leg_bin, supervisor_bin, state_dir, workspace, provider_url
            )
            final = snapshot(final_authority, final_token, session_id)
            assert final["high_water"] == 7 and final["next_request_id"] == 8, final
            assert final["last_submission"]["status"] in ("stopped", "incomplete"), final
            assert fixture_request_count(provider_url) == before_restart + 1
        finally:
            if restarted is not None:
                stop_process(restarted)
            if host is not None:
                stop_process(host)
            provider.terminate()
            try:
                provider.wait(timeout=10)
            except subprocess.TimeoutExpired:
                provider.kill()
                provider.wait(timeout=5)


def main() -> int:
    if len(sys.argv) != 4:
        print("usage: lifecycle_smoke.py LEG_WEB_BIN LEG_BIN SUPERVISOR_BIN", file=sys.stderr)
        return 2
    run(*(Path(value).resolve() for value in sys.argv[1:]))
    print("leg-web lifecycle smoke passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
