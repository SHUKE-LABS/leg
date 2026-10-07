#!/usr/bin/env python3
"""Smoke-test an unpacked Web trial bundle without Cargo or Node on PATH."""

from __future__ import annotations

import argparse
import http.client
import json
import os
import queue
import re
import signal
import shutil
import subprocess
import tempfile
import threading
import time
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import urlsplit


class PageAssets(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.paths: list[str] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        values = dict(attrs)
        if tag == "link" and values.get("rel") == "stylesheet" and values.get("href"):
            self.paths.append(str(values["href"]))
        elif tag == "script" and values.get("src"):
            self.paths.append(str(values["src"]))


def request(
    authority: str, path: str, token: str | None = None
) -> tuple[int, dict[str, str], bytes]:
    headers = {"Host": authority}
    if path.startswith("/api/"):
        headers["Origin"] = f"http://{authority}"
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    connection = http.client.HTTPConnection(authority, timeout=5)
    connection.request("GET", path, headers=headers)
    response = connection.getresponse()
    body = response.read()
    result = response.status, dict(response.getheaders()), body
    connection.close()
    return result


def wait_for_launch_url(process: subprocess.Popen[str], timeout: float = 20) -> str:
    if process.stdout is None:
        raise RuntimeError("bundle launcher has no stdout pipe")
    lines: queue.Queue[str | None] = queue.Queue()

    def read_stdout() -> None:
        assert process.stdout is not None
        for line in process.stdout:
            lines.put(line.rstrip("\r\n"))
        lines.put(None)

    threading.Thread(target=read_stdout, daemon=True).start()
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            line = lines.get(timeout=max(0.1, deadline - time.monotonic()))
        except queue.Empty:
            break
        if line is None:
            raise RuntimeError("bundle launcher exited before printing its launch URL")
        prefix = "Open this one-time launch URL:"
        if line.startswith(prefix):
            return line.split(": ", 1)[1]
    raise TimeoutError("bundle launcher did not print its one-time launch URL within 20 seconds")


def stop_process(process: subprocess.Popen[str] | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.send_signal(signal.SIGINT)
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def smoke_bundle(bundle_dir: Path) -> None:
    bundle_dir = bundle_dir.resolve()
    launcher = bundle_dir / "start-web.sh"
    if not launcher.is_file() or not os.access(launcher, os.X_OK):
        raise AssertionError("unpacked bundle is missing an executable start-web.sh")
    for name in ("leg", "leg-ui-supervisor", "leg-web"):
        binary = bundle_dir / "bin" / name
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise AssertionError(f"unpacked bundle is missing executable bin/{name}")
    if not (bundle_dir / "companions/trials/fake_provider.py").is_file():
        raise AssertionError("unpacked bundle is missing its local trial fixture")

    runtime_path = "/usr/bin:/bin:/usr/sbin:/sbin"
    for command in ("cargo", "node"):
        if shutil.which(command, path=runtime_path) is not None:
            raise AssertionError(f"runtime PATH unexpectedly includes {command}")

    with tempfile.TemporaryDirectory(prefix="leg-web-trial-bundle-smoke-") as root_string:
        root = Path(root_string)
        state_dir = root / "state"
        state_dir.mkdir()
        env = os.environ.copy()
        env["PATH"] = runtime_path
        process = subprocess.Popen(
            [str(launcher), "--no-open", "--state-dir", str(state_dir)],
            cwd=bundle_dir,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        try:
            launch_url = wait_for_launch_url(process)
            parsed = urlsplit(launch_url)
            if parsed.scheme != "http" or parsed.hostname != "127.0.0.1" or not parsed.port:
                raise AssertionError("bundle launcher printed an invalid loopback launch URL")
            if re.fullmatch(r"[0-9a-f]{64}", parsed.fragment) is None:
                raise AssertionError("bundle launcher URL is missing its one-time token fragment")

            authority = parsed.netloc
            status, headers, page = request(authority, "/")
            content_type = next(
                (value for name, value in headers.items() if name.lower() == "content-type"),
                "<missing>",
            )
            if status != 200 or "text/html" not in content_type.lower():
                raise AssertionError(
                    f"unpacked bundle HTML request returned HTTP {status}, content-type={content_type!r}"
                )
            page_text = page.decode("utf-8")
            if "theme-root" not in page_text or parsed.fragment in page_text:
                raise AssertionError("unpacked page is incomplete or exposes its launch token")

            assets = PageAssets()
            assets.feed(page_text)
            if "/app.js" not in assets.paths:
                raise AssertionError("unpacked page does not reference its embedded entry module")
            embedded_theme_assets = {
                "/controller.js",
                "/themes/registry.js",
                "/themes/default.js",
                "/themes/default.html",
                "/themes/default.css",
                "/themes/shared-renderer.js",
            }
            for asset_path in set(assets.paths) | embedded_theme_assets:
                asset_status, _asset_headers, asset_body = request(authority, asset_path)
                if asset_status != 200 or not asset_body:
                    raise AssertionError(f"unpacked page asset failed to load: {asset_path}")

            unauth_status, _unauth_headers, _unauth_body = request(authority, "/api/sessions")
            if unauth_status != 401:
                raise AssertionError("unpacked bundle accepted an unauthenticated API request")
            api_status, _api_headers, api_body = request(
                authority, "/api/sessions", parsed.fragment
            )
            if api_status != 200:
                raise AssertionError("launch-token-authenticated API request failed")
            json.loads(api_body)
        finally:
            stop_process(process)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle-dir", type=Path, required=True)
    args = parser.parse_args()
    smoke_bundle(args.bundle_dir)
    print("unpacked Web bundle smoke passed: launcher, page, assets, and authenticated API")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
