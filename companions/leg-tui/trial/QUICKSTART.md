# Leg TUI trial bundle

This local terminal interface is experimental. It is a separate trial tool,
not part of the regular `leg` installation or npm package. Do not use a
personal workspace or real provider key for the fixture trial. Tools run as
your OS user; the selected workspace is a working directory, not a sandbox.

## Prerequisites

- A supported Linux or macOS machine matching this bundle's architecture.
- Bash on `PATH` for fixture tasks that execute shell tools.
- A terminal that supports UTF-8 and a clipboard protocol, or the TUI's
  save-to-file copy fallback.
- Python 3 for the local deterministic provider fixture. The fixture uses the
  Python standard library; no Python package is needed at runtime.

The bundle includes `leg`, `leg-ui-supervisor`, `leg-tui`, and the dependency
notices. Running it does not need Cargo, Node, a CDN, or an Internet
connection.

## Build locally

From the repository root, run:

```sh
bash scripts/build-tui-trial.sh --output /tmp/leg-tui-trial-build
```

The command builds `leg` with the root `Cargo.lock`, then builds `leg-tui` and
the shared supervisor with `companions/Cargo.lock`, both in locked mode. Build
prerequisites are Rust 1.89 or newer, Node.js 22 for dependency notices,
Python 3, and `shasum` or `sha256sum`. The archive records the core, TUI, and
Web source revisions, target, versions, and lockfile hashes. These tools are
not needed to run an unpacked bundle.

## Start the deterministic trial

Download a `leg-tui-experimental-*` artifact from a CI run, unpack its
`.tar.gz` archive, and enter the extracted bundle directory. Make a new
disposable workspace outside your personal files. Start the local fixture:

```sh
workspace=$(mktemp -d "${TMPDIR:-/tmp}/leg-tui-trial-workspace.XXXXXX")
printf 'Disposable workspace: %s\n' "$workspace"
python3 companions/trials/fake_provider.py \
  --scenario trial \
  --workspace "$workspace" \
  --port 8765
```

The fixture prints environment settings for its loopback provider. In a second
terminal, change to the extracted bundle directory, apply the printed settings
to that shell, and launch the TUI with a disposable catalog:

```sh
bundle_dir=$PWD
LEG_UI_STATE_DIR="$PWD/.trial-state" ./start-tui.sh
```

Select the printed disposable workspace path when prompted. Follow the shared
tasks and safety gates in `docs/ui-experiments.md`. The fixture uses only its
fake-only key, reports deterministic workspace checks, and makes no paid API
request.

## Keyboard guide

Enter inserts a newline; Ctrl-S sends. F3 opens session actions at every size.
The rail stays hidden at 80 columns and appears from 105 columns; F8 toggles it
when available. The composer grows with the draft, up to four content rows.
Ctrl-Up/Down focuses the previous or next tool row without editing the draft.
F4 inspects that call; with no focused tool row, it toggles the latest turn
inspector. Esc returns from tool details to the same focused row. The inspector
docks only when the conversation keeps 60 columns and details keep 36;
otherwise it uses an overlay.

Ctrl-C stops the viewed active turn. If the viewed session is idle while other
TUI turns run, Ctrl-C opens a named Stop chooser: Escape cancels, and Enter
stops only the selected run while it remains active. When no TUI turns are
active, Ctrl-C saves drafts and exits. Use Ctrl-C in the fixture terminal to
stop the fake provider.

The native Windows build 26300 ConPTY check for `stop_owned_tool_process_tree`
passed. It drives ConPTY directly via pywinpty; Windows Terminal frontend
behavior is outside this check.

## Revisions and human observations

The bundle revision and target are recorded in `bundle-info.json`. The Linux
CI artifact also includes a deterministic validation report, resource
measurements, and the #79 results template. Human fields remain unmeasured
until actual paired observations are collected; fixture success is not a
winner recommendation.

## Remove the bundle

Stop the TUI and fixture, then remove the extracted bundle and disposable
workspace. In the TUI terminal, remove the bundle:

```sh
cd ..
rm -rf "$bundle_dir"
```

The trial catalog is inside the bundle directory, separate from the regular
`leg` catalog. Removing the bundle removes that trial catalog. In the fixture
terminal, remove the workspace created in that shell:

```sh
rm -rf "$workspace"
```
