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

release_repo_root() {
    local release_script_path="${BASH_SOURCE[0]}"

    (cd -- "$(dirname -- "${release_script_path}")/.." && pwd)
}

release_npm_shim_path() {
    printf '%s/packaging/npm/leg.js\n' "$(release_repo_root)"
}

release_third_party_notices_path() {
    printf '%s/THIRD_PARTY_NOTICES.txt\n' "$(release_repo_root)"
}

# Render THIRD_PARTY_NOTICES.txt from one `cargo metadata` JSON document per
# release target plus the vendored material under THIRD_PARTY_LICENSES/. The
# crate set is the union of non-dev (normal and build) dependencies reachable
# from the workspace root; the root crate itself is proprietary and excluded.
# cargo metadata's resolve can be a superset of what is compiled (it keeps
# optional deps named only by weak `dep?/feature` entries); over-listing a
# crate is harmless, missing one is not.
# Output is deterministic: sorted, LF-only, and free of local paths. Missing or
# unsupported license metadata fails instead of producing a partial bundle.
release_third_party_notices_render() {
    local third_party_dir="${1:-}"
    shift || true

    [[ -d "${third_party_dir}" && "$#" -gt 0 ]] || {
        printf 'release: third-party notice inputs are incomplete\n' >&2
        return 1
    }
    command -v node >/dev/null 2>&1 || {
        printf 'release: node is required to render third-party notices\n' >&2
        return 1
    }

    node - "${third_party_dir}" "$@" <<'NODE'
const fs = require('node:fs');
const path = require('node:path');

const [, , thirdPartyDir, ...metadataPaths] = process.argv;
const SUPPORTED_LICENSES = new Set([
  '0BSD', 'Apache-2.0', 'BSD-3-Clause', 'CDLA-Permissive-2.0', 'ISC', 'MIT',
  'Unicode-3.0', 'Unlicense', 'Zlib',
]);
const LICENSE_FILE = /^(licen[cs]e|copying|notice|unlicense|copyright)/i;
const RULE = '='.repeat(79);

function fail(message) {
  console.error(`release: third-party notices: ${message}`);
  process.exit(1);
}

function readText(filePath) {
  const text = fs.readFileSync(filePath, 'utf8').replace(/\r\n?/g, '\n');
  return text.endsWith('\n') ? text : `${text}\n`;
}

function byCodePoint(a, b) {
  return a < b ? -1 : a > b ? 1 : 0;
}

// Validates the SPDX expression grammar used by Cargo (`OR` binds looser than
// `AND`, parentheses group) and requires every license id to be allowlisted.
function checkLicense(pkg) {
  const label = `${pkg.name} ${pkg.version}`;
  if (typeof pkg.license !== 'string' || pkg.license.trim() === '') {
    fail(`${label} has no SPDX license expression`);
  }
  const tokens = pkg.license.match(/[()]|[^\s()]+/g);
  let index = 0;
  const malformed = () => fail(`${label} has malformed license expression '${pkg.license}'`);
  function atom() {
    const token = tokens[index++];
    if (token === '(') {
      disjunction();
      if (tokens[index++] !== ')') malformed();
    } else if (token === undefined || token === ')' || token === 'AND' || token === 'OR') {
      malformed();
    } else if (!SUPPORTED_LICENSES.has(token)) {
      fail(`${label} uses unsupported license term '${token}' in '${pkg.license}'`);
    }
  }
  function conjunction() {
    atom();
    while (tokens[index] === 'AND') { index++; atom(); }
  }
  function disjunction() {
    conjunction();
    while (tokens[index] === 'OR') { index++; conjunction(); }
  }
  disjunction();
  if (index !== tokens.length) malformed();
}

const crates = new Map();
for (const metadataPath of metadataPaths) {
  let metadata;
  try {
    metadata = JSON.parse(fs.readFileSync(metadataPath, 'utf8'));
  } catch (error) {
    fail(`invalid cargo metadata ${metadataPath}: ${error.message}`);
  }
  const root = metadata.resolve?.root;
  if (!root) fail(`${metadataPath} has no resolved root package`);
  const packages = new Map(metadata.packages.map((pkg) => [pkg.id, pkg]));
  const nodes = new Map(metadata.resolve.nodes.map((node) => [node.id, node]));
  const pending = [root];
  const seen = new Set();
  while (pending.length > 0) {
    const id = pending.pop();
    if (seen.has(id)) continue;
    seen.add(id);
    const node = nodes.get(id);
    if (!node) fail(`${metadataPath} has no resolve node for ${id}`);
    for (const dep of node.deps) {
      if (dep.dep_kinds.some((kind) => kind.kind !== 'dev')) pending.push(dep.pkg);
    }
  }
  seen.delete(root);
  for (const id of seen) {
    const pkg = packages.get(id);
    if (!pkg) fail(`${metadataPath} has no package entry for ${id}`);
    crates.set(id, pkg);
  }
}

const sections = [];
const ordered = [...crates.values()].sort((a, b) =>
  byCodePoint(a.name, b.name) || byCodePoint(a.version, b.version) || byCodePoint(a.id, b.id));
for (const pkg of ordered) {
  checkLicense(pkg);
  const crateDir = path.dirname(pkg.manifest_path);
  const licenseFiles = fs.readdirSync(crateDir)
    .filter((name) => LICENSE_FILE.test(name) && fs.statSync(path.join(crateDir, name)).isFile())
    .sort(byCodePoint);
  if (licenseFiles.length === 0) fail(`${pkg.name} ${pkg.version} ships no license file`);
  let section = `${RULE}\nCrate: ${pkg.name} ${pkg.version}\nLicense: ${pkg.license}\n` +
    `Source: ${pkg.source ?? 'path'}\n`;
  for (const name of licenseFiles) {
    section += `\n--- ${pkg.name} ${pkg.version}: ${name} ---\n${readText(path.join(crateDir, name))}`;
  }
  sections.push(section);
}

function walk(dir, prefix) {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) return walk(path.join(dir, entry.name), relative);
    if (entry.isFile()) return [relative];
    fail(`unsupported entry THIRD_PARTY_LICENSES/${relative}`);
  });
}
const bundled = walk(thirdPartyDir, '').sort(byCodePoint);
if (bundled.length === 0) fail('THIRD_PARTY_LICENSES/ is empty');
for (const relative of bundled) {
  sections.push(`${RULE}\nBundled material: THIRD_PARTY_LICENSES/${relative}\n\n` +
    readText(path.join(thirdPartyDir, ...relative.split('/'))));
}

process.stdout.write(
  'THIRD-PARTY NOTICES FOR leg\n\n' +
  'Generated by `scripts/release.sh third-party-notices`; do not edit by hand.\n' +
  'Lists every third-party Rust crate built into the leg release binaries with\n' +
  'its license expression and shipped license texts, followed by the bundled\n' +
  'third-party material from THIRD_PARTY_LICENSES/.\n\n' +
  sections.join('\n'));
NODE
}

release_third_party_notices_generate() {
    local repo_root="${1:-$(release_repo_root)}"
    local metadata_dir="" package_key target _os _cpu _archive _binary status=0
    local -a metadata_paths=()

    metadata_dir="$(mktemp -d)" || return 1
    while IFS='|' read -r package_key target _os _cpu _archive _binary; do
        cargo metadata --locked --format-version 1 --filter-platform "${target}" \
            --manifest-path "${repo_root}/Cargo.toml" \
            >"${metadata_dir}/${package_key}.json" || status="$?"
        (( status == 0 )) || break
        metadata_paths+=("${metadata_dir}/${package_key}.json")
    done < <(release_npm_platform_rows)
    if (( status == 0 )); then
        release_third_party_notices_render "${repo_root}/THIRD_PARTY_LICENSES" \
            "${metadata_paths[@]}" || status="$?"
    else
        printf 'release: cargo metadata failed\n' >&2
    fi
    rm -rf -- "${metadata_dir}"
    return "${status}"
}

release_third_party_notices_check() {
    local repo_root="${1:-$(release_repo_root)}"
    local notice_path="${2:-${repo_root}/THIRD_PARTY_NOTICES.txt}" generated="" status=0

    [[ -f "${notice_path}" ]] || {
        printf "release: third-party notices not found '%s'\n" "${notice_path}" >&2
        return 1
    }
    generated="$(mktemp)" || return 1
    release_third_party_notices_generate "${repo_root}" >"${generated}" || status="$?"
    if (( status == 0 )) && ! cmp -s "${generated}" "${notice_path}"; then
        printf "release: '%s' is stale; run: bash scripts/release.sh third-party-notices > THIRD_PARTY_NOTICES.txt\n" \
            "${notice_path}" >&2
        status=1
    fi
    rm -f -- "${generated}"
    return "${status}"
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
    "bin",
    "THIRD_PARTY_NOTICES.txt"
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
    "bin",
    "THIRD_PARTY_NOTICES.txt"
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
if (JSON.stringify(manifest.files) !== JSON.stringify(['bin', 'THIRD_PARTY_NOTICES.txt'])) {
  fail('files must contain only bin and THIRD_PARTY_NOTICES.txt');
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

release_npm_validate_notice() {
    local package_dir="${1:-}"

    cmp -s "${package_dir}/THIRD_PARTY_NOTICES.txt" "$(release_third_party_notices_path)" || {
        printf "release: '%s' lacks the generated THIRD_PARTY_NOTICES.txt\n" "${package_dir}" >&2
        return 1
    }
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
    release_npm_validate_notice "${package_dir}" || return 1
    file_count="$(find "${package_dir}" -type f | wc -l | tr -d ' ')"
    [[ "${file_count}" == 3 ]] || {
        printf "release: root npm package must contain exactly package.json, bin/leg.js, and THIRD_PARTY_NOTICES.txt\n" >&2
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
        release_npm_validate_notice "${package_dir}" || return 1
        file_count="$(find "${package_dir}" -type f | wc -l | tr -d ' ')"
        [[ "${file_count}" == 3 ]] || {
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
    [[ -f "$(release_third_party_notices_path)" ]] || {
        printf 'release: THIRD_PARTY_NOTICES.txt is missing\n' >&2
        return 1
    }

    mkdir -p -- "$(dirname -- "${output_dir}")"
    staging="$(mktemp -d "${output_dir}.XXXXXX")" || return 1
    mkdir -p "${staging}/leg/bin"
    cp -- "$(release_npm_shim_path)" "${staging}/leg/bin/leg.js"
    chmod +x "${staging}/leg/bin/leg.js"
    release_npm_write_root_manifest "${version}" >"${staging}/leg/package.json"
    cp -- "$(release_third_party_notices_path)" "${staging}/leg/THIRD_PARTY_NOTICES.txt"

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
        cp -- "$(release_third_party_notices_path)" "${package_dir}/THIRD_PARTY_NOTICES.txt"
        rm -rf -- "${extract_dir}"
    done < <(release_npm_platform_rows)

    if ! release_npm_validate_package_set "${version}" "${staging}"; then
        rm -rf -- "${staging}"
        return 1
    fi
    mv -- "${staging}" "${output_dir}"
}

# Check the packed artifacts themselves, not only the staging tree: every npm
# tarball must carry the generated notice and never the proprietary LICENSE.
release_npm_verify_tarballs() {
    local tarball_dir="${1:-}" notice_path="" tarball="" entries="" expected_count=0
    local -a tarballs=()

    [[ -d "${tarball_dir}" ]] || {
        printf "release: npm tarball directory not found '%s'\n" "${tarball_dir}" >&2
        return 1
    }
    notice_path="$(release_third_party_notices_path)"
    expected_count="$(release_npm_package_directories | wc -l | tr -d ' ')"
    tarballs=("${tarball_dir}"/*.tgz)
    [[ -f "${tarballs[0]}" && "${#tarballs[@]}" == "${expected_count}" ]] || {
        printf "release: expected %s npm tarballs in '%s'\n" "${expected_count}" "${tarball_dir}" >&2
        return 1
    }
    for tarball in "${tarballs[@]}"; do
        entries="$(tar -tzf "${tarball}")" || return 1
        if ! grep -qx 'package/THIRD_PARTY_NOTICES.txt' <<<"${entries}" ||
            ! tar -xzOf "${tarball}" package/THIRD_PARTY_NOTICES.txt | cmp -s - "${notice_path}"; then
            printf "release: '%s' lacks the generated THIRD_PARTY_NOTICES.txt\n" "${tarball}" >&2
            return 1
        fi
        if grep -Eiq '^package/licen[cs]e' <<<"${entries}"; then
            printf "release: '%s' must not contain a LICENSE file\n" "${tarball}" >&2
            return 1
        fi
    done
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
  scripts/release.sh verify-npm-tarballs <tarball-dir>
  scripts/release.sh npm-checksums <tarball-dir> <checksum-path>
  scripts/release.sh third-party-notices
  scripts/release.sh verify-third-party-notices
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
        verify-npm-tarballs)
            release_npm_verify_tarballs "${1:-}"
            ;;
        npm-checksums)
            release_npm_write_checksums "${1:-}" "${2:-}"
            ;;
        third-party-notices)
            release_third_party_notices_generate
            ;;
        verify-third-party-notices)
            release_third_party_notices_check
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
