#!/usr/bin/env python3
"""Measure startup, idle RSS, and comparable local bundle sizes for Web/TUI."""

from __future__ import annotations

import argparse
import asyncio
import codecs
import fcntl
import json
import os
import platform
import pty
import queue
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from pathlib import Path
from urllib.parse import urlsplit

import pyte
from playwright.async_api import async_playwright

from make_trial_archive import create_archive
from sample_process_rss import process_table, process_tree_rss


ROOT = Path(__file__).resolve().parents[2]


def tree_roots_for_profile(profile: Path, timeout_seconds: float = 5) -> list[int]:
    deadline = time.monotonic() + timeout_seconds
    needle = str(profile)
    while time.monotonic() < deadline:
        table = process_table()
        matching = {
            pid for pid, (_parent, _rss, command) in table.items() if needle in command
        }
        roots = sorted(
            pid
            for pid in matching
            if table[pid][0] not in matching
        )
        if roots:
            return roots
        time.sleep(0.1)
    raise RuntimeError(f"could not find browser process for profile {profile}")


def sample_process_set(root_pids: list[int]) -> dict[str, object]:
    time.sleep(10)
    rss_samples: list[int] = []
    process_counts: list[int] = []
    for index in range(10):
        rss, count = process_tree_rss(root_pids)
        rss_samples.append(rss)
        process_counts.append(count)
        if index < 9:
            time.sleep(1)
    ordered_samples = sorted(rss_samples)
    return {
        "root_pids": root_pids,
        "settle_seconds": 10,
        "sample_count": 10,
        "sample_interval_seconds": 1,
        "rss_bytes_samples": rss_samples,
        "process_counts": process_counts,
        "median_rss_bytes": (ordered_samples[4] + ordered_samples[5]) // 2,
    }


def start_host(web_bin: Path, leg_bin: Path, supervisor_bin: Path, workspace: Path, state: Path):
    started = time.monotonic()
    process = subprocess.Popen(
        [
            str(web_bin),
            "--no-open",
            "--bind",
            "127.0.0.1:0",
            "--state-dir",
            str(state),
            "--leg-bin",
            str(leg_bin),
            "--supervisor-bin",
            str(supervisor_bin),
        ],
        cwd=workspace,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    output: queue.Queue[str | None] = queue.Queue()

    def read_stdout() -> None:
        if process.stdout is not None:
            for line in process.stdout:
                output.put(line.rstrip())
        output.put(None)

    def drain_stderr() -> None:
        if process.stderr is not None:
            for _line in process.stderr:
                pass

    threading.Thread(target=read_stdout, daemon=True).start()
    threading.Thread(target=drain_stderr, daemon=True).start()
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        try:
            line = output.get(timeout=max(0.1, deadline - time.monotonic()))
        except queue.Empty:
            break
        if line is None:
            break
        if line.startswith("Open this one-time launch URL:"):
            return process, line.split(": ", 1)[1], started
    stop_process(process)
    raise RuntimeError("leg-web did not print its launch URL")


def stop_process(process: subprocess.Popen[object] | None) -> None:
    if process is None or process.poll() is not None:
        return
    try:
        process.send_signal(signal.SIGINT)
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def start_tui(
    tui_bin: Path, leg_bin: Path, supervisor_bin: Path, workspace: Path, state_dir: Path
):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    env = os.environ.copy()
    env["TERM"] = "xterm-256color"
    env["LEG_UI_STATE_DIR"] = str(state_dir)
    started = time.monotonic()
    process = subprocess.Popen(
        [str(tui_bin), "--leg-bin", str(leg_bin), "--supervisor-bin", str(supervisor_bin)],
        cwd=workspace,
        env=env,
        stdin=slave,
        stdout=slave,
        stderr=slave,
        start_new_session=True,
    )
    os.close(slave)
    screen = pyte.Screen(120, 40)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        visible = " ".join(" ".join(screen.display).split())
        if "Choose a workspace" in visible:
            return process, master, int((time.monotonic() - started) * 1000)
        if process.poll() is not None:
            break
        ready, _, _ = select.select([master], [], [], 0.1)
        if ready:
            try:
                chunk = os.read(master, 8192)
            except OSError:
                break
            if chunk:
                stream.feed(decoder.decode(chunk))
    stop_process(process)
    os.close(master)
    raise RuntimeError(f"leg-tui did not show its workspace chooser: {screen.display!r}")


def make_tui_reference_bundle(
    stage_root: Path,
    archive_path: Path,
    tui_bin: Path,
    leg_bin: Path,
    supervisor_bin: Path,
    target: str,
    revision: str,
) -> int:
    platform_name = "macos" if sys.platform == "darwin" else "linux"
    architecture = platform.machine().lower()
    architecture = {"amd64": "x86_64", "arm64": "aarch64"}.get(architecture, architecture)
    bundle_name = f"leg-tui-reference-{platform_name}-{architecture}-{revision[:12]}"
    bundle = stage_root / bundle_name
    (bundle / "bin").mkdir(parents=True)
    (bundle / "docs").mkdir()
    (bundle / "companions" / "trials").mkdir(parents=True)
    for binary in (tui_bin, leg_bin, supervisor_bin):
        destination = bundle / "bin" / binary.name
        destination.write_bytes(binary.read_bytes())
        destination.chmod(binary.stat().st_mode & 0o777)
    (bundle / "start-tui.sh").write_text(
        "#!/bin/sh\nset -eu\n"
        "bundle_dir=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\n"
        "exec \"${bundle_dir}/bin/leg-tui\" "
        "--leg-bin \"${bundle_dir}/bin/leg\" "
        "--supervisor-bin \"${bundle_dir}/bin/leg-ui-supervisor\" \"$@\"\n",
        encoding="utf-8",
    )
    (bundle / "start-tui.sh").chmod(0o755)
    (bundle / "QUICKSTART.md").write_text(
        "# Leg TUI trial reference bundle\n\n"
        "Experimental resource-measurement reference built from the same source "
        "revision and common fixture materials as the Web bundle. Run `./start-tui.sh`, "
        "select a disposable workspace, and use Ctrl-S to send. Ctrl-C stops an active "
        "turn or exits when idle.\n",
        encoding="utf-8",
    )
    (bundle / "LICENSE").write_bytes((ROOT / "LICENSE").read_bytes())
    (bundle / "docs" / "ui-experiments.md").write_bytes((ROOT / "docs/ui-experiments.md").read_bytes())
    for name in ("fake_provider.py", "results-template.json", "score.py"):
        (bundle / "companions" / "trials" / name).write_bytes(
            (ROOT / "companions/trials" / name).read_bytes()
        )
    (bundle / "companions" / "trials" / "sample_process_rss.py").write_bytes(
        (ROOT / "companions/trials/sample_process_rss.py").read_bytes()
    )
    notices = subprocess.check_output(
        ["bash", "scripts/release.sh", "tui-trial-notices", target], cwd=ROOT
    )
    (bundle / "THIRD_PARTY_NOTICES.txt").write_bytes(notices)
    (bundle / "bundle-info.json").write_text(
        json.dumps(
            {
                "schema": "leg-tui-trial.bundle-reference/v1",
                "experimental": True,
                "platform": platform_name,
                "architecture": architecture,
                "rust_target": target,
                "core_revision": revision,
                "ui_revision": revision,
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    create_archive(bundle, archive_path, bundle_name)
    return archive_path.stat().st_size


def environment_info(browser_name: str, browser_version: str) -> dict[str, object]:
    memory_bytes = None
    cpu_model = ""
    if sys.platform.startswith("linux"):
        for line in Path("/proc/meminfo").read_text(encoding="ascii").splitlines():
            if line.startswith("MemTotal:"):
                memory_bytes = int(line.split()[1]) * 1024
                break
        for line in Path("/proc/cpuinfo").read_text(encoding="ascii").splitlines():
            if line.lower().startswith("model name"):
                cpu_model = line.split(":", 1)[1].strip()
                break
    elif sys.platform == "darwin":
        memory_bytes = int(subprocess.check_output(["sysctl", "-n", "hw.memsize"], text=True))
        try:
            cpu_model = subprocess.check_output(
                ["sysctl", "-n", "machdep.cpu.brand_string"], text=True
            ).strip()
        except subprocess.CalledProcessError:
            pass
    return {
        "os": platform.platform(),
        "cpu_model": cpu_model or platform.processor() or platform.machine(),
        "architecture": platform.machine(),
        "logical_cpu_count": os.cpu_count(),
        "physical_memory_bytes": memory_bytes,
        "browser": browser_name,
        "browser_version": browser_version,
    }


async def measure(args: argparse.Namespace) -> dict[str, object]:
    web_archive = args.web_archive.resolve()
    web_bin = args.web_bin.resolve()
    leg_bin = args.leg_bin.resolve()
    supervisor_bin = args.supervisor_bin.resolve()
    tui_bin = args.tui_bin.resolve()
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    target = subprocess.check_output(
        ["rustc", "-vV"], cwd=ROOT, text=True
    ).split("host: ", 1)[1].splitlines()[0]

    with tempfile.TemporaryDirectory(prefix="leg-trial-measure-") as directory:
        temporary = Path(directory)
        workspace = temporary / "workspace"
        state_dir = temporary / "state"
        workspace.mkdir()
        state_dir.mkdir()
        tui_state = temporary / "tui-state"
        tui_state.mkdir()
        tui_reference_path = temporary / "tui-reference.tar.gz"
        tui_bundle_size = make_tui_reference_bundle(
            temporary, tui_reference_path, tui_bin, leg_bin, supervisor_bin, target, revision
        )

        host = None
        browser_baseline = None
        web_total = None
        web_startup_ms = None
        browser_version = "unknown"
        host_profile = temporary / "web-browser-profile"
        baseline_profile = temporary / "baseline-browser-profile"
        try:
            async with async_playwright() as playwright:
                browser_type = getattr(playwright, args.browser)
                baseline_context = await browser_type.launch_persistent_context(
                    user_data_dir=str(baseline_profile), headless=True
                )
                browser_version = baseline_context.browser.version
                baseline_page = baseline_context.pages[0]
                await baseline_page.goto("about:blank")
                baseline_roots = tree_roots_for_profile(baseline_profile)
                browser_baseline = sample_process_set(baseline_roots)
                await baseline_context.close()

                host, launch_url, host_started = start_host(
                    web_bin, leg_bin, supervisor_bin, workspace, state_dir
                )
                web_context = await browser_type.launch_persistent_context(
                    user_data_dir=str(host_profile), headless=True
                )
                web_page = web_context.pages[0]
                await web_page.goto(launch_url, wait_until="load")
                await web_page.locator("#workspace-input").wait_for(state="visible", timeout=10000)
                web_startup_ms = int((time.monotonic() - host_started) * 1000)
                browser_roots = tree_roots_for_profile(host_profile)
                web_total = sample_process_set([host.pid, *browser_roots])
                await web_context.close()
        finally:
            stop_process(host)

        tui_process, tui_master, tui_startup_ms = start_tui(
            tui_bin, leg_bin, supervisor_bin, workspace, tui_state
        )
        try:
            tui_idle = sample_process_set([tui_process.pid])
        finally:
            stop_process(tui_process)
            os.close(tui_master)

        assert browser_baseline is not None and web_total is not None and web_startup_ms is not None
        baseline_rss = int(browser_baseline["median_rss_bytes"])
        combined_rss = int(web_total["median_rss_bytes"])
        return {
            "schema": "leg-ui-trial.resource-measurements/v1",
            "source_revision": revision,
            "rust_target": target,
            "environment": environment_info(args.browser, browser_version),
            "method": {
                "startup_ready_event": "first workspace-selection control visible",
                "rss_tool": "companions/trials/sample_process_rss.py via ps process trees",
                "settle_seconds": 10,
                "sample_count": 10,
                "sample_interval_seconds": 1,
                "web_process_set": "leg-web host plus browser process tree; no submitted turn",
                "browser_baseline_process_set": "same browser engine with about:blank and no host",
                "tui_process_set": "leg-tui process tree at workspace chooser",
                "bundle_size_method": "normalized .tar.gz archive bytes; same shared trial materials",
            },
            "TUI": {
                "startup_time_ms": tui_startup_ms,
                "idle_rss_bytes": tui_idle["median_rss_bytes"],
                "rss_samples": tui_idle,
                "bundle_size_bytes": tui_bundle_size,
                "bundle_size_kind": "reference bundle assembled from same revision/common fixture",
            },
            "Web": {
                "startup_time_ms": web_startup_ms,
                "idle_rss_bytes": combined_rss - baseline_rss,
                "host_plus_browser_total_rss_bytes": combined_rss,
                "browser_baseline_rss_bytes": baseline_rss,
                "rss_samples": {
                    "host_plus_browser": web_total,
                    "browser_baseline": browser_baseline,
                },
                "bundle_size_bytes": web_archive.stat().st_size,
                "bundle_archive": web_archive.name,
            },
        }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--web-archive", type=Path, required=True)
    parser.add_argument("--web-bin", type=Path, required=True)
    parser.add_argument("--leg-bin", type=Path, required=True)
    parser.add_argument("--supervisor-bin", type=Path, required=True)
    parser.add_argument("--tui-bin", type=Path, required=True)
    parser.add_argument("--browser", choices=("chromium", "firefox"), default="chromium")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    report = asyncio.run(measure(args))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"resource_report={args.output}")
    print(json.dumps({"TUI": report["TUI"], "Web": report["Web"]}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
