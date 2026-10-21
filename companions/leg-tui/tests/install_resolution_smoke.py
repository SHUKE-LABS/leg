#!/usr/bin/env python3
"""Native Windows smoke for `leg install` and the companion leg resolver.

The smoke covers three things on a native Windows host:

* `leg install` copies the native binaries into a per-user directory, refuses
  an incomplete bundle, and updates the current user's PATH exactly once.
* With a package-manager shim ahead of the installed directory on PATH, a
  leg-tui turn without `--leg-bin` still reaches the fixture provider, which
  only happens when the resolver stepped over the shim and started the
  installed native leg binary.
* A published npm launcher on PATH resolves through `node.exe` to the
  platform package's native binary.

The smoke needs Node.js for the launcher check.
"""

from __future__ import annotations

import argparse
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

if os.name != "nt":
    raise SystemExit("this smoke covers the native Windows installation path")

sys.path.insert(0, str(Path(__file__).resolve().parent))

import responsiveness_harness as harness  # noqa: E402

try:
    import winreg
except ImportError:  # pragma: no cover - Windows-only standard library module
    raise SystemExit("the Windows PATH check needs the winreg module")

# The binaries the install command materializes, with the version prefix
# each of them reports.
VERSIONED_BINARIES = (
    ("leg", "leg "),
    ("leg-tui", "leg-tui "),
    ("leg-web", "leg-web "),
    ("leg-ui-supervisor", "leg-ui-supervisor "),
)


def build_bundle(bundle: Path, binaries: dict[str, Path]) -> Path:
    """Places the four native binaries beside one another."""
    bundle.mkdir(parents=True, exist_ok=True)
    for name, source in binaries.items():
        shutil.copy2(source, bundle / f"{name}.exe")
    return bundle / "leg.exe"


def run_install(binary: Path, destination: Path, extra: list[str]) -> subprocess.CompletedProcess:
    """Runs `leg install` against the given destination."""
    return subprocess.run(
        [str(binary), "install", "--dest", str(destination), *extra],
        capture_output=True,
        text=True,
    )


def assert_version(binary: Path, prefix: str) -> None:
    """Asserts that an installed executable reports its version."""
    output = subprocess.run([str(binary), "--version"], capture_output=True, text=True)
    assert output.returncode == 0, f"{binary} --version failed: {output.stderr}"
    assert output.stdout.strip().startswith(prefix), (
        f"unexpected version output from {binary}: {output.stdout!r}"
    )


def write_shim(directory: Path) -> None:
    """Writes the shims a package manager leaves in front of the real binary."""
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "leg").write_text(
        "#!/bin/sh\n"
        "echo 'the package-manager shim must never run' >&2\n"
        "exit 1\n",
        encoding="utf-8",
    )
    (directory / "leg.cmd").write_text(
        "@ECHO off\r\nECHO the package-manager shim must never run 1>&2\r\nEXIT /B 1\r\n",
        encoding="utf-8",
    )


def build_npm_layout(root: Path, launcher: Path, native_leg: Path) -> Path:
    """Lays out the published npm package beside its platform package."""
    package = root / "node_modules" / "@shukelabs" / "leg"
    package.mkdir(parents=True, exist_ok=True)
    shutil.copy2(launcher, package / "leg.js")
    architecture = "x64" if platform.machine().lower() in ("amd64", "x86_64") else "arm64"
    platform_package = root / "node_modules" / "@shukelabs" / f"leg-win32-{architecture}"
    (platform_package / "bin").mkdir(parents=True, exist_ok=True)
    shutil.copy2(native_leg, platform_package / "bin" / "leg.exe")
    return package


def read_user_path() -> tuple[str, int]:
    """Reads the current user's PATH from the registry."""
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, "Environment", 0, winreg.KEY_READ) as key:
        value, value_type = winreg.QueryValueEx(key, "Path")
    return value, value_type


def write_user_path(value: str, value_type: int) -> None:
    """Writes the current user's PATH back to the registry."""
    with winreg.OpenKey(
        winreg.HKEY_CURRENT_USER, "Environment", 0, winreg.KEY_SET_VALUE
    ) as key:
        winreg.SetValueEx(key, "Path", 0, value_type, value)


def path_entries(value: str) -> list[str]:
    """Splits a PATH value into its entries."""
    return [entry.strip() for entry in value.split(";") if entry.strip()]


def check_persistent_path_update(bundle_leg: Path, workspace: Path) -> None:
    """Installs without the opt-out and checks the current user's PATH."""
    original, original_type = read_user_path()
    destination = workspace / "path-install"
    try:
        for _ in range(2):
            result = run_install(bundle_leg, destination, [])
            assert result.returncode == 0, f"leg install failed: {result.stderr}"
        updated, _ = read_user_path()
        entries = path_entries(updated)
        matches = [
            entry
            for entry in entries
            if entry.rstrip("\\/").casefold() == str(destination).rstrip("\\/").casefold()
        ]
        assert len(matches) == 1, f"the install directory appears {len(matches)} times in PATH"
        for entry in path_entries(original):
            assert entry in entries, f"the install dropped the PATH entry {entry}"
    finally:
        write_user_path(original, original_type)


def run_tui_turn(
    tui_bin: Path,
    supervisor_bin: Path,
    path_prefixes: list[Path],
    workspace: Path,
) -> None:
    """Runs a leg-tui turn without --leg-bin and checks the provider request."""
    workspace.mkdir(parents=True, exist_ok=True)
    server = harness.FixtureServer(workspace, scenario="trial")
    terminal = None
    try:
        state_dir = workspace / "state"
        environment = harness._build_harness_environment(
            os.environ.copy(), server.environment, state_dir, supervisor_bin
        )
        search_path = os.pathsep.join(
            [str(prefix) for prefix in path_prefixes] + [environment.get("PATH", "")]
        )
        environment["PATH"] = search_path
        environment["TERM"] = "xterm-256color"
        terminal = harness.TerminalProcess(
            [str(tui_bin)],
            workspace,
            environment,
            rows=24,
            columns=80,
        )
        terminal.wait_contains("Path:")
        terminal.write(str(workspace) + "\r")
        terminal.wait_contains("Leg can run shell commands and modify files")
        terminal.write("\r")
        terminal.wait_contains(harness.COMPOSER_HINT)
        terminal.write("install resolution smoke")
        terminal.wait_contains("install resolution smoke")
        terminal.write_control("s")

        deadline = time.monotonic() + 30.0
        status: dict = {}
        while time.monotonic() < deadline:
            status = server.fixture.status()
            if status.get("requests", 0) > 0:
                break
            time.sleep(0.1)
        assert status.get("requests", 0) == 1, (
            f"the turn did not reach the fixture provider: {status}"
        )
        terminal.wait_contains("Succeeded")
    finally:
        if terminal is not None:
            terminal.close()
        server.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--leg-bin", required=True)
    parser.add_argument("--tui-bin", required=True)
    parser.add_argument("--web-bin", required=True)
    parser.add_argument("--supervisor-bin", required=True)
    args = parser.parse_args()

    leg_bin = Path(args.leg_bin).resolve()
    tui_bin = Path(args.tui_bin).resolve()
    web_bin = Path(args.web_bin).resolve()
    supervisor_bin = Path(args.supervisor_bin).resolve()
    binaries = {
        "leg": leg_bin,
        "leg-tui": tui_bin,
        "leg-web": web_bin,
        "leg-ui-supervisor": supervisor_bin,
    }
    node = shutil.which("node.exe") or shutil.which("node")
    assert node is not None, "the published launcher check needs Node.js on PATH"

    with tempfile.TemporaryDirectory(prefix="leg-install-smoke-") as temporary:
        workspace = Path(temporary)
        bundle_leg = build_bundle(workspace / "bundle", binaries)
        install_dir = workspace / "install"

        # A destination override with the PATH opt-out installs the native
        # files into a temporary directory without touching PATH.
        result = run_install(bundle_leg, install_dir, ["--no-path"])
        assert result.returncode == 0, f"leg install failed: {result.stderr}"
        for name in binaries:
            installed = install_dir / f"{name}.exe"
            assert installed.is_file(), f"{installed} was not installed"
        for name, prefix in VERSIONED_BINARIES:
            assert_version(install_dir / f"{name}.exe", prefix)

        # Running the command again updates the files in place.
        result = run_install(bundle_leg, install_dir, ["--no-path"])
        assert result.returncode == 0, f"the second leg install failed: {result.stderr}"
        installed_names = sorted(entry.name for entry in install_dir.iterdir())
        assert installed_names == sorted(f"{name}.exe" for name in binaries), (
            f"the repeated install left extra files behind: {installed_names}"
        )
        for name, prefix in VERSIONED_BINARIES:
            assert_version(install_dir / f"{name}.exe", prefix)

        # A missing companion fails the install before anything is written.
        incomplete = workspace / "incomplete"
        incomplete.mkdir()
        shutil.copy2(leg_bin, incomplete / "leg.exe")
        result = run_install(incomplete / "leg.exe", workspace / "never", ["--no-path"])
        assert result.returncode != 0, "a missing companion must fail the install"
        assert "leg-tui" in result.stderr, (
            f"the failure did not name the companion: {result.stderr}"
        )
        assert not (workspace / "never").exists(), "a failed install created the destination"

        check_persistent_path_update(bundle_leg, workspace)

        # A package-manager shim comes first on PATH: the resolver has to step
        # over it and start the installed native binary.
        shim_dir = workspace / "shim"
        write_shim(shim_dir)
        run_tui_turn(
            tui_bin,
            supervisor_bin,
            [shim_dir, install_dir],
            workspace / "shim-workspace",
        )

        # The published npm launcher resolves through node.exe to the platform
        # package's native binary.
        package_dir = build_npm_layout(
            workspace / "npm", harness.ROOT / "packaging" / "npm" / "leg.js", leg_bin
        )
        run_tui_turn(
            tui_bin,
            supervisor_bin,
            [package_dir],
            workspace / "launcher-workspace",
        )

    print("install resolution smoke passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
