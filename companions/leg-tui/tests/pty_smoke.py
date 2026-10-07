#!/usr/bin/env python3
"""Native Linux/macOS PTY smoke test for leg-tui terminal behavior."""

from __future__ import annotations

import argparse
import codecs
import fcntl
import json
import os
import pty
import re
import select
import signal
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unicodedata
from urllib.error import HTTPError
from urllib.parse import urlsplit
import urllib.request
from contextlib import suppress
from pathlib import Path
from typing import Any

import pyte


WARNING = (
    "Leg can run shell commands and modify files as your OS user. "
    "The workspace is its working directory, not a sandbox."
)
DEFAULT_ROWS = 40
DEFAULT_COLUMNS = 120
PASTED_TEXT = "first line: 中文\r\nsecond: 👩‍👩‍👧‍👦 e\u0301\rthird line\x13\x03\x1b\x7f\nfourth line?"
PROMPT = "?typed line\nfirst line: 中文\nsecond: 👩‍👩‍👧‍👦 e\u0301\nthird line\nfourth line?"
NEXT_DRAFT = "next draft"
EXCHANGE_SCHEMA = "baton.exchange/v1"
WARNING_ACK_KEY = "tui_first_run_warning_acknowledged"
LIVE_TEXT = "The first live text is visible."
STOP_LIVE_TEXT = "Request 2: The first live text is visible."
NEGATIVE_LIVE_TEXT = "Request 3: The first live text is visible."
RESUMED_TEXT = "The fixture resumes after its fixed pause."
ROWS = 40
COLUMNS = 160
STOP_DELAY_SECONDS = 2.1
TRANSCRIPT_FRAME_QUIET_SECONDS = 0.12


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
        screen_text = self.text()
        for border in "│─┌┐└┘├┤┬┴┼":
            screen_text = screen_text.replace(border, " ")
        screen_text = " ".join(screen_text.split())
        expected_text = " ".join(expected.split())
        return expected_text in screen_text

    def __bytes__(self) -> bytes:
        return bytes(self.raw)


def transcript_pane_text(screen: str) -> str:
    lines = screen.splitlines()
    title_index = next(
        (index for index, line in enumerate(lines) if re.search(r"Rows (\d+-\d+ of \d+)", line)),
        None,
    )
    if title_index is None:
        return ""
    title_line = lines[title_index]
    rows_match = re.search(r"Rows (\d+-\d+ of \d+)", title_line)
    assert rows_match is not None
    pane_start = title_line.rfind("┌", 0, rows_match.start())
    pane_end = title_line.find("┐", rows_match.end())
    if pane_start < 0 or pane_end < 0:
        return ""
    pane_end += 1

    body = []
    for line in lines[title_index + 1 :]:
        pane = line[pane_start:pane_end]
        if pane.startswith("└"):
            break
        body.append(pane)
    return "\n".join(body)


def workspace_path_text(capture: TerminalCapture) -> str | None:
    lines = capture.text().splitlines()
    path_index = next(
        (index for index, line in enumerate(lines) if "Path: " in line),
        None,
    )
    if path_index is None:
        return None
    first = lines[path_index].partition("Path: ")[2].partition("│")[0].strip()
    parts = [first]
    for line in lines[path_index + 1 :]:
        if "│" not in line:
            break
        continuation = line.split("│", 1)[1].rsplit("│", 1)[0].strip()
        if not continuation:
            break
        parts.append(continuation)
    return "".join(parts)


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

    wrapped = TerminalCapture(rows=2, columns=6)
    wrapped.feed(b"\x1b[1;1Hfirst second")
    assert wrapped.contains("first second"), wrapped.text()

    transcript = "\n".join(
        (
            "┌Sessions┐┌Rows 1-2 of 2┐",
            "│        ││first      │",
            "│        ││second     │",
            "└────────┘└───────────┘",
        )
    )
    assert "first" in transcript_pane_text(transcript)
    assert "second" in transcript_pane_text(transcript)


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


def read_until_not_contains(
    master_fd: int,
    child: subprocess.Popen[bytes],
    capture: TerminalCapture,
    unexpected: str,
    timeout: float = 8.0,
) -> None:
    deadline = time.monotonic() + timeout
    while capture.contains(unexpected):
        capture_owned_processes(child)
        if child.poll() is not None:
            raise AssertionError(
                f"TUI exited before screen text {unexpected!r} disappeared; "
                f"exit={child.returncode}; screen={capture.text()[-1200:]!r}"
            )
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(
                f"timed out waiting for screen text {unexpected!r} to disappear; "
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


def read_until_rows_change(
    master_fd: int,
    child: subprocess.Popen[bytes],
    capture: TerminalCapture,
    previous: str,
    previous_body: str,
    timeout: float = 2.0,
) -> str:
    deadline = time.monotonic() + timeout
    settled_at: float | None = None
    settled_view: tuple[str, str] | None = None
    while True:
        screen = capture.text()
        title_line = next(
            (line for line in screen.splitlines() if "Rows " in line), None
        )
        match = (
            re.search(r"Rows (\d+-\d+ of \d+)", title_line) if title_line else None
        )
        body = transcript_pane_text(screen)
        changed = bool(
            match
            and match.group(1) != previous
            and body != previous_body
        )
        now = time.monotonic()
        current_view = (title_line, body) if changed and title_line else None
        if current_view is not None and current_view != settled_view:
            settled_view = current_view
            settled_at = now
        elif current_view is None:
            settled_view = None
            settled_at = None
        if (
            current_view is not None
            and settled_at is not None
            and now - settled_at >= TRANSCRIPT_FRAME_QUIET_SECONDS
        ):
            assert match is not None
            return match.group(1)
        capture_owned_processes(child)
        if child.poll() is not None:
            raise AssertionError(
                f"TUI exited before transcript rows changed from {previous!r}; "
                f"exit={child.returncode}; current screen={capture.text()[-1200:]!r}"
            )
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            current_screen = capture.text()
            current_title = next(
                (line for line in current_screen.splitlines() if "Rows " in line), None
            )
            current_match = (
                re.search(r"Rows (\d+-\d+ of \d+)", current_title)
                if current_title
                else None
            )
            current_body = transcript_pane_text(current_screen)
            raise AssertionError(
                f"timed out waiting for transcript rows to change from {previous!r}; "
                f"current rows={current_match.group(1) if current_match else None!r}; "
                f"pane changed={current_body != previous_body}; "
                f"title={current_title!r}; "
                f"current screen={current_screen[-1200:]!r}"
            )
        wait_for = min(0.1, remaining)
        if current_view is not None and settled_at is not None:
            quiet_remaining = TRANSCRIPT_FRAME_QUIET_SECONDS - (now - settled_at)
            wait_for = min(wait_for, max(0.0, quiet_remaining))
        ready, _, _ = select.select([master_fd], [], [], wait_for)
        if not ready:
            continue
        try:
            chunk = os.read(master_fd, 8192)
        except OSError:
            continue
        if chunk:
            capture.feed(chunk)


def read_until_workspace_path(
    master_fd: int,
    child: subprocess.Popen[bytes],
    capture: TerminalCapture,
    expected: str,
    timeout: float = 8.0,
) -> None:
    deadline = time.monotonic() + timeout
    while workspace_path_text(capture) != expected:
        capture_owned_processes(child)
        if child.poll() is not None:
            raise AssertionError(
                f"TUI exited before workspace path {expected!r}; exit={child.returncode}; "
                f"current screen={capture.text()[-1200:]!r}"
            )
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(
                f"timed out waiting for workspace path {expected!r}; "
                f"current workspace path={workspace_path_text(capture)!r}; "
                f"screen={capture.text()[-1200:]!r}"
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


def read_until_fast(
    master_fd: int,
    child: subprocess.Popen[bytes],
    capture: TerminalCapture,
    expected: str,
    timeout: float = 0.2,
) -> float:
    started = time.perf_counter()
    deadline = started + timeout
    while not capture.contains(expected):
        if child.poll() is not None:
            raise AssertionError(
                f"TUI exited before screen text {expected!r}; exit={child.returncode}; "
                f"screen={capture.text()[-1200:]!r}"
            )
        remaining = deadline - time.perf_counter()
        if remaining <= 0:
            raise AssertionError(
                f"TUI did not show {expected!r} within {timeout * 1000:.0f} ms; "
                f"screen={capture.text()[-1200:]!r}"
            )
        ready, _, _ = select.select([master_fd], [], [], min(0.005, remaining))
        if not ready:
            continue
        try:
            chunk = os.read(master_fd, 8192)
        except OSError:
            continue
        if chunk:
            capture.feed(chunk)
    return time.perf_counter() - started


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
    if sys.platform != "linux":
        return
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
    if sys.platform != "linux":
        if child.poll() is None:
            child.kill()
            child.wait(timeout=2)
        return
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
    output: TerminalCapture | bytearray,
    timeout: float = 8.0,
) -> int:
    deadline = time.monotonic() + timeout
    while child.poll() is None:
        if isinstance(output, TerminalCapture):
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
                feed_output(output, chunk)
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
        feed_output(output, chunk)
    return child.wait(timeout=1)


def drain_for(
    master_fd: int,
    output: TerminalCapture | bytearray,
    duration: float = 0.3,
    child: subprocess.Popen[bytes] | None = None,
) -> None:
    deadline = time.monotonic() + duration
    while time.monotonic() < deadline:
        if child is not None and isinstance(output, TerminalCapture):
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
            feed_output(output, chunk)



def spawn_in_pty(
    command: list[str],
    env: dict[str, str],
    rows: int = DEFAULT_ROWS,
    columns: int = DEFAULT_COLUMNS,
) -> tuple[int, int, subprocess.Popen[bytes], list[Any]]:
    master_fd, slave_fd = pty.openpty()
    fcntl.ioctl(slave_fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
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



def output_screen_text(
    output: TerminalCapture | bytearray,
    rows: int = DEFAULT_ROWS,
    columns: int = DEFAULT_COLUMNS,
) -> str:
    if isinstance(output, TerminalCapture):
        return output.text()
    return terminal_screen_text(bytes(output), rows, columns)


def feed_output(output: TerminalCapture | bytearray, chunk: bytes) -> None:
    if isinstance(output, TerminalCapture):
        output.feed(chunk)
    else:
        output.extend(chunk)


def read_until_screen_text(
    master_fd: int,
    child: subprocess.Popen[bytes],
    output: TerminalCapture | bytearray,
    needle: str,
    timeout: float = 8.0,
    rows: int = DEFAULT_ROWS,
    columns: int = DEFAULT_COLUMNS,
) -> None:
    deadline = time.monotonic() + timeout
    while needle not in output_screen_text(output, rows, columns):
        if child.poll() is not None:
            raise AssertionError(
                f"TUI exited before screen text {needle!r}; exit={child.returncode}; "
                f"screen text={output_screen_text(output, rows, columns)[-1200:]!r}"
            )
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(
                f"timed out waiting for screen text {needle!r}; "
                f"screen text={output_screen_text(output, rows, columns)[-1200:]!r}"
            )
        ready, _, _ = select.select([master_fd], [], [], min(0.1, remaining))
        if not ready:
            continue
        try:
            chunk = os.read(master_fd, 8192)
        except OSError:
            continue
        if chunk:
            feed_output(output, chunk)


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
    raw: bytes, termios_fd: int, initial_termios: list[Any], child: subprocess.Popen[bytes]
) -> None:
    # macOS may revoke the session leader's controlling slave after exit; its
    # PTY master remains available for reading the shared terminal attributes.
    actual = termios.tcgetattr(termios_fd)
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
            "Ctrl-S send",
            "F1 help",
            "F2/Ctrl-P actions",
            "F3 sessions",
            "Ctrl-F search",
            "F4 inspect",
            "F5 copy",
            "F6 export",
            "PageUp/PageDown move by transcript rows",
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
        read_until(master_fd, child, capture, "Enter never confirms a retry")
        keyboard_help = capture.text()
        for hint in (
            "Ctrl-Z/Y undo/redo",
            "Backspace/Delete remove",
            "Ctrl-End follows the tail",
            "F3 sessions",
            "F4 inspects",
            "Ctrl-F searches",
            "F5 copies",
            "F7 saves",
            "F6 exports transcript data only",
            "Retry sends this prompt again and may repeat tool side effects.",
            "Enter never confirms a retry",
        ):
            assert hint in keyboard_help, f"keyboard help omitted {hint!r}: {keyboard_help[-2200:]!r}"
        os.write(master_fd, b"ignored-help")
        os.write(master_fd, b"\x1b")
        drain_for(master_fd, capture, 0.15, child)
        os.write(master_fd, b"a\x1b[D")
        read_until(master_fd, child, capture, "Composer (Ctrl-S send)")
        os.write(master_fd, b"\x1bOQ")
        read_until(master_fd, child, capture, "Command palette")
        assert "Browse/switch sessions" in capture.text(), capture.text()
        assert "New conversation" in capture.text(), capture.text()
        os.write(master_fd, b"iNsPeCt")
        read_until(master_fd, child, capture, "Filter: iNsPeCt")
        assert "New conversation" not in capture.text(), capture.text()
        assert "No transcript fields to inspect" in capture.text(), capture.text()
        os.write(master_fd, b"\r")
        drain_for(master_fd, capture, 0.1, child)
        assert "No transcript fields to inspect" in capture.text(), capture.text()
        assert "Explicit retry confirmation" not in capture.text(), capture.text()
        os.write(master_fd, b"\x15no-such-action")
        read_until(master_fd, child, capture, "No actions match")
        os.write(master_fd, b"\r")
        drain_for(master_fd, capture, 0.1, child)
        assert "No actions match" in capture.text(), capture.text()
        os.write(master_fd, b"\x1b[200~paste-filter\x1b[201~")
        read_until(master_fd, child, capture, "paste-filter")
        os.write(master_fd, b"\x1b")
        read_until(master_fd, child, capture, "Composer (Ctrl-S send)")
        os.write(master_fd, b"x")
        read_until(master_fd, child, capture, "xa")
        os.write(master_fd, b"\x1a\x1a")
        drain_for(master_fd, capture, 0.1, child)
        os.write(master_fd, b"\x10")
        read_until(master_fd, child, capture, "Command palette")
        os.write(master_fd, b"search")
        read_until(master_fd, child, capture, "Filter: search")
        os.write(master_fd, b"\r")
        read_until(master_fd, child, capture, "Search history")
        assert request_status(status_url)["requests"] == expected_requests
        os.write(master_fd, b"\x1b")
        read_until(master_fd, child, capture, "Composer (Ctrl-S send)")
        drain_for(master_fd, capture, 0.15, child)
        assert request_status(status_url)["requests"] == expected_requests, (
            "opening or filtering the command palette started a provider request"
        )

        os.write(master_fd, b"?typed line\r")
        read_until(master_fd, child, capture, "?typed line")
        question_screen = capture.text()
        assert "?typed line" in question_screen, (
            "? or Enter did not insert literal text and a newline in the composer: "
            f"{question_screen[-1800:]!r}"
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
            master_fd, slave_fd, child, initial_termios = spawn_in_pty(
                command, env, rows=ROWS, columns=COLUMNS
            )
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

                # Preserve a draft while resizing below the supported minimum,
                # restore the terminal, then assert busy-submit remains inert.
                os.write(master_fd, NEXT_DRAFT.encode())
                drain_for(master_fd, capture, 0.1, child)
                resize_pty(slave_fd, 23, 79)
                read_until_screen_text(
                    master_fd, child, capture, "Terminal too small", rows=23, columns=79
                )
                assert request_status(status_url)["requests"] == 1
                resize_pty(slave_fd, 24, 80)
                drain_for(master_fd, capture, 0.2, child)
                active_screen = capture.text()
                assert "status: Running" in active_screen and NEXT_DRAFT in active_screen, (
                    f"active turn or draft changed during resize recovery: {active_screen!r}"
                )
                resize_pty(slave_fd, ROWS, COLUMNS)
                drain_for(master_fd, capture, 0.2, child)

                os.write(master_fd, b"\x13")
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

                os.write(master_fd, b"\x1bOQstop")
                read_until(master_fd, child, capture, "Stop Untitled conversation")
                os.write(master_fd, b"\r")
                read_until(master_fd, child, capture, "Interrupted")
                assert capture.contains("Interrupted"), capture.text()
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
            error_master, error_slave, error_child, error_initial = spawn_in_pty(
                command, error_env, rows=ROWS, columns=COLUMNS
            )
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
                command, negative_env, rows=ROWS, columns=COLUMNS
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
                assert not negative_capture.contains("Interrupted"), negative_capture.text()

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


def start_fixture(
    fixture_path: Path,
    scenario: str,
    workspace: Path | None,
    hold_after_first_chunk: bool = False,
) -> tuple[subprocess.Popen[str], str]:
    command = [
        sys.executable,
        str(fixture_path),
        "--scenario",
        scenario,
        "--workspace",
        str(workspace) if workspace is not None else "",
        "--port",
        "0",
    ]
    if hold_after_first_chunk:
        command.append("--hold-after-first-chunk")
    fixture = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    try:
        status_url = wait_for_fixture_start(fixture)
    except BaseException:
        if fixture.poll() is None:
            fixture.kill()
        try:
            fixture.wait(timeout=2)
        except subprocess.TimeoutExpired:
            fixture.kill()
            fixture.wait(timeout=2)
        raise
    return fixture, status_url


def launch_conversation(
    args: argparse.Namespace, env: dict[str, str], workspace: Path
) -> tuple[int, int, subprocess.Popen[bytes], list[Any], TerminalCapture]:
    launcher = getattr(args, "launcher", None)
    if launcher:
        command = [str(Path(launcher).resolve())]
    else:
        command = [
            str(Path(args.tui_bin).resolve()),
            "--leg-bin",
            str(Path(args.leg_bin).resolve()),
            "--supervisor-bin",
            str(Path(args.supervisor_bin).resolve()),
        ]
    master_fd, slave_fd, child, initial_termios = spawn_in_pty(command, env)
    capture = TerminalCapture(rows=DEFAULT_ROWS, columns=DEFAULT_COLUMNS)
    try:
        read_until(master_fd, child, capture, "Path:")
        os.write(master_fd, str(workspace).encode() + b"\r")
        read_until(master_fd, child, capture, WARNING)
        os.write(master_fd, b"\r")
        read_until(master_fd, child, capture, "Idle")
        return master_fd, slave_fd, child, initial_termios, capture
    except BaseException:
        kill_owned_process_group(child)
        os.close(master_fd)
        os.close(slave_fd)
        raise


def seed_windowed_catalog(
    state_dir: Path,
    workspace: Path,
    turn_count: int = 1000,
    session_title: str = "History fixture",
) -> str:
    session_id = "session-windowed-1000"
    now_ms = int(time.time() * 1000)
    sessions_dir = state_dir / "sessions"
    sessions_dir.mkdir(parents=True)
    events: list[dict[str, Any]] = [
        {
            "schema": EXCHANGE_SCHEMA,
            "event": "session_start",
            "ts_ms": now_ms,
            "session_id": session_id,
        }
    ]
    tool_statuses = {
        turn_count - 5: "failed",
        turn_count - 4: "denied",
        turn_count - 3: "pending",
        turn_count - 2: "interrupted",
        turn_count - 1: "completed",
    }
    for index in range(turn_count):
        prompt = f"history fixture prompt {index:04d}"
        event_ms = now_ms + index * 4
        events.append(
            {
                "schema": EXCHANGE_SCHEMA,
                "event": "request",
                "ts_ms": event_ms,
                "model": "fixture",
                "base_url": "local",
                "prompt": prompt,
                "session_id": session_id,
                "turn_index": index,
            }
        )
        status = tool_statuses.get(index)
        if status:
            tool_specs: list[dict[str, Any]] = [
                {
                    "id": "reused-tool-id",
                    "name": "bash",
                    "input": {"command": f"echo output from turn {index:04d}"},
                    "status": status,
                    "result": f"literal Ω output from turn {index:04d}",
                }
            ]
            if index == turn_count - 1:
                bash_envelope = lambda code, stdout, stderr="", stdout_omitted=0, stderr_omitted=0: json.dumps(
                    {
                        "wall_time_seconds": 0.01,
                        "status": "exited",
                        "exit_code": code,
                        "stdout": stdout,
                        "stderr": stderr,
                        "stdout_omitted_bytes": stdout_omitted,
                        "stderr_omitted_bytes": stderr_omitted,
                    },
                    ensure_ascii=False,
                )
                tool_specs = [
                    tool_specs[0]
                    | {"result": bash_envelope(0, f"literal Ω output from turn {index:04d}")},
                    {
                        "id": "bash-exit-7",
                        "name": "bash",
                        "input": {"command": "false", "description": "check nonzero status"},
                        "status": "completed",
                        "result": bash_envelope(
                            7,
                            "HIDDEN_BASH_STDOUT_Ω",
                            "HIDDEN_BASH_STDERR_Ω",
                            stdout_omitted=12,
                            stderr_omitted=2,
                        ),
                    },
                    {
                        "id": "bash-truncated",
                        "name": "bash",
                        "input": {"command": "large", "description": "truncated output"},
                        "status": "completed",
                        "result": bash_envelope(
                            0,
                            "prefix … [120 bytes omitted]",
                            stdout_omitted=120,
                        ),
                    },
                    {
                        "id": "bash-malformed",
                        "name": "bash",
                        "input": {"command": "malformed", "description": "malformed envelope"},
                        "status": "completed",
                        "result": '{"status":"exited","exit_code":0,"stdout":',
                    },
                    {
                        "id": "bash-unknown-envelope",
                        "name": "bash",
                        "input": {"command": "future", "description": "unknown envelope"},
                        "status": "completed",
                        "result": json.dumps(
                            {
                                "status": "future",
                                "exit_code": 0,
                                "stdout": "unknown-envelope-output",
                                "stderr": "",
                            }
                        ),
                    },
                    {
                        "id": "read-output",
                        "name": "read",
                        "input": {"path": "history.txt", "offset": 2, "limit": 3},
                        "status": "completed",
                        "result": "READ_OUTPUT_SENTINEL",
                    },
                    {
                        "id": "edit-diff",
                        "name": "edit",
                        "input": {"path": "edit.txt"},
                        "status": "completed",
                        "result": (
                            "Successfully replaced 1 occurrence in edit.txt.\n"
                            "--- a/edit.txt\n+++ b/edit.txt\n@@ -1 +1 @@\n-old\n+new\n"
                            "... [diff truncated: 2 more lines]"
                        ),
                    },
                    {
                        "id": "denied-call",
                        "name": "bash",
                        "input": {"command": "blocked command"},
                        "status": "denied",
                        "error": "hook denied",
                    },
                    {
                        "id": "missing-result",
                        "name": "write",
                        "input": {"path": "missing-result.txt"},
                        "status": None,
                    },
                    {
                        "id": "unknown-tool",
                        "name": "lookup",
                        "input": {"query": "HIDDEN_UNKNOWN_INPUT"},
                        "status": "completed",
                        "result": "unknown tool literal result",
                    },
                ]
            events.append(
                {
                    "schema": EXCHANGE_SCHEMA,
                    "event": "tool_round",
                    "ts_ms": event_ms + 1,
                    "content": [
                        {
                            "type": "tool_use",
                            "id": tool["id"],
                            "name": tool["name"],
                            "input": tool["input"],
                        }
                        for tool in tool_specs
                    ],
                    "session_id": session_id,
                    "turn_index": index,
                }
            )
            for tool in tool_specs:
                events.append(
                    {
                        "schema": EXCHANGE_SCHEMA,
                        "event": "tool_call",
                        "ts_ms": event_ms + 2,
                        "tool_use_id": tool["id"],
                        "tool_name": tool["name"],
                        "input": tool["input"],
                        "session_id": session_id,
                        "turn_index": index,
                    }
                )
                tool_status = tool["status"]
                if tool_status is None or tool_status in ("pending", "interrupted"):
                    continue
                result: dict[str, Any] = {
                    "schema": EXCHANGE_SCHEMA,
                    "event": "tool_result",
                    "ts_ms": event_ms + 3,
                    "tool_use_id": tool["id"],
                    "tool_name": tool["name"],
                    "status": tool_status,
                    "session_id": session_id,
                    "turn_index": index,
                }
                if tool_status in ("failed", "denied"):
                    result["error"] = tool.get("error", f"fixture tool {tool_status} from turn {index:04d}")
                else:
                    result["result"] = tool.get("result", f"literal Ω output from turn {index:04d}")
                events.append(result)
            if status == "interrupted":
                events.append(
                    {
                        "schema": EXCHANGE_SCHEMA,
                        "event": "response_error",
                        "ts_ms": event_ms + 3,
                        "kind": "interrupted",
                        "message": "fixture turn interrupted",
                        "session_id": session_id,
                        "turn_index": index,
                    }
                )
                continue
            if status == "pending":
                continue
        events.append(
            {
                "schema": EXCHANGE_SCHEMA,
                "event": "response_ok",
                "ts_ms": event_ms + 4,
                "reply": f"history fixture reply {index:04d}",
                "stop_reason": "end_turn",
                "session_id": session_id,
                "turn_index": index,
            }
        )

    trail_path = sessions_dir / f"{session_id}.jsonl"
    trail_path.write_text(
        "".join(json.dumps(event, ensure_ascii=False) + "\n" for event in events),
        encoding="utf-8",
    )
    trail_path.chmod(0o600)
    metadata = {
        session_id: {
            "name": session_title,
            "cwd": str(workspace.resolve()),
            "created_at_ms": now_ms,
            "updated_at_ms": now_ms,
            "drafts": {"tui": "Alpha draft survives session changes"},
            "display": {
                "private_catalog_marker": "DO_NOT_EXPORT_CATALOG_DATA",
                WARNING_ACK_KEY: True,
            },
        },
        "draft-windowed-beta": {
            "name": "Beta fixture",
            "cwd": str(workspace.resolve()),
            "created_at_ms": now_ms - 1,
            "updated_at_ms": now_ms - 1,
            "drafts": {"tui": "Beta draft stays in its own session"},
        },
    }
    state_dir.mkdir(parents=True, exist_ok=True)
    (state_dir / "catalog.json").write_text(
        json.dumps({"version": 1, "sessions": metadata}, ensure_ascii=False, indent=2),
        encoding="utf-8",
    )
    return session_id


def launch_web_host(
    args: argparse.Namespace, env: dict[str, str], state_dir: Path
) -> tuple[subprocess.Popen[bytes], str, str]:
    command = [
        str(Path(args.web_bin).resolve()),
        "--no-open",
        "--bind",
        "127.0.0.1:0",
        "--state-dir",
        str(state_dir),
        "--leg-bin",
        str(Path(args.leg_bin).resolve()),
        "--supervisor-bin",
        str(Path(args.supervisor_bin).resolve()),
    ]
    host = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    assert host.stdout is not None
    buffered = bytearray()
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        if host.poll() is not None:
            raise AssertionError(f"leg-web exited during startup: {host.returncode}")
        ready, _, _ = select.select([host.stdout.fileno()], [], [], 0.1)
        if not ready:
            continue
        buffered.extend(os.read(host.stdout.fileno(), 8192))
        while b"\n" in buffered:
            line, _, rest = buffered.partition(b"\n")
            buffered = bytearray(rest)
            decoded = line.decode("utf-8", errors="replace")
            if decoded.startswith("Open this one-time launch URL: "):
                launch_url = decoded.split(": ", 1)[1]
                return host, urlsplit(launch_url).netloc, urlsplit(launch_url).fragment
    host.kill()
    host.wait(timeout=2)
    raise AssertionError("leg-web did not print its launch URL within eight seconds")


def web_json_request(
    authority: str,
    token: str,
    method: str,
    path: str,
    payload: dict[str, Any] | None = None,
) -> tuple[int, dict[str, Any]]:
    body = json.dumps(payload).encode() if payload is not None else None
    request = urllib.request.Request(
        f"http://{authority}{path}",
        data=body,
        headers={
            "Authorization": f"Bearer {token}",
            "Origin": f"http://{authority}",
            "Content-Type": "application/json",
        },
        method=method,
    )
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            return response.status, json.loads(response.read())
    except HTTPError as error:
        return error.code, json.loads(error.read())


def select_picker_session(
    master_fd: int,
    child: subprocess.Popen[bytes],
    capture: TerminalCapture,
    title: str,
) -> None:
    os.write(master_fd, b"\x1bOR")
    read_until(master_fd, child, capture, "Sessions · title · workspace · recent · status")
    os.write(master_fd, b"/" + title.encode() + b"\r\r")
    read_until_not_contains(
        master_fd,
        child,
        capture,
        "Sessions · title · workspace · recent · status",
    )
    read_until_not_contains(
        master_fd,
        child,
        capture,
        "S search transcript  ·  F3/Esc back  ·  browse is read-only and never sends",
    )
    read_until(master_fd, child, capture, "Composer (")


def seed_catalog_session(
    state_dir: Path,
    session_id: str,
    title: str,
    workspace: Path | None,
    draft: str,
    prompt: str,
    reply: str,
    updated_at_ms: int,
    recovered: bool = False,
    other_drafts: dict[str, str] | None = None,
) -> None:
    sessions_dir = state_dir / "sessions"
    sessions_dir.mkdir(parents=True, exist_ok=True)
    events = [
        {
            "schema": EXCHANGE_SCHEMA,
            "event": "session_start",
            "ts_ms": updated_at_ms,
            "session_id": session_id,
        },
        {
            "schema": EXCHANGE_SCHEMA,
            "event": "request",
            "ts_ms": updated_at_ms + 1,
            "model": "fixture",
            "base_url": "local",
            "prompt": prompt,
            "session_id": session_id,
            "turn_index": 0,
        },
        {
            "schema": EXCHANGE_SCHEMA,
            "event": "response_ok",
            "ts_ms": updated_at_ms + 2,
            "reply": reply,
            "stop_reason": "end_turn",
            "session_id": session_id,
            "turn_index": 0,
        },
    ]
    (sessions_dir / f"{session_id}.jsonl").write_text(
        "".join(json.dumps(event, ensure_ascii=False) + "\n" for event in events),
        encoding="utf-8",
    )
    catalog_path = state_dir / "catalog.json"
    if catalog_path.exists():
        catalog = json.loads(catalog_path.read_text(encoding="utf-8"))
    else:
        catalog = {"version": 1, "sessions": {}}
    record: dict[str, Any] = {
        "name": title,
        "created_at_ms": updated_at_ms,
        "updated_at_ms": updated_at_ms,
        "drafts": {"tui": draft, **(other_drafts or {})},
    }
    if workspace is not None:
        record["cwd"] = str(workspace)
    if recovered:
        record["recovered"] = True
    catalog["sessions"][session_id] = record
    catalog_path.write_text(json.dumps(catalog, ensure_ascii=False, indent=2), encoding="utf-8")


def stop_fixture(fixture: subprocess.Popen[str]) -> None:
    if fixture.poll() is None:
        with suppress(ProcessLookupError):
            fixture.send_signal(signal.SIGTERM)
    try:
        fixture.wait(timeout=3)
    except subprocess.TimeoutExpired:
        fixture.kill()
        fixture.wait(timeout=2)


def run_turn_contract_smoke(args: argparse.Namespace) -> None:
    repository = Path(__file__).resolve().parents[2]
    fixture_path = repository / "trials" / "fake_provider.py"
    with tempfile.TemporaryDirectory(prefix="leg-tui-contract-pty-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        fixture, status_url = start_fixture(fixture_path, "trial", workspace)
        master_fd = slave_fd = None
        child = None
        output = TerminalCapture()
        try:
            env = os.environ.copy()
            env.update(
                {
                    "LEG_PROVIDER": "anthropic",
                    "ANTHROPIC_BASE_URL": status_url.removesuffix("/__trial/status"),
                    "ANTHROPIC_API_KEY": "trial-only-not-a-secret",
                    "LEG_MODEL": "trial-fixture",
                    "LEG_MAX_RETRIES": "0",
                    "LEG_MAX_TOOL_ROUNDS": "2",
                    "LEG_UI_STATE_DIR": str(root / "state"),
                    "LEG_UI_SUPERVISOR_BIN": str(Path(args.supervisor_bin).resolve()),
                    "LEG_EVENT_LOG": str(root / "leg-events.jsonl"),
                }
            )
            master_fd, slave_fd, child, initial_termios, output = launch_conversation(
                args, env, workspace
            )

            os.write(master_fd, b"TRIAL-TOOL-TEXT\x13")
            read_until(
                master_fd, child, output, "The write result returned; this is the final text."
            )
            read_until(master_fd, child, output, "Succeeded")
            screen = output.text()
            assert screen.count("First the text, then a verified fixture write.") == 1, screen
            assert screen.count("The write result returned; this is the final text.") == 1, screen
            assert "write · completed · fixture-write.txt" in screen, screen
            status = request_status(status_url)
            assert status["requests"] == 2, status

            os.write(master_fd, b"TRIAL-TUI-MULTI-TOOL\x13")
            read_until(master_fd, child, output, "Final multi-round text.")
            drain_for(master_fd, output, 0.25)
            multi_round_screen = output.text()
            for segment in (
                "First multi-round text.",
                "Second multi-round text.",
                "Final multi-round text.",
            ):
                assert multi_round_screen.count(segment) == 1, multi_round_screen
            assert request_status(status_url)["requests"] == 5

            os.write(master_fd, b"TRIAL-TUI-MAX-TOKENS\x13")
            read_until(master_fd, child, output, "Reply truncated at max tokens.")
            assert request_status(status_url)["requests"] == 6

            os.write(master_fd, b"TRIAL-CAP\x13")
            read_until(master_fd, child, output, "Capped")
            read_until(master_fd, child, output, "Tool-round limit reached.")
            assert request_status(status_url)["requests"] == 9

            os.write(master_fd, b"TRIAL-TUI-ANSI\x13")
            read_until(master_fd, child, output, "Before red after Ω")
            drain_for(master_fd, output, 0.25)
            ansi_screen = output.text()
            assert "secret title" not in ansi_screen, ansi_screen
            assert "Before red after Ω" in ansi_screen, ansi_screen
            assert request_status(status_url)["requests"] == 10

            os.write(master_fd, b"TRIAL-LARGE-TOOL\x13")
            read_until(master_fd, child, output, "The large tool result was returned.")
            drain_for(master_fd, output, 0.25)
            large_tool_screen = output.text()
            assert "bash · completed" in large_tool_screen, large_tool_screen
            assert "characters]" not in large_tool_screen, large_tool_screen
            assert request_status(status_url)["requests"] == 12

            os.write(master_fd, b"\x06" + b"12000\r")
            read_until(master_fd, child, output, "Tool bash · id")
            os.write(master_fd, b"\x1b[B\x1b[B")
            read_until(master_fd, child, output, "Tool bash · stdout")
            read_until(master_fd, child, output, "BEGIN_TOOL_OUTPUT")
            read_until(master_fd, child, output, "bytes below; Shift-PageDown to continue")
            for _ in range(20):
                if "END_TOOL_OUTPUT" in output.text():
                    break
                os.write(master_fd, b"\x1b[6;2~")
                drain_for(master_fd, output, 0.1, child)
            assert "END_TOOL_OUTPUT" in output.text(), output.text()
            os.write(master_fd, b"\x1b")
            read_until_not_contains(master_fd, child, output, "Inspector · Up/Down field")
            os.write(master_fd, b"\x1b[1;5F")

            os.write(master_fd, b"TRIAL-TUI-LONG-PAUSE")
            read_until(master_fd, child, output, "TRIAL-TUI-LONG-PAUSE")
            os.write(master_fd, b"\x13")
            read_until(master_fd, child, output, "Long fixture line 090")
            for _ in range(6):
                screen = output.text()
                if "Long fixture line 020" in screen:
                    break
                row_window = re.search(r"Rows (\d+-\d+ of \d+)", screen)
                assert row_window, screen
                previous_body = transcript_pane_text(screen)
                os.write(master_fd, b"\x1b[5~")
                read_until_rows_change(
                    master_fd, child, output, row_window.group(1), previous_body
                )
            assert "Long fixture line 020" in output.text(), output.text()
            read_until(master_fd, child, output, "new content below")
            assert "Long fixture line 020" in output.text(), (
                f"stream update moved the transcript reading position: {output.text()!r}"
            )
            os.write(master_fd, b"\x1bOS\x1b[B\x1b[B")
            read_until(master_fd, child, output, "Inspector")
            os.write(master_fd, NEXT_DRAFT.encode())
            read_until(master_fd, child, output, NEXT_DRAFT)
            assert NEXT_DRAFT in output.text(), (
                f"composer stopped accepting text while inspecting: {output.text()!r}"
            )
            os.write(master_fd, b"\x7f" * len(NEXT_DRAFT))
            drain_for(master_fd, output, 0.3, child)
            assert NEXT_DRAFT not in output.text(), output.text()
            read_until(master_fd, child, output, "Succeeded")
            os.write(master_fd, b"\x1bOS")
            drain_for(master_fd, output, 0.1, child)
            assert "Long fixture line 020" in output.text(), (
                f"completed stream moved the transcript reading position: {output.text()!r}"
            )
            for _ in range(8):
                screen = output.text()
                if "TRIAL-LARGE-TOOL" in screen:
                    break
                row_window = re.search(r"Rows (\d+-\d+ of \d+)", screen)
                assert row_window, screen
                previous_body = transcript_pane_text(screen)
                os.write(master_fd, b"\x1b[5~")
                read_until_rows_change(
                    master_fd, child, output, row_window.group(1), previous_body
                )
            history_screen = output.text()
            assert "TRIAL-LARGE-TOOL" in history_screen, (
                f"PageUp did not reveal the prior transcript turn: {history_screen!r}"
            )
            os.write(master_fd, b"\x1bOS\x1b[B\x1b[B")
            read_until(master_fd, child, output, "Inspector")
            os.write(master_fd, b"\x1b[B")
            read_until(master_fd, child, output, "Assistant reply")
            os.write(master_fd, b"\x1b[6;2~\x1b[6;2~")
            read_until(master_fd, child, output, "Long fixture line 062")
            read_until(master_fd, child, output, "new content below")
            os.write(master_fd, b"\x1b[6;2~" * 22)
            read_until(master_fd, child, output, "END OF FIXTURE ANSWER")
            os.write(master_fd, b"\x1b[1;5F")
            read_until_not_contains(master_fd, child, output, "new content below")
            newest_screen = output.text()
            assert "END OF FIXTURE ANSWER" in newest_screen, newest_screen
            assert "new content below" not in newest_screen, newest_screen
            assert request_status(status_url)["requests"] == 13

            status = drain_until_exit_after_close(master_fd, child, output, slave_fd, initial_termios)
            master_fd = slave_fd = None
            assert status == 0
            final_status = request_status(status_url)
            assert final_status["scenario_requests"] == {
                "TRIAL-TOOL-TEXT": 2,
                "TRIAL-TUI-MULTI-TOOL": 3,
                "TRIAL-TUI-MAX-TOKENS": 1,
                "TRIAL-CAP": 3,
                "TRIAL-TUI-ANSI": 1,
                "TRIAL-LARGE-TOOL": 2,
                "TRIAL-TUI-LONG-PAUSE": 1,
            }, final_status
            for check in (
                "tool_result_returned",
                "multi_tool_results_returned",
                "large_tool_result_returned",
            ):
                assert final_status["input_checks"].get(check) is True, final_status
            assert any(
                check["path"] == "trial-rounds.txt" and check["ok"]
                for check in final_status["workspace_checks"]
            ), final_status
        finally:
            if child is not None:
                kill_owned_process_group(child)
            if master_fd is not None:
                os.close(master_fd)
            if slave_fd is not None:
                os.close(slave_fd)
            stop_fixture(fixture)


def run_issue82_trial_coverage_smoke(args: argparse.Namespace) -> None:
    repository = Path(__file__).resolve().parents[2]
    fixture_path = Path(args.fixture) if getattr(args, "fixture", None) else (
        repository / "trials" / "fake_provider.py"
    )
    with tempfile.TemporaryDirectory(prefix="leg-tui-trial-coverage-pty-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        state_dir = root / "state"
        secret = "tui-trial-fixture-key-must-not-be-displayed"
        hook = root / "deny-hook.py"
        hook.write_text(
            f"#!{sys.executable}\n"
            "import json, sys\n"
            "event = json.load(sys.stdin)\n"
            "command = event.get('tool_input', {}).get('command', '')\n"
            "if 'TRIAL-DENY' in command:\n"
            "    print(json.dumps({'decision':'deny','reason':'trial denial'}))\n"
            "else:\n"
            "    print(json.dumps({'decision':'allow'}))\n",
            encoding="utf-8",
        )
        hook.chmod(0o700)
        fixture, status_url = start_fixture(fixture_path, "trial", workspace)
        master_fd = slave_fd = None
        child = None
        output = TerminalCapture()
        try:
            env = os.environ.copy()
            env.update(
                {
                    "TERM": "xterm-256color",
                    "LEG_PROVIDER": "anthropic",
                    "ANTHROPIC_BASE_URL": status_url.removesuffix("/__trial/status"),
                    "ANTHROPIC_API_KEY": secret,
                    "LEG_MODEL": "trial-fixture",
                    "LEG_MAX_RETRIES": "0",
                    "LEG_MAX_TOOL_ROUNDS": "2",
                    "LEG_BASH_TIMEOUT_SECS": "600",
                    "LEG_PRETOOL_HOOK": str(hook),
                    "LEG_UI_STATE_DIR": str(state_dir),
                    "LEG_UI_SUPERVISOR_BIN": str(Path(args.supervisor_bin).resolve()),
                    "LEG_EVENT_LOG": str(root / "events.jsonl"),
                }
            )
            master_fd, slave_fd, child, initial_termios, output = launch_conversation(
                args, env, workspace
            )

            # The first live-provider turn is a bracketed CJK/multiline paste.
            chinese_prompt = (
                "TRIAL-CHINESE\n"
                "Line one: keep this first.\n"
                "第二行：保留中文。\n"
                "Line three: keep this third."
            )
            os.write(master_fd, b"\x1b[200~" + chinese_prompt.encode() + b"\x1b[201~")
            os.write(master_fd, b"\x13")
            read_until(master_fd, child, output, "Three lines received, including the Chinese second line.")
            chinese_status = request_status(status_url)
            assert chinese_status["input_checks"].get("chinese_multiline_prompt") is True, chinese_status

            os.write(master_fd, b"TRIAL-TOOL-TEXT: inspect this write\x13")
            read_until(master_fd, child, output, "The write result returned; this is the final text.")
            read_until(master_fd, child, output, "write · completed · fixture-write.txt")
            read_until(master_fd, child, output, "Succeeded")
            assert (workspace / "fixture-write.txt").read_text(encoding="utf-8") == "fixture-write-ok\n"
            os.write(master_fd, b"\x1b[1;5A\x1bOS")
            read_until(master_fd, child, output, "Tool write · id")
            os.write(master_fd, b"\x1b[B\x1b[B")
            read_until(master_fd, child, output, "Inspector · Up/Down field")
            read_until(master_fd, child, output, "Tool write · result")
            read_until(master_fd, child, output, "Successfully wrote to")
            os.write(master_fd, b"\x1b")
            read_until(master_fd, child, output, "› write · completed · fixture-write.txt")
            os.write(master_fd, b"\x1b[1;5F")

            os.write(master_fd, b"TRIAL-CONTINUE: continue after inspecting the tool\x13")
            read_until(
                master_fd,
                child,
                output,
                "Continuation response: the earlier fixture answer is still in this session.",
            )

            os.write(master_fd, b"TRIAL-AUTH: verify the fixture auth failure\x13")
            read_until(master_fd, child, output, "Failed")
            auth_status = request_status(status_url)
            assert auth_status["input_checks"].get("auth_error_sent") is True, auth_status

            os.write(master_fd, b"TRIAL-DENIED: exercise the pretool hook\x13")
            read_until(master_fd, child, output, "The tool was denied and its error result was returned.")
            denied_status = request_status(status_url)
            assert denied_status["input_checks"].get("denied_tool_error_returned") is True, denied_status
            assert not (workspace / "denied-marker.txt").exists()

            os.write(master_fd, b"TRIAL-FAILED: exercise an invalid tool call\x13")
            read_until(master_fd, child, output, "The tool failed and its error result was returned.")
            final_status = request_status(status_url)
            assert final_status["input_checks"].get("failed_tool_error_returned") is True, final_status
            assert not (workspace / "failed-marker.txt").exists()
            os.write(master_fd, b"TRIAL-STOP: stop the active tool process tree\x13")
            pid_file = workspace / "trial-stalled-child.pid"
            wait_for_file(pid_file, master_fd, output, child)
            stopped_pid = int(pid_file.read_text(encoding="utf-8").strip())
            os.write(master_fd, b"\x03")
            read_until(master_fd, child, output, "Interrupted")
            wait_for_pid_exit(stopped_pid)
            assert not (workspace / "trial-stall-finished.txt").exists()

            final_status = request_status(status_url)
            assert final_status["requests"] == 10, final_status
            assert final_status["scenario_requests"] == {
                "TRIAL-CHINESE": 1,
                "TRIAL-TOOL-TEXT": 2,
                "TRIAL-CONTINUE": 1,
                "TRIAL-AUTH": 1,
                "TRIAL-DENIED": 2,
                "TRIAL-FAILED": 2,
                "TRIAL-STOP": 1,
            }, final_status
            workspace_checks = {
                check["path"]: check["ok"] for check in final_status["workspace_checks"]
            }
            assert workspace_checks.get("trial-stalled-child.pid") is True, final_status
            assert workspace_checks.get("trial-stall-finished.txt") is True, final_status

            status = drain_until_exit_after_close(
                master_fd, child, output, slave_fd, initial_termios
            )
            master_fd = slave_fd = None
            child = None
            assert status == 0
            assert secret.encode() not in bytes(output.raw), "fixture credential appeared in terminal output"
            events = (root / "events.jsonl").read_text(encoding="utf-8")
            assert secret not in events, "fixture credential appeared in the event log"
        finally:
            if child is not None:
                kill_owned_process_group(child)
            if master_fd is not None:
                os.close(master_fd)
            if slave_fd is not None:
                os.close(slave_fd)
            stop_fixture(fixture)


def drain_until_exit_after_close(
    master_fd: int,
    child: subprocess.Popen[bytes],
    output: TerminalCapture,
    slave_fd: int,
    initial_termios: list[Any],
) -> int:
    os.write(master_fd, b"\x03")
    status = drain_until_exit(master_fd, child, output)
    termios_fd = master_fd if sys.platform == "darwin" else slave_fd
    assert_terminal_restored(bytes(output.raw), termios_fd, initial_termios, child)
    os.close(master_fd)
    os.close(slave_fd)
    return status


def run_retry_confirmation_smoke(args: argparse.Namespace) -> None:
    repository = Path(__file__).resolve().parents[2]
    fixture_path = repository / "trials" / "fake_provider.py"
    with tempfile.TemporaryDirectory(prefix="leg-tui-retry-pty-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        fixture, status_url = start_fixture(fixture_path, "trial", workspace)
        master_fd = slave_fd = None
        child = None
        output = TerminalCapture()
        try:
            env = os.environ.copy()
            env.update(
                {
                    "LEG_PROVIDER": "anthropic",
                    "ANTHROPIC_BASE_URL": status_url.removesuffix("/__trial/status"),
                    "ANTHROPIC_API_KEY": "trial-only-not-a-secret",
                    "LEG_MODEL": "trial-fixture",
                    "LEG_MAX_RETRIES": "0",
                    "LEG_UI_STATE_DIR": str(root / "state"),
                    "LEG_UI_SUPERVISOR_BIN": str(Path(args.supervisor_bin).resolve()),
                    "LEG_EVENT_LOG": str(root / "leg-events.jsonl"),
                }
            )
            master_fd, slave_fd, child, initial_termios, output = launch_conversation(
                args, env, workspace
            )
            retry_prompt = "TRIAL-REOPEN-FAILURE retry this failed prompt"
            os.write(master_fd, retry_prompt.encode() + b"\x13")
            read_until(master_fd, child, output, "Failed")
            assert request_status(status_url)["requests"] == 1
            drain_for(master_fd, output, 0.15)
            assert request_status(status_url)["requests"] == 1, "reading a failure retried it"

            os.write(master_fd, b"\r")
            drain_for(master_fd, output, 0.15)
            assert request_status(status_url)["requests"] == 1, "Enter retried a failed prompt"
            os.write(master_fd, b"\x7f")
            drain_for(master_fd, output, 0.1)

            warning = "Retry sends this prompt again and may repeat tool side effects."
            os.write(master_fd, b"\x1bOQReTrY")
            read_until(master_fd, child, output, "Retry latest failed turn")
            drain_for(master_fd, output, 0.1, child)
            assert "No eligible failed or incomplete latest turn" not in output.text()
            os.write(master_fd, b"\r")
            read_until(master_fd, child, output, warning)
            assert request_status(status_url)["requests"] == 1
            os.write(master_fd, b"\r")
            drain_for(master_fd, output, 0.15)
            assert request_status(status_url)["requests"] == 1, "Enter confirmed a palette retry"
            assert warning in output.text()
            os.write(master_fd, b"n")
            drain_for(master_fd, output, 0.1)
            assert request_status(status_url)["requests"] == 1
            assert "Explicit retry confirmation" not in output.text()

            os.write(master_fd, b"\x13")
            read_until(master_fd, child, output, warning)
            assert request_status(status_url)["requests"] == 1
            os.write(master_fd, b"\r")
            drain_for(master_fd, output, 0.15)
            assert request_status(status_url)["requests"] == 1, "Enter confirmed a retry"
            assert warning in output.text()

            os.write(master_fd, b"n")
            drain_for(master_fd, output, 0.1)
            assert request_status(status_url)["requests"] == 1
            assert "Explicit retry confirmation" not in output.text()
            os.write(master_fd, b"\x13")
            read_until(master_fd, child, output, warning)
            assert request_status(status_url)["requests"] == 1
            os.write(master_fd, b"y")
            read_until(master_fd, child, output, "The explicit retry succeeded.")
            drain_for(master_fd, output, 0.25)
            assert request_status(status_url)["requests"] == 2

            status = drain_until_exit_after_close(master_fd, child, output, slave_fd, initial_termios)
            master_fd = slave_fd = None
            assert status == 0
            final_status = request_status(status_url)
            assert final_status["scenario_requests"] == {"TRIAL-REOPEN-FAILURE": 2}, final_status
            assert final_status["input_checks"].get("retry_or_reopen_succeeded") is True, final_status
        finally:
            if child is not None:
                kill_owned_process_group(child)
            if master_fd is not None:
                os.close(master_fd)
            if slave_fd is not None:
                os.close(slave_fd)
            stop_fixture(fixture)


def run_session_navigation_smoke(args: argparse.Namespace) -> None:
    repository = Path(__file__).resolve().parents[2]
    fixture_path = repository / "trials" / "fake_provider.py"
    with tempfile.TemporaryDirectory(prefix="leg-tui-navigation-pty-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        state_dir = root / "state"
        fixture, status_url = start_fixture(fixture_path, "trial", workspace)
        master_fd = slave_fd = None
        child = None
        output = TerminalCapture()
        try:
            env = os.environ.copy()
            env.update(
                {
                    "TERM": "xterm-256color",
                    "LEG_PROVIDER": "anthropic",
                    "ANTHROPIC_BASE_URL": status_url.removesuffix("/__trial/status"),
                    "ANTHROPIC_API_KEY": "trial-only-not-a-secret",
                    "LEG_MODEL": "trial-fixture",
                    "LEG_MAX_RETRIES": "0",
                    "LEG_UI_STATE_DIR": str(state_dir),
                    "LEG_UI_SUPERVISOR_BIN": str(Path(args.supervisor_bin).resolve()),
                }
            )
            master_fd, slave_fd, child, initial_termios, output = launch_conversation(
                args, env, workspace
            )

            os.write(master_fd, b"\x1bOQrename\r")
            read_until(master_fd, child, output, "Rename session")
            os.write(master_fd, b"Alpha\r")
            read_until(master_fd, child, output, "Alpha")
            os.write(master_fd, b"\r")
            read_until(master_fd, child, output, "Alpha  |  model:")

            seed_prompt = "TRIAL-NAV-SEED: create successful tool history"
            os.write(master_fd, seed_prompt.encode() + b"\x13")
            read_until(master_fd, child, output, "The navigation seed is complete.")
            read_until(master_fd, child, output, "Succeeded")
            seed_status = request_status(status_url)
            assert seed_status["scenario_requests"] == {"TRIAL-NAV-SEED": 2}, seed_status
            assert seed_status["input_checks"].get("navigation_seed_tool_result_returned") is True

            alpha_draft = "Alpha draft before restart"
            os.write(master_fd, alpha_draft.encode())
            read_until(master_fd, child, output, alpha_draft)
            os.write(master_fd, b"\x1bOQNew conversation\r")
            read_until(master_fd, child, output, "Untitled conversation  |  model:")
            os.write(master_fd, b"\x1bORr")
            read_until(master_fd, child, output, "Rename session")
            os.write(master_fd, b"Beta\r")
            read_until(master_fd, child, output, "Beta")
            os.write(master_fd, b"\r")
            read_until(master_fd, child, output, "Beta  |  model:")

            loser_prompt = "TRIAL-NAV-LOSER: belongs only to Beta"
            os.write(master_fd, loser_prompt.encode() + b"\x13")
            read_until(master_fd, child, output, "Deterministic fixture response for TRIAL-NAV-LOSER.")
            read_until(master_fd, child, output, "Succeeded")
            beta_draft = "Beta draft before restart"
            os.write(master_fd, beta_draft.encode())
            read_until(master_fd, child, output, beta_draft)
            status = drain_until_exit_after_close(
                master_fd, child, output, slave_fd, initial_termios
            )
            master_fd = slave_fd = None
            child = None
            assert status == 0
            assert request_status(status_url)["requests"] == 3

            catalog_path = state_dir / "catalog.json"
            catalog = json.loads(catalog_path.read_text(encoding="utf-8"))
            alpha_id = next(
                session_id
                for session_id, record in catalog["sessions"].items()
                if record.get("name") == "Alpha" and not session_id.startswith("draft-")
            )
            assert any(
                record.get("name") == "Beta" and not session_id.startswith("draft-")
                for session_id, record in catalog["sessions"].items()
            ), catalog
            alpha_record = catalog["sessions"][alpha_id]
            assert alpha_record.get("drafts", {}).get("tui") == alpha_draft, alpha_record
            alpha_record.setdefault("display", {})["private_catalog_marker"] = (
                "DO_NOT_EXPORT_CATALOG_DATA"
            )
            alpha_record["display"]["inherited_key_marker"] = "DO_NOT_EXPORT_INHERITED_KEY"
            catalog_path.write_text(json.dumps(catalog, indent=2), encoding="utf-8")
            export_path = workspace / "leg-transcript.json"
            export_path.write_text("keep this until overwrite is confirmed", encoding="utf-8")

            command = [
                str(Path(args.tui_bin).resolve()),
                "--leg-bin",
                str(Path(args.leg_bin).resolve()),
                "--supervisor-bin",
                str(Path(args.supervisor_bin).resolve()),
            ]
            master_fd, slave_fd, child, initial_termios = spawn_in_pty(command, env)
            output = TerminalCapture()
            read_until(master_fd, child, output, "Sessions · title · workspace · recent · status")
            startup_picker = output.text()
            for item in ("Alpha", "Beta", str(workspace.resolve()), "Ready", "just now"):
                assert item in startup_picker, f"startup picker omitted {item!r}: {startup_picker!r}"
            os.write(master_fd, b"/Alpha\r\r")
            read_until(master_fd, child, output, "Alpha  |  model:")
            read_until(master_fd, child, output, alpha_draft)
            assert request_status(status_url)["requests"] == 3, "opening history called the provider"

            os.write(master_fd, b"\x06")
            read_until(master_fd, child, output, "Search history")
            os.write(master_fd, b"TRIAL-NAV")
            read_until(master_fd, child, output, "Match 1 of 2")
            os.write(master_fd, b"\x1b[B")
            read_until(master_fd, child, output, "Match 2 of 2")
            os.write(master_fd, b"\x1b[A")
            read_until(master_fd, child, output, "Match 1 of 2")
            os.write(master_fd, b"\x15")
            read_until(master_fd, child, output, "Search titles and complete sanitized transcript source.")
            os.write(master_fd, b"session-rail-tool-history")
            read_until(master_fd, child, output, "Match 1 of 1")
            os.write(master_fd, b"\r")
            read_until(master_fd, child, output, "Inspector · Up/Down field")
            read_until(master_fd, child, output, "Tool bash · id")
            os.write(master_fd, b"\x1b[B" * 2)
            read_until(master_fd, child, output, "Tool bash · stdout")
            read_until(master_fd, child, output, "tool-history")
            before_copy = bytes(output.raw)
            os.write(master_fd, b"\x1b[15~")
            drain_for(master_fd, output, 0.1, child)
            assert b"\x1b]52;c;" in bytes(output.raw[len(before_copy) :]), (
                "F5 did not send the selected tool output through OSC 52"
            )
            assert "F7 to save" in output.text(), output.text()
            os.write(master_fd, b"\x1b[18~")
            read_until(master_fd, child, output, "Save selected text")
            os.write(master_fd, b"\r")
            read_until(master_fd, child, output, "Saved")
            copy_path = workspace / "leg-copy.txt"
            assert "session-rail-tool-history" in copy_path.read_text(encoding="utf-8")
            assert request_status(status_url)["requests"] == 3, "copy invoked the provider"

            os.write(master_fd, b"\x1b[17~\r")
            read_until(master_fd, child, output, "This file already exists.")
            os.write(master_fd, b"n")
            read_until(master_fd, child, output, "Export cancelled")
            assert export_path.read_text(encoding="utf-8") == "keep this until overwrite is confirmed"
            os.write(master_fd, b"\x1b[17~\r")
            read_until(master_fd, child, output, "This file already exists.")
            os.write(master_fd, b"y")
            read_until(master_fd, child, output, "Saved")
            exported = json.loads(export_path.read_text(encoding="utf-8"))
            assert len(exported["turns"]) == 1, exported.keys()
            assert "TRIAL-NAV-SEED" in json.dumps(exported)
            assert "DO_NOT_EXPORT_CATALOG_DATA" not in json.dumps(exported)
            assert "DO_NOT_EXPORT_INHERITED_KEY" not in json.dumps(exported)
            assert "catalog_private_marker" not in json.dumps(exported)

            select_picker_session(master_fd, child, output, "Beta")
            read_until(master_fd, child, output, beta_draft)
            select_picker_session(master_fd, child, output, "Alpha")
            read_until(master_fd, child, output, alpha_draft)
            os.write(master_fd, b"\x7f" * len(alpha_draft))
            drain_for(master_fd, output, 0.1, child)
            continuation = "TRIAL-NAV-CONTINUE: inspect the reopened tool history and cwd"
            os.write(master_fd, continuation.encode() + b"\x13")
            read_until(
                master_fd,
                child,
                output,
                "The reopened session kept its prior tool history and workspace.",
            )
            final_status = request_status(status_url)
            assert final_status["requests"] == 4, final_status
            for check in (
                "navigation_prior_text_and_tool_history_returned",
                "navigation_recorded_workspace_returned",
                "navigation_other_session_history_excluded",
            ):
                assert final_status["input_checks"].get(check) is True, final_status

            status = drain_until_exit_after_close(
                master_fd, child, output, slave_fd, initial_termios
            )
            master_fd = slave_fd = None
            child = None
            assert status == 0
        finally:
            if child is not None:
                kill_owned_process_group(child)
            if master_fd is not None:
                os.close(master_fd)
            if slave_fd is not None:
                os.close(slave_fd)
            stop_fixture(fixture)


def run_workspace_flow_smoke(args: argparse.Namespace) -> None:
    command = [
        str(Path(args.tui_bin).resolve()),
        "--leg-bin",
        str(Path(args.leg_bin).resolve()),
        "--supervisor-bin",
        str(Path(args.supervisor_bin).resolve()),
    ]
    now_ms = int(time.time() * 1000)

    with tempfile.TemporaryDirectory(prefix="leg-tui-replacement-pty-") as temporary:
        root = Path(temporary)
        state_dir = root / "state"
        alpha_workspace = root / "alpha-workspace"
        replacement_workspace = root / "replacement-workspace"
        second_replacement_workspace = root / "second-replacement-workspace"
        alpha_workspace.mkdir()
        replacement_workspace.mkdir()
        second_replacement_workspace.mkdir()
        missing_workspace = root / "removed-beta-workspace"
        alpha_original_draft = "Alpha original draft before replacement"
        beta_original_draft = "Beta original draft before replacement"
        alpha_draft = "Alpha edited draft stays with Alpha"
        beta_draft = "Beta edited draft stays with Beta"
        seed_catalog_session(
            state_dir,
            "session-workspace-alpha",
            "Alpha",
            alpha_workspace,
            alpha_original_draft,
            "Alpha transcript marker",
            "Alpha history reply",
            now_ms,
        )
        seed_catalog_session(
            state_dir,
            "session-workspace-beta",
            "Beta",
            missing_workspace,
            beta_original_draft,
            "Beta transcript marker",
            "Beta history reply",
            now_ms - 1,
        )
        env = os.environ.copy()
        env.update(
            {
                "TERM": "xterm-256color",
                "LEG_UI_STATE_DIR": str(state_dir),
                "LEG_UI_SUPERVISOR_BIN": str(Path(args.supervisor_bin).resolve()),
            }
        )
        master_fd = slave_fd = None
        child = None
        output = TerminalCapture()
        try:
            master_fd, slave_fd, child, initial_termios = spawn_in_pty(command, env)
            read_until(
                master_fd,
                child,
                output,
                "Sessions · title · workspace · recent · status",
            )
            os.write(master_fd, b"/Alpha\r\r")
            read_until(master_fd, child, output, "Alpha  |  model:")
            read_until(master_fd, child, output, "Alpha transcript marker")
            read_until(master_fd, child, output, alpha_original_draft)
            os.write(master_fd, b"\x7f" * (len(alpha_original_draft) + 2))
            os.write(master_fd, alpha_draft.encode())
            read_until(master_fd, child, output, alpha_draft)

            os.write(master_fd, b"\x1bOR")
            read_until(
                master_fd,
                child,
                output,
                "Sessions · title · workspace · recent · status",
            )
            os.write(master_fd, b"/Beta\r")
            read_until(master_fd, child, output, "Filter: Beta")
            os.write(master_fd, b"w")
            read_until(master_fd, child, output, "Choose an existing replacement workspace")
            read_until_workspace_path(master_fd, child, output, str(missing_workspace))
            assert workspace_path_text(output) == str(missing_workspace), output.text()
            os.write(master_fd, b"\x7f" * (len(str(missing_workspace)) + 2))
            os.write(master_fd, str(replacement_workspace).encode() + b"\r")
            read_until(master_fd, child, output, WARNING)
            assert "Press Enter to acknowledge" in output.text(), output.text()
            os.write(master_fd, b"\x1b")
            read_until_not_contains(master_fd, child, output, WARNING)
            read_until(master_fd, child, output, "Choose a workspace")
            read_until_workspace_path(
                master_fd,
                child,
                output,
                str(replacement_workspace),
            )
            assert workspace_path_text(output) == str(replacement_workspace), output.text()
            os.write(master_fd, b"\x7f" * (len(str(replacement_workspace)) + 2))
            os.write(master_fd, str(second_replacement_workspace).encode())
            read_until_workspace_path(
                master_fd,
                child,
                output,
                str(second_replacement_workspace),
            )
            assert workspace_path_text(output) == str(second_replacement_workspace), output.text()
            os.write(master_fd, b"\r")
            read_until(master_fd, child, output, WARNING)
            assert "Press Enter to acknowledge" in output.text(), output.text()
            os.write(master_fd, b"\r")
            read_until_not_contains(master_fd, child, output, WARNING)
            read_until(master_fd, child, output, "Beta  |  model:")
            read_until_not_contains(
                master_fd,
                child,
                output,
                "Select an existing workspace directory before starting a turn.",
            )
            read_until(master_fd, child, output, beta_original_draft)
            replacement_screen = output.text()
            assert "Beta transcript marker" in replacement_screen, replacement_screen
            assert "Alpha transcript marker" not in replacement_screen, replacement_screen
            os.write(master_fd, b"\x7f" * (len(beta_original_draft) + 2))
            os.write(master_fd, beta_draft.encode())
            read_until(master_fd, child, output, beta_draft)

            select_picker_session(master_fd, child, output, "Alpha")
            read_until(master_fd, child, output, "Alpha  |  model:")
            read_until(master_fd, child, output, alpha_draft)
            saved_alpha = json.loads(
                (state_dir / "catalog.json").read_text(encoding="utf-8")
            )["sessions"]["session-workspace-alpha"]["drafts"]["tui"]
            assert saved_alpha == alpha_draft, saved_alpha
            alpha_screen = output.text()
            assert "Alpha transcript marker" in alpha_screen, alpha_screen
            assert "Alpha history reply" in alpha_screen, alpha_screen
            assert alpha_draft in alpha_screen, alpha_screen
            assert "Beta transcript marker" not in alpha_screen, alpha_screen

            select_picker_session(master_fd, child, output, "Beta")
            read_until(master_fd, child, output, beta_draft)
            beta_screen = output.text()
            assert "Beta  |  model:" in beta_screen, beta_screen
            assert "Beta transcript marker" in beta_screen, beta_screen
            assert "Beta history reply" in beta_screen, beta_screen
            assert beta_draft in beta_screen, beta_screen
            assert "Alpha transcript marker" not in beta_screen, beta_screen

            catalog = json.loads((state_dir / "catalog.json").read_text(encoding="utf-8"))
            alpha_record = catalog["sessions"]["session-workspace-alpha"]
            beta_record = catalog["sessions"]["session-workspace-beta"]
            assert set(catalog["sessions"]) == {
                "session-workspace-alpha",
                "session-workspace-beta",
            }, catalog
            assert alpha_record["cwd"] == str(alpha_workspace), alpha_record
            assert beta_record["cwd"] == str(second_replacement_workspace.resolve()), beta_record
            assert alpha_record["drafts"]["tui"] == alpha_draft, alpha_record
            assert beta_record["drafts"]["tui"] == beta_draft, beta_record

            status = drain_until_exit_after_close(
                master_fd, child, output, slave_fd, initial_termios
            )
            master_fd = slave_fd = None
            child = None
            assert status == 0
        finally:
            if child is not None:
                kill_owned_process_group(child)
            if master_fd is not None:
                os.close(master_fd)
            if slave_fd is not None:
                os.close(slave_fd)

    with tempfile.TemporaryDirectory(prefix="leg-tui-new-recovered-pty-") as temporary:
        root = Path(temporary)
        state_dir = root / "state"
        workspace = root / "new-session-workspace"
        workspace.mkdir()
        recovered_draft = "Recovered draft stays untouched"
        seed_catalog_session(
            state_dir,
            "session-recovered-no-workspace",
            "Recovered",
            None,
            recovered_draft,
            "Recovered transcript marker",
            "Recovered history reply",
            now_ms,
            recovered=True,
            other_drafts={"web": "Existing web draft"},
        )
        env = os.environ.copy()
        env.update(
            {
                "TERM": "xterm-256color",
                "LEG_UI_STATE_DIR": str(state_dir),
                "LEG_UI_SUPERVISOR_BIN": str(Path(args.supervisor_bin).resolve()),
            }
        )
        master_fd = slave_fd = None
        child = None
        output = TerminalCapture()
        try:
            master_fd, slave_fd, child, initial_termios = spawn_in_pty(command, env)
            read_until(
                master_fd,
                child,
                output,
                "Sessions · title · workspace · recent · status",
            )
            os.write(master_fd, b"/Recovered\r\r")
            read_until(master_fd, child, output, "Recovered  |  model:")
            read_until(master_fd, child, output, "Recovered transcript marker")

            os.write(master_fd, b"\x1bOR")
            read_until(
                master_fd,
                child,
                output,
                "Sessions · title · workspace · recent · status",
            )
            os.write(master_fd, b"n")
            read_until(master_fd, child, output, "Choose a workspace for the new session")
            os.write(master_fd, str(workspace).encode() + b"\r")
            read_until(master_fd, child, output, WARNING)
            assert "Press Enter to acknowledge" in output.text(), output.text()
            blocked_prompt = "This must not be sent before warning acknowledgement"
            os.write(master_fd, blocked_prompt.encode() + b"\x13")
            drain_for(master_fd, output, 0.15, child)
            assert blocked_prompt not in output.text(), output.text()
            assert output.contains(WARNING), output.text()
            os.write(master_fd, b"\r")
            read_until_not_contains(master_fd, child, output, WARNING)
            new_draft = "This belongs to the new session"
            os.write(master_fd, new_draft.encode())
            read_until(master_fd, child, output, new_draft)

            status = drain_until_exit_after_close(
                master_fd, child, output, slave_fd, initial_termios
            )
            master_fd = slave_fd = None
            child = None
            assert status == 0
            catalog = json.loads((state_dir / "catalog.json").read_text(encoding="utf-8"))
            recovered_record = catalog["sessions"]["session-recovered-no-workspace"]
            assert recovered_record.get("cwd") is None, recovered_record
            assert recovered_record.get("recovered") is True, recovered_record
            assert recovered_record["drafts"]["tui"] == recovered_draft, recovered_record
            assert recovered_record["drafts"]["web"] == "Existing web draft", recovered_record
            new_sessions = {
                session_id: record
                for session_id, record in catalog["sessions"].items()
                if session_id != "session-recovered-no-workspace"
            }
            assert len(new_sessions) == 1, catalog
            new_session = next(iter(new_sessions.values()))
            assert new_session["cwd"] == str(workspace.resolve()), new_session
            assert new_session["drafts"]["tui"] == new_draft, new_session
            assert new_session["display"][WARNING_ACK_KEY] is True, new_session
        finally:
            if child is not None:
                kill_owned_process_group(child)
            if master_fd is not None:
                os.close(master_fd)
            if slave_fd is not None:
                os.close(slave_fd)


def run_background_session_busy_smoke(args: argparse.Namespace) -> None:
    repository = Path(__file__).resolve().parents[2]
    fixture_path = repository / "trials" / "fake_provider.py"
    with tempfile.TemporaryDirectory(prefix="leg-tui-background-pty-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        state_dir = root / "state"
        fixture, status_url = start_fixture(
            fixture_path,
            "paused-live-text",
            workspace,
            hold_after_first_chunk=True,
        )
        master_fd = slave_fd = None
        child = None
        web_host = None
        output = TerminalCapture()
        try:
            env = os.environ.copy()
            env.update(
                {
                    "TERM": "xterm-256color",
                    "LEG_PROVIDER": "anthropic",
                    "ANTHROPIC_BASE_URL": status_url.removesuffix("/__trial/status"),
                    "ANTHROPIC_API_KEY": "trial-only-not-a-secret",
                    "LEG_MODEL": "trial-fixture",
                    "LEG_MAX_RETRIES": "0",
                    "LEG_UI_STATE_DIR": str(state_dir),
                    "LEG_UI_SUPERVISOR_BIN": str(Path(args.supervisor_bin).resolve()),
                }
            )
            master_fd, slave_fd, child, initial_termios, output = launch_conversation(
                args, env, workspace
            )
            os.write(master_fd, b"\x1bORr")
            read_until(master_fd, child, output, "Rename session")
            os.write(master_fd, b"Alpha\r\r")
            read_until(master_fd, child, output, "Alpha  |  model:")
            os.write(master_fd, b"Hold this turn while another session is open\x13")
            read_until(master_fd, child, output, "The first live text is visible.")
            held = wait_for_status(
                status_url,
                lambda value: value["requests"] == 1
                and value["active_requests"] == [1]
                and value["pause_gates"].get("1") == "held",
                "the TUI-owned request to be held",
            )
            assert held["requests"] == 1, held
            alpha_draft = "Alpha draft while its turn runs"
            os.write(master_fd, alpha_draft.encode())
            read_until(master_fd, child, output, alpha_draft)

            catalog = json.loads((state_dir / "catalog.json").read_text(encoding="utf-8"))
            alpha_id = next(
                session_id
                for session_id in catalog["sessions"]
                if not session_id.startswith("draft-")
            )
            web_host, authority, token = launch_web_host(args, env, state_dir)
            snapshot_status, snapshot = web_json_request(
                authority, token, "GET", f"/api/sessions/{alpha_id}/snapshot"
            )
            assert snapshot_status == 200, snapshot
            submit_status, busy = web_json_request(
                authority,
                token,
                "POST",
                f"/api/sessions/{alpha_id}/submit",
                {
                    "request_id": snapshot["next_request_id"],
                    "prompt": "TRIAL-NAV-LOSER: cross-interface busy probe",
                },
            )
            assert (submit_status, busy) == (409, {"error": "session_busy"}), (submit_status, busy)
            assert request_status(status_url)["requests"] == 1, "busy submission called the provider"

            os.write(master_fd, b"\x1bOR")
            read_until(master_fd, child, output, "Sessions · title · workspace · recent · status")
            assert "Alpha  [Busy]" in output.text(), output.text()
            os.write(master_fd, b"n")
            read_until(master_fd, child, output, "Untitled conversation  |  model:")
            os.write(master_fd, b"\x1bORr")
            read_until(master_fd, child, output, "Rename session")
            os.write(master_fd, b"Beta\r")
            read_until(master_fd, child, output, "Beta")
            os.write(master_fd, b"\r")
            read_until(master_fd, child, output, "Beta  |  model:")
            beta_draft = "Beta draft while Alpha runs"
            os.write(master_fd, beta_draft.encode())
            read_until(master_fd, child, output, beta_draft)

            select_picker_session(master_fd, child, output, "Alpha")
            read_until(master_fd, child, output, alpha_draft)
            assert "status: Running" in output.text(), output.text()
            select_picker_session(master_fd, child, output, "Beta")
            read_until(master_fd, child, output, beta_draft)
            assert "Background: Alpha (Running)" in output.text(), output.text()

            release_gate(status_url.removesuffix("/__trial/status"), 1)
            wait_for_status(
                status_url,
                lambda value: value["request_outcomes"].get("1") == "completed",
                "the background TUI turn to finish while Beta is open",
            )
            select_picker_session(master_fd, child, output, "Alpha")
            read_until(master_fd, child, output, "Succeeded")
            read_until(master_fd, child, output, "The first live text is visible.")
            assert "status: Succeeded" in output.text(), output.text()
            read_until(master_fd, child, output, alpha_draft)
            assert request_status(status_url)["requests"] == 1
            select_picker_session(master_fd, child, output, "Beta")
            read_until(master_fd, child, output, beta_draft)

            status = drain_until_exit_after_close(
                master_fd, child, output, slave_fd, initial_termios
            )
            master_fd = slave_fd = None
            child = None
            assert status == 0
        finally:
            if web_host is not None and web_host.poll() is None:
                web_host.send_signal(signal.SIGINT)
                try:
                    web_host.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    web_host.kill()
                    web_host.wait(timeout=2)
            if child is not None:
                kill_owned_process_group(child)
            if master_fd is not None:
                os.close(master_fd)
            if slave_fd is not None:
                os.close(slave_fd)
            stop_fixture(fixture)


def run_background_stop_chooser_smoke(args: argparse.Namespace) -> None:
    repository = Path(__file__).resolve().parents[2]
    fixture_path = repository / "trials" / "fake_provider.py"
    with tempfile.TemporaryDirectory(prefix="leg-tui-stop-chooser-pty-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        state_dir = root / "state"
        fixture, status_url = start_fixture(
            fixture_path,
            "paused-live-text",
            workspace,
            hold_after_first_chunk=True,
        )
        master_fd = slave_fd = None
        child = None
        output = TerminalCapture()
        try:
            env = os.environ.copy()
            env.update(
                {
                    "TERM": "xterm-256color",
                    "LEG_PROVIDER": "anthropic",
                    "ANTHROPIC_BASE_URL": status_url.removesuffix("/__trial/status"),
                    "ANTHROPIC_API_KEY": "trial-only-not-a-secret",
                    "LEG_MODEL": "trial-fixture",
                    "LEG_MAX_RETRIES": "0",
                    "LEG_UI_STATE_DIR": str(state_dir),
                    "LEG_UI_SUPERVISOR_BIN": str(Path(args.supervisor_bin).resolve()),
                }
            )
            master_fd, slave_fd, child, initial_termios, output = launch_conversation(
                args, env, workspace
            )

            os.write(master_fd, b"\x1bORr")
            read_until(master_fd, child, output, "Rename session")
            os.write(master_fd, b"Alpha\r\r")
            read_until(master_fd, child, output, "Alpha  |  model:")
            os.write(master_fd, b"Alpha run held for chooser\x13")
            read_until(master_fd, child, output, LIVE_TEXT)
            wait_for_status(
                status_url,
                lambda value: value["requests"] == 1
                and value["active_requests"] == [1]
                and value["pause_gates"].get("1") == "held",
                "Alpha's request to be active and held",
            )
            alpha_draft = "Alpha draft stays with Alpha"
            os.write(master_fd, alpha_draft.encode())
            read_until(master_fd, child, output, alpha_draft)

            os.write(master_fd, b"\x1bORn")
            read_until(master_fd, child, output, "Untitled conversation  |  model:")
            os.write(master_fd, b"\x1bORr")
            read_until(master_fd, child, output, "Rename session")
            os.write(master_fd, b"Beta\r")
            read_until(master_fd, child, output, "Beta")
            os.write(master_fd, b"\r")
            read_until(master_fd, child, output, "Beta  |  model:")
            os.write(master_fd, b"Beta run held for chooser\x13")
            read_until(master_fd, child, output, STOP_LIVE_TEXT)
            wait_for_status(
                status_url,
                lambda value: value["requests"] == 2
                and value["active_requests"] == [1, 2]
                and value["pause_gates"].get("2") == "held",
                "Beta's request to be active beside Alpha",
            )
            beta_draft = "Beta draft stays with Beta"
            os.write(master_fd, beta_draft.encode())
            read_until(master_fd, child, output, beta_draft)

            os.write(master_fd, b"\x1bORn")
            read_until(master_fd, child, output, "Untitled conversation  |  model:")
            os.write(master_fd, b"\x1bORr")
            read_until(master_fd, child, output, "Rename session")
            os.write(master_fd, b"Gamma\r")
            read_until(master_fd, child, output, "Gamma")
            os.write(master_fd, b"\r")
            read_until(master_fd, child, output, "Gamma  |  model:")
            gamma_draft = "Gamma remains idle while two runs continue"
            os.write(master_fd, gamma_draft.encode())
            read_until(master_fd, child, output, gamma_draft)
            assert "Background: Alpha (Running), Beta (Running)" in output.text(), output.text()

            select_picker_session(master_fd, child, output, "Beta")
            read_until(master_fd, child, output, beta_draft)
            select_picker_session(master_fd, child, output, "Alpha")
            read_until(master_fd, child, output, alpha_draft)
            select_picker_session(master_fd, child, output, "Gamma")
            read_until(master_fd, child, output, gamma_draft)
            assert request_status(status_url)["requests"] == 2, (
                "switching sessions replayed a background prompt"
            )

            os.write(master_fd, b"\x1bOQexit")
            read_until(master_fd, child, output, "Active turn in ")
            os.write(master_fd, b"\r")
            drain_for(master_fd, output, 0.1, child)
            assert child.poll() is None, "disabled palette Exit exited during background work"
            assert request_status(status_url)["active_requests"] == [1, 2]
            os.write(master_fd, b"\x1b")
            drain_for(master_fd, output, 0.1, child)

            os.write(master_fd, b"\x1bOQstop\r")
            read_until(master_fd, child, output, "Choose a background session to stop")
            read_until(master_fd, child, output, "Alpha")
            read_until(master_fd, child, output, "Beta")
            held = request_status(status_url)
            assert held["active_requests"] == [1, 2], (
                f"opening the chooser stopped a run: {held!r}"
            )
            os.write(master_fd, b"\x1b")
            drain_for(master_fd, output, 0.1, child)
            assert "Choose a background session to stop" not in output.text(), output.text()
            held = request_status(status_url)
            assert held["active_requests"] == [1, 2], (
                f"cancelling the chooser stopped a run: {held!r}"
            )

            os.write(master_fd, b"\x03")
            read_until(master_fd, child, output, "Choose a background session to stop")
            os.write(master_fd, b"\x1b[B\r")
            stopped = wait_for_status(
                status_url,
                lambda value: value["request_outcomes"].get("2") == "interrupted"
                and value["active_requests"] == [1],
                "only Beta to stop from the selected background target",
            )
            assert stopped["request_outcomes"].get("1") == "active", stopped
            assert stopped["requests"] == 2, stopped

            select_picker_session(master_fd, child, output, "Beta")
            read_until(master_fd, child, output, beta_draft)
            assert "status: Interrupted" in output.text(), output.text()
            assert request_status(status_url)["active_requests"] == [1]
            select_picker_session(master_fd, child, output, "Gamma")
            read_until(master_fd, child, output, gamma_draft)

            # Let Alpha finish while its entry remains visible in the chooser.
            # Enter must recheck that exact target and leave no other run to stop.
            os.write(master_fd, b"\x03")
            read_until(master_fd, child, output, "Choose a background session to stop")
            assert "Alpha" in output.text(), output.text()
            release_gate(status_url.removesuffix("/__trial/status"), 1)
            completed = wait_for_status(
                status_url,
                lambda value: value["request_outcomes"].get("1") == "completed",
                "Alpha to finish while the chooser is open",
            )
            assert completed["active_requests"] == [], completed
            read_until(master_fd, child, output, "No longer active")
            os.write(master_fd, b"\r")
            read_until(master_fd, child, output, "changed state; nothing was stopped")
            assert request_status(status_url)["request_outcomes"] == {
                "1": "completed",
                "2": "interrupted",
            }

            # The palette's Stop entry is available while Gamma is running.
            # Complete the turn through the real stream/event path before Enter;
            # the stale entry must then refuse to stop anything.
            os.write(master_fd, b"\x7f" * len(gamma_draft))
            palette_race_prompt = "Gamma run held for palette race"
            os.write(master_fd, palette_race_prompt.encode() + b"\x13")
            read_until(master_fd, child, output, "Request 3:")
            wait_for_status(
                status_url,
                lambda value: value["requests"] == 3
                and value["active_requests"] == [3]
                and value["pause_gates"].get("3") == "held",
                "Gamma's request to be active and held for the palette race",
            )
            os.write(master_fd, b"\x1bOQstop")
            read_until(master_fd, child, output, "Stop Gamma")
            release_gate(status_url.removesuffix("/__trial/status"), 3)
            completed = wait_for_status(
                status_url,
                lambda value: value["request_outcomes"].get("3") == "completed",
                "Gamma to finish while the palette is open",
            )
            assert completed["active_requests"] == [], completed
            read_until(master_fd, child, output, "status: Succeeded")
            os.write(master_fd, b"\r")
            read_until(master_fd, child, output, "No active TUI session to stop")
            assert "Stop background run" not in output.text(), output.text()
            assert request_status(status_url)["request_outcomes"] == {
                "1": "completed",
                "2": "interrupted",
                "3": "completed",
            }
            os.write(master_fd, b"\x1b")
            drain_for(master_fd, output, 0.1, child)
            os.write(master_fd, gamma_draft.encode())
            read_until(master_fd, child, output, gamma_draft)

            os.write(master_fd, b"\x03")
            status = drain_until_exit(master_fd, child, output)
            assert status == 0, f"TUI exit status was {status}"
            termios_fd = master_fd if sys.platform == "darwin" else slave_fd
            assert_terminal_restored(bytes(output.raw), termios_fd, initial_termios, child)
            catalog = json.loads((state_dir / "catalog.json").read_text(encoding="utf-8"))
            gamma_record = next(
                record
                for record in catalog["sessions"].values()
                if record.get("name") == "Gamma"
            )
            assert gamma_record.get("drafts", {}).get("tui") == gamma_draft, gamma_record
        finally:
            if child is not None and child.poll() is None:
                kill_owned_process_group(child)
            if master_fd is not None:
                os.close(master_fd)
            if slave_fd is not None:
                os.close(slave_fd)
            stop_fixture(fixture)


def run_windowed_history_smoke(args: argparse.Namespace) -> None:
    with tempfile.TemporaryDirectory(prefix="leg-tui-windowed-pty-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        state_dir = root / "state"
        session_id = seed_windowed_catalog(state_dir, workspace)
        env = os.environ.copy()
        env.update(
            {
                "TERM": "xterm-256color",
                "LEG_UI_STATE_DIR": str(state_dir),
                "LEG_UI_SUPERVISOR_BIN": str(Path(args.supervisor_bin).resolve()),
            }
        )
        command = [
            str(Path(args.tui_bin).resolve()),
            "--leg-bin",
            str(Path(args.leg_bin).resolve()),
            "--supervisor-bin",
            str(Path(args.supervisor_bin).resolve()),
        ]
        master_fd = slave_fd = None
        child = None
        output = TerminalCapture()
        try:
            master_fd, slave_fd, child, initial_termios = spawn_in_pty(command, env)
            read_until(master_fd, child, output, "Sessions · title · workspace · recent · status")
            picker = output.text()
            for item in ("History fixture", "Beta fixture", str(workspace.resolve()), "Ready"):
                assert item in picker, f"picker omitted {item!r}: {picker!r}"

            open_started = time.perf_counter()
            os.write(master_fd, b"/History fixture\r\r")
            read_until_fast(master_fd, child, output, "Rows ", timeout=0.2)
            open_elapsed = time.perf_counter() - open_started
            assert open_elapsed <= 0.2, f"opening known 1,000-turn session took {open_elapsed * 1000:.1f} ms"
            read_until(master_fd, child, output, "lookup · completed · HIDDEN_UNKNOWN_INPUT")
            statuses = (
                "bash · completed",
                "bash · interrupted",
                "bash · running",
                "bash · denied",
                "bash · failed",
            )
            for status in statuses:
                for _ in range(6):
                    screen = output.text()
                    if status in screen:
                        break
                    row_window = re.search(r"Rows (\d+-\d+ of \d+)", screen)
                    assert row_window, screen
                    previous_body = transcript_pane_text(screen)
                    os.write(master_fd, b"\x1b[5~")
                    read_until_rows_change(
                        master_fd, child, output, row_window.group(1), previous_body
                    )
                assert status in output.text(), (
                    f"history browsing did not reveal {status!r}: {output.text()!r}"
                )
            os.write(master_fd, b"\x1b[1;5F")
            read_until(master_fd, child, output, "lookup · completed · HIDDEN_UNKNOWN_INPUT")
            os.write(master_fd, b"\x1b[5~")
            read_until(master_fd, child, output, "bash · failed")
            os.write(master_fd, b"\x1b[1;5F")
            read_until(master_fd, child, output, "lookup · completed · HIDDEN_UNKNOWN_INPUT")
            row_window = re.search(r"Rows (\d+-\d+ of \d+)", output.text())
            assert row_window, output.text()

            inspector_started = time.perf_counter()
            os.write(master_fd, b"\x1bOS")
            inspector_ms = read_until_fast(
                master_fd, child, output, "Inspector · Up/Down field", timeout=0.2
            )
            assert inspector_ms <= 0.2, f"opening inspector took {inspector_ms * 1000:.1f} ms"
            assert time.perf_counter() - inspector_started <= 0.2, "inspector feedback exceeded 200 ms"

            os.write(master_fd, b"\x1b[B" * 6)
            read_until(master_fd, child, output, "Tool bash · stdout")
            read_until(master_fd, child, output, "literal Ω output from turn 0999")
            before_page = output.text()
            previous_window = re.search(r"Rows (\d+-\d+ of \d+)", before_page)
            assert previous_window, before_page
            previous_body = transcript_pane_text(before_page)
            os.write(master_fd, b"\x1b[5~")
            read_until_rows_change(
                master_fd, child, output, previous_window.group(1), previous_body
            )
            before_switch = re.search(r"Rows (\d+-\d+ of \d+)", output.text())
            assert before_switch, output.text()
            scroll_position = before_switch.group(1)
            assert "Alpha draft survives session changes" in output.text()

            select_picker_session(master_fd, child, output, "Beta fixture")
            read_until(master_fd, child, output, "Beta draft stays in its own session")
            select_picker_session(master_fd, child, output, "History fixture")
            read_until(master_fd, child, output, "Alpha draft survives session changes")
            after_switch = re.search(r"Rows (\d+-\d+ of \d+)", output.text())
            assert after_switch and after_switch.group(1) == scroll_position, (
                scroll_position,
                output.text(),
            )
            assert "Alpha draft survives session changes" in output.text()

            os.write(master_fd, b"\x06history fixture reply 099")
            read_until(master_fd, child, output, "Match 1 of 8")
            os.write(master_fd, b"\x1b[B")
            read_until(master_fd, child, output, "Match 2 of 8")
            os.write(master_fd, b"\x15")
            read_until(master_fd, child, output, "Search titles and complete sanitized transcript source.")
            os.write(master_fd, "literal Ω output from turn 0999".encode())
            read_until(master_fd, child, output, "Match 1 of 1")
            os.write(master_fd, b"\r")
            read_until(master_fd, child, output, "Inspector · Up/Down field")
            assert "Tool bash · id reused-tool-id · status" in output.text(), output.text()
            assert session_id in json.loads((state_dir / "catalog.json").read_text())["sessions"]

            status = drain_until_exit_after_close(
                master_fd, child, output, slave_fd, initial_termios
            )
            master_fd = slave_fd = None
            child = None
            assert status == 0
            print(
                f"1,000-turn TUI open/inspector latency: {open_elapsed * 1000:.1f}/"
                f"{inspector_ms * 1000:.1f} ms on {sys.platform} {os.uname().machine}"
            )
        finally:
            if child is not None:
                kill_owned_process_group(child)
            if master_fd is not None:
                os.close(master_fd)
            if slave_fd is not None:
                os.close(slave_fd)


def terminal_text(output: bytes) -> str:
    # Ratatui moves the cursor between adjacent words; turn those moves into
    # spaces before dropping the remaining ANSI controls.
    separated = re.sub(rb"\x1b\[[0-9;]*H", b" ", output)
    plain = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", separated)
    return plain.decode("utf-8", errors="replace")

def terminal_screen_text(
    output: bytes, rows: int = DEFAULT_ROWS, columns: int = DEFAULT_COLUMNS
) -> str:
    """Replay the cursor and erase controls used by ratatui into a small screen."""
    screen = [[" " for _ in range(columns)] for _ in range(rows)]
    row = 0
    column = 0
    index = 0
    while index < len(output):
        byte = output[index]
        if byte == 0x1B:
            if index + 1 < len(output) and output[index + 1] == ord("["):
                end = index + 2
                while end < len(output) and not 0x40 <= output[end] <= 0x7E:
                    end += 1
                if end == len(output):
                    break
                params = output[index + 2 : end].decode("ascii", errors="ignore")
                final = chr(output[end])
                values = [value for value in params.lstrip("?").split(";") if value]
                if final == "H":
                    row = max(0, int(values[0]) - 1) if values else 0
                    column = max(0, int(values[1]) - 1) if len(values) > 1 else 0
                elif final == "J" and values and values[0] in ("2", "3"):
                    screen = [[" " for _ in range(columns)] for _ in range(rows)]
                elif final == "K":
                    mode = values[0] if values else "0"
                    start, stop = (0, columns) if mode == "2" else (column, columns)
                    for cell in range(start, stop):
                        screen[row][cell] = " "
                index = end + 1
                continue
            index += min(2, len(output) - index)
            continue
        if byte == 0x0D:
            column = 0
            index += 1
            continue
        if byte == 0x0A:
            row += 1
            column = 0
            index += 1
            continue
        if byte < 0x20 or byte == 0x7F:
            index += 1
            continue
        width = 1
        for size in range(1, min(4, len(output) - index) + 1):
            try:
                character = output[index : index + size].decode("utf-8")
                break
            except UnicodeDecodeError:
                continue
        else:
            character = "�"
            size = 1
        if unicodedata.combining(character):
            width = 0
        elif unicodedata.east_asian_width(character) in ("W", "F"):
            width = 2
        if column >= columns:
            row += 1
            column = 0
        if row < rows and column < columns:
            screen[row][column] = character
            if width == 2 and column + 1 < columns:
                screen[row][column + 1] = ""
        column += width
        index += size
    return "\n".join("".join(line) for line in screen)


def normalized_workbench_capture(screen: str) -> str:
    lines = []
    for line in screen.splitlines():
        line = re.sub(r"(workspace: ).*?( · catalog: )", r"\1<workspace>\2", line)
        line = re.sub(r"(catalog: [^·]+ · )[^ ]+", r"\1<elapsed>", line)
        lines.append(line.rstrip())
    return "\n".join(lines).rstrip() + "\n"


def compare_workbench_capture(name: str, screen: str, update: bool) -> None:
    capture_path = Path(__file__).resolve().parent / "captures" / name
    actual = normalized_workbench_capture(screen)
    if update:
        capture_path.parent.mkdir(parents=True, exist_ok=True)
        capture_path.write_text(actual, encoding="utf-8")
        return
    expected = capture_path.read_text(encoding="utf-8")
    assert actual == expected, (
        f"{name} differs from the checked-in screen capture; "
        f"rerun with --update-workbench-captures to review a replacement"
    )


def screen_contains(screen: str, expected: str) -> bool:
    for border in "│─┌┐└┘├┤┬┴┼":
        screen = screen.replace(border, " ")
    return " ".join(expected.split()) in " ".join(screen.split())

def resize_pty(slave_fd: int, rows: int, columns: int) -> None:
    fcntl.ioctl(slave_fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))

def wait_for_file(
    path: Path,
    master_fd: int,
    output: bytearray,
    child: subprocess.Popen[bytes],
    timeout: float = 8.0,
) -> None:
    deadline = time.monotonic() + timeout
    while not path.is_file():
        remaining = deadline - time.monotonic()
        if child.poll() is not None:
            drain_for(master_fd, output, 0.05)
            raise AssertionError(
                f"TUI exited before creating {path}; exit={child.returncode}; "
                f"output={terminal_text(bytes(output))[-1200:]!r}"
            )
        if remaining <= 0:
            raise AssertionError(
                f"timed out waiting for file {path}; "
                f"output={terminal_text(bytes(output))[-1200:]!r}"
            )
        drain_for(master_fd, output, min(0.05, remaining))

def kill_pty_child(master_fd: int, child: subprocess.Popen[bytes], output: bytearray) -> None:
    if child.poll() is not None:
        return
    child.kill()
    deadline = time.monotonic() + 10.0
    while child.poll() is None:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        drain_for(master_fd, output, min(0.05, remaining))
    if child.poll() is None:
        try:
            child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            # Preserve the test failure that led to cleanup.
            drain_for(master_fd, output, 0.05)
            return
    drain_for(master_fd, output, 0.05)

def wait_for_pid_exit(pid: int, timeout: float = 6.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return
        status = subprocess.run(
            ["ps", "-o", "stat=", "-p", str(pid)],
            check=False,
            capture_output=True,
            text=True,
            timeout=2,
        ).stdout.strip()
        if not status or status.startswith("Z"):
            return
        time.sleep(0.05)
    raise AssertionError(f"owned tool process {pid} is still running")

def has_color_styling(output: bytes) -> bool:
    color_codes = set(range(30, 38)) | set(range(40, 48)) | set(range(90, 98)) | set(range(100, 108))
    color_codes.update((38, 48, 58))
    for params in re.findall(rb"\x1b\[([0-9;]*)m", output):
        codes = [int(value) for value in params.split(b";") if value]
        if any(code in color_codes for code in codes):
            return True
    return False

def run_tool_summary_smoke(args: argparse.Namespace) -> None:
    with tempfile.TemporaryDirectory(prefix="leg-tui-tool-summary-pty-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        state_dir = root / "state"
        seed_windowed_catalog(
            state_dir,
            workspace,
            turn_count=1,
            session_title="Tool summary fixture",
        )
        env = os.environ.copy()
        env.update(
            {
                "TERM": "xterm-256color",
                "LEG_UI_STATE_DIR": str(state_dir),
                "LEG_UI_SUPERVISOR_BIN": str(Path(args.supervisor_bin).resolve()),
            }
        )
        command = [
            str(Path(args.tui_bin).resolve()),
            "--leg-bin",
            str(Path(args.leg_bin).resolve()),
            "--supervisor-bin",
            str(Path(args.supervisor_bin).resolve()),
        ]
        master_fd = slave_fd = None
        child = None
        output = TerminalCapture()
        try:
            master_fd, slave_fd, child, initial_termios = spawn_in_pty(command, env)
            read_until(master_fd, child, output, "Sessions · title · workspace · recent · status")
            os.write(master_fd, b"/Tool summary fixture\r\r")
            read_until(master_fd, child, output, "lookup · completed · HIDDEN_UNKNOWN_INPUT")
            os.write(master_fd, b"q")
            read_until(master_fd, child, output, "q")
            screen_120 = terminal_screen_text(bytes(output.raw), 40, 120)
            footer_120 = screen_120.rstrip().splitlines()[-1]
            assert "F2/Ctrl-P actions" in footer_120, footer_120
            assert "Ctrl-S send" in footer_120, footer_120
            assert "F8 rail" in footer_120, footer_120
            os.write(master_fd, b"\x1bOQ")
            read_until(master_fd, child, output, "Command palette")
            drain_for(master_fd, output, 0.1, child)
            os.write(master_fd, b"\x1b[B")
            drain_for(master_fd, output, 0.1, child)
            selected_120 = terminal_screen_text(bytes(output.raw), 40, 120)
            assert "> New conversation" in selected_120, selected_120
            os.write(master_fd, b"\x1b[A")
            drain_for(master_fd, output, 0.1, child)
            palette_120 = terminal_screen_text(bytes(output.raw), 40, 120)
            assert "Browse/switch sessions" in palette_120, palette_120
            assert "Choose/replace workspace" in palette_120, palette_120
            assert "Retry latest failed turn" in palette_120, palette_120
            assert "stop" in palette_120.lower(), palette_120
            compare_workbench_capture(
                "command-palette-120x40.txt",
                palette_120,
                getattr(args, "update_workbench_captures", False),
            )
            os.write(master_fd, b"retry")
            read_until(master_fd, child, output, "No eligible failed or incomplete latest turn")
            os.write(master_fd, b"\x15")
            drain_for(master_fd, output, 0.1, child)
            reset_palette = terminal_screen_text(bytes(output.raw), 40, 120)
            assert "Filter: ▏" in reset_palette, reset_palette
            assert "> Browse/switch sessions" in reset_palette, reset_palette
            os.write(master_fd, b"\x1b")
            drain_for(master_fd, output, 0.1, child)
            os.write(master_fd, b"\x10")
            read_until(master_fd, child, output, "Command palette")
            assert "Browse/switch sessions" in output.text(), output.text()
            os.write(master_fd, b"\x1b")
            drain_for(master_fd, output, 0.1, child)
            os.write(master_fd, b"\x7f")
            drain_for(master_fd, output, 0.1, child)
            os.write(master_fd, b"\x1bOS")
            read_until(master_fd, child, output, "Inspector · Up/Down field")
            os.write(master_fd, b"\x1bOS")
            read_until_not_contains(master_fd, child, output, "Inspector · Up/Down field")

            resize_pty(slave_fd, 24, 80)
            drain_for(master_fd, output, 0.2, child)
            screen_80 = terminal_screen_text(bytes(output.raw), 24, 80)
            footer_80 = screen_80.rstrip().splitlines()[-1]
            assert "F2/Ctrl-P actions" in footer_80, footer_80
            assert "Ctrl-S send" in footer_80, footer_80
            assert "F8 rail" not in footer_80, footer_80
            for summary in (
                "bash · exit 7 · check nonzero status",
                "read · completed · history.txt · offset 2 · limit 3",
                "edit · completed · edit.txt",
                "lookup · completed · HIDDEN_UNKNOWN_INPUT",
            ):
                assert any(summary in line for line in screen_80.splitlines()), (
                    f"80-column tool row wrapped or disappeared: {summary!r}: {screen_80!r}"
                )
            compare_workbench_capture(
                "tool-summary-80x24.txt",
                screen_80,
                getattr(args, "update_workbench_captures", False),
            )
            os.write(master_fd, b"\x1bOQ")
            read_until(master_fd, child, output, "Command palette")
            drain_for(master_fd, output, 0.1, child)
            palette_80 = terminal_screen_text(bytes(output.raw), 24, 80)
            assert "Browse/switch sessions" in palette_80, palette_80
            assert "Show/hide session rail" in palette_80, palette_80
            assert "disabled" in palette_80, palette_80
            compare_workbench_capture(
                "command-palette-80x24.txt",
                palette_80,
                getattr(args, "update_workbench_captures", False),
            )
            os.write(master_fd, b"rAiL")
            read_until(master_fd, child, output, "Available at 105 columns and wider")
            os.write(master_fd, b"\x1b")
            drain_for(master_fd, output, 0.1, child)
            os.write(master_fd, b"\x10")
            read_until(master_fd, child, output, "Command palette")
            assert "Browse/switch sessions" in output.text(), output.text()
            os.write(master_fd, b"\x1b")
            drain_for(master_fd, output, 0.1, child)
            resize_pty(slave_fd, 40, 120)
            drain_for(master_fd, output, 0.2, child)
            screen_120 = terminal_screen_text(bytes(output.raw), 40, 120)
            for summary in (
                "bash · exit 7 · check nonzero status",
                "bash · completed · malformed envelope",
                "bash · completed · unknown envelope",
                "read · completed · history.txt · offset 2 · limit 3",
                "edit · completed · edit.txt",
                "bash · denied · blocked command",
                "write · missing result · missing-result.txt",
                "lookup · completed · HIDDEN_UNKNOWN_INPUT",
            ):
                assert summary in screen_120, f"tool summary omitted {summary!r}: {screen_120!r}"
            assert "bash-exit-7" not in screen_120, screen_120
            compare_workbench_capture(
                "tool-summary-120x40.txt",
                screen_120,
                getattr(args, "update_workbench_captures", False),
            )

            os.write(master_fd, b"\x1b[1;5A")
            read_until(master_fd, child, output, "› lookup · completed · HIDDEN_UNKNOWN_INPUT")
            os.write(master_fd, b"\x1bOS")
            read_until(master_fd, child, output, "Tool lookup · id unknown-tool · status")
            os.write(master_fd, b"\x1b")
            read_until(master_fd, child, output, "› lookup · completed · HIDDEN_UNKNOWN_INPUT")
            os.write(master_fd, b"\x1b[1;5A")
            read_until(master_fd, child, output, "› write · missing result · missing-result.txt")
            os.write(master_fd, b"\x1bOS")
            read_until(master_fd, child, output, "Tool write · id missing-result · status")
            assert "missing result" in output.text(), output.text()
            os.write(master_fd, b"\x1b")
            read_until(master_fd, child, output, "› write · missing result · missing-result.txt")
            os.write(master_fd, b"\x1b[1;5B")
            read_until(master_fd, child, output, "› lookup · completed · HIDDEN_UNKNOWN_INPUT")
            assert "Alpha draft survives session changes" in output.text(), output.text()

            os.write(master_fd, b"\x06" + "HIDDEN_BASH_STDOUT_Ω\r".encode())
            read_until(master_fd, child, output, "Tool bash · id bash-exit-7 · status")
            os.write(master_fd, b"\x1b[B\x1b[B")
            read_until(master_fd, child, output, "Tool bash · stdout")
            assert "HIDDEN_BASH_STDOUT_Ω" in output.text(), output.text()
            before_copy = bytes(output.raw)
            os.write(master_fd, b"\x1b[15~")
            drain_for(master_fd, output, 0.1, child)
            assert b"\x1b]52;c;" in bytes(output.raw[len(before_copy) :]), (
                "F5 did not copy the readable bash stdout field"
            )
            os.write(master_fd, b"\x1b[18~")
            read_until(master_fd, child, output, "Save copied text to:")
            os.write(master_fd, b"\r")
            readable_copy = workspace / "leg-copy.txt"
            read_until(master_fd, child, output, "Saved ")
            assert readable_copy.read_text(encoding="utf-8") == "HIDDEN_BASH_STDOUT_Ω"

            os.write(master_fd, b"\x1b[B" * 6)
            read_until(master_fd, child, output, "Tool bash · result (literal)")
            assert '"exit_code": 7' in output.text(), output.text()
            os.write(master_fd, b"\x1b[15~")
            read_until(master_fd, child, output, "Clipboard request sent")
            os.write(master_fd, b"\x1b[18~")
            read_until(master_fd, child, output, "Save copied text to:")
            os.write(master_fd, b"\x15leg-result.txt\r")
            raw_copy = workspace / "leg-result.txt"
            read_until(master_fd, child, output, "Saved ")
            raw_value = raw_copy.read_text(encoding="utf-8")
            assert '"exit_code": 7' in raw_value and "HIDDEN_BASH_STDOUT_Ω" in raw_value

            os.write(master_fd, b"\x06HIDDEN_UNKNOWN_INPUT\r")
            read_until(master_fd, child, output, "Tool lookup · id unknown-tool · status")
            os.write(master_fd, b"\x1b[B")
            read_until(master_fd, child, output, "Tool lookup · input")
            assert "HIDDEN_UNKNOWN_INPUT" in output.text(), output.text()

            os.write(master_fd, b"\x06READ_OUTPUT_SENTINEL\r")
            read_until(master_fd, child, output, "Tool read · id read-output · status")
            os.write(master_fd, b"\x1b[B\x1b[B")
            read_until(master_fd, child, output, "Tool read · result")
            assert "READ_OUTPUT_SENTINEL" in output.text(), output.text()
            assert "stdout" not in output.text(), output.text()

            os.write(master_fd, b"\x06malformed envelope\r")
            read_until(master_fd, child, output, "Tool bash · id bash-malformed · status")
            os.write(master_fd, b"\x1b[B\x1b[B")
            read_until(master_fd, child, output, "Tool bash · result")
            assert '{"status":"exited","exit_code":0,"stdout":' in output.text()
            assert "Tool bash · stdout" not in output.text()

            os.write(master_fd, b"\x06unknown envelope\r")
            read_until(master_fd, child, output, "Tool bash · id bash-unknown-envelope · status")
            os.write(master_fd, b"\x1b[B\x1b[B")
            read_until(master_fd, child, output, "Tool bash · result")
            assert '"status": "future"' in output.text(), output.text()
            assert "Tool bash · stdout" not in output.text()

            os.write(master_fd, b"\x06truncated output\r")
            read_until(master_fd, child, output, "Tool bash · id bash-truncated · status")
            os.write(master_fd, b"\x1b[B\x1b[B")
            read_until(master_fd, child, output, "Tool bash · stdout")
            assert "120 bytes omitted" in output.text(), output.text()
            os.write(master_fd, b"\x1b[B" * 4)
            read_until(master_fd, child, output, "Tool bash · stdout_omitted_bytes")
            assert "120" in output.text(), output.text()

            os.write(master_fd, b"\x06hook denied\r")
            read_until(master_fd, child, output, "Tool bash · id denied-call · status")
            assert "denied" in output.text(), output.text()

            os.write(master_fd, b"\x06edit.txt\r")
            read_until(master_fd, child, output, "Tool edit · id edit-diff · status")
            os.write(master_fd, b"\x1b[B\x1b[B")
            read_until(master_fd, child, output, "Tool edit · diff")
            assert "... [diff truncated: 2 more lines]" in output.text(), output.text()

            os.write(master_fd, b"\x03")
            status = drain_until_exit(master_fd, child, output)
            assert status == 0, f"TUI exit status was {status}"
            assert_terminal_restored(bytes(output.raw), master_fd, initial_termios, child)
            master_fd = slave_fd = None
            child = None
        finally:
            if child is not None:
                kill_owned_process_group(child)
            if master_fd is not None:
                os.close(master_fd)
            if slave_fd is not None:
                os.close(slave_fd)


def run_resize_and_non_tty_smoke(args: argparse.Namespace) -> None:
    repository = Path(__file__).resolve().parents[2]
    fixture_path = repository / "trials" / "fake_provider.py"
    with tempfile.TemporaryDirectory(prefix="leg-tui-terminal-pty-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        state_dir = root / "state"
        event_log = root / "leg-events.jsonl"
        fixture, status_url = start_fixture(fixture_path, "trial", workspace)
        master_fd = slave_fd = None
        child = None
        output = bytearray()
        try:
            env = os.environ.copy()
            env.update(
                {
                    "LEG_PROVIDER": "anthropic",
                    "ANTHROPIC_BASE_URL": status_url.removesuffix("/__trial/status"),
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
            read_until_screen_text(
                master_fd, child, output, "Path:", rows=DEFAULT_ROWS, columns=DEFAULT_COLUMNS
            )
            assert "Workspace" in terminal_screen_text(bytes(output), 40, 120)

            resize_pty(slave_fd, 24, 80)
            drain_for(master_fd, output, 0.2)
            workspace_screen = terminal_screen_text(bytes(output), 24, 80)
            assert "Workspace" in workspace_screen and "Path:" in workspace_screen, (
                f"workspace selection was not usable at 80x24: {workspace_screen!r}"
            )

            os.write(master_fd, str(workspace).encode() + b"\r")
            read_until_screen_text(
                master_fd,
                child,
                output,
                "Leg can run shell commands",
                rows=24,
                columns=80,
            )
            drain_for(master_fd, output, 0.2)
            warning_screen = terminal_screen_text(bytes(output), 24, 80)
            assert screen_contains(warning_screen, WARNING), (
                f"first-run warning clipped at 80x24: {warning_screen!r}"
            )
            assert "Press Enter to acknowledge" in warning_screen, warning_screen

            resize_pty(slave_fd, 40, 120)
            drain_for(master_fd, output, 0.2)
            warning_screen = terminal_screen_text(bytes(output), 40, 120)
            assert screen_contains(warning_screen, WARNING) and "Press Enter to acknowledge" in warning_screen, (
                f"first-run warning was not usable at 120x40: {warning_screen!r}"
            )
            os.write(master_fd, b"\r")
            read_until_screen_text(
                master_fd, child, output, "Idle", rows=40, columns=120
            )

            resize_pty(slave_fd, 24, 80)
            drain_for(master_fd, output, 0.2)
            conversation_screen = terminal_screen_text(bytes(output), 24, 80)
            for hint in (
                "Composer",
                "status: Idle",
                "F2/Ctrl-P actions",
                "Ctrl-C exit",
                "F1 help",
            ):
                assert hint in conversation_screen, (
                    f"conversation omitted {hint!r} at 80x24: {conversation_screen!r}"
                )
            footer_line = conversation_screen.rstrip().splitlines()[-1]
            assert "Ctrl-S send" not in footer_line, footer_line
            assert "Sessions" not in conversation_screen, (
                f"session rail should be hidden at 80x24: {conversation_screen!r}"
            )
            os.write(master_fd, b"\x1bOR")
            read_until_screen_text(
                master_fd,
                child,
                output,
                "Sessions · title · workspace · recent · status",
                rows=24,
                columns=80,
            )
            os.write(master_fd, b"\x1b")
            read_until_screen_text(
                master_fd, child, output, "status: Idle", rows=24, columns=80
            )

            layout_draft = "one\r\ntwo\r\nthree\r\nfour"
            os.write(
                master_fd,
                b"\x1b[200~" + layout_draft.encode() + b"\x1b[201~",
            )
            read_until_screen_text(
                master_fd, child, output, "four", rows=24, columns=80
            )
            layout_screen = terminal_screen_text(bytes(output), 24, 80)
            layout_lines = layout_screen.splitlines()
            conversation_row = next(
                index for index, line in enumerate(layout_lines) if "Conversation" in line
            )
            composer_row = next(
                index for index, line in enumerate(layout_lines) if "Composer" in line
            )
            assert composer_row - conversation_row >= 12, (
                f"four-row draft left too little conversation at 80x24: {layout_screen!r}"
            )
            for offset, line in enumerate(("one", "two", "three", "four"), start=1):
                assert line in layout_lines[composer_row + offset], (
                    f"composer did not show its four content rows: {layout_screen!r}"
                )
            os.write(master_fd, b"\x1a")
            drain_for(master_fd, output, 0.1)
            shrink_screen = terminal_screen_text(bytes(output), 24, 80)
            shrink_lines = shrink_screen.splitlines()
            shrink_conversation_row = next(
                index for index, line in enumerate(shrink_lines) if "Conversation" in line
            )
            shrink_composer_row = next(
                index for index, line in enumerate(shrink_lines) if "Composer" in line
            )
            assert shrink_composer_row - shrink_conversation_row > composer_row - conversation_row, (
                f"composer did not shrink with its draft: {shrink_screen!r}"
            )

            prompt = "terminal size recovery draft"
            os.write(master_fd, prompt.encode())
            drain_for(master_fd, output, 0.1)
            resize_pty(slave_fd, 23, 79)
            read_until_screen_text(
                master_fd,
                child,
                output,
                "Terminal too small",
                rows=23,
                columns=79,
            )
            small_screen = terminal_screen_text(bytes(output), 23, 79)
            assert "Minimum supported size: 80 columns x 24 rows" in small_screen, small_screen
            os.write(master_fd, b"\x13")
            read_until_screen_text(
                master_fd,
                child,
                output,
                "Send rejected",
                rows=23,
                columns=79,
            )
            assert request_status(status_url)["requests"] == 0, (
                "a below-minimum send attempt invoked the provider"
            )

            resize_pty(slave_fd, 24, 80)
            drain_for(master_fd, output, 0.2)
            restored_screen = terminal_screen_text(bytes(output), 24, 80)
            assert prompt in restored_screen, f"draft was lost after resize recovery: {restored_screen!r}"
            assert "Composer" in restored_screen
            assert request_status(status_url)["requests"] == 0
            os.write(master_fd, b"\x13")
            read_until_screen_text(
                master_fd, child, output, "Succeeded", rows=24, columns=80
            )
            assert request_status(status_url)["requests"] == 1
            final_80_screen = terminal_screen_text(bytes(output), 24, 80)
            assert "You: terminal size recovery draft" in final_80_screen, final_80_screen
            assert "Assistant" in final_80_screen, final_80_screen
            assert "Outcome: succeeded" in final_80_screen, final_80_screen
            compare_workbench_capture(
                "workbench-80x24.txt",
                final_80_screen,
                getattr(args, "update_workbench_captures", False),
            )
            requests = [
                json.loads(line)
                for line in event_log.read_text(encoding="utf-8").splitlines()
                if json.loads(line).get("event") == "request"
            ]
            assert len(requests) == 1 and requests[0]["prompt"] == prompt, requests

            resize_pty(slave_fd, 40, 120)
            drain_for(master_fd, output, 0.2)
            final_screen = terminal_screen_text(bytes(output), 40, 120)
            assert "Succeeded" in final_screen and "Rows " in final_screen, final_screen
            compare_workbench_capture(
                "workbench-120x40.txt",
                final_screen,
                getattr(args, "update_workbench_captures", False),
            )
            resize_pty(slave_fd, 40, 100)
            drain_for(master_fd, output, 0.1)
            medium_screen = terminal_screen_text(bytes(output), 40, 100)
            assert "Sessions" not in medium_screen, (
                f"rail should stay hidden below its width threshold: {medium_screen!r}"
            )
            resize_pty(slave_fd, 40, 105)
            drain_for(master_fd, output, 0.1)
            threshold_screen = terminal_screen_text(bytes(output), 40, 105)
            assert "Sessions" in threshold_screen, (
                f"rail should appear when the conversation retains 80 columns: {threshold_screen!r}"
            )
            resize_pty(slave_fd, 40, 120)
            drain_for(master_fd, output, 0.1)
            os.write(master_fd, b"\x1bOS")
            read_until_screen_text(
                master_fd,
                child,
                output,
                "Inspector · Up/Down field · F5 copy",
                rows=40,
                columns=120,
            )
            inspector_screen = terminal_screen_text(bytes(output), 40, 120)
            inspector_title = next(
                line for line in inspector_screen.splitlines() if "Inspector · Up/Down field" in line
            )
            assert 0 < inspector_title.find("Inspector") < 25, (
                f"120x40 rail should force the inspector overlay: {inspector_screen!r}"
            )
            os.write(master_fd, b"\x1b[19~")
            drain_for(master_fd, output, 0.1)
            inspector_screen = terminal_screen_text(bytes(output), 40, 120)
            inspector_title = next(
                line for line in inspector_screen.splitlines() if "Inspector · Up/Down field" in line
            )
            assert "Sessions" not in inspector_screen, inspector_screen
            assert inspector_title.find("Inspector") >= 70, (
                f"hiding the rail should allow the inspector to dock at 120x40: {inspector_screen!r}"
            )
            os.write(master_fd, b"\x1b[19~")
            drain_for(master_fd, output, 0.1)
            inspector_screen = terminal_screen_text(bytes(output), 40, 120)
            assert "Sessions" in inspector_screen, inspector_screen
            os.write(master_fd, b"\x1bOS")
            drain_for(master_fd, output, 0.1)
            os.write(master_fd, b"\x03")
            status = drain_until_exit(master_fd, child, output)
            assert status == 0, f"TUI exit status was {status}"
            assert_terminal_restored(bytes(output), master_fd, initial_termios, child)
            master_fd = slave_fd = None
            child = None

            # Each redirected stream is checked independently while the other
            # remains attached to a PTY, proving validation precedes raw mode.
            help_result = subprocess.run(
                command + ["--help"],
                input=b"",
                capture_output=True,
                env=env,
                timeout=5,
                check=False,
            )
            assert help_result.returncode == 0
            assert b"Usage: leg-tui" in help_result.stdout
            assert b"requires a terminal" not in help_result.stderr

            for redirected in ("stdin", "stdout"):
                tty_master, tty_slave = pty.openpty()
                original = termios.tcgetattr(tty_slave)
                if redirected == "stdin":
                    no_tty_child = subprocess.Popen(
                        command,
                        stdin=subprocess.PIPE,
                        stdout=tty_slave,
                        stderr=tty_slave,
                        env=env,
                        close_fds=True,
                    )
                    assert no_tty_child.stdin is not None
                    no_tty_child.stdin.close()
                    result_stdout = b""
                    status = no_tty_child.wait(timeout=5)
                else:
                    no_tty_child = subprocess.Popen(
                        command,
                        stdin=tty_slave,
                        stdout=subprocess.PIPE,
                        stderr=tty_slave,
                        env=env,
                        close_fds=True,
                    )
                    result_stdout, _ = no_tty_child.communicate(timeout=5)
                    status = no_tty_child.returncode
                no_tty_output = bytearray()
                drain_for(tty_master, no_tty_output, 0.15)
                combined = result_stdout + bytes(no_tty_output)
                assert status != 0, f"TUI accepted redirected {redirected}"
                assert b"requires a terminal" in combined.lower(), combined
                for sequence in (b"\x1b[?1049h", b"\x1b[?2004h"):
                    assert sequence not in combined, (
                        f"TUI entered terminal mode with redirected {redirected}: {combined!r}"
                    )
                assert termios.tcgetattr(tty_slave) == original
                os.close(tty_master)
                os.close(tty_slave)
            assert request_status(status_url)["requests"] == 1
        finally:
            if child is not None and child.poll() is None:
                child.kill()
                child.wait(timeout=2)
            if master_fd is not None:
                os.close(master_fd)
            if slave_fd is not None:
                os.close(slave_fd)
            stop_fixture(fixture)

def run_signal_smoke(args: argparse.Namespace) -> None:
    repository = Path(__file__).resolve().parents[2]
    fixture_path = repository / "trials" / "fake_provider.py"
    command = [
        str(Path(args.tui_bin).resolve()),
        "--leg-bin",
        str(Path(args.leg_bin).resolve()),
        "--supervisor-bin",
        str(Path(args.supervisor_bin).resolve()),
    ]
    for signal_number in (signal.SIGINT, signal.SIGTERM):
        signal_name = signal.Signals(signal_number).name
        for active in (False, True):
            prefix = "active" if active else "idle"
            with tempfile.TemporaryDirectory(
                prefix=f"leg-tui-{prefix}-{signal_name.lower()}-"
            ) as temporary:
                root = Path(temporary)
                workspace = root / "workspace"
                workspace.mkdir()
                state_dir = root / "state"
                fixture, status_url = start_fixture(
                    fixture_path, "stalled-bash", workspace
                )
                master_fd = slave_fd = None
                child = None
                output = bytearray()
                try:
                    env = os.environ.copy()
                    env.update(
                        {
                            "LEG_PROVIDER": "anthropic",
                            "ANTHROPIC_BASE_URL": status_url.removesuffix(
                                "/__trial/status"
                            ),
                            "ANTHROPIC_API_KEY": "trial-only-not-a-secret",
                            "LEG_MODEL": "trial-fixture",
                            "LEG_MAX_RETRIES": "0",
                            "LEG_UI_STATE_DIR": str(state_dir),
                            "LEG_UI_SUPERVISOR_BIN": str(
                                Path(args.supervisor_bin).resolve()
                            ),
                        }
                    )
                    master_fd, slave_fd, child, initial_termios, output = launch_conversation(
                        args, env, workspace
                    )
                    if active:
                        os.write(master_fd, b"TRIAL-STOP\x13")
                        pid_file = workspace / "trial-stalled-child.pid"
                        wait_for_file(pid_file, master_fd, output, child)
                        next_draft = f"draft survives {signal_name}"
                        os.write(master_fd, next_draft.encode())
                    else:
                        next_draft = f"idle draft survives {signal_name}"
                        os.write(master_fd, next_draft.encode())

                    read_until_screen_text(
                        master_fd,
                        child,
                        output,
                        next_draft,
                        rows=DEFAULT_ROWS,
                        columns=DEFAULT_COLUMNS,
                    )

                    child.send_signal(signal_number)
                    status = drain_until_exit(master_fd, child, output)
                    assert status == 0, (
                        f"{signal_name} {prefix} shutdown returned {status}: "
                        f"{terminal_text(bytes(output))[-1200:]!r}"
                    )
                    assert_terminal_restored(
                        bytes(output), master_fd, initial_termios, child
                    )

                    if active:
                        pid = int(pid_file.read_text(encoding="utf-8").strip())
                        wait_for_pid_exit(pid)
                        assert not (workspace / "trial-stall-finished.txt").exists(), (
                            f"the stalled tool continued after {signal_name} shutdown"
                        )
                    catalog = json.loads(
                        (state_dir / "catalog.json").read_text(encoding="utf-8")
                    )
                    assert len(catalog["sessions"]) == 1, catalog
                    saved = next(iter(catalog["sessions"].values()))["drafts"]["tui"]
                    assert saved == next_draft, (
                        f"{signal_name} {prefix} shutdown lost the draft: {saved!r}"
                    )
                    expected_requests = 1 if active else 0
                    assert request_status(status_url)["requests"] == expected_requests
                finally:
                    if child is not None and child.poll() is None:
                        kill_pty_child(master_fd, child, output)
                    if master_fd is not None:
                        os.close(master_fd)
                    if slave_fd is not None:
                        os.close(slave_fd)
                    stop_fixture(fixture)

def run_color_policy_smoke(args: argparse.Namespace) -> None:
    command = [
        str(Path(args.tui_bin).resolve()),
        "--leg-bin",
        str(Path(args.leg_bin).resolve()),
        "--supervisor-bin",
        str(Path(args.supervisor_bin).resolve()),
    ]
    with tempfile.TemporaryDirectory(prefix="leg-tui-color-pty-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        for label, term, no_color in (
            ("NO_COLOR", "xterm-256color", "1"),
            ("TERM=dumb", "dumb", None),
        ):
            state_dir = root / label.replace("=", "-")
            env = os.environ.copy()
            env.update({"TERM": term, "LEG_UI_STATE_DIR": str(state_dir)})
            if no_color is None:
                env.pop("NO_COLOR", None)
            else:
                env["NO_COLOR"] = no_color
            master_fd, slave_fd, child, initial_termios = spawn_in_pty(command, env)
            output = bytearray()
            try:
                read_until_screen_text(
                    master_fd, child, output, "Path:", rows=DEFAULT_ROWS, columns=DEFAULT_COLUMNS
                )
                os.write(master_fd, str(workspace).encode() + b"\r")
                read_until_screen_text(
                    master_fd,
                    child,
                    output,
                    "Leg can run shell commands",
                    rows=DEFAULT_ROWS,
                    columns=DEFAULT_COLUMNS,
                )
                drain_for(master_fd, output, 0.15)
                warning_screen = terminal_screen_text(
                    bytes(output), DEFAULT_ROWS, DEFAULT_COLUMNS
                )
                assert screen_contains(warning_screen, WARNING), warning_screen
                os.write(master_fd, b"\r")
                read_until_screen_text(
                    master_fd, child, output, "Idle", rows=DEFAULT_ROWS, columns=DEFAULT_COLUMNS
                )
                drain_for(master_fd, output, 0.15)
                screen = terminal_screen_text(
                    bytes(output), DEFAULT_ROWS, DEFAULT_COLUMNS
                )
                assert "Idle" in screen
                assert not has_color_styling(bytes(output)), (
                    f"{label} still emitted terminal color styles"
                )
                os.write(master_fd, b"\x03")
                status = drain_until_exit(master_fd, child, output)
                assert status == 0
                assert_terminal_restored(bytes(output), master_fd, initial_termios, child)
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait(timeout=2)
                os.close(master_fd)
                os.close(slave_fd)


def main() -> None:
    if sys.platform not in ("linux", "darwin"):
        raise SystemExit("pty_smoke.py requires native Linux or macOS")
    parser = argparse.ArgumentParser()
    parser.add_argument("--tui-bin", required=True)
    parser.add_argument("--leg-bin", required=True)
    parser.add_argument("--supervisor-bin", required=True)
    parser.add_argument("--web-bin")
    parser.add_argument("--fixture")
    parser.add_argument("--launcher")
    parser.add_argument(
        "--trial-coverage-only",
        action="store_true",
        help="run the bundle-safe issue #82 workflow against the selected fixture",
    )
    parser.add_argument(
        "--workbench-only",
        action="store_true",
        help="run the adaptive layout and background Stop PTY checks",
    )
    parser.add_argument(
        "--tool-summary-only",
        action="store_true",
        help="run focused tool summary/detail PTY checks and captures",
    )
    parser.add_argument(
        "--update-workbench-captures",
        action="store_true",
        help="rewrite the checked-in 80x24 and 120x40 screen captures",
    )
    args = parser.parse_args()
    if args.tool_summary_only:
        run_tool_summary_smoke(args)
        print("leg-tui compact tool summary PTY checks passed")
        return
    if args.workbench_only:
        test_terminal_screen_redraw()
        if sys.platform == "linux":
            run_smoke(args)
        run_tool_summary_smoke(args)
        run_resize_and_non_tty_smoke(args)
        run_session_navigation_smoke(args)
        if args.web_bin:
            run_background_session_busy_smoke(args)
        run_background_stop_chooser_smoke(args)
        print("leg-tui adaptive workbench and background Stop PTY checks passed")
        return
    if args.trial_coverage_only:
        runtime_path = os.environ.get("PATH", "")
        for command in ("cargo", "node"):
            if shutil.which(command, path=runtime_path) is not None:
                raise SystemExit(f"bundle smoke runtime PATH unexpectedly includes {command}")
        run_issue82_trial_coverage_smoke(args)
        print("unpacked TUI bundle smoke passed: fixture prompt, tool flow, and error handling")
        return
    if not args.web_bin:
        parser.error("--web-bin is required for the full native PTY suite")
    test_terminal_screen_redraw()
    if sys.platform == "linux":
        run_smoke(args)
    run_turn_contract_smoke(args)
    run_issue82_trial_coverage_smoke(args)
    run_retry_confirmation_smoke(args)
    run_session_navigation_smoke(args)
    run_workspace_flow_smoke(args)
    run_background_session_busy_smoke(args)
    run_background_stop_chooser_smoke(args)
    run_windowed_history_smoke(args)
    run_tool_summary_smoke(args)
    run_resize_and_non_tty_smoke(args)
    run_signal_smoke(args)
    run_color_policy_smoke(args)
    print("leg-tui native Linux/macOS PTY smoke passed")



if __name__ == "__main__":
    main()
