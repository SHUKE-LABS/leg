#!/usr/bin/env python3
"""Linux PTY smoke test for the first leg-tui conversation."""

from __future__ import annotations

import argparse
import codecs
import fcntl
import json
import os
import pty
import pyte
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import urllib.request
from contextlib import suppress
from pathlib import Path
from typing import Any


WARNING = (
    "Leg can run shell commands and modify files as your OS user. "
    "The workspace is its working directory, not a sandbox."
)
PASTED_TEXT = "first line: 中文\r\nsecond: 👩‍👩‍👧‍👦 e\u0301\rthird line\x13\x03\x1b\x7f\nfourth line?"
PROMPT = "?typed line\nfirst line: 中文\nsecond: 👩‍👩‍👧‍👦 e\u0301\nthird line\nfourth line?"
NEXT_DRAFT = "next draft"
LIVE_TEXT = "The first live text is visible."
STOP_LIVE_TEXT = "Request 2: The first live text is visible."
NEGATIVE_LIVE_TEXT = "Request 3: The first live text is visible."
RESUMED_TEXT = "The fixture resumes after its fixed pause."
ROWS = 40
COLUMNS = 160
STOP_DELAY_SECONDS = 2.1


class TerminalCapture:
    """Keep raw PTY bytes and the VT parser's current screen separately."""

    def __init__(self, rows: int = ROWS, columns: int = COLUMNS) -> None:
        self.raw = bytearray()
        self.screen = pyte.Screen(columns, rows)
        self.stream = pyte.Stream(self.screen)
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, chunk: bytes) -> None:
        self.raw.extend(chunk)
        self.stream.feed(self.decoder.decode(chunk))

    def text(self) -> str:
        return "\n".join(line.rstrip() for line in self.screen.display)

    def contains(self, expected: str) -> bool:
        return expected in self.text()


def test_terminal_screen_redraw() -> None:
    capture = TerminalCapture(rows=3, columns=24)
    chunks = (
        b"\x1b[1;1HStopping",
        b"\x1b[1;6H",
        b"e\x1b[1;",
        b"7Hd\x1b[1;8H\x1b[K",
    )
    for chunk in chunks:
        capture.feed(chunk)
    assert b"Stopped" not in capture.raw, (
        "redraw fixture unexpectedly printed contiguous Stopped bytes"
    )
    assert capture.contains("Stopped"), capture.text()
    assert not capture.contains("Stopping"), capture.text()

    erased = TerminalCapture(rows=3, columns=24)
    erased.feed(b"\x1b[1;1HStopped")
    assert erased.contains("Stopped"), erased.text()
    erased.feed(b"\x1b[1;1H\x1b[K")
    assert not erased.contains("Stopped"), "an erased historical status still matched"


def read_until(
    master_fd: int,
    child: subprocess.Popen[bytes],
    capture: TerminalCapture,
    expected: str,
    timeout: float = 8.0,
) -> None:
    deadline = time.monotonic() + timeout
    while not capture.contains(expected):
        capture_owned_processes(child)
        if child.poll() is not None:
            raise AssertionError(
                f"TUI exited before visible screen text {expected!r}; exit={child.returncode}; "
                f"current screen={capture.text()[-1200:]!r}"
            )
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(
                f"timed out waiting for visible screen text {expected!r}; "
                f"current screen={capture.text()[-1200:]!r}"
            )
        ready, _, _ = select.select([master_fd], [], [], min(0.1, remaining))
        if not ready:
            continue
        try:
            chunk = os.read(master_fd, 8192)
        except OSError:
            continue
        if chunk:
            capture.feed(chunk)


def linux_process_table() -> dict[int, tuple[int, int, int, str]]:
    processes: dict[int, tuple[int, int, int, str]] = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            stat = (entry / "stat").read_text(encoding="utf-8")
            close = stat.rfind(")")
            fields = stat[close + 1 :].split()
            processes[int(entry.name)] = (
                int(fields[1]),  # parent pid
                int(fields[2]),  # process group id
                int(fields[19]),  # start time, used to reject a reused pid
                fields[0],  # state
            )
        except (OSError, ValueError, IndexError):
            continue
    return processes


def capture_owned_processes(child: subprocess.Popen[bytes]) -> None:
    table = linux_process_table()
    owned = getattr(child, "_pty_owned_processes", {})
    frontier = [child.pid, *owned]
    visited = set(frontier)
    while frontier:
        parent = frontier.pop()
        for pid, (ppid, pgid, start_time, state) in table.items():
            if ppid == parent and pid not in visited and state != "Z":
                visited.add(pid)
                owned[pid] = (start_time, pgid)
                frontier.append(pid)
    child._pty_owned_processes = owned


def process_matches(pid: int, start_time: int) -> bool:
    try:
        stat = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
        close = stat.rfind(")")
        fields = stat[close + 1 :].split()
        return int(fields[19]) == start_time and fields[0] != "Z"
    except (OSError, ValueError, IndexError):
        return False


def kill_owned_process_group(child: subprocess.Popen[bytes]) -> None:
    # The PTY UI has its own session. Its supervisor and leg children create
    # separate groups, so snapshot descendants before closing the UI's pipes.
    capture_owned_processes(child)
    owned: dict[int, tuple[int, int]] = getattr(child, "_pty_owned_processes", {})

    if child.poll() is None:
        with suppress(ProcessLookupError):
            os.killpg(child.pid, signal.SIGKILL)
    if child.poll() is None:
        try:
            child.wait(timeout=2)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=2)

    def pending_owned() -> dict[int, tuple[int, int]]:
        return {
            pid: identity
            for pid, identity in owned.items()
            if process_matches(pid, identity[0])
        }

    deadline = time.monotonic() + 4.0
    pending = pending_owned()
    while pending and time.monotonic() < deadline:
        time.sleep(0.05)
        pending = pending_owned()
    if pending:
        groups = {
            pgid
            for pid, (_, pgid) in pending.items()
            if any(candidate == pgid for candidate in pending)
        }
        for pgid in groups:
            with suppress(ProcessLookupError):
                os.killpg(pgid, signal.SIGKILL)
        for pid, (start_time, pgid) in pending.items():
            if pgid not in groups and process_matches(pid, start_time):
                with suppress(ProcessLookupError):
                    os.kill(pid, signal.SIGKILL)
        deadline = time.monotonic() + 1.0
        pending = pending_owned()
        while pending and time.monotonic() < deadline:
            time.sleep(0.05)
            pending = pending_owned()
    if pending:
        raise AssertionError(f"owned UI child processes survived cleanup: {sorted(pending)}")


def drain_until_exit(
    master_fd: int,
    child: subprocess.Popen[bytes],
    capture: TerminalCapture,
    timeout: float = 8.0,
) -> int:
    deadline = time.monotonic() + timeout
    while child.poll() is None:
        capture_owned_processes(child)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            kill_owned_process_group(child)
            raise AssertionError("TUI did not exit after Ctrl-C")
        ready, _, _ = select.select([master_fd], [], [], min(0.1, remaining))
        if ready:
            try:
                chunk = os.read(master_fd, 8192)
            except OSError:
                chunk = b""
            if chunk:
                capture.feed(chunk)
    while True:
        ready, _, _ = select.select([master_fd], [], [], 0)
        if not ready:
            break
        try:
            chunk = os.read(master_fd, 8192)
        except OSError:
            break
        if not chunk:
            break
        capture.feed(chunk)
    return child.wait(timeout=1)


def drain_for(
    master_fd: int,
    capture: TerminalCapture,
    duration: float = 0.3,
    child: subprocess.Popen[bytes] | None = None,
) -> None:
    deadline = time.monotonic() + duration
    while time.monotonic() < deadline:
        if child is not None:
            capture_owned_processes(child)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        ready, _, _ = select.select([master_fd], [], [], min(0.05, remaining))
        if not ready:
            continue
        try:
            chunk = os.read(master_fd, 8192)
        except OSError:
            continue
        if chunk:
            capture.feed(chunk)


def spawn_in_pty(
    command: list[str], env: dict[str, str]
) -> tuple[int, int, subprocess.Popen[bytes], list[Any]]:
    master_fd, slave_fd = pty.openpty()
    fcntl.ioctl(slave_fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLUMNS, 0, 0))
    initial_termios = termios.tcgetattr(slave_fd)

    def attach_controlling_terminal() -> None:
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    try:
        child = subprocess.Popen(
            command,
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=slave_fd,
            env=env,
            close_fds=True,
            preexec_fn=attach_controlling_terminal,
        )
    except BaseException:
        os.close(master_fd)
        os.close(slave_fd)
        raise
    return master_fd, slave_fd, child, initial_termios


def request_status(status_url: str, timeout: float = 3.0) -> dict[str, Any]:
    with urllib.request.urlopen(status_url, timeout=timeout) as response:
        return json.loads(response.read())


def release_gate(base_url: str, request: int, timeout: float = 2.0) -> None:
    request_url = f"{base_url}/__trial/release?request={request}"
    release = urllib.request.Request(request_url, data=b"", method="POST")
    with urllib.request.urlopen(release, timeout=timeout) as response:
        value = json.loads(response.read())
    assert value == {"released": request}, value


def wait_for_status(
    status_url: str,
    predicate: Any,
    description: str,
    timeout: float = 3.0,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    last: dict[str, Any] = {}
    while time.monotonic() < deadline:
        last = request_status(status_url, timeout=1.0)
        if predicate(last):
            return last
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for fixture {description}; last status={last!r}")


def wait_for_fixture_start(fixture: subprocess.Popen[str], timeout: float = 5.0) -> str:
    assert fixture.stdout is not None
    deadline = time.monotonic() + timeout
    buffered = bytearray()
    while time.monotonic() < deadline:
        if fixture.poll() is not None:
            raise AssertionError(f"fake provider exited during startup: {fixture.returncode}")
        stdout_fd = fixture.stdout.fileno()
        ready, _, _ = select.select([stdout_fd], [], [], min(0.1, deadline - time.monotonic()))
        if not ready:
            continue
        try:
            buffered.extend(os.read(stdout_fd, 8192))
        except OSError:
            continue
        while b"\n" in buffered:
            line, _, rest = buffered.partition(b"\n")
            buffered = bytearray(rest)
            decoded = line.decode("utf-8", errors="replace")
            if decoded.startswith("Status: "):
                return decoded.split(": ", 1)[1].strip()
    raise AssertionError("fake provider did not print its status URL within five seconds")


def assert_terminal_restored(
    raw: bytes, slave_fd: int, initial_termios: list[Any], child: subprocess.Popen[bytes]
) -> None:
    actual = termios.tcgetattr(slave_fd)
    assert actual == initial_termios, f"terminal attributes were not restored: {actual!r}"
    assert b"\x1b[?1049l" in raw, "alternate screen was not left"
    assert b"\x1b[?25h" in raw, "cursor was not shown"
    assert b"\x1b[?2004h" in raw, f"bracketed paste was not enabled: {raw[:240]!r}"
    assert b"\x1b[?2004l" in raw, f"bracketed paste was not disabled: {raw[-240:]!r}"
    assert child.returncode is not None


def contains_string(value: Any, expected: str) -> bool:
    if isinstance(value, str):
        return value == expected
    if isinstance(value, dict):
        return any(contains_string(item, expected) for item in value.values())
    if isinstance(value, list):
        return any(contains_string(item, expected) for item in value)
    return False


def start_prompt(
    master_fd: int,
    child: subprocess.Popen[bytes],
    capture: TerminalCapture,
    workspace: Path,
    status_url: str,
    expected_requests: int,
    expected_live_text: str = LIVE_TEXT,
    exercise_keyboard: bool = False,
) -> None:
    read_until(master_fd, child, capture, "Path:")
    os.write(master_fd, str(workspace).encode() + b"\r")
    read_until(master_fd, child, capture, WARNING)
    if exercise_keyboard:
        drain_for(master_fd, capture, 0.1, child)
        first_run_help = capture.text()
        for hint in (
            "Enter newline",
            "Ctrl-S send",
            "Backspace/Delete edit",
            "Ctrl-Z undo",
            "Ctrl-Y redo",
            "Ctrl-C stops",
            "F1 help",
            "F2 keyboard actions",
            "? is prompt text",
        ):
            assert hint in first_run_help, (
                f"first-run help omitted {hint!r}: {first_run_help[-1800:]!r}"
            )
    before_ack = request_status(status_url)
    assert before_ack["requests"] == expected_requests, before_ack

    os.write(master_fd, b"\r")
    read_until(master_fd, child, capture, "Ready")
    os.write(master_fd, b"\x13")
    drain_for(master_fd, capture, child=child)
    blank_prompt = request_status(status_url)
    assert blank_prompt["requests"] == expected_requests, (
        "a blank prompt started a provider request", blank_prompt
    )

    if exercise_keyboard:
        os.write(master_fd, b"\x1bOP")
        read_until(master_fd, child, capture, "Keyboard help")
        os.write(master_fd, b"ignored-help")
        os.write(master_fd, b"\x1b")
        drain_for(master_fd, capture, 0.15, child)
        os.write(master_fd, b"\x1bOQ")
        read_until(master_fd, child, capture, "Keyboard actions")
        action_menu = capture.text()
        for action in (
            "Insert a newline",
            "Send the complete nonblank prompt",
            "Move by grapheme",
            "Remove one grapheme",
            "Ctrl-Z / Ctrl-Y",
            "Stop the turn",
            "F1 / F2",
        ):
            assert action in action_menu, f"keyboard action menu omitted {action!r}"
        os.write(master_fd, b"ignored-menu")
        os.write(master_fd, b"\x1b[200~ignored-paste\x1b[201~")
        os.write(master_fd, b"\x1b")
        drain_for(master_fd, capture, 0.15, child)
        assert request_status(status_url)["requests"] == expected_requests, (
            "opening help or the action menu started a provider request"
        )

        os.write(master_fd, b"?typed line\r")
        drain_for(master_fd, capture, 0.1, child)
        question_screen = capture.text()
        assert "?typed line" in question_screen, (
            "? or Enter did not insert literal text and a newline in the composer"
        )
        assert "Keyboard help" not in question_screen, "? opened help instead of entering the draft"
        os.write(master_fd, b"\x1b[200~" + PASTED_TEXT.encode() + b"\x1b[201~")
        drain_for(master_fd, capture, child=child)
        assert child.poll() is None, "a control byte in bracketed paste exited the TUI"
        assert request_status(status_url)["requests"] == expected_requests, (
            "paste content invoked a provider or keyboard shortcut"
        )

    if exercise_keyboard:
        os.write(master_fd, b"\x13")
    else:
        os.write(master_fd, PROMPT.encode() + b"\x13")
    read_until(master_fd, child, capture, expected_live_text)
    assert capture.contains(expected_live_text), (
        f"streamed response was not visible: {capture.text()[-2000:]!r}"
    )


def run_smoke(args: argparse.Namespace) -> None:
    repository = Path(__file__).resolve().parents[2]
    fixture_path = repository / "trials" / "fake_provider.py"

    with tempfile.TemporaryDirectory(prefix="leg-tui-pty-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        negative_workspace = root / "negative-workspace"
        negative_workspace.mkdir()
        state_dir = root / "state"
        event_log = root / "leg-events.jsonl"

        fixture = subprocess.Popen(
            [
                sys.executable,
                str(fixture_path),
                "--scenario",
                "paused-live-text",
                "--hold-after-first-chunk",
                "--workspace",
                str(workspace),
                "--port",
                "0",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        status_url: str | None = None
        try:
            status_url = wait_for_fixture_start(fixture)
            base_url = status_url.removesuffix("/__trial/status")

            env = os.environ.copy()
            env.update(
                {
                    "LEG_PROVIDER": "anthropic",
                    "ANTHROPIC_BASE_URL": base_url,
                    "ANTHROPIC_API_KEY": "trial-only-not-a-secret",
                    "LEG_MODEL": "trial-fixture",
                    "LEG_MAX_RETRIES": "0",
                    "LEG_UI_STATE_DIR": str(state_dir),
                    "LEG_UI_SUPERVISOR_BIN": str(Path(args.supervisor_bin).resolve()),
                    "LEG_EVENT_LOG": str(event_log),
                }
            )

            command = [
                str(Path(args.tui_bin).resolve()),
                "--leg-bin",
                str(Path(args.leg_bin).resolve()),
                "--supervisor-bin",
                str(Path(args.supervisor_bin).resolve()),
            ]
            master_fd, slave_fd, child, initial_termios = spawn_in_pty(command, env)
            capture = TerminalCapture()
            try:
                start_prompt(
                    master_fd,
                    child,
                    capture,
                    workspace,
                    status_url,
                    0,
                    exercise_keyboard=True,
                )
                held = wait_for_status(
                    status_url,
                    lambda value: value["requests"] == 1
                    and value["active_requests"] == [1]
                    and value["pause_gates"].get("1") == "held",
                    "the streamed request to be active and held",
                )
                assert held["requests"] == 1, held
                assert held["active_requests"] == [1], held
                assert held["pause_gates"] == {"1": "held"}, held

                # Keep the Unicode draft checks on the first turn, including the
                # busy-submit guard, then let that turn complete normally.
                os.write(master_fd, NEXT_DRAFT.encode() + b"\x13")
                read_until(master_fd, child, capture, "Busy: wait for the active turn")
                busy = request_status(status_url)
                assert busy["requests"] == 1, busy
                assert busy["active_requests"] == [1], busy
                assert busy["pause_gates"] == {"1": "held"}, busy
                release_gate(base_url, 1)
                completed_first = wait_for_status(
                    status_url,
                    lambda value: value["request_outcomes"].get("1") == "completed",
                    "the first held response to complete after release",
                )
                assert completed_first["pause_gates"].get("1") == "completed", completed_first
                read_until(master_fd, child, capture, "Succeeded")
                assert capture.contains(NEXT_DRAFT), (
                    "the next draft typed during the first turn was lost on success"
                )

                # Submit the preserved draft. The second request is the one held
                # for Stop, and its unique first chunk proves the current screen
                # shows this turn rather than stale text from the first one.
                os.write(master_fd, b"\x13")
                held_stop = wait_for_status(
                    status_url,
                    lambda value: value["requests"] == 2
                    and value["active_requests"] == [2]
                    and value["pause_gates"].get("2") == "held",
                    "the follow-up request to be active and held",
                )
                assert held_stop["request_outcomes"].get("2") == "active", held_stop
                read_until(master_fd, child, capture, STOP_LIVE_TEXT)
                assert capture.contains(STOP_LIVE_TEXT), capture.text()

                # Exercise a delayed Stop while the provider response is still
                # held, and keep draining the PTY so visible state is current.
                drain_for(master_fd, capture, STOP_DELAY_SECONDS, child)
                delayed = request_status(status_url)
                assert delayed["requests"] == 2, delayed
                assert delayed["active_requests"] == [2], delayed
                assert delayed["request_outcomes"].get("2") == "active", delayed
                assert delayed["pause_gates"].get("2") == "held", delayed
                assert capture.contains(STOP_LIVE_TEXT), capture.text()

                os.write(master_fd, b"\x03")
                read_until(master_fd, child, capture, "Stopped")
                assert capture.contains("Stopped"), capture.text()
                os.write(master_fd, b"\x03")
                status = drain_until_exit(master_fd, child, capture)
                assert status == 0, f"TUI exit status was {status}: {bytes(capture.raw)!r}"
                assert_terminal_restored(bytes(capture.raw), slave_fd, initial_termios, child)
            finally:
                kill_owned_process_group(child)
                os.close(master_fd)
                os.close(slave_fd)

            stopped_fixture = wait_for_status(
                status_url,
                lambda value: value["active_requests"] == []
                and value["request_outcomes"].get("2") == "interrupted",
                "the stopped request to disconnect without completing",
            )
            assert stopped_fixture["requests"] == 2, stopped_fixture
            assert stopped_fixture["scenario_requests"] == {"paused-live-text": 2}, stopped_fixture
            assert stopped_fixture["request_outcomes"] == {
                "1": "completed",
                "2": "interrupted",
            }, stopped_fixture
            assert stopped_fixture["pause_gates"].get("2") == "client_disconnected", stopped_fixture

            exchange_events = [
                json.loads(line) for line in event_log.read_text(encoding="utf-8").splitlines()
            ]
            requests = [event for event in exchange_events if event.get("event") == "request"]
            assert len(requests) == 2, (
                f"expected one completed and one stopped exchange, found {requests!r}"
            )
            assert requests[0]["prompt"] == PROMPT, (
                f"exchange request carried the wrong prompt: {requests[0]!r}"
            )
            assert requests[1]["prompt"] == NEXT_DRAFT, (
                f"the delayed Stop round carried the wrong prompt: {requests[1]!r}"
            )

            trails = list((state_dir / "sessions").glob("*.jsonl"))
            assert len(trails) == 1, f"expected one session trail, found {trails!r}"
            records = [
                json.loads(line) for line in trails[0].read_text(encoding="utf-8").splitlines()
            ]
            assert any(contains_string(record, PROMPT) for record in records), (
                "the pasted provider exchange did not carry the exact submitted prompt"
            )
            assert any(contains_string(record, NEXT_DRAFT) for record in records), (
                "the stopped follow-up exchange was not recorded in the session trail"
            )
            interrupted = [
                record
                for record in records
                if record.get("event") == "response_error" and record.get("kind") == "interrupted"
            ]
            assert len(interrupted) == 1, (
                f"expected exactly one stopped exchange, found {interrupted!r}"
            )
            assert not any(
                record.get("event") == "response_error" and record.get("kind") == "incomplete"
                for record in records
            ), f"Stop produced an incomplete driver outcome: {records!r}"
            session_id = trails[0].stem
            catalog = json.loads((state_dir / "catalog.json").read_text(encoding="utf-8"))
            assert catalog["sessions"][session_id]["drafts"]["tui"] == NEXT_DRAFT, (
                "the stopped follow-up prompt was not restored as the composer draft"
            )

            # A catalog startup error occurs after raw/alternate mode begins.
            blocker = root / "not-a-directory"
            blocker.write_text("file", encoding="utf-8")
            error_env = env.copy()
            error_env["LEG_UI_STATE_DIR"] = str(blocker)
            error_master, error_slave, error_child, error_initial = spawn_in_pty(command, error_env)
            error_capture = TerminalCapture()
            try:
                error_status = drain_until_exit(error_master, error_child, error_capture)
                assert error_status != 0, "invalid catalog path unexpectedly succeeded"
                assert_terminal_restored(
                    bytes(error_capture.raw), error_slave, error_initial, error_child
                )
            finally:
                kill_owned_process_group(error_child)
                os.close(error_master)
                os.close(error_slave)

            after_error = request_status(status_url)
            assert after_error["requests"] == 2, after_error

            # Negative control: release another held stream before Stop. It
            # completes as a normal response and exits from the idle composer.
            negative_env = env.copy()
            negative_env["LEG_UI_STATE_DIR"] = str(root / "negative-state")
            negative_env["LEG_EVENT_LOG"] = str(root / "negative-events.jsonl")
            negative_master, negative_slave, negative_child, negative_initial = spawn_in_pty(
                command, negative_env
            )
            negative_capture = TerminalCapture()
            try:
                start_prompt(
                    negative_master,
                    negative_child,
                    negative_capture,
                    negative_workspace,
                    status_url,
                    2,
                    expected_live_text=NEGATIVE_LIVE_TEXT,
                )
                held_negative = wait_for_status(
                    status_url,
                    lambda value: value["requests"] == 3
                    and value["active_requests"] == [3]
                    and value["pause_gates"].get("3") == "held",
                    "the negative-control response to be held",
                )
                assert held_negative["request_outcomes"].get("3") == "active", held_negative
                release_gate(base_url, 3)
                completed_negative = wait_for_status(
                    status_url,
                    lambda value: value["active_requests"] == []
                    and value["request_outcomes"].get("3") == "completed",
                    "the released negative-control response to complete",
                )
                assert completed_negative["pause_gates"].get("3") == "completed", completed_negative
                read_until(negative_master, negative_child, negative_capture, "Succeeded")
                assert negative_capture.contains(RESUMED_TEXT), negative_capture.text()
                assert not negative_capture.contains("Stopped"), negative_capture.text()

                os.write(negative_master, b"\x03")
                negative_status = drain_until_exit(
                    negative_master, negative_child, negative_capture
                )
                assert negative_status == 0, (
                    f"negative-control TUI exit status was {negative_status}"
                )
                assert_terminal_restored(
                    bytes(negative_capture.raw),
                    negative_slave,
                    negative_initial,
                    negative_child,
                )
            finally:
                kill_owned_process_group(negative_child)
                os.close(negative_master)
                os.close(negative_slave)

            final_fixture = request_status(status_url)
            assert final_fixture["requests"] == 3, final_fixture
            assert final_fixture["scenario_requests"] == {"paused-live-text": 3}, final_fixture
            assert final_fixture["request_outcomes"] == {
                "1": "completed",
                "2": "interrupted",
                "3": "completed",
            }, final_fixture
            assert final_fixture["active_requests"] == [], final_fixture
            assert final_fixture["pause_gates"] == {
                "1": "completed",
                "2": "client_disconnected",
                "3": "completed",
            }, final_fixture
            assert "paused_response_completed" in final_fixture["input_checks"], final_fixture
        finally:
            if status_url is not None:
                with suppress(Exception):
                    current = request_status(status_url, timeout=1.0)
                    base_url = status_url.removesuffix("/__trial/status")
                    for request, state in current.get("pause_gates", {}).items():
                        if state in ("waiting", "held"):
                            with suppress(Exception):
                                release_gate(base_url, int(request), timeout=1.0)
            if fixture.poll() is None:
                with suppress(ProcessLookupError):
                    fixture.send_signal(signal.SIGTERM)
                try:
                    fixture.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    fixture.kill()
                    fixture.wait(timeout=2)


def main() -> None:
    if sys.platform != "linux":
        raise SystemExit("pty_smoke.py is Linux-only")
    parser = argparse.ArgumentParser()
    parser.add_argument("--tui-bin", required=True)
    parser.add_argument("--leg-bin", required=True)
    parser.add_argument("--supervisor-bin", required=True)
    args = parser.parse_args()
    test_terminal_screen_redraw()
    run_smoke(args)
    print("leg-tui Linux PTY smoke passed")


if __name__ == "__main__":
    main()
