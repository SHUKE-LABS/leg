#!/usr/bin/env python3
"""Measure leg-tui input-to-visible-cell responsiveness on native PTYs."""

from __future__ import annotations

import argparse
import bisect
import codecs
from contextlib import ExitStack
import importlib.util
import json
import math
import os
import platform
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from statistics import median
from typing import Any, Callable

import psutil
import pyte

if os.name == "nt":
    try:
        from winpty import Backend, PtyProcess
    except ImportError as error:  # pragma: no cover - Windows-only dependency
        raise SystemExit(
            "Windows ConPTY support requires pywinpty; install "
            "companions/leg-tui/tests/requirements.txt"
        ) from error
else:
    import fcntl
    import pty
    import struct
    import termios


ROOT = Path(__file__).resolve().parents[3]
FIXTURE_PATH = ROOT / "companions" / "trials" / "fake_provider.py"
_CREDENTIAL_ENV_VARS = (
    # Mirrors CREDENTIAL_ENV_VARS in src/config.rs.
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_API_KEY",
)
PICKER_HEADER = "Sessions · title · workspace · recent · status"
PALETTE_HEADER = "Command palette · F2/Ctrl-P"
COMPOSER_HINT = "Composer ("
INSPECTOR_HEADER = "Inspector · Up/Down field"
DIMENSIONS = ((24, 80), (40, 120))
SAMPLE_COUNT = 100
SAMPLE_TIMEOUT_SECONDS = 2.0
STREAM_PAIR_RENEWAL_MARGIN_SECONDS = 5.0
STREAM_PAIR_POOL_SIZE = 2
NARROW_STATUS_TITLE_WIDTH = 14
ACTIVE_SESSION_STATUS_LABELS = {"Starting", "Running", "Stopping"}
TRANSCRIPT_WINDOW_PATTERN = re.compile(r"Rows (\d+)-(\d+) of (\d+)")
MEMORY_SAMPLE_INTERVAL_SECONDS = 0.1
IDLE_MEMORY_WINDOW_SECONDS = 2.0


def load_fake_provider() -> Any:
    spec = importlib.util.spec_from_file_location("leg_responsiveness_fake_provider", FIXTURE_PATH)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load provider fixture at {FIXTURE_PATH}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _exchange_event(
    event: str,
    timestamp_ms: int,
    session_id: str,
    turn_index: int | None = None,
    **values: Any,
) -> dict[str, Any]:
    result: dict[str, Any] = {
        "schema": "baton.exchange/v1",
        "event": event,
        "ts_ms": timestamp_ms,
        "session_id": session_id,
    }
    if turn_index is not None:
        result["turn_index"] = turn_index
    result.update(values)
    return result


def _write_session(path: Path, events: list[dict[str, Any]]) -> None:
    path.write_text(
        "".join(json.dumps(event, ensure_ascii=False, separators=(",", ":")) + "\n" for event in events),
        encoding="utf-8",
    )
    try:
        path.chmod(0o600)
    except OSError:
        pass


def _validate_seeded_session_logs(
    leg_bin: Path, state_dir: Path, dataset: dict[str, Any], env: dict[str, str]
) -> None:
    session_paths = [Path(dataset["history_path"])]
    session_paths.extend(
        state_dir / "sessions" / f"{session_id}.jsonl"
        for session_id in dataset["stream_session_ids"].values()
    )
    for session_path in session_paths:
        try:
            result = subprocess.run(
                [str(leg_bin.resolve()), "log", "show", "--file", str(session_path.resolve())],
                cwd=ROOT,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=30,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise RuntimeError(
                f"leg log show timed out validating seeded session {session_path.name}"
            ) from error
        if result.returncode != 0:
            detail = result.stderr.strip() or f"exit status {result.returncode}"
            raise RuntimeError(f"leg rejected seeded session {session_path.name}: {detail}")


def _catalog_workspace_path(workspace: Path) -> str:
    resolved = str(workspace.resolve())
    if os.name != "nt":
        return resolved
    import ctypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    get_long_path_name = kernel32.GetLongPathNameW
    get_long_path_name.argtypes = (ctypes.c_wchar_p, ctypes.c_wchar_p, ctypes.c_uint32)
    get_long_path_name.restype = ctypes.c_uint32
    capacity = max(260, len(resolved) + 1)
    while True:
        buffer = ctypes.create_unicode_buffer(capacity)
        length = get_long_path_name(resolved, buffer, capacity)
        if length == 0:
            raise ctypes.WinError(ctypes.get_last_error())
        if length < capacity:
            resolved = buffer.value
            break
        capacity = length + 1
    if resolved.startswith("\\\\?\\"):
        return resolved
    if resolved.startswith("\\\\"):
        return "\\\\?\\UNC\\" + resolved[2:]
    return "\\\\?\\" + resolved


def seed_responsiveness_catalog(
    state_dir: Path,
    workspace: Path,
    turn_count: int = 1000,
    reply_bytes: int = 4096,
    long_answer_lines: int = 10_000,
    tool_result_bytes: int = 1024 * 1024,
) -> dict[str, Any]:
    """Write a deterministic catalog and the measured transcript fixtures."""
    if turn_count < 1 or reply_bytes < 1 or long_answer_lines < 1 or tool_result_bytes < 1:
        raise ValueError("dataset sizes must be positive")

    state_dir.mkdir(parents=True, exist_ok=True)
    sessions_dir = state_dir / "sessions"
    sessions_dir.mkdir(parents=True, exist_ok=True)
    workspace.mkdir(parents=True, exist_ok=True)
    workspace_cwd = _catalog_workspace_path(workspace)
    now_ms = int(time.time() * 1000)
    history_id = "responsiveness-history"
    stream_ids: dict[str, str] = {}
    stream_pair_pool: list[dict[str, Any]] = []

    events = [_exchange_event("session_start", now_ms, history_id)]
    for index in range(turn_count):
        event_ms = now_ms + index * 10 + 1
        prompt = f"history fixture prompt {index:04d}"
        prefix = f"History fixture reply {index:04d}: "
        if len(prefix.encode("utf-8")) > reply_bytes:
            raise ValueError("reply_bytes is too small for the history prefix")
        reply = prefix + ("r" * (reply_bytes - len(prefix.encode("utf-8"))))
        assert len(reply.encode("utf-8")) == reply_bytes
        events.extend(
            (
                _exchange_event(
                    "request",
                    event_ms,
                    history_id,
                    index,
                    model="fixture",
                    base_url="local",
                    prompt=prompt,
                ),
                _exchange_event(
                    "response_ok",
                    event_ms + 1,
                    history_id,
                    index,
                    reply=reply,
                    duration_ms=1,
                    stop_reason="end_turn",
                ),
            )
        )

    long_turn = turn_count
    long_start_ms = now_ms + long_turn * 10 + 1
    long_answer = "\n".join(
        f"Long answer line {line_index + 1:05d}: responsiveness fixture text"
        for line_index in range(long_answer_lines)
    )
    assert len(long_answer.splitlines()) == long_answer_lines
    events.extend(
        (
            _exchange_event(
                "request",
                long_start_ms,
                history_id,
                long_turn,
                model="fixture",
                base_url="local",
                prompt="long answer fixture",
            ),
            _exchange_event(
                "response_ok",
                long_start_ms + 1,
                history_id,
                long_turn,
                reply=long_answer,
                duration_ms=1,
                stop_reason="end_turn",
            ),
        )
    )

    tool_turn = turn_count + 1
    tool_start_ms = now_ms + tool_turn * 10 + 1
    tool_id = "responsiveness-large-tool-result"
    tool_result = "t" * tool_result_bytes
    assert len(tool_result.encode("utf-8")) == tool_result_bytes
    events.extend(
        (
            _exchange_event(
                "request",
                tool_start_ms,
                history_id,
                tool_turn,
                model="fixture",
                base_url="local",
                prompt="large tool result fixture",
            ),
            _exchange_event(
                "tool_round",
                tool_start_ms + 1,
                history_id,
                tool_turn,
                content=[
                    {
                        "type": "tool_use",
                        "id": tool_id,
                        "name": "bash",
                        "input": {"command": "print responsiveness fixture"},
                    }
                ],
            ),
            _exchange_event(
                "tool_call",
                tool_start_ms + 2,
                history_id,
                tool_turn,
                tool_use_id=tool_id,
                tool_name="bash",
                input={"command": "print responsiveness fixture"},
            ),
            _exchange_event(
                "tool_result",
                tool_start_ms + 3,
                history_id,
                tool_turn,
                tool_use_id=tool_id,
                tool_name="bash",
                status="completed",
                result=tool_result,
            ),
            _exchange_event(
                "response_ok",
                tool_start_ms + 4,
                history_id,
                tool_turn,
                reply="Large tool result fixture completed.",
                duration_ms=1,
                stop_reason="end_turn",
            ),
        )
    )
    _write_session(sessions_dir / f"{history_id}.jsonl", events)

    session_records: dict[str, dict[str, Any]] = {
        history_id: {
            "name": "History fixture",
            "cwd": workspace_cwd,
            "created_at_ms": now_ms,
            "updated_at_ms": now_ms + (turn_count + 2) * 10,
            "drafts": {"tui": ""},
            "display": {"tui_first_run_warning_acknowledged": True},
        }
    }
    for pool_index in range(STREAM_PAIR_POOL_SIZE):
        pair_number = pool_index + 1
        pool_sessions: dict[str, dict[str, str]] = {}
        for side in ("A", "B"):
            marker = f"RESPONSIVENESS-STREAM-{side}-PAIR-{pair_number:03d}"
            session_id = f"responsiveness-stream-{side.lower()}-pool-{pair_number:02d}"
            created_ms = now_ms - pool_index * 2 - (0 if side == "A" else 1)
            stream_events = [
                _exchange_event("session_start", created_ms, session_id),
                _exchange_event(
                    "request",
                    created_ms + 1,
                    session_id,
                    0,
                    model="fixture",
                    base_url="local",
                    prompt="background stream session seed",
                ),
                _exchange_event(
                    "response_ok",
                    created_ms + 2,
                    session_id,
                    0,
                    reply="Background stream fixture ready.",
                    duration_ms=1,
                    stop_reason="end_turn",
                ),
            ]
            _write_session(sessions_dir / f"{session_id}.jsonl", stream_events)
            session_records[session_id] = {
                "name": f"Stream {pair_number} {side}",
                "cwd": workspace_cwd,
                "created_at_ms": created_ms,
                "updated_at_ms": created_ms,
                "drafts": {"tui": marker},
                "display": {"tui_first_run_warning_acknowledged": True},
            }
            stream_ids[marker] = session_id
            pool_sessions[side] = {
                "marker": marker,
                "session_id": session_id,
                "title": f"Stream {pair_number} {side}",
            }
        stream_pair_pool.append({"pool_index": pool_index, "sessions": pool_sessions})

    catalog_path = state_dir / "catalog.json"
    catalog_path.write_text(
        json.dumps({"version": 1, "sessions": session_records}, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    try:
        catalog_path.chmod(0o600)
    except OSError:
        pass
    return {
        "base_turns": turn_count,
        "reply_bytes": reply_bytes,
        "long_answer_lines": long_answer_lines,
        "tool_result_bytes": tool_result_bytes,
        "history_session_id": history_id,
        "stream_session_ids": stream_ids,
        "stream_pair_pool": stream_pair_pool,
        "total_history_turns": turn_count + 2,
        "catalog_path": str(catalog_path),
        "history_path": str(sessions_dir / f"{history_id}.jsonl"),
    }


def p95(values: list[float]) -> float:
    if not values:
        raise ValueError("p95 requires at least one sample")
    ordered = sorted(values)
    return ordered[max(0, math.ceil(0.95 * len(ordered)) - 1)]


def summarize_latencies(samples: list[dict[str, Any]]) -> dict[str, Any]:
    values = [float(sample["latency_ms"]) for sample in samples]
    return {
        "sample_count": len(values),
        "p95_ms": round(p95(values), 3),
        "max_ms": round(max(values), 3),
    }


def provider_context(
    emissions: list[dict[str, int]], sample_ns: int, run_started_ns: int
) -> dict[str, float | int | None]:
    timestamps = [event["emitted_ns"] for event in emissions]
    index = bisect.bisect_right(timestamps, sample_ns)
    previous = emissions[index - 1] if index else None
    following = emissions[index] if index < len(emissions) else None
    previous_ns = previous["emitted_ns"] if previous else None
    following_ns = following["emitted_ns"] if following else None
    gap_ms = (
        (following_ns - previous_ns) / 1_000_000
        if previous_ns is not None and following_ns is not None
        else None
    )
    return {
        "emit_before_offset_ms": round((previous_ns - run_started_ns) / 1_000_000, 3)
        if previous_ns is not None
        else None,
        "emit_after_offset_ms": round((following_ns - run_started_ns) / 1_000_000, 3)
        if following_ns is not None
        else None,
        "emit_age_before_ms": round((sample_ns - previous_ns) / 1_000_000, 3)
        if previous_ns is not None
        else None,
        "emit_wait_after_ms": round((following_ns - sample_ns) / 1_000_000, 3)
        if following_ns is not None
        else None,
        "inter_emit_gap_ms": round(gap_ms, 3) if gap_ms is not None else None,
        "previous_chunk_index": previous["chunk_index"] if previous else None,
        "next_chunk_index": following["chunk_index"] if following else None,
    }


class ScreenCapture:
    def __init__(self, rows: int, columns: int) -> None:
        self.lock = threading.Lock()
        self.changed = threading.Event()
        self.rows = rows
        self.columns = columns
        self.raw = bytearray()
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.screen = pyte.Screen(columns, rows)
        self.stream = pyte.Stream(self.screen)

    def feed(self, data: bytes | str) -> None:
        raw = data.encode("utf-8", errors="replace") if isinstance(data, str) else data
        with self.lock:
            self.raw.extend(raw)
            decoded = self.decoder.decode(raw)
            if decoded:
                self.stream.feed(decoded)
        self.changed.set()

    def resize(self, rows: int, columns: int) -> None:
        with self.lock:
            self.rows = rows
            self.columns = columns
            self.screen.resize(rows, columns)
        self.changed.set()

    def text(self) -> str:
        with self.lock:
            return "\n".join(line.rstrip() for line in self.screen.display)

    def raw_size(self) -> int:
        with self.lock:
            return len(self.raw)


class TerminalProcess:
    def __init__(
        self,
        command: list[str],
        cwd: Path,
        env: dict[str, str],
        rows: int,
        columns: int,
    ) -> None:
        self.capture = ScreenCapture(rows, columns)
        self._stop_reader = threading.Event()
        self.master_fd: int | None = None
        self.slave_fd: int | None = None
        self.process: Any
        if os.name == "nt":
            self.process = PtyProcess.spawn(
                command,
                cwd=str(cwd),
                env=env,
                dimensions=(rows, columns),
                backend=Backend.ConPTY,
            )
        else:
            self.master_fd, self.slave_fd = pty.openpty()
            fcntl.ioctl(
                self.slave_fd,
                termios.TIOCSWINSZ,
                struct.pack("HHHH", rows, columns, 0, 0),
            )
            self.process = subprocess.Popen(
                command,
                stdin=self.slave_fd,
                stdout=self.slave_fd,
                stderr=self.slave_fd,
                cwd=str(cwd),
                env=env,
                start_new_session=True,
                close_fds=True,
            )
            os.close(self.slave_fd)
            self.slave_fd = None
        self.reader = threading.Thread(target=self._read_output, name="leg-tui-terminal-reader", daemon=True)
        self.reader.start()

    @property
    def pid(self) -> int:
        value = getattr(self.process, "pid", None)
        if value is None:
            raise RuntimeError("terminal process did not expose its child PID")
        return int(value)

    def alive(self) -> bool:
        if os.name == "nt":
            return bool(self.process.isalive())
        return self.process.poll() is None

    def _read_output(self) -> None:
        if os.name == "nt":
            while not self._stop_reader.is_set():
                try:
                    chunk = self.process.read(4096)
                except (EOFError, OSError, ValueError):
                    break
                if chunk:
                    self.capture.feed(chunk)
                elif not self.alive():
                    break
                else:
                    time.sleep(0.005)
            return

        assert self.master_fd is not None
        while not self._stop_reader.is_set():
            try:
                chunk = os.read(self.master_fd, 8192)
            except OSError:
                break
            if not chunk:
                break
            self.capture.feed(chunk)

    def write(self, value: str) -> None:
        if os.name == "nt":
            remaining = value
            characters_written = 0
            while remaining:
                encoded = remaining.encode("utf-8")
                written = self.process.write(remaining)
                if not isinstance(written, int) or written <= 0 or written > len(encoded):
                    raise RuntimeError(
                        f"Windows ConPTY write made invalid progress: {written} bytes for {len(encoded)} bytes"
                    )
                if written == len(encoded):
                    written_text = remaining
                else:
                    try:
                        # pywinpty reports bytes written while this API accepts text.
                        written_text = encoded[:written].decode("utf-8")
                    except UnicodeDecodeError as error:
                        raise RuntimeError(
                            "Windows ConPTY partial write ended inside a UTF-8 character"
                        ) from error
                if not written_text:
                    raise RuntimeError("Windows ConPTY write made no text progress")
                characters_written += len(written_text)
                remaining = remaining[len(written_text) :]
            if characters_written != len(value):
                raise RuntimeError(
                    f"Windows ConPTY wrote {characters_written} of {len(value)} input characters"
                )
        else:
            assert self.master_fd is not None
            data = value.encode("utf-8")
            written = 0
            while written < len(data):
                written += os.write(self.master_fd, data[written:])

    def write_control(self, key: str) -> None:
        if len(key) != 1 or not ("a" <= key.lower() <= "z"):
            raise ValueError("control key must be one ASCII letter")
        if os.name == "nt":
            self.process.sendcontrol(key.lower())
        else:
            self.write(chr(ord(key.lower()) - ord("a") + 1))

    def wait_for(
        self,
        predicate: Callable[[str], bool],
        description: str,
        timeout: float = 5.0,
    ) -> tuple[str, int]:
        deadline = time.monotonic() + timeout
        while True:
            text = self.capture.text()
            if predicate(text):
                return text, time.perf_counter_ns()
            if not self.alive():
                self.reader.join(timeout=0.2)
                text = self.capture.text()
                if predicate(text):
                    return text, time.perf_counter_ns()
                raise RuntimeError(
                    f"TUI exited before {description}; current screen:\n{text[-3000:]}"
                )
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"timed out waiting for {description}; screen:\n{text[-3000:]}")
            self.capture.changed.wait(min(remaining, 0.1))
            self.capture.changed.clear()

    def wait_contains(self, expected: str, timeout: float = 5.0) -> tuple[str, int]:
        return self.wait_for(
            lambda text: expected in text,
            repr(expected),
            timeout,
        )

    def resize(self, rows: int, columns: int) -> None:
        previous_raw_size = self.capture.raw_size()
        self.capture.resize(rows, columns)
        self.capture.changed.clear()
        if os.name == "nt":
            self.process.setwinsize(rows, columns)
        else:
            assert self.master_fd is not None
            fcntl.ioctl(
                self.master_fd,
                termios.TIOCSWINSZ,
                struct.pack("HHHH", rows, columns, 0, 0),
            )
        deadline = time.monotonic() + 2.0
        while self.capture.raw_size() == previous_raw_size and time.monotonic() < deadline:
            self.capture.changed.wait(min(0.05, max(0.0, deadline - time.monotonic())))
            self.capture.changed.clear()

    def wait_exit(self, timeout: float = 5.0) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if not self.alive():
                return True
            time.sleep(0.02)
        return not self.alive()

    def close(self) -> None:
        self._stop_reader.set()
        if os.name == "nt":
            if self.alive():
                terminate_process_tree(self.pid)
            try:
                self.process.close(force=True)
            except (AttributeError, TypeError, OSError):
                try:
                    self.process.terminate(force=True)
                except (AttributeError, TypeError, OSError):
                    pass
        elif self.alive():
            try:
                os.killpg(self.process.pid, signal.SIGTERM)
                self.process.wait(timeout=2)
            except (ProcessLookupError, subprocess.TimeoutExpired):
                try:
                    os.killpg(self.process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                self.process.wait(timeout=2)
        if os.name != "nt":
            try:
                self.process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                pass
        if self.master_fd is not None:
            try:
                os.close(self.master_fd)
            except OSError:
                pass
            self.master_fd = None
        if self.slave_fd is not None:
            try:
                os.close(self.slave_fd)
            except OSError:
                pass
            self.slave_fd = None
        self.reader.join(timeout=1)


def terminate_process_tree(root_pid: int) -> None:
    try:
        root = psutil.Process(root_pid)
        children = root.children(recursive=True)
    except psutil.Error:
        return
    processes = children + [root]
    for process in processes:
        try:
            process.terminate()
        except psutil.Error:
            pass
    _, alive = psutil.wait_procs(processes, timeout=2)
    for process in alive:
        try:
            process.kill()
        except psutil.Error:
            pass
    if alive:
        psutil.wait_procs(alive, timeout=2)


class ProcessTreeRssSampler:
    def __init__(self, root_pid: int) -> None:
        self.root_pid = root_pid
        self.samples: list[tuple[int, int]] = []
        self.lock = threading.Lock()
        self.stop_event = threading.Event()
        self.thread = threading.Thread(target=self._run, name="leg-tui-rss-sampler", daemon=True)

    def start(self) -> None:
        self.thread.start()

    def _sample(self) -> int:
        try:
            root = psutil.Process(self.root_pid)
            processes = [root, *root.children(recursive=True)]
        except psutil.Error:
            return 0
        total = 0
        for process in processes:
            try:
                total += process.memory_info().rss
            except psutil.Error:
                continue
        return total

    def _run(self) -> None:
        while not self.stop_event.is_set():
            sample = (time.perf_counter_ns(), self._sample())
            with self.lock:
                self.samples.append(sample)
            self.stop_event.wait(MEMORY_SAMPLE_INTERVAL_SECONDS)

    def stop(self) -> None:
        self.stop_event.set()
        self.thread.join(timeout=2)

    def values_between(self, start_ns: int, end_ns: int) -> list[int]:
        with self.lock:
            return [
                rss
                for sampled_ns, rss in self.samples
                if start_ns <= sampled_ns < end_ns and rss > 0
            ]

    def all_values(self) -> list[int]:
        with self.lock:
            return [rss for _, rss in self.samples if rss > 0]


class FixtureServer:
    def __init__(self, workspace: Path) -> None:
        self.module = load_fake_provider()
        self.fixture = self.module.Fixture("responsiveness", workspace)
        self.fixture.prepare_hook()
        handler = type("ResponsivenessFixtureHandler", (self.module.Handler,), {"fixture": self.fixture})
        self.server = self.module.LoopbackThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.server.daemon_threads = True
        host, port = self.server.server_address
        self.base_url = f"http://{host}:{port}"
        self.thread = threading.Thread(target=self.server.serve_forever, name="leg-tui-fixture", daemon=True)
        self.thread.start()
        self.environment = self.module.environment(self.base_url, "responsiveness", self.fixture.denial_hook)

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


def _build_harness_environment(
    parent_environment: dict[str, str],
    fixture_environment: dict[str, str],
    state_dir: Path,
    supervisor_bin: Path,
) -> dict[str, str]:
    environment = parent_environment.copy()
    for name in _CREDENTIAL_ENV_VARS:
        environment.pop(name, None)
    environment.update(fixture_environment)
    environment.update(
        {
            "TERM": "xterm-256color",
            "LEG_UI_STATE_DIR": str(state_dir),
            "LEG_UI_SUPERVISOR_BIN": str(supervisor_bin.resolve()),
        }
    )
    return environment


def _provider_summary(fixture_server: FixtureServer, run_started_ns: int) -> dict[str, Any]:
    fixture = fixture_server.fixture
    module = fixture_server.module
    timings = fixture.status()["responsiveness_streams"]
    streams: dict[str, Any] = {}
    for stream_id, events in fixture.stream_emissions.items():
        timestamps = [event["emitted_ns"] for event in events]
        intervals_ms = [
            (right - left) / 1_000_000
            for left, right in zip(timestamps, timestamps[1:])
        ]
        timing = timings.get(stream_id, {})
        streams[stream_id] = {
            "request": timing.get("request"),
            "chunk_count": len(events),
            "chunk_bytes": module.RESPONSIVENESS_CHUNK_BYTES,
            "configured_chunks_per_second": module.RESPONSIVENESS_CHUNKS_PER_SECOND,
            "configured_chunk_interval_ms": module.RESPONSIVENESS_CHUNK_INTERVAL_MS,
            "started_offset_ms": round((timing.get("started_ns", 0) - run_started_ns) / 1_000_000, 3),
            "ended_offset_ms": round((timing.get("ended_ns", 0) - run_started_ns) / 1_000_000, 3),
            "actual_duration_ms": round(
                (timing.get("ended_ns", 0) - timing.get("started_ns", 0)) / 1_000_000,
                3,
            ),
            "actual_inter_emit_interval_ms": {
                "min": round(min(intervals_ms), 3) if intervals_ms else None,
                "p50": round(median(intervals_ms), 3) if intervals_ms else None,
                "p95": round(p95(intervals_ms), 3) if intervals_ms else None,
                "max": round(max(intervals_ms), 3) if intervals_ms else None,
            },
            "emit_offsets_ms": [
                round((timestamp - run_started_ns) / 1_000_000, 3)
                for timestamp in timestamps
            ],
        }
    return {
        "scenario": "responsiveness",
        "stream_count": len(streams),
        "stream_pair_count": len(streams) // 2,
        "streams": streams,
        "timestamps_share_the_harness_monotonic_clock": True,
    }


def _cpu_model() -> str:
    if os.name == "nt":
        try:
            import winreg

            with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, r"HARDWARE\DESCRIPTION\System\CentralProcessor\0") as key:
                return str(winreg.QueryValueEx(key, "ProcessorNameString")[0]).strip()
        except OSError:
            pass
    if sys.platform.startswith("linux"):
        try:
            for line in Path("/proc/cpuinfo").read_text(encoding="utf-8").splitlines():
                if line.lower().startswith("model name"):
                    return line.partition(":")[2].strip()
        except OSError:
            pass
    value = platform.processor().strip()
    if value:
        return value
    return platform.machine() or "unknown"


def _terminal_version() -> str:
    if os.name == "nt":
        version = sys.getwindowsversion()
        return f"ConPTY on Windows build {version.build}"
    return f"kernel PTY on {platform.system()} {platform.release()}"


def _git_revision() -> str:
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()


def _build_profile(args: argparse.Namespace) -> str:
    if args.build_profile:
        return args.build_profile
    paths = (args.tui_bin, args.leg_bin, args.supervisor_bin)
    return "release" if any("release" in Path(path).parts for path in paths) else "debug"


def _wait_fixture_requests(
    fixture: Any,
    count: int,
    timeout: float = 8.0,
    terminal: TerminalProcess | None = None,
) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if fixture.status()["requests"] >= count:
            return
        time.sleep(0.01)
    screen = f"\ncurrent TUI screen:\n{terminal.capture.text()[-3000:]}" if terminal else ""
    raise TimeoutError(
        f"provider fixture saw fewer than {count} requests: {fixture.status()}{screen}"
    )


def _transcript_window(text: str) -> tuple[int, int, int] | None:
    match = TRANSCRIPT_WINDOW_PATTERN.search(text)
    return tuple(map(int, match.groups())) if match else None


def _restore_transcript_bottom(terminal: TerminalProcess) -> None:
    terminal.write("\x1b[1;5F")  # Ctrl-End returns to the newest transcript rows.

    def at_bottom(text: str) -> bool:
        window = _transcript_window(text)
        return window is not None and window[1] == window[2]

    terminal.wait_for(at_bottom, "transcript to return to its newest rows", timeout=2.0)


def _position_transcript_for_scrolling(terminal: TerminalProcess) -> None:
    for attempt in range(4):
        window = _transcript_window(terminal.capture.text())
        if window is None:
            raise RuntimeError("transcript row counter is not visible before history scrolling")
        first, last, total = window
        page_rows = last - first + 1
        if first - 1 >= 2 * page_rows and total - last >= 2 * page_rows:
            return

        key_name, key = (
            ("PageDown", "\x1b[6~")
            if first - 1 < 2 * page_rows
            else ("PageUp", "\x1b[5~")
        )
        terminal.write(key)
        try:
            terminal.wait_for(
                lambda text, previous=window: _transcript_window(text) != previous,
                f"untimed {key_name} preposition {attempt} from {first}-{last} of {total}",
                timeout=2.0,
            )
        except TimeoutError:
            after = _transcript_window(terminal.capture.text())
            after_text = (
                f"{after[0]}-{after[1]} of {after[2]}"
                if after is not None
                else "<counter unavailable>"
            )
            raise RuntimeError(
                f"history scroll preposition {key_name} {attempt}: "
                f"rows {first}-{last} of {total} -> {after_text}"
            ) from None
    else:
        window = _transcript_window(terminal.capture.text())
        position = (
            f"{window[0]}-{window[1]} of {window[2]}"
            if window is not None
            else "<counter unavailable>"
        )
        raise RuntimeError(
            "could not position transcript away from both scroll boundaries: "
            f"rows {position}"
        )


def _visible_status_title(title: str) -> str:
    if len(title) > NARROW_STATUS_TITLE_WIDTH:
        return title[: NARROW_STATUS_TITLE_WIDTH - 1]
    return title


def _session_title_visible(text: str, title: str) -> bool:
    visible_title = _visible_status_title(title)
    return any(
        line.startswith("status:") and f"|  {visible_title}" in line
        for line in text.splitlines()
    )


def _session_is_inactive(text: str, title: str) -> bool:
    visible_title = _visible_status_title(title)
    for line in text.splitlines():
        if not line.startswith("status:") or f"|  {visible_title}" not in line:
            continue
        status = line[len("status:") :].split("  |", 1)[0].strip()
        return status not in ACTIVE_SESSION_STATUS_LABELS
    return False


def _select_session(
    terminal: TerminalProcess,
    title: str,
    already_in_picker: bool = False,
) -> tuple[int, int]:
    if not already_in_picker:
        terminal.write("\x1bOR")  # F3
        terminal.wait_contains(PICKER_HEADER)
    terminal.write("/" + title)
    terminal.wait_contains("Filter: " + title)
    terminal.write("\r")
    terminal.wait_contains(PICKER_HEADER)
    selected_at = time.perf_counter_ns()
    terminal.write("\r")
    _, visible_at = terminal.wait_for(
        lambda text: _session_title_visible(text, title),
        f"selected session title {title}",
        timeout=10.0,
    )
    return selected_at, visible_at


class ResponsivenessStreamPairManager:
    def __init__(
        self,
        terminal: TerminalProcess,
        fixture_server: FixtureServer,
        stream_pair_pool: list[dict[str, Any]],
    ) -> None:
        if len(stream_pair_pool) < 2:
            raise ValueError("the responsiveness harness needs at least two reusable stream pairs")
        self.terminal = terminal
        self.fixture = fixture_server.fixture
        self.module = fixture_server.module
        self.stream_pair_pool = stream_pair_pool
        self.pairs: list[dict[str, Any]] = []
        self.active_pair: dict[str, Any] | None = None

    def start_next_pair(
        self,
        before_switch: Callable[[], None] | None = None,
        after_restore: Callable[[], None] | None = None,
    ) -> dict[str, Any]:
        if before_switch is not None:
            before_switch()
        if self.active_pair is not None:
            self._wait_for_pair_requests(self.active_pair)

        pair_number = len(self.pairs) + 1
        pair_id = f"pair-{pair_number:03d}"
        pool = self.stream_pair_pool[(pair_number - 1) % len(self.stream_pair_pool)]
        previous_markers = pool.get("last_markers", {})
        self._wait_for_pool_requests(previous_markers)
        stream_ids = {
            side: f"RESPONSIVENESS-STREAM-{side}-PAIR-{pair_number:03d}"
            for side in ("A", "B")
        }

        for side in ("A", "B"):
            session = pool["sessions"][side]
            previous_marker = previous_markers.get(side)
            _select_session(self.terminal, session["title"])
            requests_before = self.fixture.status()["requests"]
            if previous_marker:
                self.terminal.write_control("u")
                self.terminal.write(stream_ids[side])
            self.terminal.write_control("s")
            _wait_fixture_requests(self.fixture, requests_before + 1, terminal=self.terminal)
            previous_markers[side] = stream_ids[side]
        pool["last_markers"] = previous_markers

        _wait_for_stream_pair(self.fixture, stream_ids.values())
        timings = self.fixture.status()["responsiveness_streams"]
        overlap_start_ns = max(timings[stream_id]["started_ns"] for stream_id in stream_ids.values())
        duration_ns = self.module.RESPONSIVENESS_DURATION_SECONDS * 1_000_000_000
        planned_overlap_end_ns = min(
            timings[stream_id]["started_ns"] + duration_ns - 100_000_000
            for stream_id in stream_ids.values()
        )
        if planned_overlap_end_ns <= overlap_start_ns:
            raise RuntimeError(f"provider stream pair {pair_id} has no planned overlap")

        pair = {
            "pair_id": pair_id,
            "stream_ids": list(stream_ids.values()),
            "pool_index": pool["pool_index"],
            "overlap_start_ns": overlap_start_ns,
            "planned_overlap_end_ns": planned_overlap_end_ns,
        }
        self.pairs.append(pair)
        self.active_pair = pair
        _select_session(self.terminal, "History fixture")
        if after_restore is not None:
            after_restore()
        else:
            _restore_transcript_bottom(self.terminal)
        return pair

    def pair_for_next_sample(
        self,
        before_switch: Callable[[], None] | None = None,
        after_restore: Callable[[], None] | None = None,
    ) -> dict[str, Any]:
        now_ns = time.perf_counter_ns()
        renewal_margin_ns = int(STREAM_PAIR_RENEWAL_MARGIN_SECONDS * 1_000_000_000)
        if (
            self.active_pair is None
            or not self._pair_requests_active(self.active_pair)
            or now_ns + renewal_margin_ns >= self.active_pair["planned_overlap_end_ns"]
        ):
            return self.start_next_pair(before_switch, after_restore)
        return self.active_pair

    def _pair_requests_active(self, pair: dict[str, Any]) -> bool:
        status = self.fixture.status()
        timings = status["responsiveness_streams"]
        active_requests = set(status["active_requests"])
        return all(
            stream_id in timings and timings[stream_id]["request"] in active_requests
            for stream_id in pair["stream_ids"]
        )

    def _wait_for_pool_requests(self, previous_markers: dict[str, str]) -> None:
        if not previous_markers:
            return
        status = self.fixture.status()
        previous_requests = {
            status["responsiveness_streams"][marker]["request"]
            for marker in previous_markers.values()
            if marker in status["responsiveness_streams"]
        }
        self._wait_for_requests(previous_requests, f"stream pool requests did not finish before reuse: {previous_markers}")

    def _wait_for_pair_requests(self, pair: dict[str, Any]) -> None:
        status = self.fixture.status()
        timings = status["responsiveness_streams"]
        requests = {
            int(timings[stream_id]["request"])
            for stream_id in pair["stream_ids"]
            if stream_id in timings
        }
        self._wait_for_requests(requests, f"stream pair {pair['pair_id']} did not finish")

    def _wait_for_requests(self, request_ids: set[int], error: str) -> None:
        deadline = time.monotonic() + self.module.RESPONSIVENESS_DURATION_SECONDS + 5.0
        while request_ids.intersection(self.fixture.status()["active_requests"]):
            if time.monotonic() >= deadline:
                raise TimeoutError(f"{error}: {self.fixture.status()}")
            time.sleep(0.02)

    def wait_for_all_pairs(self) -> None:
        request_ids: set[int] = set()
        status = self.fixture.status()
        for pair in self.pairs:
            for stream_id in pair["stream_ids"]:
                timing = status["responsiveness_streams"].get(stream_id)
                if timing is None:
                    raise RuntimeError(f"provider fixture did not record stream {stream_id}")
                request_ids.add(int(timing["request"]))
        deadline = time.monotonic() + self.module.RESPONSIVENESS_DURATION_SECONDS + 5.0
        while request_ids.intersection(self.fixture.status()["active_requests"]):
            if time.monotonic() >= deadline:
                raise TimeoutError(f"provider stream pairs did not finish: {self.fixture.status()}")
            time.sleep(0.02)

    def wait_for_tui_sessions_to_finish(self) -> None:
        for pool in self.stream_pair_pool:
            for session in pool["sessions"].values():
                title = session["title"]
                _select_session(self.terminal, title)
                self.terminal.wait_for(
                    lambda text, session_title=title: _session_is_inactive(text, session_title),
                    f"stream session {title} to finish in the TUI",
                    timeout=10.0,
                )
        _select_session(self.terminal, "History fixture")
        _restore_transcript_bottom(self.terminal)


def _wait_for_stream_pair(fixture: Any, stream_ids: Any, timeout: float = 8.0) -> None:
    expected = tuple(stream_ids)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        status = fixture.status()
        timings = status["responsiveness_streams"]
        active = set(status["active_requests"])
        if all(
            stream_id in timings
            and fixture.stream_emissions.get(stream_id)
            and timings[stream_id]["request"] in active
            for stream_id in expected
        ):
            return
        time.sleep(0.01)
    raise TimeoutError(f"provider stream pair {expected} did not become active: {fixture.status()}")


def _measure_sample(
    terminal: TerminalProcess,
    action: str,
    dimension: dict[str, int],
    index: int,
    payload: str,
    predicate: Callable[[str], bool],
    description: str,
    timeout: float = SAMPLE_TIMEOUT_SECONDS,
) -> dict[str, Any]:
    injected_ns = time.perf_counter_ns()
    terminal.write(payload)
    _, visible_ns = terminal.wait_for(predicate, description, timeout=timeout)
    return {
        "action": action,
        "sample_index": index,
        "dimensions": dict(dimension),
        "input_monotonic_ns": injected_ns,
        "visible_monotonic_ns": visible_ns,
        "latency_ms": round((visible_ns - injected_ns) / 1_000_000, 3),
    }


def _measure_streamed_sample(
    stream_pairs: ResponsivenessStreamPairManager,
    terminal: TerminalProcess,
    action: str,
    dimension: dict[str, int],
    index: int,
    payload: str,
    predicate: Callable[[str], bool],
    description: str,
    before_pair_switch: Callable[[], None] | None = None,
    after_pair_restore: Callable[[], None] | None = None,
    before_injection: Callable[[], None] | None = None,
) -> dict[str, Any]:
    pair = stream_pairs.pair_for_next_sample(before_pair_switch, after_pair_restore)
    if before_injection is not None:
        before_injection()
    sample = _measure_sample(
        terminal,
        action,
        dimension,
        index,
        payload,
        predicate,
        description,
    )
    for timestamp_key in ("input_monotonic_ns", "visible_monotonic_ns"):
        timestamp = int(sample[timestamp_key])
        if not pair["overlap_start_ns"] <= timestamp < pair["planned_overlap_end_ns"]:
            raise RuntimeError(
                f"{action} sample {index} did not fit stream pair {pair['pair_id']} overlap "
                f"at {timestamp_key}={timestamp}"
            )
    sample["stream_pair_id"] = pair["pair_id"]
    return sample


def _measure_dimension(
    terminal: TerminalProcess,
    rows: int,
    columns: int,
    sample_count: int,
    stream_pairs: ResponsivenessStreamPairManager,
) -> dict[str, list[dict[str, Any]]]:
    dimension = {"rows": rows, "columns": columns}
    suffix = f"{columns}x{rows}"
    groups: dict[str, list[dict[str, Any]]] = {
        f"{action}@{suffix}": []
        for action in ("draft_editing", "history_scrolling", "palette_filtering", "inspection")
    }

    for index in range(sample_count):
        marker = f"draft{index:03d}"
        groups[f"draft_editing@{suffix}"].append(
            _measure_streamed_sample(
                stream_pairs,
                terminal,
                "draft_editing",
                dimension,
                index,
                marker,
                lambda text, expected=marker: expected in text,
                f"draft text {marker}",
            )
        )
        terminal.write("\x7f" * len(marker))
        terminal.wait_for(
            lambda text, expected=marker: expected not in text,
            f"cleared draft text {marker}",
            timeout=2.0,
        )

    _position_transcript_for_scrolling(terminal)

    for index in range(sample_count):
        before: tuple[int, int, int] | None = None

        def capture_history_scroll_baseline() -> None:
            nonlocal before
            before = _transcript_window(terminal.capture.text())
            if before is None:
                raise RuntimeError(
                    f"history scroll sample {index}: transcript row counter is unavailable"
                )
            first, last, total = before
            page_rows = last - first + 1
            if first - 1 < page_rows or total - last < page_rows:
                raise RuntimeError(
                    f"history scroll sample {index} starts at a boundary: "
                    f"rows {first}-{last} of {total}"
                )

        key_name, key = (
            ("PageUp", "\x1b[5~") if index % 2 == 0 else ("PageDown", "\x1b[6~")
        )
        try:
            sample = _measure_streamed_sample(
                stream_pairs,
                terminal,
                "history_scrolling",
                dimension,
                index,
                key,
                lambda text: _transcript_window(text) != before,
                f"{key_name} sample {index} transcript row counter change",
                after_pair_restore=lambda: _position_transcript_for_scrolling(terminal),
                before_injection=capture_history_scroll_baseline,
            )
        except TimeoutError:
            after = _transcript_window(terminal.capture.text())
            before_text = (
                f"{before[0]}-{before[1]} of {before[2]}"
                if before is not None
                else "<counter unavailable>"
            )
            after_text = (
                f"{after[0]}-{after[1]} of {after[2]}"
                if after is not None
                else "<counter unavailable>"
            )
            raise TimeoutError(
                f"{key_name} sample {index} rows "
                f"{before_text} -> {after_text} did not change"
            ) from None
        groups[f"history_scrolling@{suffix}"].append(sample)

    def close_palette() -> None:
        terminal.write("\x1b")
        terminal.wait_for(lambda text: PALETTE_HEADER not in text, "palette to close")

    def open_palette() -> None:
        _restore_transcript_bottom(terminal)
        terminal.write_control("p")  # Ctrl-P
        terminal.wait_contains(PALETTE_HEADER)

    def clear_palette() -> None:
        terminal.write_control("u")  # Ctrl-U clears the prior query.

    open_palette()
    for index in range(sample_count):
        query = f"q{index:03d}"
        groups[f"palette_filtering@{suffix}"].append(
            _measure_streamed_sample(
                stream_pairs,
                terminal,
                "palette_filtering",
                dimension,
                index,
                query,
                lambda text, expected=query: expected in text,
                f"palette query {query}",
                before_pair_switch=close_palette,
                after_pair_restore=open_palette,
                before_injection=clear_palette,
            )
        )
    close_palette()

    for index in range(sample_count):
        groups[f"inspection@{suffix}"].append(
            _measure_streamed_sample(
                stream_pairs,
                terminal,
                "inspection",
                dimension,
                index,
                "\x1bOS",  # F4
                lambda text: INSPECTOR_HEADER in text,
                "inspector header",
            )
        )
        terminal.write("\x1b")
        terminal.wait_for(
            lambda text: INSPECTOR_HEADER not in text,
            "inspector to close",
        )
    return groups


def _decorate_sample_provider_context(
    groups: dict[str, list[dict[str, Any]]],
    fixture: Any,
    run_started_ns: int,
    stream_pairs: list[dict[str, Any]],
) -> None:
    pairs_by_id = {pair["pair_id"]: pair for pair in stream_pairs}
    for samples in groups.values():
        for sample in samples:
            input_ns = int(sample.pop("input_monotonic_ns"))
            visible_ns = int(sample.pop("visible_monotonic_ns"))
            pair = pairs_by_id[sample["stream_pair_id"]]
            sample["input_offset_ms"] = round((input_ns - run_started_ns) / 1_000_000, 3)
            sample["visible_offset_ms"] = round((visible_ns - run_started_ns) / 1_000_000, 3)
            sample["provider_fixture_context"] = {
                stream_id: provider_context(
                    fixture.stream_emissions[stream_id], input_ns, run_started_ns
                )
                for stream_id in pair["stream_ids"]
            }


def _check_workload_overlap(
    groups: dict[str, list[dict[str, Any]]],
    fixture_server: FixtureServer,
    stream_pairs: list[dict[str, Any]],
) -> None:
    fixture = fixture_server.fixture
    module = fixture_server.module
    status = fixture.status()
    timings = status["responsiveness_streams"]
    pairs_by_id: dict[str, dict[str, Any]] = {}
    for pair in stream_pairs:
        stream_ids = pair["stream_ids"]
        if any(stream_id not in timings for stream_id in stream_ids):
            raise RuntimeError(f"provider fixture did not record both streams for {pair['pair_id']}: {status}")
        if any(
            timings[stream_id]["chunks_emitted"] != module.RESPONSIVENESS_CHUNK_COUNT
            for stream_id in stream_ids
        ):
            raise RuntimeError(f"provider stream pair {pair['pair_id']} did not emit all configured chunks")
        overlap_start = max(timings[stream_id]["started_ns"] for stream_id in stream_ids)
        overlap_end = min(timings[stream_id]["ended_ns"] for stream_id in stream_ids)
        if overlap_end <= overlap_start:
            raise RuntimeError(f"provider stream pair {pair['pair_id']} did not overlap")
        pair["overlap_start_ns"] = overlap_start
        pair["overlap_end_ns"] = overlap_end
        pairs_by_id[pair["pair_id"]] = pair

    for samples in groups.values():
        for sample in samples:
            injected = int(sample["input_monotonic_ns"])
            visible = int(sample["visible_monotonic_ns"])
            pair_id = sample.get("stream_pair_id")
            pair = pairs_by_id.get(pair_id)
            if pair is None:
                raise RuntimeError(
                    f"{sample['action']} sample {sample['sample_index']} has no valid stream pair id: {pair_id}"
                )
            if not (
                pair["overlap_start_ns"] <= injected < pair["overlap_end_ns"]
                and pair["overlap_start_ns"] <= visible < pair["overlap_end_ns"]
            ):
                raise RuntimeError(
                    f"{sample['action']} sample {sample['sample_index']} was outside stream pair "
                    f"{pair_id} overlap (input={injected}, visible={visible}, "
                    f"window={pair['overlap_start_ns']}..{pair['overlap_end_ns']})"
                )


def _stream_pair_summaries(
    stream_pairs: list[dict[str, Any]], run_started_ns: int
) -> list[dict[str, Any]]:
    return [
        {
            "pair_id": pair["pair_id"],
            "stream_ids": pair["stream_ids"],
            "overlap_start_offset_ms": round(
                (pair["overlap_start_ns"] - run_started_ns) / 1_000_000, 3
            ),
            "overlap_end_offset_ms": round(
                (pair["overlap_end_ns"] - run_started_ns) / 1_000_000, 3
            ),
            "overlap_ms": round(
                (pair["overlap_end_ns"] - pair["overlap_start_ns"]) / 1_000_000, 3
            ),
        }
        for pair in stream_pairs
    ]


def run_harness(args: argparse.Namespace) -> dict[str, Any]:
    run_started_ns = time.perf_counter_ns()
    output_path = Path(args.output).resolve()
    profile = _build_profile(args)
    groups: dict[str, list[dict[str, Any]]] = {}
    terminal: TerminalProcess | None = None
    sampler: ProcessTreeRssSampler | None = None
    fixture_server: FixtureServer | None = None
    idle_memory: list[int] = []
    catalog_load_ms: float | None = None
    history_open_ms: float | None = None
    with ExitStack() as workspace_cleanup:
        temporary = workspace_cleanup.enter_context(
            tempfile.TemporaryDirectory(prefix="leg-tui-responsiveness-")
        )
        with ExitStack() as process_cleanup:
            temp_root = Path(temporary)
            workspace = temp_root / "workspace"
            state_dir = temp_root / "state"
            dataset = seed_responsiveness_catalog(state_dir, workspace)
            fixture_server = FixtureServer(workspace)
            process_cleanup.callback(fixture_server.close)
            env = _build_harness_environment(
                os.environ.copy(),
                fixture_server.environment,
                state_dir,
                Path(args.supervisor_bin),
            )
            _validate_seeded_session_logs(Path(args.leg_bin), state_dir, dataset, env)
            command = [
                str(Path(args.tui_bin).resolve()),
                "--leg-bin",
                str(Path(args.leg_bin).resolve()),
                "--supervisor-bin",
                str(Path(args.supervisor_bin).resolve()),
            ]
            startup_started_ns = time.perf_counter_ns()
            terminal = TerminalProcess(command, workspace, env, rows=24, columns=80)
            process_cleanup.callback(terminal.close)
            sampler = ProcessTreeRssSampler(terminal.pid)
            sampler.start()
            process_cleanup.callback(sampler.stop)
            _, startup_visible_ns = terminal.wait_contains(PICKER_HEADER, timeout=15.0)
            startup_ms = (startup_visible_ns - startup_started_ns) / 1_000_000

            selected_ns, visible_ns = _select_session(
                terminal,
                "History fixture",
                already_in_picker=True,
            )
            _restore_transcript_bottom(terminal)
            catalog_load_ms = (visible_ns - selected_ns) / 1_000_000

            idle_started_ns = time.perf_counter_ns()
            time.sleep(IDLE_MEMORY_WINDOW_SECONDS)
            idle_memory = sampler.values_between(idle_started_ns, time.perf_counter_ns())

            stream_pairs = ResponsivenessStreamPairManager(
                terminal,
                fixture_server,
                dataset["stream_pair_pool"],
            )
            stream_pairs.start_next_pair()
            selected_ns, visible_ns = _select_session(
                terminal,
                "History fixture",
            )
            _restore_transcript_bottom(terminal)
            history_open_ms = (visible_ns - selected_ns) / 1_000_000

            for rows, columns in DIMENSIONS:
                if (rows, columns) != (24, 80):
                    terminal.resize(rows, columns)
                    terminal.wait_for(
                        lambda text: any(COMPOSER_HINT in line for line in text.splitlines()[-14:]),
                        "composer to render at 120x40",
                        timeout=3.0,
                    )
                groups.update(
                    _measure_dimension(terminal, rows, columns, args.samples, stream_pairs)
                )

            stream_pairs.wait_for_all_pairs()
            _check_workload_overlap(groups, fixture_server, stream_pairs.pairs)
            stream_pairs.wait_for_tui_sessions_to_finish()
            inspector_samples = [
                sample
                for samples in groups.values()
                for sample in samples
                if sample["action"] == "inspection"
            ]
            inspector_open_ms = p95([float(sample["latency_ms"]) for sample in inspector_samples])

            terminal.write_control("c")
            if not terminal.wait_exit(timeout=5.0):
                raise RuntimeError(
                    "leg-tui did not exit after the harness sent Ctrl-C; "
                    f"current screen:\n{terminal.capture.text()[-3000:]}\n"
                    f"provider fixture status: {fixture_server.fixture.status()}"
                )

            assert catalog_load_ms is not None and history_open_ms is not None
            _decorate_sample_provider_context(
                groups, fixture_server.fixture, run_started_ns, stream_pairs.pairs
            )
            summaries = {
                group: {
                    **summarize_latencies(samples),
                    "stream_pair_ids": sorted({sample["stream_pair_id"] for sample in samples}),
                }
                for group, samples in groups.items()
            }
            memory_samples = sampler.all_values() if sampler else []
            if not idle_memory:
                raise RuntimeError("memory sampler collected no RSS samples during the idle window")
            if not memory_samples:
                raise RuntimeError("memory sampler collected no process-tree RSS samples")
            report = {
                "schema": "leg-tui.responsiveness-report/v1",
                "source_revision": _git_revision(),
                "os": platform.platform(),
                "cpu": _cpu_model(),
                "terminal_version": _terminal_version(),
                "terminal_transport": "Windows ConPTY via pywinpty" if os.name == "nt" else "native Unix PTY",
                "dimensions": [
                    {"rows": rows, "columns": columns}
                    for rows, columns in DIMENSIONS
                ],
                "build_profile": profile,
                "dataset": {
                    "base_turns": dataset["base_turns"],
                    "reply_bytes": dataset["reply_bytes"],
                    "long_answer_lines": dataset["long_answer_lines"],
                    "tool_result_bytes": dataset["tool_result_bytes"],
                    "total_history_turns": dataset["total_history_turns"],
                    "stream_pair_pool_size": len(dataset["stream_pair_pool"]),
                },
                "timings": {
                    "startup_ms": round(startup_ms, 3),
                    "catalog_load_ms": round(catalog_load_ms, 3),
                    "cached_history_open_ms": round(history_open_ms, 3),
                    "inspector_open_ms": round(inspector_open_ms, 3),
                },
                "memory": {
                    "scope": "leg-tui process tree; excludes harness and provider fixture",
                    "sampling_interval_ms": round(MEMORY_SAMPLE_INTERVAL_SECONDS * 1000, 3),
                    "idle_window_seconds": IDLE_MEMORY_WINDOW_SECONDS,
                    "idle_process_tree_rss_bytes": int(median(idle_memory)),
                    "peak_process_tree_rss_bytes": max(memory_samples),
                },
                "provider_fixture": _provider_summary(fixture_server, run_started_ns),
                "stream_pairs": _stream_pair_summaries(stream_pairs.pairs, run_started_ns),
                "groups": summaries,
                "samples": [sample for samples in groups.values() for sample in samples],
            }
            output_path.parent.mkdir(parents=True, exist_ok=True)
            temporary_output = output_path.with_name(output_path.name + ".tmp")
            temporary_output.write_text(
                json.dumps(report, ensure_ascii=False, indent=2) + "\n",
                encoding="utf-8",
            )
            temporary_output.replace(output_path)
            return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tui-bin", required=True, help="leg-tui executable")
    parser.add_argument("--leg-bin", required=True, help="leg executable")
    parser.add_argument("--supervisor-bin", required=True, help="leg-ui-supervisor executable")
    parser.add_argument("--output", required=True, type=Path, help="path for the JSON report")
    parser.add_argument(
        "--build-profile",
        choices=("debug", "release"),
        help="build profile (inferred from binary paths when omitted)",
    )
    parser.add_argument(
        "--samples",
        type=int,
        default=SAMPLE_COUNT,
        help=f"samples per action and terminal size (minimum {SAMPLE_COUNT})",
    )
    args = parser.parse_args()
    if args.samples < SAMPLE_COUNT:
        parser.error(f"--samples must be at least {SAMPLE_COUNT}")
    for binary in (args.tui_bin, args.leg_bin, args.supervisor_bin):
        if not Path(binary).is_file():
            parser.error(f"binary does not exist: {binary}")
    if not (sys.platform.startswith("linux") or sys.platform == "darwin" or os.name == "nt"):
        parser.error("the harness supports native Windows, Linux, and macOS only")

    report = run_harness(args)
    print(f"responsiveness_report={args.output.resolve()}")
    print(
        json.dumps(
            {
                "source_revision": report["source_revision"],
                "terminal_transport": report["terminal_transport"],
                "dimensions": report["dimensions"],
                "groups": report["groups"],
                "timings": report["timings"],
                "memory": report["memory"],
            },
            indent=2,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
