#!/usr/bin/env bash
# Focused tests for scripts/release.sh.

set -euo pipefail
export BASH_ENV=/dev/null

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=../scripts/release.sh
source "${ROOT}/scripts/release.sh"

fail() {
    printf 'FAIL: %s\n' "${1}" >&2
    return 1
}

assert_eq() {
    local expected="${1}" actual="${2}" message="${3:-values differ}"
    [[ "${expected}" == "${actual}" ]] || \
        fail "${message}: expected '${expected}', got '${actual}'"
}

assert_rc_nonzero() {
    local status="${1}"
    (( status != 0 )) || fail "expected a non-zero status"
}

make_fixture() {
    local repo="${1}" version="${2:-0.1.0}"

    printf '%s\n' \
        '[package]' \
        'name = "leg"' \
        "version = \"${version}\"" \
        >"${repo}/Cargo.toml"
    printf '%s\n' \
        'version = 4' \
        '' \
        '[[package]]' \
        'name = "leg"' \
        "version = \"${version}\"" \
        'dependencies = []' \
        >"${repo}/Cargo.lock"
}

make_npm_archive_fixture() {
    local repo="${1}" version="${2}" package_key target _npm_os _npm_cpu archive binary
    local archive_dir staging archive_path source_windows archive_windows

    archive_dir="${repo}/dist"
    mkdir -p "${archive_dir}"
    while IFS='|' read -r package_key target _npm_os _npm_cpu archive binary; do
        staging="${repo}/staging-${package_key}"
        mkdir -p "${staging}"
        if [[ "${binary}" == 'leg' ]]; then
            printf '#!/bin/sh\nprintf "leg %s\\n"\n' "${version}" >"${staging}/${binary}"
            chmod +x "${staging}/${binary}"
        else
            printf 'fake windows leg %s\n' "${version}" >"${staging}/${binary}"
        fi
        archive_path="${archive_dir}/leg-${version}-${target}.${archive}"
        case "${archive}" in
            tar.gz)
                tar -C "${staging}" -czf "${archive_path}" "${binary}"
                ;;
            zip)
                if command -v zip >/dev/null 2>&1; then
                    (cd "${staging}" && zip -q "${archive_path}" "${binary}")
                elif command -v powershell.exe >/dev/null 2>&1 && command -v cygpath >/dev/null 2>&1; then
                    source_windows="$(cygpath -w "${staging}/${binary}")"
                    archive_windows="$(cygpath -w "${archive_path}")"
                    powershell.exe -NoProfile -NonInteractive -Command \
                        "Compress-Archive -LiteralPath '${source_windows}' -DestinationPath '${archive_windows}' -Force"
                else
                    printf 'npm archive fixture requires zip or PowerShell Compress-Archive\n' >&2
                    return 1
                fi
                ;;
        esac
        rm -rf "${staging}"
    done < <(release_npm_platform_rows)
}

link_npm_shim_node_modules() {
    local repo="${1}" package_key

    mkdir -p "${repo}/npm-packages/leg/node_modules/@shukelabs"
    for package_key in linux-x64 linux-arm64 linux-arm darwin-x64 darwin-arm64 win32-x64; do
        mkdir -p "${repo}/npm-packages/leg/node_modules/@shukelabs/leg-${package_key}/bin"
        cp "${repo}/npm-packages/leg-${package_key}/package.json" \
            "${repo}/npm-packages/leg/node_modules/@shukelabs/leg-${package_key}/package.json"
        cp "${repo}/npm-packages/leg-${package_key}/bin/"* \
            "${repo}/npm-packages/leg/node_modules/@shukelabs/leg-${package_key}/bin/"
    done
}

test_manifest_and_lockfile_version_reads() (
    set -euo pipefail
    local repo
    repo="$(mktemp -d)"
    trap 'rm -rf "${repo}"' EXIT
    make_fixture "${repo}" "0.3.7"

    assert_eq "0.3.7" "$(release_manifest_version "${repo}/Cargo.toml")" "manifest version read"
    assert_eq "0.3.7" "$(release_lockfile_version "${repo}/Cargo.lock")" "lockfile version read"
)

test_verify_tag_matches_manifest() (
    set -euo pipefail
    local repo status
    repo="$(mktemp -d)"
    trap 'rm -rf "${repo}"' EXIT
    make_fixture "${repo}" "0.2.0"

    release_verify_tag_matches_manifest "v0.2.0" "${repo}/Cargo.toml" "${repo}/Cargo.lock"

    status=0
    release_verify_tag_matches_manifest "v0.2.1" "${repo}/Cargo.toml" "${repo}/Cargo.lock" \
        >/dev/null 2>&1 || status="$?"
    assert_rc_nonzero "${status}"

    status=0
    release_verify_tag_matches_manifest "not-a-tag" "${repo}/Cargo.toml" "${repo}/Cargo.lock" \
        >/dev/null 2>&1 || status="$?"
    assert_rc_nonzero "${status}"
)

test_npm_platform_matrix_and_staging() (
    set -euo pipefail
    local repo version expected output status host_platform resolved
    repo="$(mktemp -d)"
    trap 'rm -rf "${repo}"' EXIT
    version="0.4.25"

    expected="leg
leg-linux-x64
leg-linux-arm64
leg-linux-arm
leg-darwin-x64
leg-darwin-arm64
leg-win32-x64"
    assert_eq "${expected}" "$(release_npm_package_directories)" \
        "npm package directory matrix"

    make_npm_archive_fixture "${repo}" "${version}"
    release_npm_stage_packages "${version}" "${repo}/dist" "${repo}/npm-packages"
    release_npm_validate_package_set "${version}" "${repo}/npm-packages"

    assert_eq '@shukelabs/leg' \
        "$(node -e 'console.log(require(process.argv[1]).name)' "${repo}/npm-packages/leg/package.json")" \
        "root npm package name"
    assert_eq "${version}" \
        "$(node -e 'console.log(require(process.argv[1]).version)' "${repo}/npm-packages/leg-linux-arm/package.json")" \
        "linux-arm (armv7 musl) platform npm package version"
    assert_eq 'arm' \
        "$(node -e 'console.log(require(process.argv[1]).cpu[0])' "${repo}/npm-packages/leg-linux-arm/package.json")" \
        "linux-arm platform npm package cpu"

    link_npm_shim_node_modules "${repo}"
    host_platform="$(node -p 'process.platform')"
    if [[ "${host_platform}" == 'win32' ]]; then
        # The fixture's leg.exe is intentionally not a PE binary. On Windows
        # verify the shim's real resolver without trying to execute the text
        # placeholder; package staging above still covers the win32-x64 row.
        resolved="$(node - "${repo}/npm-packages/leg/bin/leg.js" <<'NODE'
const path = require('path');
const { resolvePlatformBinary } = require(process.argv[2]);
const result = resolvePlatformBinary('win32', 'x64');
console.log(result.packageName);
console.log(path.basename(result.binaryPath));
NODE
)"
        assert_eq "@shukelabs/leg-win32-x64
leg.exe" "${resolved}" "Windows npm shim resolves win32-x64 binary"
    else
        output="$(node "${repo}/npm-packages/leg/bin/leg.js" --version)"
        assert_eq "leg ${version}" "${output}" "npm shim forwards to native binary"

        resolved="$(node - "${repo}/npm-packages/leg/bin/leg.js" <<'NODE'
const { resolvePlatformBinary } = require(process.argv[2]);
const result = resolvePlatformBinary('linux', 'arm');
console.log(result.packageName);
NODE
)"
        assert_eq "@shukelabs/leg-linux-arm" "${resolved}" \
            "linux/arm (armv7 musl) shim resolution"
    fi

    resolved="$(node - "${repo}/npm-packages/leg/bin/leg.js" <<'NODE'
const { resolvePlatformBinary } = require(process.argv[2]);
const unsupported = [['freebsd', 'x64'], ['linux', 'ppc64']];
for (const [platform, architecture] of unsupported) {
  const expected = `platform not supported (${platform}/${architecture})`;
  try {
    resolvePlatformBinary(platform, architecture);
    process.exit(1);
  } catch (error) {
    if (error.message !== expected) process.exit(1);
    console.log(error.message);
  }
}
NODE
)"
    assert_eq "platform not supported (freebsd/x64)
platform not supported (linux/ppc64)" "${resolved}" \
        "unsupported npm platforms fail clearly"

    printf '%s\n' '{"name":"@shukelabs/leg-linux-x64","version":"0.0.1"}' \
        >"${repo}/npm-packages/leg-linux-x64/package.json"
    status=0
    release_npm_validate_package_set "${version}" "${repo}/npm-packages" >/dev/null 2>&1 || status="$?"
    assert_rc_nonzero "${status}"
)

test_npm_pack_checksums() (
    set -euo pipefail
    local repo version package_dir
    repo="$(mktemp -d)"
    trap 'rm -rf "${repo}"' EXIT
    version="0.4.25"
    make_npm_archive_fixture "${repo}" "${version}"
    release_npm_stage_packages "${version}" "${repo}/dist" "${repo}/npm-packages"

    mkdir -p "${repo}/npm-tarballs"
    while read -r package_dir; do
        (cd "${repo}/npm-packages/${package_dir}" && \
            npm pack --ignore-scripts --pack-destination "${repo}/npm-tarballs" >/dev/null)
    done < <(release_npm_package_directories)
    release_npm_write_checksums "${repo}/npm-tarballs" "${repo}/npm-SHA256SUMS"
    (cd "${repo}/npm-tarballs" && release_sha256_check ../npm-SHA256SUMS)
    assert_eq "7" "$(find "${repo}/npm-tarballs" -maxdepth 1 -type f -name '*.tgz' | wc -l | tr -d ' ')" \
        "one npm tarball per package"
)

tests=(
    test_manifest_and_lockfile_version_reads
    test_verify_tag_matches_manifest
    test_npm_platform_matrix_and_staging
    test_npm_pack_checksums
)

for test_name in "${tests[@]}"; do
    "${test_name}"
    printf 'ok - %s\n' "${test_name}"
done
printf 'release tests: %s passed\n' "${#tests[@]}"
