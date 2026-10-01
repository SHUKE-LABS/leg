#!/usr/bin/env python3
"""Linux PTY smoke test for the first leg-tui conversation."""

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
PASTED_TEXT = "first line: 中文\r\nsecond: 👩‍👩‍👧‍👦 e\u0301\rthird line\x13\x03\x1b\x7f\nfourth line?"
PROMPT = "?typed line\nfirst line: 中文\nsecond: 👩‍👩‍👧‍👦 e\u0301\nthird line\nfourth line?"
NEXT_DRAFT = "next draft"
LIVE_TEXT = "The first live text is visible."


def read_until(
    master_fd: int,
    child: subprocess.Popen[bytes],
    output: bytearray,
    needle: bytes,
    timeout: float = 8.0,
) -> None:
    deadline = time.monotonic() + timeout
    while needle not in output:
        if child.poll() is not None:
            raise AssertionError(
                f"TUI exited before output {needle!r}; exit={child.returncode}; "
                f"screen text={terminal_text(bytes(output))[-1200:]!r}"
            )
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(
                f"timed out waiting for {needle!r}; "
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


def read_until_screen_text(
    master_fd: int,
    child: subprocess.Popen[bytes],
    output: bytearray,
    needle: str,
    timeout: float = 8.0,
) -> None:
    deadline = time.monotonic() + timeout
    while needle not in terminal_screen_text(bytes(output)):
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
    command: list[str], env: dict[str, str], rows: int = 40, columns: int = 160
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


def request_status(status_url: str) -> dict[str, Any]:
    with urllib.request.urlopen(status_url, timeout=3) as response:
        return json.loads(response.read())


def terminal_text(output: bytes) -> str:
    # Ratatui moves the cursor between adjacent words; turn those moves into
    # spaces before dropping the remaining ANSI controls.
    separated = re.sub(rb"\x1b\[[0-9;]*H", b" ", output)
    plain = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", separated)
    return plain.decode("utf-8", errors="replace")


def terminal_screen_text(output: bytes, rows: int = 40, columns: int = 160) -> str:
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


def contains_string(value: Any, expected: str) -> bool:
    if isinstance(value, str):
        return value == expected
    if isinstance(value, dict):
        return any(contains_string(item, expected) for item in value.values())
    if isinstance(value, list):
        return any(contains_string(item, expected) for item in value)
    return False


def assert_terminal_restored(
    output: bytes, slave_fd: int, initial_termios: list[Any], child: subprocess.Popen[bytes]
) -> None:
    actual = termios.tcgetattr(slave_fd)
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
                read_until(master_fd, child, output, b"Path:")
                os.write(master_fd, str(workspace).encode() + b"\r")
                read_until(master_fd, child, output, WARNING.encode())
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
                ):
                    assert hint in first_run_help, (
                        f"first-run help omitted {hint!r}: {first_run_help[-1800:]!r}"
                    )
                before_ack = request_status(status_url)
                assert before_ack["requests"] == 0, "a provider request ran before warning acknowledgement"

                os.write(master_fd, b"\r")
                read_until(master_fd, child, output, b"Ready")
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
                read_until(master_fd, child, output, b"visible.")
                assert LIVE_TEXT in terminal_text(bytes(output)), (
                    f"streamed response was not visible: {terminal_text(bytes(output))[-2000:]!r}"
                )
                os.write(master_fd, NEXT_DRAFT.encode() + b"\x13")
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
                read_until(master_fd, child, output, b"Stopped")
                os.write(master_fd, b"\x03")
                status = drain_until_exit(master_fd, child, output)
                assert status == 0, f"TUI exit status was {status}: {bytes(output)!r}"
                assert_terminal_restored(bytes(output), slave_fd, initial_termios, child)
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
                    bytes(error_output), error_slave, error_initial, error_child
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


def main() -> None:
    if sys.platform != "linux":
        raise SystemExit("pty_smoke.py is Linux-only")
    parser = argparse.ArgumentParser()
    parser.add_argument("--tui-bin", required=True)
    parser.add_argument("--leg-bin", required=True)
    parser.add_argument("--supervisor-bin", required=True)
    args = parser.parse_args()
    run_smoke(args)
    print("leg-tui Linux PTY smoke passed")


if __name__ == "__main__":
    main()
