#!/usr/bin/env bash
#
# leg release helpers. The functions are intentionally sourceable so the
# release workflow and the focused shell tests exercise the same code path.

set -euo pipefail

release_version_regex() {
    printf '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
}

release_tag_regex() {
    printf '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
}

release_validate_version() {
    local version="${1:-}"

    [[ "${version}" =~ $(release_version_regex) ]] || {
        printf "release: invalid version '%s'\n" "${version}" >&2
        return 1
    }
}

release_validate_tag() {
    local tag="${1:-}"

    [[ "${tag}" =~ $(release_tag_regex) ]] || {
        printf "release: invalid release tag '%s'\n" "${tag}" >&2
        return 1
    }
    release_validate_version "${tag#v}"
}

release_manifest_version() {
    local manifest_path="${1:-Cargo.toml}"

    [[ -f "${manifest_path}" ]] || {
        printf "release: manifest not found '%s'\n" "${manifest_path}" >&2
        return 1
    }

    awk '
        BEGIN { in_package = 0; found = 0 }
        /^\[package\][[:space:]]*$/ { in_package = 1; next }
        /^\[/ && $0 !~ /^\[package\][[:space:]]*$/ { in_package = 0 }
        in_package && /^version[[:space:]]*=[[:space:]]*"/ {
            line = $0
            sub(/^[^"]*"/, "", line)
            sub(/".*$/, "", line)
            print line
            found = 1
            exit
        }
        END { if (!found) exit 1 }
    ' "${manifest_path}"
}

release_lockfile_version() {
    local lockfile_path="${1:-Cargo.lock}"

    [[ -f "${lockfile_path}" ]] || {
        printf "release: lockfile not found '%s'\n" "${lockfile_path}" >&2
        return 1
    }

    awk '
        BEGIN { in_package = 0; is_leg = 0; found = 0 }
        /^\[\[package\]\]$/ {
            in_package = 1
            is_leg = 0
            next
        }
        in_package && /^name[[:space:]]*=[[:space:]]*"leg"[[:space:]]*$/ {
            is_leg = 1
            next
        }
        in_package && is_leg && /^version[[:space:]]*=[[:space:]]*"/ {
            line = $0
            sub(/^[^"]*"/, "", line)
            sub(/".*$/, "", line)
            print line
            found = 1
            exit
        }
        /^\[/ && $0 !~ /^\[\[package\]\]$/ {
            in_package = 0
            is_leg = 0
        }
        END { if (!found) exit 1 }
    ' "${lockfile_path}"
}

# Fail closed when the tag driving a release does not match what's actually
# checked into Cargo.toml/Cargo.lock, rather than trusting the tag alone.
release_verify_tag_matches_manifest() {
    local tag="${1:-}" manifest_path="${2:-Cargo.toml}" lockfile_path="${3:-Cargo.lock}"
    local expected_version="" manifest_version="" lockfile_version=""

    release_validate_tag "${tag}" || return 1
    expected_version="${tag#v}"
    manifest_version="$(release_manifest_version "${manifest_path}")" || return 1
    lockfile_version="$(release_lockfile_version "${lockfile_path}")" || return 1

    [[ "${manifest_version}" == "${expected_version}" ]] || {
        printf "release: tag '%s' does not match manifest version '%s'\n" \
            "${tag}" "${manifest_version}" >&2
        return 1
    }
    [[ "${lockfile_version}" == "${expected_version}" ]] || {
        printf "release: tag '%s' does not match lockfile version '%s'\n" \
            "${tag}" "${lockfile_version}" >&2
        return 1
    }
}

# Keep the npm package matrix next to the Rust target matrix. The fields are
# package directory, Rust target, npm os, npm cpu, archive type, and binary
# filename. The release workflow consumes the same rows through the staging
# and validation functions below.
release_npm_platform_rows() {
    printf '%s\n' \
        'linux-x64|x86_64-unknown-linux-gnu|linux|x64|tar.gz|leg' \
        'linux-arm64|aarch64-unknown-linux-gnu|linux|arm64|tar.gz|leg' \
        'linux-arm|armv7-unknown-linux-musleabihf|linux|arm|tar.gz|leg' \
        'darwin-x64|x86_64-apple-darwin|darwin|x64|tar.gz|leg' \
        'darwin-arm64|aarch64-apple-darwin|darwin|arm64|tar.gz|leg' \
        'win32-x64|x86_64-pc-windows-msvc|win32|x64|zip|leg.exe'
}

release_npm_package_directories() {
    printf 'leg\n'
    while IFS='|' read -r package_key _target _os _cpu _archive _binary; do
        printf 'leg-%s\n' "${package_key}"
    done < <(release_npm_platform_rows)
}

release_npm_shim_path() {
    local release_script_path="${BASH_SOURCE[0]}"

    printf '%s/../packaging/npm/leg.js\n' \
        "$(cd -- "$(dirname -- "${release_script_path}")" && pwd)"
}

release_npm_write_root_manifest() {
    local version="${1:-}"

    release_validate_version "${version}" || return 1
    cat <<EOF
{
  "name": "@shukelabs/leg",
  "version": "${version}",
  "description": "Agent-friendly headless agent (ask/session) CLI.",
  "license": "UNLICENSED",
  "bin": {
    "leg": "bin/leg.js"
  },
  "files": [
    "bin"
  ],
  "os": [
    "darwin",
    "linux",
    "win32"
  ],
  "cpu": [
    "x64",
    "arm64",
    "arm"
  ],
  "publishConfig": {
    "access": "public"
  },
  "optionalDependencies": {
    "@shukelabs/leg-linux-x64": "${version}",
    "@shukelabs/leg-linux-arm64": "${version}",
    "@shukelabs/leg-linux-arm": "${version}",
    "@shukelabs/leg-darwin-x64": "${version}",
    "@shukelabs/leg-darwin-arm64": "${version}",
    "@shukelabs/leg-win32-x64": "${version}"
  }
}
EOF
}

release_npm_write_platform_manifest() {
    local version="${1:-}" package_key="${2:-}" npm_os="${3:-}" npm_cpu="${4:-}"

    release_validate_version "${version}" || return 1
    [[ -n "${package_key}" && -n "${npm_os}" && -n "${npm_cpu}" ]] || {
        printf 'release: incomplete npm platform metadata\n' >&2
        return 1
    }
    cat <<EOF
{
  "name": "@shukelabs/leg-${package_key}",
  "version": "${version}",
  "description": "Native leg binary for ${npm_os}/${npm_cpu}.",
  "license": "UNLICENSED",
  "files": [
    "bin"
  ],
  "os": [
    "${npm_os}"
  ],
  "cpu": [
    "${npm_cpu}"
  ],
  "publishConfig": {
    "access": "public"
  }
}
EOF
}

release_npm_validate_manifest() {
    local manifest_path="${1:-}" expected_name="${2:-}" version="${3:-}"
    local kind="${4:-}" npm_os="${5:-}" npm_cpu="${6:-}"

    [[ -f "${manifest_path}" ]] || {
        printf "release: npm manifest not found '%s'\n" "${manifest_path}" >&2
        return 1
    }
    command -v node >/dev/null 2>&1 || {
        printf 'release: node is required to validate npm manifests\n' >&2
        return 1
    }

    node - "${manifest_path}" "${expected_name}" "${version}" "${kind}" \
        "${npm_os}" "${npm_cpu}" <<'NODE'
const fs = require('node:fs');

const [, , manifestPath, expectedName, expectedVersion, kind, expectedOs, expectedCpu] = process.argv;
let manifest;
try {
  manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
} catch (error) {
  console.error(`release: invalid npm manifest ${manifestPath}: ${error.message}`);
  process.exit(1);
}

function fail(message) {
  console.error(`release: ${manifestPath}: ${message}`);
  process.exit(1);
}

if (manifest.name !== expectedName) fail(`name '${manifest.name}' does not match '${expectedName}'`);
if (manifest.version !== expectedVersion) fail(`version '${manifest.version}' does not match '${expectedVersion}'`);
if (manifest.license !== 'UNLICENSED') fail("license must be UNLICENSED");
if (manifest.scripts) fail('scripts are not allowed in registry packages');
if (!Array.isArray(manifest.files) || manifest.files.length !== 1 || manifest.files[0] !== 'bin') {
  fail('files must contain only bin');
}
if (manifest.publishConfig?.access !== 'public') fail('publishConfig.access must be public');
function sameObject(actual, expected) {
  if (!actual || typeof actual !== 'object' || Array.isArray(actual)) return false;
  const actualKeys = Object.keys(actual).sort();
  const expectedKeys = Object.keys(expected).sort();
  return actualKeys.length === expectedKeys.length &&
    actualKeys.every((key, index) => key === expectedKeys[index] && actual[key] === expected[key]);
}

if (kind === 'root') {
  if (manifest.bin?.leg !== 'bin/leg.js') fail('bin.leg must be bin/leg.js');
  if (JSON.stringify(manifest.os) !== JSON.stringify(['darwin', 'linux', 'win32'])) {
    fail('os matrix is incorrect');
  }
  if (JSON.stringify(manifest.cpu) !== JSON.stringify(['x64', 'arm64', 'arm'])) {
    fail('cpu matrix is incorrect');
  }
  const expectedDependencies = {
    '@shukelabs/leg-linux-x64': expectedVersion,
    '@shukelabs/leg-linux-arm64': expectedVersion,
    '@shukelabs/leg-linux-arm': expectedVersion,
    '@shukelabs/leg-darwin-x64': expectedVersion,
    '@shukelabs/leg-darwin-arm64': expectedVersion,
    '@shukelabs/leg-win32-x64': expectedVersion,
  };
  if (!sameObject(manifest.optionalDependencies, expectedDependencies)) {
    fail('optionalDependencies must list all six platform packages at the release version');
  }
} else if (kind === 'platform') {
  if (JSON.stringify(manifest.os) !== JSON.stringify([expectedOs])) fail(`os must be ${expectedOs}`);
  if (JSON.stringify(manifest.cpu) !== JSON.stringify([expectedCpu])) fail(`cpu must be ${expectedCpu}`);
} else {
  fail(`unknown manifest kind '${kind}'`);
}
NODE
}

release_npm_validate_package_set() {
    local version="${1:-}" package_root="${2:-}"
    local package_key target npm_os npm_cpu archive binary package_dir
    local entry entry_name file_count

    release_validate_version "${version}" || return 1
    [[ -d "${package_root}" ]] || {
        printf "release: npm package directory not found '%s'\n" "${package_root}" >&2
        return 1
    }

    package_dir="${package_root}/leg"
    release_npm_validate_manifest "${package_dir}/package.json" \
        '@shukelabs/leg' "${version}" root || return 1
    [[ -f "${package_dir}/bin/leg.js" ]] || {
        printf "release: root npm shim not found in '%s'\n" "${package_dir}" >&2
        return 1
    }
    cmp -s "${package_dir}/bin/leg.js" "$(release_npm_shim_path)" || {
        printf "release: staged npm shim differs from packaging/npm/leg.js\n" >&2
        return 1
    }
    file_count="$(find "${package_dir}" -type f | wc -l | tr -d ' ')"
    [[ "${file_count}" == 2 ]] || {
        printf "release: root npm package must contain exactly package.json and bin/leg.js\n" >&2
        return 1
    }

    while IFS='|' read -r package_key target npm_os npm_cpu archive binary; do
        package_dir="${package_root}/leg-${package_key}"
        release_npm_validate_manifest "${package_dir}/package.json" \
            "@shukelabs/leg-${package_key}" "${version}" platform \
            "${npm_os}" "${npm_cpu}" || return 1
        [[ -f "${package_dir}/bin/${binary}" ]] || {
            printf "release: native binary missing from '%s'\n" "${package_dir}" >&2
            return 1
        }
        file_count="$(find "${package_dir}" -type f | wc -l | tr -d ' ')"
        [[ "${file_count}" == 2 ]] || {
            printf "release: npm package '%s' contains unexpected files\n" "${package_dir}" >&2
            return 1
        }
    done < <(release_npm_platform_rows)

    for entry in "${package_root}"/*; do
        [[ -d "${entry}" ]] || {
            printf "release: unexpected file in npm package staging '%s'\n" "${entry}" >&2
            return 1
        }
        entry_name="${entry##*/}"
        case "${entry_name}" in
            leg|leg-linux-x64|leg-linux-arm64|leg-linux-arm|leg-darwin-x64|leg-darwin-arm64|leg-win32-x64) ;;
            *)
                printf "release: unexpected npm package directory '%s'\n" "${entry_name}" >&2
                return 1
                ;;
        esac
    done
}

release_npm_stage_packages() {
    local version="${1:-}" archive_dir="${2:-}" output_dir="${3:-}"
    local staging="" package_key target npm_os npm_cpu archive binary
    local archive_path package_dir extract_dir extracted_binary

    release_validate_version "${version}" || return 1
    [[ -d "${archive_dir}" ]] || {
        printf "release: archive directory not found '%s'\n" "${archive_dir}" >&2
        return 1
    }
    [[ -n "${output_dir}" && ! -e "${output_dir}" ]] || {
        printf "release: npm staging output must be a new path '%s'\n" "${output_dir}" >&2
        return 1
    }
    [[ -f "$(release_npm_shim_path)" ]] || {
        printf 'release: npm shim source is missing\n' >&2
        return 1
    }

    mkdir -p -- "$(dirname -- "${output_dir}")"
    staging="$(mktemp -d "${output_dir}.XXXXXX")" || return 1
    mkdir -p "${staging}/leg/bin"
    cp -- "$(release_npm_shim_path)" "${staging}/leg/bin/leg.js"
    chmod +x "${staging}/leg/bin/leg.js"
    release_npm_write_root_manifest "${version}" >"${staging}/leg/package.json"

    while IFS='|' read -r package_key target npm_os npm_cpu archive binary; do
        archive_path="${archive_dir}/leg-${version}-${target}.${archive}"
        package_dir="${staging}/leg-${package_key}"
        extract_dir="${staging}/.extract-${package_key}"
        [[ -f "${archive_path}" ]] || {
            printf "release: target archive not found '%s'\n" "${archive_path}" >&2
            rm -rf -- "${staging}"
            return 1
        }
        mkdir -p "${package_dir}/bin" "${extract_dir}"
        case "${archive}" in
            tar.gz)
                tar -xzf "${archive_path}" -C "${extract_dir}"
                ;;
            zip)
                unzip -q "${archive_path}" -d "${extract_dir}"
                ;;
            *)
                printf "release: unsupported npm archive type '%s'\n" "${archive}" >&2
                rm -rf -- "${staging}"
                return 1
                ;;
        esac
        extracted_binary="${extract_dir}/${binary}"
        [[ -f "${extracted_binary}" ]] || {
            printf "release: expected binary '%s' missing from '%s'\n" "${binary}" "${archive_path}" >&2
            rm -rf -- "${staging}"
            return 1
        }
        cp -- "${extracted_binary}" "${package_dir}/bin/${binary}"
        [[ "${npm_os}" == 'win32' ]] || chmod +x "${package_dir}/bin/${binary}"
        release_npm_write_platform_manifest "${version}" "${package_key}" \
            "${npm_os}" "${npm_cpu}" >"${package_dir}/package.json"
        rm -rf -- "${extract_dir}"
    done < <(release_npm_platform_rows)

    if ! release_npm_validate_package_set "${version}" "${staging}"; then
        rm -rf -- "${staging}"
        return 1
    fi
    mv -- "${staging}" "${output_dir}"
}

release_sha256_sum() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$@"
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$@"
    else
        printf 'release: sha256sum or shasum is required\n' >&2
        return 1
    fi
}

release_sha256_check() {
    local checksum_path="${1:-}"

    [[ -f "${checksum_path}" ]] || {
        printf "release: checksum file not found '%s'\n" "${checksum_path}" >&2
        return 1
    }
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum --check "${checksum_path}"
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 --check "${checksum_path}"
    else
        printf 'release: sha256sum or shasum is required\n' >&2
        return 1
    fi
}

release_npm_write_checksums() {
    local tarball_dir="${1:-}" checksum_path="${2:-}" checksum_dir="" checksum_name=""
    local tarball=""
    local -a tarballs=() tarball_names=()

    [[ -d "${tarball_dir}" && -n "${checksum_path}" ]] || {
        printf 'release: npm checksum inputs are incomplete\n' >&2
        return 1
    }
    tarballs=("${tarball_dir}"/*.tgz)
    [[ -f "${tarballs[0]}" ]] || {
        printf "release: no npm tarballs found in '%s'\n" "${tarball_dir}" >&2
        return 1
    }
    for tarball in "${tarballs[@]}"; do
        tarball_names+=("${tarball##*/}")
    done
    checksum_dir="$(cd -- "$(dirname -- "${checksum_path}")" && pwd)"
    checksum_name="$(basename -- "${checksum_path}")"
    (cd -- "${tarball_dir}" && release_sha256_sum -- "${tarball_names[@]}") \
        >"${checksum_dir}/${checksum_name}"
    (cd -- "${tarball_dir}" && release_sha256_check "${checksum_dir}/${checksum_name}")
}

release_usage() {
    cat >&2 <<'EOF'
usage:
  scripts/release.sh manifest-version [path]
  scripts/release.sh lockfile-version [path]
  scripts/release.sh verify-tag-matches-manifest <tag> [manifest] [lockfile]
  scripts/release.sh npm-package-directories
  scripts/release.sh stage-npm-packages <version> <archive-dir> <output-dir>
  scripts/release.sh verify-npm-packages <version> <package-dir>
  scripts/release.sh npm-checksums <tarball-dir> <checksum-path>
EOF
}

release_main() {
    local command="${1:-}"
    shift || true

    case "${command}" in
        manifest-version)
            release_manifest_version "${1:-Cargo.toml}"
            ;;
        lockfile-version)
            release_lockfile_version "${1:-Cargo.lock}"
            ;;
        verify-tag-matches-manifest)
            release_verify_tag_matches_manifest "${1:-}" "${2:-Cargo.toml}" "${3:-Cargo.lock}"
            ;;
        npm-package-directories)
            release_npm_package_directories
            ;;
        stage-npm-packages)
            release_npm_stage_packages "${1:-}" "${2:-}" "${3:-}"
            ;;
        verify-npm-packages)
            release_npm_validate_package_set "${1:-}" "${2:-}"
            ;;
        npm-checksums)
            release_npm_write_checksums "${1:-}" "${2:-}"
            ;;
        *)
            release_usage
            return 1
            ;;
    esac
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    release_main "$@"
fi
