#!/usr/bin/env python3
"""Record native Windows ConPTY behavior checks for leg-tui."""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import tempfile
import time
from pathlib import Path
from typing import Any, Callable

import psutil

import responsiveness_harness as harness


PASTED_PROMPT = (
    "TRIAL-CHINESE\r\n"
    "Line one: keep this first.\r\n"
    "第二行：保留中文。\r\n"
    "Line three: keep this third.\r\n"
    "Emoji: 😀"
)
RETRY_WARNING = "Retry sends this prompt again and may repeat tool side effects."
TERMINAL_RESTORE_SEQUENCES = (
    (b"\x1b[?2004l", "bracketed paste disabled"),
    (b"\x1b[?1049l", "alternate screen left"),
    (b"\x1b[?25h", "cursor shown"),
)


def has_color_styling(output: bytes) -> bool:
    color_codes = set(range(30, 38)) | set(range(40, 48)) | set(range(90, 98)) | set(range(100, 108))
    color_codes.update((38, 48, 58))
    for params in re.findall(rb"\x1b\[([0-9;]*)m", output):
        codes = [int(value) for value in params.split(b";") if value]
        if any(code in color_codes for code in codes):
            return True
    return False


def flatten_terminal_text(output: str) -> str:
    without_borders = re.sub(r"[│┌┐└┘─]", " ", output)
    return " ".join(without_borders.split())


def assert_terminal_restored(terminal: harness.TerminalProcess) -> dict[str, bool]:
    if not terminal.wait_exit(timeout=10.0):
        raise AssertionError(f"leg-tui did not exit normally; screen:\n{terminal.capture.text()[-2000:]}")
    terminal.reader.join(timeout=1.0)
    raw = bytes(terminal.capture.raw)
    results = {description: sequence in raw for sequence, description in TERMINAL_RESTORE_SEQUENCES}
    failed = [name for name, passed in results.items() if not passed]
    if failed:
        raise AssertionError(f"terminal restoration was incomplete: {failed}; output tail={raw[-500:]!r}")
    return results


def start_conversation(
    root: Path,
    server: harness.FixtureServer,
    tui_bin: Path,
    leg_bin: Path,
    supervisor_bin: Path,
    env_overrides: dict[str, str] | None = None,
) -> tuple[harness.TerminalProcess, Path, Path]:
    workspace = root / "workspace"
    workspace.mkdir(parents=True, exist_ok=True)
    state_dir = root / "state"
    env = harness._build_harness_environment(
        os.environ.copy(),
        server.environment,
        state_dir,
        supervisor_bin,
    )
    env.update({"TERM": "xterm-256color", "LEG_EVENT_LOG": str(root / "leg-events.jsonl")})
    if env_overrides:
        env.update(env_overrides)
    terminal = harness.TerminalProcess(
        [str(tui_bin), "--leg-bin", str(leg_bin), "--supervisor-bin", str(supervisor_bin)],
        workspace,
        env,
        rows=24,
        columns=80,
    )
    terminal.wait_contains("Path:")
    terminal.write(str(workspace) + "\r")
    terminal.wait_contains("Leg can run shell commands and modify files")
    terminal.write("\r")
    terminal.wait_contains(harness.COMPOSER_HINT)
    return terminal, workspace, state_dir


def with_tui(
    args: argparse.Namespace,
    name: str,
    scenario: str,
    action: Callable[[harness.TerminalProcess, Path, harness.FixtureServer], dict[str, Any]],
    hold_after_first_chunk: bool = False,
    env_overrides: dict[str, str] | None = None,
) -> dict[str, Any]:
    with tempfile.TemporaryDirectory(prefix=f"leg-tui-windows-{name}-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        server = harness.FixtureServer(
            workspace,
            scenario=scenario,
            hold_after_first_chunk=hold_after_first_chunk,
        )
        terminal: harness.TerminalProcess | None = None
        try:
            terminal, workspace, _ = start_conversation(
                root,
                server,
                args.tui_bin.resolve(),
                args.leg_bin.resolve(),
                args.supervisor_bin.resolve(),
                env_overrides,
            )
            return action(terminal, workspace, server)
        finally:
            if terminal is not None:
                terminal.close()
            server.close()


def exit_normally(terminal: harness.TerminalProcess) -> dict[str, bool]:
    terminal.write_control("c")
    return assert_terminal_restored(terminal)


def wait_for_fixture(server: harness.FixtureServer, predicate: Callable[[dict[str, Any]], bool], description: str) -> dict[str, Any]:
    deadline = time.monotonic() + 15.0
    while time.monotonic() < deadline:
        status = server.fixture.status()
        if predicate(status):
            return status
        time.sleep(0.05)
    raise TimeoutError(f"timed out waiting for fixture {description}; last status={server.fixture.status()}")


def rename_current_session(terminal: harness.TerminalProcess, title: str) -> None:
    terminal.write("\x1bOQ")
    terminal.wait_contains(harness.PALETTE_HEADER)
    terminal.write("rename")
    terminal.write("\r")
    terminal.wait_contains("Rename session")
    terminal.write(title)
    terminal.write("\r")
    terminal.wait_contains(title)
    terminal.write("\r")
    terminal.wait_contains(f"{title}  |  model:")


def create_new_session(terminal: harness.TerminalProcess, title: str) -> None:
    terminal.write("\x1bOQ")
    terminal.wait_contains(harness.PALETTE_HEADER)
    terminal.write("New conversation")
    terminal.write("\r")
    terminal.wait_for(
        lambda screen: harness._session_title_visible(screen, "Untitled conversation"),
        "new conversation header",
    )
    rename_current_session(terminal, title)


def process_tree_identities(root_pid: int) -> list[dict[str, float | int | str]]:
    root = psutil.Process(root_pid)
    processes = [root, *root.children(recursive=True)]
    identities = []
    for process in processes:
        identities.append(
            {
                "pid": process.pid,
                "create_time": process.create_time(),
                "ppid": process.ppid(),
                "name": process.name(),
            }
        )
    if len(identities) < 2:
        raise AssertionError(f"stalled bash root {root_pid} had no owned child process to verify")
    return identities


def wait_for_identities_to_exit(
    identities: list[dict[str, float | int | str]], timeout: float = 10.0
) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        alive = []
        for identity in identities:
            try:
                process = psutil.Process(int(identity["pid"]))
                if abs(process.create_time() - float(identity["create_time"])) < 0.01:
                    alive.append(int(identity["pid"]))
            except psutil.NoSuchProcess:
                continue
        if not alive:
            return
        time.sleep(0.05)
    raise AssertionError(
        f"owned tool process tree still exists after Stop: {alive}; identities={identities}"
    )


def check_paste(args: argparse.Namespace) -> dict[str, Any]:
    def action(terminal: harness.TerminalProcess, _workspace: Path, server: harness.FixtureServer) -> dict[str, Any]:
        terminal.write("\x1b[200~" + PASTED_PROMPT + "\x1b[201~")
        terminal.wait_contains("Line three: keep this third.")
        terminal.wait_contains("Emoji:")
        if server.fixture.status()["requests"] != 0:
            raise AssertionError("paste submitted a request before Ctrl-S")
        terminal.write_control("s")
        terminal.wait_contains("Three lines received, including the Chinese second line.")
        status = server.fixture.status()
        if (
            status["requests"] != 1
            or not status["input_checks"].get("chinese_multiline_prompt")
            or not status["input_checks"].get("emoji_paste")
        ):
            raise AssertionError(f"pasted Unicode prompt was not received exactly once: {status}")
        restored = exit_normally(terminal)
        return {
            "fixture_requests": status["requests"],
            "chinese_multiline_prompt": True,
            "emoji_paste": True,
            "terminal_restored": restored,
        }

    return with_tui(args, "paste", "trial", action)


def check_resize(args: argparse.Namespace) -> dict[str, Any]:
    def action(terminal: harness.TerminalProcess, _workspace: Path, server: harness.FixtureServer) -> dict[str, Any]:
        draft = "TRIAL-RESIZE-RECOVERY"
        terminal.write(draft)
        terminal.wait_contains(draft)
        terminal.resize(23, 79)
        terminal.wait_contains("Terminal too small")
        terminal.write_control("s")
        terminal.wait_contains("Send rejected")
        if server.fixture.status()["requests"] != 0:
            raise AssertionError("below-minimum send reached the fixture")
        terminal.resize(24, 80)
        terminal.wait_contains(draft)
        terminal.write_control("s")
        terminal.wait_contains("Succeeded")
        status = server.fixture.status()
        if status["requests"] != 1:
            raise AssertionError(f"recovered draft did not submit exactly once: {status}")
        restored = exit_normally(terminal)
        return {"draft_preserved": True, "below_minimum_send_requests": 0, "final_requests": 1, "terminal_restored": restored}

    return with_tui(args, "resize", "trial", action)


def check_no_color(args: argparse.Namespace) -> dict[str, Any]:
    def action(terminal: harness.TerminalProcess, _workspace: Path, server: harness.FixtureServer) -> dict[str, Any]:
        output = bytes(terminal.capture.raw)
        if has_color_styling(output):
            raise AssertionError("NO_COLOR still emitted terminal color styles")
        if server.fixture.status()["requests"] != 0:
            raise AssertionError("NO_COLOR check unexpectedly submitted a prompt")
        restored = exit_normally(terminal)
        return {"color_styles_emitted": False, "terminal_restored": restored}

    return with_tui(
        args,
        "no-color",
        "trial",
        action,
        env_overrides={"NO_COLOR": "1", "TERM": "xterm-256color"},
    )


def check_clipboard_fallback(args: argparse.Namespace) -> dict[str, Any]:
    def action(terminal: harness.TerminalProcess, workspace: Path, server: harness.FixtureServer) -> dict[str, Any]:
        terminal.write("COPY-FALLBACK-CHECK")
        terminal.write_control("s")
        terminal.wait_contains("Fixture ready. This answer came from a local fake provider.")
        terminal.write("\x1bOS")
        terminal.wait_contains(harness.INSPECTOR_HEADER)
        previous_raw_size = terminal.capture.raw_size()
        terminal.write("\x1b[15~")
        terminal.wait_contains("Clipboard request sent")
        osc52 = bytes(terminal.capture.raw[previous_raw_size:])
        if b"\x1b]52;c;" not in osc52:
            raise AssertionError("F5 did not issue an OSC 52 clipboard request")
        terminal.write("\x1b[18~")
        terminal.wait_contains("Save copied text to:")
        terminal.write("\r")
        terminal.wait_contains("Saved ")
        saved_path = workspace / "leg-copy.txt"
        if not saved_path.is_file() or not saved_path.read_text(encoding="utf-8").strip():
            raise AssertionError("F7 did not save the selected text")
        status = server.fixture.status()
        if status["requests"] != 1:
            raise AssertionError(f"copy/save unexpectedly submitted another prompt: {status}")
        restored = exit_normally(terminal)
        return {
            "clipboard_backend": "ConPTY has no Windows Terminal frontend or OSC 52 responder",
            "osc52_request_emitted": True,
            "f7_saved_nonempty_text": True,
            "fixture_requests": status["requests"],
            "terminal_restored": restored,
        }

    return with_tui(args, "clipboard", "first-answer", action)


def check_normal_terminal_restoration(args: argparse.Namespace) -> dict[str, Any]:
    def action(terminal: harness.TerminalProcess, _workspace: Path, _server: harness.FixtureServer) -> dict[str, Any]:
        restored = exit_normally(terminal)
        return {"terminal_restored": restored}

    return with_tui(args, "terminal-restoration", "trial", action)


def check_stop_process_tree(args: argparse.Namespace) -> dict[str, Any]:
    def action(terminal: harness.TerminalProcess, workspace: Path, server: harness.FixtureServer) -> dict[str, Any]:
        terminal.write("TRIAL-STOP")
        terminal.write_control("s")
        root_pid_file = workspace / "trial-stalled-root.pid"
        child_pid_file = workspace / "trial-stalled-child.pid"
        deadline = time.monotonic() + 15.0
        while time.monotonic() < deadline and not (root_pid_file.is_file() and child_pid_file.is_file()):
            if not terminal.alive():
                raise AssertionError(f"TUI exited before stalled bash started: {terminal.capture.text()[-2000:]}")
            time.sleep(0.05)
        if not root_pid_file.is_file() or not child_pid_file.is_file():
            raise TimeoutError(
                "stalled bash did not publish its root and child process IDs; "
                f"fixture={server.fixture.status()}; screen={terminal.capture.text()[-2000:]}"
            )
        root_pid = int(root_pid_file.read_text(encoding="utf-8").strip())
        try:
            identities = process_tree_identities(root_pid)
        except psutil.NoSuchProcess as error:
            raise AssertionError(
                f"spawned tool root PID {root_pid} disappeared before Stop; "
                f"fixture={server.fixture.status()}; screen={terminal.capture.text()[-2000:]}"
            ) from error
        terminal.wait_contains("Tool running: bash")
        terminal.write_control("c")
        try:
            wait_for_identities_to_exit(identities)
        except AssertionError as error:
            raise AssertionError(
                f"{error}; fixture={server.fixture.status()}; screen={terminal.capture.text()[-2000:]}"
            ) from error
        terminal.wait_contains("Interrupted")
        status = server.fixture.status()
        if status["requests"] != 1:
            raise AssertionError(f"Stop changed the fixture request count: {status}")
        if (workspace / "trial-stall-finished.txt").exists():
            raise AssertionError("the stalled bash command continued after Stop")
        terminal.write_control("c")
        restored = assert_terminal_restored(terminal)
        return {
            "tool_root_pid": root_pid,
            "owned_process_tree": identities,
            "owned_process_tree_gone": True,
            "tui_reported_interrupted": True,
            "fixture_requests": status["requests"],
            "terminal_restored": restored,
        }

    return with_tui(args, "stop-process-tree", "stalled-bash", action)


def check_session_switching(args: argparse.Namespace) -> dict[str, Any]:
    def action(terminal: harness.TerminalProcess, _workspace: Path, server: harness.FixtureServer) -> dict[str, Any]:
        rename_current_session(terminal, "Alpha")
        terminal.write("TRIAL-PAUSE")
        terminal.write_control("s")
        terminal.wait_contains("The first live text is visible.")
        wait_for_fixture(server, lambda status: status["pause_gates"].get("1") == "held", "Alpha stream pause")
        create_new_session(terminal, "Beta")
        terminal.write("Beta draft stays local")
        terminal.wait_contains("Beta draft stays local")
        harness._select_session(terminal, "Alpha")
        if server.fixture.status()["requests"] != 1:
            raise AssertionError("switching to the active background session replayed its prompt")
        harness._select_session(terminal, "Beta")
        terminal.wait_contains("Beta draft stays local")
        held = server.fixture.status()
        if held["requests"] != 1 or held["pause_gates"].get("1") != "held":
            raise AssertionError(f"background browsing changed the active request: {held}")
        server.fixture.release_pause_gate(1)
        wait_for_fixture(server, lambda status: status["pause_gates"].get("1") == "completed", "Alpha stream completion")
        harness._select_session(terminal, "Alpha")
        terminal.wait_contains("The fixture resumes after its fixed pause.")
        completed = server.fixture.status()
        if completed["requests"] != 1:
            raise AssertionError(f"switching/background browsing submitted or replayed a prompt: {completed}")
        restored = exit_normally(terminal)
        return {
            "fixture_requests": completed["requests"],
            "background_stream_completed": True,
            "beta_draft_preserved": True,
            "terminal_restored": restored,
        }

    return with_tui(
        args,
        "session-switching",
        "trial",
        action,
        hold_after_first_chunk=True,
    )


def check_retry_cancel(args: argparse.Namespace) -> dict[str, Any]:
    def action(terminal: harness.TerminalProcess, _workspace: Path, server: harness.FixtureServer) -> dict[str, Any]:
        terminal.write("TRIAL-RETRY cancellation must not resend")
        terminal.write_control("s")
        terminal.wait_contains("Failed")
        if server.fixture.status()["requests"] != 1:
            raise AssertionError("initial retry fixture request count was not one")
        terminal.write_control("p")
        terminal.wait_contains(harness.PALETTE_HEADER)
        terminal.write("retry")
        terminal.write("\r")
        terminal.wait_for(
            lambda screen: RETRY_WARNING in flatten_terminal_text(screen),
            "retry confirmation warning",
        )
        terminal.write("n")
        terminal.wait_for(
            lambda text: "Explicit retry confirmation" not in text,
            "retry confirmation to close",
        )
        status = server.fixture.status()
        if status["requests"] != 1:
            raise AssertionError(f"cancelling retry replayed the failed prompt: {status}")
        restored = exit_normally(terminal)
        return {"fixture_requests_after_cancel": status["requests"], "retry_cancelled": True, "terminal_restored": restored}

    return with_tui(args, "retry-cancel", "trial", action)


def check_same_session_busy(args: argparse.Namespace) -> dict[str, Any]:
    def action(terminal: harness.TerminalProcess, _workspace: Path, server: harness.FixtureServer) -> dict[str, Any]:
        terminal.write("TRIAL-PAUSE")
        terminal.write_control("s")
        terminal.wait_contains("The first live text is visible.")
        wait_for_fixture(server, lambda status: status["pause_gates"].get("1") == "held", "active same-session request")
        terminal.write("DO-NOT-SUBMIT-WHILE-BUSY")
        terminal.write_control("s")
        terminal.wait_contains("Busy: wait for the active turn")
        held = server.fixture.status()
        if held["requests"] != 1 or held["active_requests"] != [1]:
            raise AssertionError(f"busy same-session send reached the provider: {held}")
        server.fixture.release_pause_gate(1)
        wait_for_fixture(server, lambda status: status["pause_gates"].get("1") == "completed", "active request completion")
        completed = server.fixture.status()
        if completed["requests"] != 1:
            raise AssertionError(f"same-session busy send replayed a prompt: {completed}")
        restored = exit_normally(terminal)
        return {"fixture_requests": completed["requests"], "busy_rejected": True, "terminal_restored": restored}

    return with_tui(
        args,
        "same-session-busy",
        "trial",
        action,
        hold_after_first_chunk=True,
    )


def run_check(name: str, operation: Callable[[], dict[str, Any]]) -> dict[str, Any]:
    try:
        return {"status": "passed", "evidence": operation()}
    except Exception as error:
        return {"status": "failed", "error": f"{type(error).__name__}: {error}"}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tui-bin", required=True, type=Path)
    parser.add_argument("--leg-bin", required=True, type=Path)
    parser.add_argument("--supervisor-bin", required=True, type=Path)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if os.name != "nt":
        parser.error("native Windows behavior checks require Windows ConPTY")
    for binary in (args.tui_bin, args.leg_bin, args.supervisor_bin):
        if not binary.is_file():
            parser.error(f"binary does not exist: {binary}")

    operations: tuple[tuple[str, Callable[[], dict[str, Any]]], ...] = (
        ("chinese_emoji_multiline_paste", lambda: check_paste(args)),
        ("minimum_size_recovery", lambda: check_resize(args)),
        ("no_color", lambda: check_no_color(args)),
        ("clipboard_denial_and_save_fallback", lambda: check_clipboard_fallback(args)),
        ("normal_terminal_restoration", lambda: check_normal_terminal_restoration(args)),
        ("stop_owned_tool_process_tree", lambda: check_stop_process_tree(args)),
        ("background_browsing_and_session_switching", lambda: check_session_switching(args)),
        ("retry_cancellation", lambda: check_retry_cancel(args)),
        ("same_session_busy_rejection", lambda: check_same_session_busy(args)),
    )
    checks = {name: run_check(name, operation) for name, operation in operations}
    report = {
        "schema": "leg-tui.windows-behavior-report/v1",
        "source_revision": args.source_revision,
        "os": platform.platform(),
        "cpu": harness._cpu_model(),
        "terminal": {
            "backend": "Windows ConPTY via pywinpty",
            "frontend": "none; pywinpty drove the ConPTY API directly",
            "version": harness._terminal_version(),
        },
        "checks": checks,
        "status": "passed" if all(result["status"] == "passed" for result in checks.values()) else "failed",
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    temporary = args.output.with_name(args.output.name + ".tmp")
    temporary.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    temporary.replace(args.output)
    print(f"windows_behavior_report={args.output.resolve()}")
    print(json.dumps(report, ensure_ascii=True, indent=2))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
