#!/usr/bin/env python3
"""Behavior tests for scripts/dev.sh using local Git and fake Cargo."""

import os
import shlex
import shutil
import subprocess
import tempfile
import time
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
DEV_SCRIPT = REPO_ROOT / "scripts" / "dev.sh"


def package_version(manifest):
    in_package = False
    for line in manifest.read_text(encoding="utf-8").splitlines():
        if line.strip() == "[package]":
            in_package = True
            continue
        if in_package and line.lstrip().startswith("["):
            break
        if in_package and line.strip().startswith("version ="):
            value = line.partition("=")[2].strip()
            if value.startswith('"') and value.endswith('"'):
                return value[1:-1]
    raise AssertionError(f"no literal package version found in {manifest}")


FAKE_CARGO = r"""#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$CARGO_LOG"
[[ "$*" == *"--locked"* && "$*" == *"--force"* && "$*" == *"--path"* && "$*" == *"--bin"* ]] || {
    printf 'missing required install flags\n' >&2
    exit 90
}
bin_name=""
while (($#)); do
    if [[ "$1" == "--bin" ]]; then
        bin_name="$2"
        shift 2
    else
        shift
    fi
done
sleep 0.02
if [[ "$FAIL_BIN" == "$bin_name" ]]; then
    printf 'fake cargo failure for %s\n' "$bin_name" >&2
    exit 23
fi
mkdir -p "$CARGO_HOME/bin"
if [[ "$bin_name" == "leg" ]]; then
    cat > "$CARGO_HOME/bin/leg" <<'BIN'
#!/usr/bin/env bash
printf 'leg %s\n' "$LEG_VERSION"
BIN
else
    cat > "$CARGO_HOME/bin/$bin_name" <<'BIN'
#!/usr/bin/env bash
exit 0
BIN
fi
chmod +x "$CARGO_HOME/bin/$bin_name"
"""


class DevScriptTests(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory(prefix="leg-dev-script-test-")
        self.addCleanup(self.temp_dir.cleanup)
        self.root = Path(self.temp_dir.name)
        self.remote = self.root / "remote.git"
        self.seed = self.root / "seed"
        self.checkout = self.root / "checkout"
        self.subdir = self.checkout / "subdir"
        self.fake_bin = self.root / "fake-bin"
        self.cargo_home = self.root / "cargo-home"
        self.cargo_log = self.root / "cargo.log"
        self.version = package_version(REPO_ROOT / "Cargo.toml")

        self.git(None, "init", "--bare", "--initial-branch=main", str(self.remote))
        self.seed.mkdir()
        self.git(None, "init", "--initial-branch=main", str(self.seed))
        self.git(self.seed, "config", "user.name", "Dev Script Test")
        self.git(self.seed, "config", "user.email", "dev-script-test@example.invalid")

        (self.seed / "scripts").mkdir()
        shutil.copy2(DEV_SCRIPT, self.seed / "scripts" / "dev.sh")
        (self.seed / "Cargo.toml").write_text(
            f'[package]\nname = "leg"\nversion = "{self.version}"\n',
            encoding="utf-8",
        )
        (self.seed / "README.md").write_text("fixture\n", encoding="utf-8")
        for package in ("leg-ui-client", "leg-tui", "leg-web"):
            package_dir = self.seed / "companions" / package
            package_dir.mkdir(parents=True)
            (package_dir / "Cargo.toml").write_text(
                f'[package]\nname = "{package}"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )

        self.git(self.seed, "add", ".")
        self.git(self.seed, "commit", "-m", "initial fixture")
        self.git(self.seed, "remote", "add", "origin", str(self.remote))
        self.git(self.seed, "push", "-u", "origin", "main")
        self.git(None, "clone", "--quiet", str(self.remote), str(self.checkout))
        self.git(self.checkout, "config", "user.name", "Dev Script Test")
        self.git(self.checkout, "config", "user.email", "dev-script-test@example.invalid")
        self.subdir.mkdir()

        self.fake_bin.mkdir()
        cargo = self.fake_bin / "cargo"
        cargo.write_text(FAKE_CARGO, encoding="utf-8")
        cargo.chmod(0o755)

    def git(self, cwd, *args):
        result = subprocess.run(
            ["git", *args],
            cwd=cwd,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            result.returncode,
            0,
            f"git {' '.join(args)} failed:\n{result.stdout}{result.stderr}",
        )
        return result.stdout.strip()

    def run_dev(self, fail_bin=""):
        env = os.environ.copy()
        env["PATH"] = os.pathsep.join(
            (str(self.fake_bin), env.get("PATH", ""))
        )
        env["CARGO_HOME"] = str(self.cargo_home)
        env["CARGO_LOG"] = str(self.cargo_log)
        env["FAIL_BIN"] = fail_bin
        env["LEG_VERSION"] = self.version
        return subprocess.run(
            ["bash", "../scripts/dev.sh"],
            cwd=self.subdir,
            env=env,
            check=False,
            capture_output=True,
            text=True,
        )

    def output(self, result):
        return result.stdout + result.stderr

    def test_help_prints_usage_and_other_options_are_rejected(self):
        help_result = subprocess.run(
            ["bash", "../scripts/dev.sh", "--help"],
            cwd=self.subdir,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(help_result.returncode, 0, self.output(help_result))
        self.assertIn("usage: scripts/dev.sh", help_result.stdout)

        invalid_result = subprocess.run(
            ["bash", "../scripts/dev.sh", "--unknown"],
            cwd=self.subdir,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(invalid_result.returncode, 2, self.output(invalid_result))
        self.assertIn("usage: scripts/dev.sh", invalid_result.stderr)
        self.assertFalse(self.cargo_log.exists())

    def cargo_calls(self):
        if not self.cargo_log.exists():
            return []
        return [shlex.split(line) for line in self.cargo_log.read_text().splitlines()]

    def assert_install_calls(self, expected_bins):
        calls = self.cargo_calls()
        self.assertEqual(
            [call[call.index("--bin") + 1] for call in calls],
            list(expected_bins),
        )
        for call in calls:
            self.assertEqual(call[0], "install")
            for flag in ("--locked", "--force", "--path", "--bin"):
                self.assertIn(flag, call)
        expected_paths = {
            "leg": self.checkout,
            "leg-ui-supervisor": self.checkout / "companions" / "leg-ui-client",
            "leg-tui": self.checkout / "companions" / "leg-tui",
            "leg-web": self.checkout / "companions" / "leg-web",
        }
        self.assertEqual(
            [Path(call[call.index("--path") + 1]) for call in calls],
            [expected_paths[binary] for binary in expected_bins],
        )

    def advance_remote(self):
        with (self.seed / "README.md").open("a", encoding="utf-8") as readme:
            readme.write("remote advance\n")
        self.git(self.seed, "add", "README.md")
        self.git(self.seed, "commit", "-m", "advance remote")
        self.git(self.seed, "push", "origin", "main")

    def test_fast_forwards_from_subdirectory_and_installs_all_binaries(self):
        self.advance_remote()
        run_started_ns = time.time_ns()

        result = self.run_dev()
        output = self.output(result)

        self.assertEqual(result.returncode, 0, output)
        self.assertEqual(
            self.git(self.checkout, "rev-parse", "HEAD"),
            self.git(self.seed, "rev-parse", "main"),
        )
        revision = self.git(self.checkout, "rev-parse", "--short", "HEAD")
        self.assertIn(f"Installed revision: {revision}", output)
        self.assertIn(f"leg version: leg {self.version}", output)
        for binary in ("leg", "leg-ui-supervisor", "leg-tui", "leg-web"):
            binary_path = self.cargo_home / "bin" / binary
            self.assertTrue(binary_path.is_file(), f"missing {binary_path}")
            self.assertGreater(binary_path.stat().st_mtime_ns, run_started_ns)
            self.assertIn(str(binary_path), output)
        self.assert_install_calls(
            ("leg", "leg-ui-supervisor", "leg-tui", "leg-web")
        )

    def test_dirty_upstream_worktree_is_untouched_and_not_installed(self):
        marker = self.checkout / "dirty-marker"
        marker.write_text("preserve me\n", encoding="utf-8")
        readme = self.checkout / "README.md"
        readme_contents = readme.read_text(encoding="utf-8") + "local edit\n"
        readme.write_text(readme_contents, encoding="utf-8")
        head_before = self.git(self.checkout, "rev-parse", "HEAD")

        result = self.run_dev()
        output = self.output(result)

        self.assertNotEqual(result.returncode, 0, output)
        self.assertIn("refusing to update dirty worktree", output)
        self.assertEqual(self.git(self.checkout, "rev-parse", "HEAD"), head_before)
        self.assertEqual(marker.read_text(encoding="utf-8"), "preserve me\n")
        self.assertEqual(readme.read_text(encoding="utf-8"), readme_contents)
        self.assertEqual(self.cargo_calls(), [])

    def test_branch_without_upstream_skips_pull_and_installs(self):
        self.git(self.checkout, "branch", "--unset-upstream")

        result = self.run_dev()
        output = self.output(result)

        self.assertEqual(result.returncode, 0, output)
        self.assertIn("No upstream configured; skipping pull", output)
        self.assertNotIn("Updating from", output)
        self.assert_install_calls(
            ("leg", "leg-ui-supervisor", "leg-tui", "leg-web")
        )

    def test_failed_install_names_package_and_stops(self):
        self.git(self.checkout, "branch", "--unset-upstream")

        result = self.run_dev(fail_bin="leg-tui")
        output = self.output(result)

        self.assertNotEqual(result.returncode, 0, output)
        self.assertIn("cargo install failed for package leg-tui", output)
        self.assert_install_calls(("leg", "leg-ui-supervisor", "leg-tui"))

    def test_non_fast_forward_upstream_is_rejected(self):
        with (self.checkout / "README.md").open("a", encoding="utf-8") as readme:
            readme.write("local divergence\n")
        self.git(self.checkout, "add", "README.md")
        self.git(self.checkout, "commit", "-m", "local divergence")
        self.advance_remote()
        head_before = self.git(self.checkout, "rev-parse", "HEAD")

        result = self.run_dev()
        output = self.output(result)

        self.assertNotEqual(result.returncode, 0, output)
        self.assertIn("git pull --ff-only failed", output)
        self.assertEqual(self.git(self.checkout, "rev-parse", "HEAD"), head_before)
        self.assertEqual(self.cargo_calls(), [])


if __name__ == "__main__":
    unittest.main()
