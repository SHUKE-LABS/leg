#!/usr/bin/env python3
"""Native Linux/macOS PTY smoke test for leg-tui terminal behavior."""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unicodedata
import urllib.request
from pathlib import Path
from typing import Any


WARNING = (
    "Leg can run shell commands and modify files as your OS user. "
    "The workspace is its working directory, not a sandbox."
)
DEFAULT_ROWS = 40
DEFAULT_COLUMNS = 120
PASTED_TEXT = "first line: 中文\r\nsecond: 👩‍👩‍👧‍👦 e\u0301\rthird line\x13\x03\x1b\x7f\nfourth line?"
PROMPT = "?typed line\nfirst line: 中文\nsecond: 👩‍👩‍👧‍👦 e\u0301\nthird line\nfourth line?"
NEXT_DRAFT = "next draft"
LIVE_TEXT = "The first live text is visible."


def read_until_screen_text(
    master_fd: int,
    child: subprocess.Popen[bytes],
    output: bytearray,
    needle: str,
    timeout: float = 8.0,
    rows: int = DEFAULT_ROWS,
    columns: int = DEFAULT_COLUMNS,
) -> None:
    deadline = time.monotonic() + timeout
    while needle not in terminal_screen_text(bytes(output), rows, columns):
        if child.poll() is not None:
            raise AssertionError(
                f"TUI exited before screen text {needle!r}; exit={child.returncode}; "
                f"screen text={terminal_text(bytes(output))[-1200:]!r}"
            )
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(
                f"timed out waiting for screen text {needle!r}; "
                f"screen text={terminal_text(bytes(output))[-1200:]!r}"
            )
        ready, _, _ = select.select([master_fd], [], [], min(0.1, remaining))
        if not ready:
            continue
        try:
            chunk = os.read(master_fd, 8192)
        except OSError:
            continue
        if chunk:
            output.extend(chunk)


def drain_until_exit(
    master_fd: int, child: subprocess.Popen[bytes], output: bytearray, timeout: float = 8.0
) -> int:
    deadline = time.monotonic() + timeout
    while child.poll() is None:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            child.kill()
            raise AssertionError("TUI did not exit after Ctrl-C")
        ready, _, _ = select.select([master_fd], [], [], min(0.1, remaining))
        if ready:
            try:
                chunk = os.read(master_fd, 8192)
            except OSError:
                chunk = b""
            if chunk:
                output.extend(chunk)
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
        output.extend(chunk)
    return child.wait(timeout=1)


def drain_for(master_fd: int, output: bytearray, duration: float = 0.3) -> None:
    deadline = time.monotonic() + duration
    while time.monotonic() < deadline:
        ready, _, _ = select.select([master_fd], [], [], min(0.05, deadline - time.monotonic()))
        if not ready:
            continue
        try:
            chunk = os.read(master_fd, 8192)
        except OSError:
            continue
        if chunk:
            output.extend(chunk)


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

    child = subprocess.Popen(
        command,
        stdin=slave_fd,
        stdout=slave_fd,
        stderr=slave_fd,
        env=env,
        close_fds=True,
        preexec_fn=attach_controlling_terminal,
    )
    return master_fd, slave_fd, child, initial_termios


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


def request_status(status_url: str) -> dict[str, Any]:
    with urllib.request.urlopen(status_url, timeout=3) as response:
        return json.loads(response.read())


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


def screen_contains(screen: str, expected: str) -> bool:
    for border in "│─┌┐└┘├┤┬┴┼":
        screen = screen.replace(border, " ")
    return " ".join(expected.split()) in " ".join(screen.split())


def contains_string(value: Any, expected: str) -> bool:
    if isinstance(value, str):
        return value == expected
    if isinstance(value, dict):
        return any(contains_string(item, expected) for item in value.values())
    if isinstance(value, list):
        return any(contains_string(item, expected) for item in value)
    return False


def assert_terminal_restored(
    output: bytes, termios_fd: int, initial_termios: list[Any], child: subprocess.Popen[bytes]
) -> None:
    # BSD revokes a session leader's controlling slave after it exits. The
    # master remains usable for reading the PTY's terminal attributes.
    actual = termios.tcgetattr(termios_fd)
    assert actual == initial_termios, f"terminal attributes were not restored: {actual!r}"
    assert b"\x1b[?1049l" in output, "alternate screen was not left"
    assert b"\x1b[?25h" in output, "cursor was not shown"
    assert b"\x1b[?2004h" in output, f"bracketed paste was not enabled: {output[:240]!r}"
    assert b"\x1b[?2004l" in output, f"bracketed paste was not disabled: {output[-240:]!r}"
    assert child.returncode is not None


def run_smoke(args: argparse.Namespace) -> None:
    repository = Path(__file__).resolve().parents[2]
    fixture_path = repository / "trials" / "fake_provider.py"

    with tempfile.TemporaryDirectory(prefix="leg-tui-pty-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        state_dir = root / "state"
        event_log = root / "leg-events.jsonl"

        fixture = subprocess.Popen(
            [
                sys.executable,
                str(fixture_path),
                "--scenario",
                "paused-live-text",
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
        assert fixture.stdout is not None
        status_url = None
        try:
            for line in fixture.stdout:
                if line.startswith("Status: "):
                    status_url = line.split(": ", 1)[1].strip()
                    break
            assert status_url is not None, "fake provider did not print its status URL"
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
                drain_for(master_fd, output, 0.1)
                first_run_help = terminal_screen_text(bytes(output))
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
                    "PageUp/PageDown scroll",
                    "Ctrl-End newest",
                ):
                    assert hint in first_run_help, (
                        f"first-run help omitted {hint!r}: {first_run_help[-1800:]!r}"
                    )
                before_ack = request_status(status_url)
                assert before_ack["requests"] == 0, "a provider request ran before warning acknowledgement"

                os.write(master_fd, b"\r")
                read_until_screen_text(
                    master_fd, child, output, "Ready", rows=DEFAULT_ROWS, columns=DEFAULT_COLUMNS
                )
                os.write(master_fd, b"\x13")
                drain_for(master_fd, output)
                assert request_status(status_url)["requests"] == 0, (
                    "a blank prompt started a provider request"
                )

                # Help and the action menu are overlays: input cannot edit or submit there.
                os.write(master_fd, b"\x1bOP")
                read_until_screen_text(master_fd, child, output, "Keyboard help")
                os.write(master_fd, b"ignored-help")
                os.write(master_fd, b"\x1b")
                drain_for(master_fd, output, 0.15)
                os.write(master_fd, b"\x1bOQ")
                read_until_screen_text(master_fd, child, output, "Keyboard actions")
                action_menu = terminal_screen_text(bytes(output))
                for action in (
                    "Insert a newline",
                    "Send the complete nonblank prompt",
                    "Move by grapheme",
                    "Remove one grapheme",
                    "Ctrl-Z / Ctrl-Y",
                    "Stop the turn",
                    "Scroll transcript",
                    "F1 / F2",
                ):
                    assert action in action_menu, f"keyboard action menu omitted {action!r}"
                os.write(master_fd, b"ignored-menu")
                os.write(master_fd, b"\x1b[200~ignored-paste\x1b[201~")
                os.write(master_fd, b"\x1b")
                drain_for(master_fd, output, 0.15)
                assert request_status(status_url)["requests"] == 0, (
                    "opening help or the action menu started a provider request"
                )

                # A question mark is literal prompt text. Control bytes in this one
                # bracketed paste must never become send, stop, or help key events.
                os.write(master_fd, b"?typed line\r")
                drain_for(master_fd, output, 0.1)
                question_screen = terminal_screen_text(bytes(output))
                assert "?typed line" in question_screen, (
                    "? or Enter did not insert literal text and a newline in the composer"
                )
                assert "Keyboard help" not in question_screen, "? opened help instead of entering the draft"
                os.write(master_fd, b"\x1b[200~" + PASTED_TEXT.encode() + b"\x1b[201~")
                drain_for(master_fd, output)
                assert child.poll() is None, "a control byte in bracketed paste exited the TUI"
                assert request_status(status_url)["requests"] == 0, (
                    "paste content invoked a provider or keyboard shortcut"
                )
                os.write(master_fd, b"\x13")
                read_until_screen_text(
                    master_fd,
                    child,
                    output,
                    LIVE_TEXT,
                    rows=DEFAULT_ROWS,
                    columns=DEFAULT_COLUMNS,
                )
                os.write(master_fd, NEXT_DRAFT.encode())
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
                assert request_status(status_url)["requests"] == 1
                resize_pty(slave_fd, 24, 80)
                drain_for(master_fd, output, 0.2)
                active_screen = terminal_screen_text(bytes(output), 24, 80)
                assert "status: Running" in active_screen and NEXT_DRAFT in active_screen, (
                    f"active turn or draft changed during resize recovery: {active_screen!r}"
                )
                resize_pty(slave_fd, DEFAULT_ROWS, DEFAULT_COLUMNS)
                drain_for(master_fd, output, 0.2)
                os.write(master_fd, b"\x13")
                read_until_screen_text(
                    master_fd, child, output, "Busy: wait for the active turn"
                )
                assert request_status(status_url)["requests"] == 1, (
                    "busy Ctrl-S started a second provider request"
                )

                read_until_screen_text(master_fd, child, output, "Succeeded")
                assert request_status(status_url)["requests"] == 1, (
                    "the pasted prompt caused more than one exchange invocation"
                )
                assert NEXT_DRAFT in terminal_screen_text(bytes(output)), (
                    "the next draft typed during the turn was lost on success"
                )

                # Submit the preserved next draft, then verify Ctrl-C stops a live turn.
                os.write(master_fd, b"\x13")
                deadline = time.monotonic() + 5.0
                fixture_status = request_status(status_url)
                while fixture_status["requests"] < 2 and time.monotonic() < deadline:
                    drain_for(master_fd, output, 0.05)
                    fixture_status = request_status(status_url)
                assert fixture_status["requests"] == 2, fixture_status
                os.write(master_fd, b"\x03")
                read_until_screen_text(master_fd, child, output, "Interrupted")
                os.write(master_fd, b"\x03")
                status = drain_until_exit(master_fd, child, output)
                assert status == 0, f"TUI exit status was {status}: {bytes(output)!r}"
                assert_terminal_restored(bytes(output), master_fd, initial_termios, child)
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait(timeout=2)
                os.close(master_fd)
                os.close(slave_fd)

            fixture_status = request_status(status_url)
            assert fixture_status["requests"] == 2, fixture_status
            assert fixture_status["scenario_requests"] == {"paused-live-text": 2}, fixture_status
            assert fixture_status["input_checks"].get("paused_response_completed") is True, (
                "the first paused turn did not complete while preserving the active draft"
            )

            exchange_events = [
                json.loads(line) for line in event_log.read_text(encoding="utf-8").splitlines()
            ]
            requests = [event for event in exchange_events if event.get("event") == "request"]
            assert len(requests) == 2, f"expected two exchange request events, found {requests!r}"
            assert requests[0]["prompt"] == PROMPT, (
                f"exchange request carried the wrong prompt: {requests[0]!r}"
            )
            assert requests[1]["prompt"] == NEXT_DRAFT, (
                f"follow-up exchange carried the wrong prompt: {requests[1]!r}"
            )

            trails = list((state_dir / "sessions").glob("*.jsonl"))
            assert len(trails) == 1, f"expected one session trail, found {trails!r}"
            records = [json.loads(line) for line in trails[0].read_text(encoding="utf-8").splitlines()]
            assert any(contains_string(record, PROMPT) for record in records), (
                "the pasted provider exchange did not carry the exact submitted prompt"
            )
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
            error_output = bytearray()
            try:
                error_status = drain_until_exit(error_master, error_child, error_output)
                assert error_status != 0, "invalid catalog path unexpectedly succeeded"
                assert_terminal_restored(
                    bytes(error_output), error_master, error_initial, error_child
                )
            finally:
                if error_child.poll() is None:
                    error_child.kill()
                    error_child.wait(timeout=2)
                os.close(error_master)
                os.close(error_slave)

            after_error = request_status(status_url)
            assert after_error["requests"] == 2, after_error
        finally:
            fixture.send_signal(signal.SIGTERM)
            try:
                fixture.wait(timeout=3)
            except subprocess.TimeoutExpired:
                fixture.kill()
                fixture.wait(timeout=2)


def start_fixture(
    fixture_path: Path, scenario: str, workspace: Path | None
) -> tuple[subprocess.Popen[str], str]:
    fixture = subprocess.Popen(
        [
            sys.executable,
            str(fixture_path),
            "--scenario",
            scenario,
            "--workspace",
            str(workspace) if workspace is not None else "",
            "--port",
            "0",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    assert fixture.stdout is not None
    status_url = None
    for line in fixture.stdout:
        if line.startswith("Status: "):
            status_url = line.split(": ", 1)[1].strip()
            break
    assert status_url is not None, "fake provider did not print its status URL"
    return fixture, status_url


def launch_conversation(
    args: argparse.Namespace, env: dict[str, str], workspace: Path
) -> tuple[int, int, subprocess.Popen[bytes], list[Any], bytearray]:
    command = [
        str(Path(args.tui_bin).resolve()),
        "--leg-bin",
        str(Path(args.leg_bin).resolve()),
        "--supervisor-bin",
        str(Path(args.supervisor_bin).resolve()),
    ]
    master_fd, slave_fd, child, initial_termios = spawn_in_pty(command, env)
    output = bytearray()
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
    os.write(master_fd, b"\r")
    read_until_screen_text(master_fd, child, output, "Idle")
    return master_fd, slave_fd, child, initial_termios, output


def stop_fixture(fixture: subprocess.Popen[str]) -> None:
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
            read_until_screen_text(
                master_fd, child, output, "The write result returned; this is the final text."
            )
            read_until_screen_text(master_fd, child, output, "Succeeded")
            screen = terminal_screen_text(bytes(output))
            assert screen.count("First the text, then a verified fixture write.") == 1, screen
            assert screen.count("The write result returned; this is the final text.") == 1, screen
            assert "Tool completed: write" in screen, screen
            status = request_status(status_url)
            assert status["requests"] == 2, status

            os.write(master_fd, b"TRIAL-TUI-MULTI-TOOL\x13")
            read_until_screen_text(master_fd, child, output, "Final multi-round text.")
            drain_for(master_fd, output, 0.25)
            multi_round_screen = terminal_screen_text(bytes(output))
            for segment in (
                "First multi-round text.",
                "Second multi-round text.",
                "Final multi-round text.",
            ):
                assert multi_round_screen.count(segment) == 1, multi_round_screen
            assert request_status(status_url)["requests"] == 5

            os.write(master_fd, b"TRIAL-TUI-MAX-TOKENS\x13")
            read_until_screen_text(master_fd, child, output, "Reply truncated at max tokens.")
            assert request_status(status_url)["requests"] == 6

            os.write(master_fd, b"TRIAL-CAP\x13")
            read_until_screen_text(master_fd, child, output, "Capped")
            read_until_screen_text(master_fd, child, output, "Tool-round limit reached.")
            assert request_status(status_url)["requests"] == 9

            os.write(master_fd, b"TRIAL-TUI-ANSI\x13")
            read_until_screen_text(master_fd, child, output, "Before red after Ω")
            drain_for(master_fd, output, 0.25)
            ansi_screen = terminal_screen_text(bytes(output))
            assert "secret title" not in ansi_screen, ansi_screen
            assert "Before red after Ω" in ansi_screen, ansi_screen
            assert request_status(status_url)["requests"] == 10

            os.write(master_fd, b"TRIAL-LARGE-TOOL\x13")
            read_until_screen_text(master_fd, child, output, "The large tool result was returned.")
            drain_for(master_fd, output, 0.25)
            large_tool_screen = terminal_screen_text(bytes(output))
            assert "more characters" in large_tool_screen, large_tool_screen
            assert request_status(status_url)["requests"] == 12

            os.write(master_fd, b"TRIAL-TUI-LONG-PAUSE\x13")
            read_until_screen_text(master_fd, child, output, "Long fixture line 090")
            os.write(master_fd, b"\x1b[5~")
            drain_for(master_fd, output, 0.15)
            history_screen = terminal_screen_text(bytes(output))
            visible_rows = re.findall(r"Long fixture line \d{3}", history_screen)
            assert visible_rows, f"PageUp did not reveal transcript history: {history_screen!r}"
            anchor = visible_rows[len(visible_rows) // 2]
            read_until_screen_text(master_fd, child, output, "new content below")
            preserved_screen = terminal_screen_text(bytes(output))
            assert anchor in preserved_screen, (
                f"streaming moved the historical viewport away from {anchor!r}: {preserved_screen!r}"
            )
            os.write(master_fd, b"\x1b[1;5F")
            read_until_screen_text(master_fd, child, output, "END OF FIXTURE ANSWER")
            newest_screen = terminal_screen_text(bytes(output))
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
            if child is not None and child.poll() is None:
                child.kill()
                child.wait(timeout=2)
            if master_fd is not None:
                os.close(master_fd)
            if slave_fd is not None:
                os.close(slave_fd)
            stop_fixture(fixture)


def drain_until_exit_after_close(
    master_fd: int,
    child: subprocess.Popen[bytes],
    output: bytearray,
    slave_fd: int,
    initial_termios: list[Any],
) -> int:
    os.write(master_fd, b"\x03")
    status = drain_until_exit(master_fd, child, output)
    assert_terminal_restored(bytes(output), master_fd, initial_termios, child)
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
            read_until_screen_text(master_fd, child, output, "Failed")
            assert request_status(status_url)["requests"] == 1
            drain_for(master_fd, output, 0.15)
            assert request_status(status_url)["requests"] == 1, "reading a failure retried it"

            os.write(master_fd, b"\r")
            drain_for(master_fd, output, 0.15)
            assert request_status(status_url)["requests"] == 1, "Enter retried a failed prompt"
            os.write(master_fd, b"\x7f")
            drain_for(master_fd, output, 0.1)

            warning = "Retry sends this prompt again and may repeat tool side effects."
            os.write(master_fd, b"\x13")
            read_until_screen_text(master_fd, child, output, warning)
            assert request_status(status_url)["requests"] == 1
            os.write(master_fd, b"\r")
            drain_for(master_fd, output, 0.15)
            assert request_status(status_url)["requests"] == 1, "Enter confirmed a retry"
            assert warning in terminal_screen_text(bytes(output))

            os.write(master_fd, b"n")
            drain_for(master_fd, output, 0.1)
            assert request_status(status_url)["requests"] == 1
            assert "Explicit retry confirmation" not in terminal_screen_text(bytes(output))
            os.write(master_fd, b"\x13")
            read_until_screen_text(master_fd, child, output, warning)
            assert request_status(status_url)["requests"] == 1
            os.write(master_fd, b"y")
            read_until_screen_text(master_fd, child, output, "The explicit retry succeeded.")
            drain_for(master_fd, output, 0.25)
            assert request_status(status_url)["requests"] == 2

            status = drain_until_exit_after_close(master_fd, child, output, slave_fd, initial_termios)
            master_fd = slave_fd = None
            assert status == 0
            final_status = request_status(status_url)
            assert final_status["scenario_requests"] == {"TRIAL-REOPEN-FAILURE": 2}, final_status
            assert final_status["input_checks"].get("retry_or_reopen_succeeded") is True, final_status
        finally:
            if child is not None and child.poll() is None:
                child.kill()
                child.wait(timeout=2)
            if master_fd is not None:
                os.close(master_fd)
            if slave_fd is not None:
                os.close(slave_fd)
            stop_fixture(fixture)


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
            for hint in ("Composer", "status: Idle", "Ctrl-S send", "Ctrl-C stop/exit", "F1 help"):
                assert hint in conversation_screen, (
                    f"conversation omitted {hint!r} at 80x24: {conversation_screen!r}"
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
            requests = [
                json.loads(line)
                for line in event_log.read_text(encoding="utf-8").splitlines()
                if json.loads(line).get("event") == "request"
            ]
            assert len(requests) == 1 and requests[0]["prompt"] == prompt, requests

            resize_pty(slave_fd, 40, 120)
            drain_for(master_fd, output, 0.2)
            final_screen = terminal_screen_text(bytes(output), 40, 120)
            assert "Succeeded" in final_screen and "Conversation" in final_screen, final_screen
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
                        drain_for(master_fd, output, 0.2)
                        assert next_draft in terminal_screen_text(
                            bytes(output), DEFAULT_ROWS, DEFAULT_COLUMNS
                        )
                    else:
                        next_draft = f"idle draft survives {signal_name}"
                        os.write(master_fd, next_draft.encode())
                        drain_for(master_fd, output, 0.15)

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


def has_color_styling(output: bytes) -> bool:
    color_codes = set(range(30, 38)) | set(range(40, 48)) | set(range(90, 98)) | set(range(100, 108))
    color_codes.update((38, 48, 58))
    for params in re.findall(rb"\x1b\[([0-9;]*)m", output):
        codes = [int(value) for value in params.split(b";") if value]
        if any(code in color_codes for code in codes):
            return True
    return False


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
    args = parser.parse_args()
    run_smoke(args)
    run_turn_contract_smoke(args)
    run_retry_confirmation_smoke(args)
    run_resize_and_non_tty_smoke(args)
    run_signal_smoke(args)
    run_color_policy_smoke(args)
    print("leg-tui native Linux/macOS PTY smoke passed")


if __name__ == "__main__":
    main()
