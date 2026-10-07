#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
build_root="$(mktemp -d "${TMPDIR:-/tmp}/leg-web-theme-e2e.XXXXXX")"
trap 'rm -rf "$build_root"' EXIT

cargo build --locked --manifest-path "$repo_root/Cargo.toml" --bin leg
cargo build --locked --manifest-path "$repo_root/companions/Cargo.toml" -p leg-ui-client --bin leg-ui-supervisor
cargo build --locked --manifest-path "$repo_root/companions/Cargo.toml" -p leg-web
cp "$repo_root/companions/target/debug/leg-web" "$build_root/leg-web-default"
cargo build --locked --manifest-path "$repo_root/companions/Cargo.toml" -p leg-web --features browser-e2e-themes
cp "$repo_root/companions/target/debug/leg-web" "$build_root/leg-web-themes"

for browser in chromium firefox; do
  python3 "$repo_root/companions/leg-web/tests/browser_e2e.py" \
    --browser "$browser" \
    "$build_root/leg-web-default" \
    "$repo_root/target/debug/leg" \
    "$repo_root/companions/target/debug/leg-ui-supervisor"
  python3 "$repo_root/companions/leg-web/tests/theme_browser_e2e.py" \
    --browser "$browser" \
    "$build_root/leg-web-themes" \
    "$repo_root/target/debug/leg" \
    "$repo_root/companions/target/debug/leg-ui-supervisor"
done
