#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
usage: scripts/build-web-trial.sh --output <new-directory>

Builds the experimental self-contained Leg Web bundle for the current host.
EOF
}

output_dir=""
while (($#)); do
    case "$1" in
        --output)
            (($# >= 2)) || { usage; exit 2; }
            output_dir="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage
            exit 2
            ;;
    esac
done

[[ -n "${output_dir}" ]] || { usage; exit 2; }
if [[ "${output_dir}" != /* ]]; then
    output_dir="$(pwd)/${output_dir}"
fi
[[ ! -e "${output_dir}" ]] || {
    printf "build-web-trial: output path already exists: %s\n" "${output_dir}" >&2
    exit 1
}

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
rust_target="$(rustc -vV | sed -n 's/^host: //p')"
[[ -n "${rust_target}" ]] || {
    printf 'build-web-trial: rustc did not report a host target\n' >&2
    exit 1
}
case "$(uname -s)" in
    Linux) platform="linux" ;;
    Darwin) platform="macos" ;;
    *)
        printf 'build-web-trial: unsupported host OS %s\n' "$(uname -s)" >&2
        exit 1
        ;;
esac
case "$(uname -m)" in
    x86_64|amd64) architecture="x86_64" ;;
    aarch64|arm64) architecture="aarch64" ;;
    *)
        printf 'build-web-trial: unsupported host architecture %s\n' "$(uname -m)" >&2
        exit 1
        ;;
esac

revision="$(git -C "${repo_root}" rev-parse HEAD)"
short_revision="${revision:0:12}"
worktree_dirty=false
if [[ -n "$(git -C "${repo_root}" status --porcelain=v1)" ]]; then
    worktree_dirty=true
fi
temporary="$(mktemp -d "${TMPDIR:-/tmp}/leg-web-trial.XXXXXX")"
cleanup() { rm -rf -- "${temporary}"; }
trap cleanup EXIT
bundle_name="leg-web-experimental-${platform}-${architecture}-${short_revision}"
bundle="${temporary}/${bundle_name}"
mkdir -p "${bundle}/bin" "${bundle}/docs" "${bundle}/companions/trials"

cd "${repo_root}"
cargo build --locked --release --bin leg
cargo build --locked --release --manifest-path companions/Cargo.toml \
    -p leg-ui-client --bin leg-ui-supervisor -p leg-web --bin leg-web

cp -- target/release/leg "${bundle}/bin/leg"
cp -- companions/target/release/leg-ui-supervisor "${bundle}/bin/leg-ui-supervisor"
cp -- companions/target/release/leg-web "${bundle}/bin/leg-web"
cp -- LICENSE "${bundle}/LICENSE"
cp -- companions/leg-web/trial/start-web.sh "${bundle}/start-web.sh"
chmod +x "${bundle}/start-web.sh"
cp -- companions/leg-web/trial/QUICKSTART.md "${bundle}/QUICKSTART.md"
cp -- docs/ui-experiments.md "${bundle}/docs/ui-experiments.md"
cp -- companions/trials/fake_provider.py "${bundle}/companions/trials/fake_provider.py"
cp -- companions/trials/results-template.json "${bundle}/companions/trials/results-template.json"
cp -- companions/trials/score.py "${bundle}/companions/trials/score.py"
cp -- companions/trials/sample_process_rss.py "${bundle}/companions/trials/sample_process_rss.py"

bash scripts/release.sh web-trial-notices "${rust_target}" \
    >"${bundle}/THIRD_PARTY_NOTICES.txt"
core_version="$("${bundle}/bin/leg" --version | sed 's/^leg //')"
ui_version="$(cargo metadata --locked --no-deps --format-version 1 \
    --manifest-path companions/Cargo.toml \
    | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "leg-web"))')"
core_lock_sha256="$(sha256sum Cargo.lock 2>/dev/null | cut -d ' ' -f 1 || shasum -a 256 Cargo.lock | cut -d ' ' -f 1)"
companion_lock_sha256="$(sha256sum companions/Cargo.lock 2>/dev/null | cut -d ' ' -f 1 || shasum -a 256 companions/Cargo.lock | cut -d ' ' -f 1)"
cat >"${bundle}/bundle-info.json" <<EOF
{
  "schema": "leg-web-trial.bundle/v1",
  "experimental": true,
  "platform": "${platform}",
  "architecture": "${architecture}",
  "rust_target": "${rust_target}",
  "core_revision": "${revision}",
  "ui_revision": "${revision}",
  "core_version": "${core_version}",
  "web_version": "${ui_version}",
  "worktree_dirty": ${worktree_dirty},
  "root_lock_sha256": "${core_lock_sha256}",
  "companion_lock_sha256": "${companion_lock_sha256}"
}
EOF

mkdir -p -- "${output_dir}"
archive="${output_dir}/${bundle_name}.tar.gz"
python3 companions/trials/make_trial_archive.py "${bundle}" "${archive}" "${bundle_name}"
if command -v sha256sum >/dev/null 2>&1; then
    (cd "${output_dir}" && sha256sum "${bundle_name}.tar.gz" >"${bundle_name}.tar.gz.sha256")
else
    (cd "${output_dir}" && shasum -a 256 "${bundle_name}.tar.gz" >"${bundle_name}.tar.gz.sha256")
fi
printf 'Web trial bundle: %s\n' "${archive}"
printf 'Bundle metadata: %s/bundle-info.json (inside the archive)\n' "${bundle_name}"
