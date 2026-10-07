#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
usage: scripts/dev.sh [-h|--help]

Update this checkout when it has an upstream, then install leg and its UI companions.
EOF
}

if (($# > 0)); then
    if (($# == 1)) && [[ "$1" == "-h" || "$1" == "--help" ]]; then
        usage
        exit 0
    fi
    usage >&2
    exit 2
fi

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
if ! git -C "$repo_root" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    printf 'dev: not inside a Git worktree: %s\n' "$repo_root" >&2
    exit 1
fi

if upstream="$(git -C "$repo_root" rev-parse --abbrev-ref --symbolic-full-name "@{upstream}" 2>/dev/null)"; then
    if [[ -n "$(git -C "$repo_root" status --porcelain --untracked-files=all)" ]]; then
        printf 'dev: refusing to update dirty worktree at %s\n' "$repo_root" >&2
        exit 1
    fi

    printf 'Updating from %s...\n' "$upstream"
    if ! git -C "$repo_root" pull --ff-only; then
        printf 'dev: git pull --ff-only failed for %s; see the Git error above and resolve the issue before retrying\n' "$upstream" >&2
        exit 1
    fi
else
    printf 'No upstream configured; skipping pull and building the current checkout.\n'
fi

cargo_home="${CARGO_HOME:-$HOME/.cargo}"
cargo_bin="${cargo_home%/}/bin"
export PATH="$cargo_bin${PATH:+:$PATH}"

install_package() {
    local package="$1"
    local package_path="$2"
    local bin="$3"

    printf 'Installing %s...\n' "$package"
    if cargo install --locked --force --path "$package_path" --bin "$bin"; then
        return 0
    else
        local status=$?
        printf 'dev: cargo install failed for package %s\n' "$package" >&2
        return "$status"
    fi
}

install_package leg "$repo_root" leg
install_package "leg-ui-client (leg-ui-supervisor)" "$repo_root/companions/leg-ui-client" leg-ui-supervisor
install_package leg-tui "$repo_root/companions/leg-tui" leg-tui
install_package leg-web "$repo_root/companions/leg-web" leg-web

installed_revision="$(git -C "$repo_root" rev-parse --short HEAD)"
printf 'Installed revision: %s\n' "$installed_revision"
printf 'leg version: '
leg --version
printf 'Installed binary paths:\n'
printf '  leg: %s\n' "$cargo_bin/leg"
printf '  leg-ui-supervisor: %s\n' "$cargo_bin/leg-ui-supervisor"
printf '  leg-tui: %s\n' "$cargo_bin/leg-tui"
printf '  leg-web: %s\n' "$cargo_bin/leg-web"
