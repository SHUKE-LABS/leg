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

make_git_fixture() {
    local repo="${1}" subject="${2:-chore: release baseline}"
    local date="${3:-2026-01-01}" version="${4:-0.1.0}"

    make_fixture "${repo}" "${version}"
    git -C "${repo}" init -q
    git -C "${repo}" config user.email "release-test@leg.local"
    git -C "${repo}" config user.name "leg release test"
    git -C "${repo}" add Cargo.toml Cargo.lock
    GIT_AUTHOR_DATE="${date}T12:00:00+0000" GIT_COMMITTER_DATE="${date}T12:00:00+0000" \
        git -C "${repo}" commit -q -m "${subject}"
}

changelog_commit() {
    local repo="${1}" date="${2}" subject="${3}"

    printf '%s\n' "${subject}" >>"${repo}/log.txt"
    git -C "${repo}" add log.txt
    GIT_AUTHOR_DATE="${date}T12:00:00+0000" GIT_COMMITTER_DATE="${date}T12:00:00+0000" \
        git -C "${repo}" commit -q -m "${subject}"
}

changelog_tag() {
    local repo="${1}" date="${2}" tag="${3}"

    GIT_COMMITTER_DATE="${date}T12:00:00+0000" git -C "${repo}" tag -f "${tag}" >/dev/null
}

make_changelog_fixture() {
    local repo="${1}"

    make_git_fixture "${repo}" "chore: release baseline" "2026-01-01"
    changelog_tag "${repo}" 2026-01-01 v0.1.0

    changelog_commit "${repo}" 2026-02-01 "docs: describe the first feature"
    changelog_commit "${repo}" 2026-02-01 "feat: add the first feature"
    changelog_commit "${repo}" 2026-02-01 "feat: add the second feature"
    changelog_commit "${repo}" 2026-02-01 "chore(release): v0.2.0 [skip ci]"
    changelog_tag "${repo}" 2026-02-01 v0.2.0
    changelog_commit "${repo}" 2026-02-01 "unconventional subject line"
    changelog_commit "${repo}" 2026-02-01 "perf: speed up the first feature"
    changelog_commit "${repo}" 2026-02-01 "fix: correct the first feature"
    changelog_commit "${repo}" 2026-02-01 "refactor: tidy the first feature"
    changelog_commit "${repo}" 2026-02-01 "docs: regenerate changelog [skip ci]"
    changelog_tag "${repo}" 2026-02-01 v0.2.1

    changelog_commit "${repo}" 2026-03-05 "fix: adjust after the release"
    changelog_tag "${repo}" 2026-03-05 v0.2.2
}

make_npm_archive_fixture() {
    local repo="${1}" version="${2}" package_key target _npm_os _npm_cpu archive binary
    local archive_dir staging archive_path source_windows archive_windows name suffix
    local -a binaries=()

    archive_dir="${repo}/dist"
    mkdir -p "${archive_dir}"
    while IFS='|' read -r package_key target _npm_os _npm_cpu archive binary; do
        staging="${repo}/staging-${package_key}"
        mkdir -p "${staging}"
        suffix="${binary#leg}"
        binaries=()
        for name in leg leg-tui leg-web leg-ui-supervisor; do
            binaries+=("${name}${suffix}")
            if [[ -z "${suffix}" ]]; then
                # shellcheck disable=SC2016 # $1 belongs to the generated fixture.
                printf '#!/bin/sh\ntest "$1" = "--version" || exit 9\nprintf "%s %s\\n"\n' \
                    "${name}" "${version}" >"${staging}/${name}"
                chmod +x "${staging}/${name}"
            else
                printf 'fake windows %s %s\n' "${name}" "${version}" >"${staging}/${name}${suffix}"
            fi
        done
        archive_path="${archive_dir}/leg-${version}-${target}.${archive}"
        case "${archive}" in
            tar.gz)
                tar -C "${staging}" -czf "${archive_path}" "${binaries[@]}"
                ;;
            zip)
                if command -v zip >/dev/null 2>&1; then
                    (cd "${staging}" && zip -q "${archive_path}" "${binaries[@]}")
                elif command -v powershell.exe >/dev/null 2>&1 && command -v cygpath >/dev/null 2>&1; then
                    source_windows="$(cygpath -w "${staging}")"
                    archive_windows="$(cygpath -w "${archive_path}")"
                    powershell.exe -NoProfile -NonInteractive -Command \
                        "\$files = Get-ChildItem -LiteralPath '${source_windows}' -File; Compress-Archive -LiteralPath \$files.FullName -DestinationPath '${archive_windows}' -Force"
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

test_release_bump_rules() (
    set -euo pipefail
    assert_eq "minor" "$(release_bump_kind_for_subject 'feat(core): add a feature')" \
        "feature subjects bump minor"
    assert_eq "patch" "$(release_bump_kind_for_subject 'fix: correct a bug')" \
        "fix subjects bump patch"
    assert_eq "patch" "$(release_bump_kind_for_subject 'maintenance update')" \
        "other subjects bump patch"
    assert_eq "0.2.0" "$(release_next_version v0.1.7 minor)" \
        "minor bump resets patch"
    assert_eq "v0.1.1" "$(release_next_tag v0.1.0 patch)" \
        "patch bump increments patch"
)

test_first_release_uses_current_manifest_version() (
    set -euo pipefail
    local repo before_head tag
    repo="$(mktemp -d)"
    trap 'rm -rf "${repo}"' EXIT
    make_git_fixture "${repo}" "fix: prepare first release"
    before_head="$(git -C "${repo}" rev-parse HEAD)"

    tag="$(cd "${repo}" && release_create_tag)"

    assert_eq "v0.1.0" "${tag}" "first release uses the current version"
    assert_eq "0.1.0" "$(cd "${repo}" && release_manifest_version)" \
        "first release leaves the manifest version unchanged"
    assert_eq "0.1.0" "$(cd "${repo}" && release_lockfile_version)" \
        "first release leaves the lockfile version unchanged"
    assert_eq "${before_head}" "$(git -C "${repo}" rev-parse "${tag}^{commit}")" \
        "first release tags the existing commit"
)

test_first_release_commits_generated_changelog_after_tag() (
    set -euo pipefail
    local repo tag notes
    repo="$(mktemp -d)"
    trap 'rm -rf "${repo}"' EXIT
    make_git_fixture "${repo}" "fix: prepare first release"

    tag="$(cd "${repo}" && release_create_tag)"
    (
        cd "${repo}"
        release_generate_changelog CHANGELOG.md
        release_generate_release_notes "${tag}" >release-notes.md
        git add CHANGELOG.md
        git commit -q -m "docs: regenerate changelog [skip ci]"
    )
    notes="$(<"${repo}/release-notes.md")"

    assert_eq "v0.1.0" "${tag}" "first release tag"
    assert_eq "docs: regenerate changelog [skip ci]" \
        "$(git -C "${repo}" log -1 --format=%s)" \
        "changelog is committed separately after tagging"
    assert_eq "$(git -C "${repo}" rev-parse "${tag}^{commit}")" \
        "$(git -C "${repo}" rev-parse HEAD^)" \
        "release tag remains on the pre-changelog commit"
    grep -F "## ${tag}" "${repo}/CHANGELOG.md" >/dev/null
    grep -F "fix: prepare first release" "${repo}/CHANGELOG.md" >/dev/null
    grep -F "fix: prepare first release" <<<"${notes}" >/dev/null
)

test_feature_release_updates_manifest_and_lockfile() (
    set -euo pipefail
    local repo tag version
    repo="$(mktemp -d)"
    trap 'rm -rf "${repo}"' EXIT
    make_git_fixture "${repo}"
    git -C "${repo}" tag v0.1.0
    printf 'feature\n' >"${repo}/feature.txt"
    git -C "${repo}" add feature.txt
    git -C "${repo}" commit -q -m "feat: add release behavior"

    tag="$(cd "${repo}" && release_create_tag)"
    version="${tag#v}"

    assert_eq "v0.2.0" "${tag}" "feature commit creates a minor release"
    assert_eq "${version}" "$(cd "${repo}" && release_manifest_version)" \
        "manifest matches the feature tag"
    assert_eq "${version}" "$(cd "${repo}" && release_lockfile_version)" \
        "lockfile matches the feature tag"
    assert_eq "$(git -C "${repo}" rev-parse HEAD)" \
        "$(git -C "${repo}" rev-parse "${tag}^{commit}")" \
        "feature tag points at the version update commit"
    assert_eq "chore(release): ${tag} [skip ci]" \
        "$(git -C "${repo}" log -1 --format=%s "${tag}")" \
        "version update commit is excluded from the next release"
)

test_patch_release_updates_manifest_and_lockfile() (
    set -euo pipefail
    local repo tag version
    repo="$(mktemp -d)"
    trap 'rm -rf "${repo}"' EXIT
    make_git_fixture "${repo}"
    git -C "${repo}" tag v0.1.0
    printf 'fix\n' >"${repo}/fix.txt"
    git -C "${repo}" add fix.txt
    git -C "${repo}" commit -q -m "fix: repair release behavior"

    tag="$(cd "${repo}" && release_create_tag)"
    version="${tag#v}"

    assert_eq "v0.1.1" "${tag}" "fix commit creates a patch release"
    assert_eq "${version}" "$(cd "${repo}" && release_manifest_version)" \
        "patch manifest matches tag"
    assert_eq "${version}" "$(cd "${repo}" && release_lockfile_version)" \
        "patch lockfile matches tag"
)

test_changelog_groups_tags_and_filters_skip_ci_commits() (
    set -euo pipefail
    local repo generated group
    repo="$(mktemp -d)"
    trap 'rm -rf "${repo}"' EXIT
    make_changelog_fixture "${repo}"

    generated="$(cd "${repo}" && release_generate_changelog -)"
    assert_eq "_Generated from release tags with \`bash scripts/release.sh generate-changelog\`._" \
        "$(printf '%s\n' "${generated}" | grep -F 'Generated from release tags')" \
        "generated-from-tags header"
    assert_eq "## v0.2.2 (2026-03-05)
## v0.2.1 … v0.2.0 (2026-02-01)
## v0.1.0 (2026-01-01)" \
        "$(printf '%s\n' "${generated}" | grep '^## ')" \
        "same-day release tags are grouped"

    group="$(printf '%s\n' "${generated}" | sed -n '/^## v0.2.1 /,/^## v0.1.0 /p')"
    assert_eq "### Features
### Fixes
### Refactors
### Performance
### Docs
### Other Changes" \
        "$(printf '%s\n' "${group}" | grep '^### ')" \
        "changelog buckets use a fixed order"
    assert_eq "- feat: add the first feature
- feat: add the second feature" \
        "$(printf '%s\n' "${group}" | sed -n '/^### Features$/,/^$/p' | grep '^- ')" \
        "feature commits are listed in commit order"
    assert_eq "- unconventional subject line" \
        "$(printf '%s\n' "${group}" | sed -n '/^### Other Changes$/,/^$/p' | grep '^- ')" \
        "unconventional subjects are retained"
    assert_eq "" "$(printf '%s\n' "${generated}" | grep -F '[skip ci]' || true)" \
        "release and changelog commits are omitted"
)

test_release_notes_cover_only_requested_tag() (
    set -euo pipefail
    local repo generated
    repo="$(mktemp -d)"
    trap 'rm -rf "${repo}"' EXIT
    make_changelog_fixture "${repo}"

    generated="$(cd "${repo}" && release_generate_release_notes v0.2.1)"

    assert_eq "## v0.2.1 (2026-02-01)" \
        "$(printf '%s\n' "${generated}" | grep '^## ')" \
        "release notes name the requested tag"
    assert_eq "- fix: correct the first feature
- refactor: tidy the first feature
- perf: speed up the first feature
- unconventional subject line" \
        "$(printf '%s\n' "${generated}" | grep '^- ')" \
        "release notes include only commits between tags"
    assert_eq "" "$(printf '%s\n' "${generated}" | grep -F 'fix: adjust after the release' || true)" \
        "later commits are excluded"
    assert_eq "" "$(printf '%s\n' "${generated}" | grep -F '[skip ci]' || true)" \
        "release commits are excluded"
)

test_changelog_writes_idempotently_and_preserves_existing_file_on_error() (
    set -euo pipefail
    local repo generated status
    repo="$(mktemp -d)"
    trap 'rm -rf "${repo}"' EXIT
    make_changelog_fixture "${repo}"

    generated="$(cd "${repo}" && release_generate_changelog -)"
    (cd "${repo}" && release_generate_changelog CHANGELOG.md)
    assert_eq "${generated}" "$(cat "${repo}/CHANGELOG.md")" \
        "file output matches generated output"
    (cd "${repo}" && release_generate_changelog CHANGELOG.md)
    assert_eq "${generated}" "$(cat "${repo}/CHANGELOG.md")" \
        "a second run leaves the changelog unchanged"

    printf 'sentinel\n' >"${repo}/CHANGELOG.md"
    status=0
    (
        cd "${repo}"
        # shellcheck disable=SC2329  # invoked indirectly by the changelog writer
        release_tags_desc() { return 1; }
        release_generate_changelog CHANGELOG.md
    ) >/dev/null 2>&1 || status="$?"
    assert_rc_nonzero "${status}"
    assert_eq "sentinel" "$(cat "${repo}/CHANGELOG.md")" \
        "a failed tag lookup does not replace the changelog"
    assert_eq "" "$(cd "${repo}" && find . -maxdepth 1 -name 'CHANGELOG.md.*' -print)" \
        "a failed generation leaves no temporary file"
)

test_changelog_without_release_tags() (
    set -euo pipefail
    local repo generated
    repo="$(mktemp -d)"
    trap 'rm -rf "${repo}"' EXIT
    make_git_fixture "${repo}" "feat: untagged work"

    generated="$(cd "${repo}" && release_generate_changelog -)"

    assert_eq "No release tags yet." \
        "$(printf '%s\n' "${generated}" | grep -F 'No release tags')" \
        "an untagged repository renders the empty-changelog notice"
)

test_npm_platform_matrix_and_staging() (
    set -euo pipefail
    local repo version expected output status host_platform command
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
    assert_eq 'leg,leg-tui,leg-web' \
        "$(node -e 'console.log(Object.keys(require(process.argv[1]).bin).sort().join(","))' "${repo}/npm-packages/leg/package.json")" \
        "root exposes the three user commands only"
    local package_key target _npm_os _npm_cpu archive binary suffix name
    while IFS='|' read -r package_key target _npm_os _npm_cpu archive binary; do
        suffix="${binary#leg}"
        for name in leg leg-tui leg-web leg-ui-supervisor; do
            [[ -f "${repo}/npm-packages/leg-${package_key}/bin/${name}${suffix}" ]] || fail "${package_key} missing ${name}"
            if [[ -z "${suffix}" ]]; then
                [[ -x "${repo}/npm-packages/leg-${package_key}/bin/${name}" ]] || fail "${name} must be executable"
            fi
        done
    done < <(release_npm_platform_rows)
    assert_eq "${version}" \
        "$(node -e 'console.log(require(process.argv[1]).version)' "${repo}/npm-packages/leg-linux-arm/package.json")" \
        "linux-arm (armv7 musl) platform npm package version"
    assert_eq 'arm' \
        "$(node -e 'console.log(require(process.argv[1]).cpu[0])' "${repo}/npm-packages/leg-linux-arm/package.json")" \
        "linux-arm platform npm package cpu"

    link_npm_shim_node_modules "${repo}"
    host_platform="$(node -p 'process.platform')"
    for command in leg leg-tui leg-web; do
        # Windows fixtures are text, so exercise the real resolver on every
        # platform and execute the shell fixtures only on Unix hosts.
        node - "${repo}/npm-packages/leg/bin/${command}.js" "${command}" <<'NODE'
const assert = require('assert/strict');
const path = require('path');
const { resolvePlatformBinary } = require(process.argv[2]);
for (const [platform, arch] of [
  ['linux', 'x64'], ['linux', 'arm64'], ['linux', 'arm'],
  ['darwin', 'x64'], ['darwin', 'arm64'], ['win32', 'x64'],
]) {
  const result = resolvePlatformBinary(platform, arch);
  assert.equal(result.packageName, `@shukelabs/leg-${platform}-${arch}`);
  assert.equal(path.basename(result.binaryPath), process.argv[3] + (platform === 'win32' ? '.exe' : ''));
}
for (const [platform, arch] of [['freebsd', 'x64'], ['linux', 'ppc64']]) {
  assert.throws(() => resolvePlatformBinary(platform, arch), {
    message: `platform not supported (${platform}/${arch})`,
  });
}
NODE
        if [[ "${host_platform}" != 'win32' ]]; then
            output="$(node "${repo}/npm-packages/leg/bin/${command}.js" --version)"
            assert_eq "${command} ${version}" "${output}" "${command} npm shim forwards to native binary"
            status=0
            node "${repo}/npm-packages/leg/bin/${command}.js" --invalid >/dev/null 2>&1 || status="$?"
            assert_eq 9 "${status}" "${command} shim forwards argv and native exit status"
        fi
    done

    # The shim fixture's node_modules would otherwise fail the root package's
    # file count and mask which check rejects the cases below.
    rm -rf "${repo}/npm-packages/leg/node_modules"
    release_npm_validate_package_set "${version}" "${repo}/npm-packages"
    for command in leg-tui leg-web; do
        mv "${repo}/npm-packages/leg/bin/${command}.js" "${repo}/shim-backup"
        status=0
        release_npm_validate_package_set "${version}" "${repo}/npm-packages" >/dev/null 2>&1 || status="$?"
        assert_rc_nonzero "${status}"
        mv "${repo}/shim-backup" "${repo}/npm-packages/leg/bin/${command}.js"
    done
    for name in leg leg-tui leg-web leg-ui-supervisor; do
        mv "${repo}/npm-packages/leg-win32-x64/bin/${name}.exe" "${repo}/binary-backup"
        status=0
        release_npm_validate_package_set "${version}" "${repo}/npm-packages" >/dev/null 2>&1 || status="$?"
        assert_rc_nonzero "${status}"
        mv "${repo}/binary-backup" "${repo}/npm-packages/leg-win32-x64/bin/${name}.exe"
    done
    cp "${repo}/npm-packages/leg/package.json" "${repo}/manifest-backup"
    for command in leg-tui leg-web leg-ui-supervisor; do
        node - "${repo}/npm-packages/leg/package.json" "${command}" <<'NODE'
const fs = require('fs');
const file = process.argv[2];
const manifest = JSON.parse(fs.readFileSync(file));
if (process.argv[3] === 'leg-ui-supervisor') manifest.bin['leg-ui-supervisor'] = 'bin/leg-ui-supervisor.js';
else manifest.bin[process.argv[3]] = 'bin/leg.js';
fs.writeFileSync(file, JSON.stringify(manifest));
NODE
        status=0
        release_npm_validate_package_set "${version}" "${repo}/npm-packages" >/dev/null 2>&1 || status="$?"
        assert_rc_nonzero "${status}"
        cp "${repo}/manifest-backup" "${repo}/npm-packages/leg/package.json"
    done
    printf 'extra\n' >"${repo}/npm-packages/leg-linux-arm/bin/extra"
    status=0
    release_npm_validate_package_set "${version}" "${repo}/npm-packages" >/dev/null 2>&1 || status="$?"
    assert_rc_nonzero "${status}"
    rm "${repo}/npm-packages/leg-linux-arm/bin/extra"
    mv "${repo}/npm-packages/leg-darwin-arm64/THIRD_PARTY_NOTICES.txt" "${repo}/notice-backup"
    status=0
    release_npm_validate_package_set "${version}" "${repo}/npm-packages" >/dev/null 2>&1 || status="$?"
    assert_rc_nonzero "${status}"
    mv "${repo}/notice-backup" "${repo}/npm-packages/leg-darwin-arm64/THIRD_PARTY_NOTICES.txt"

    printf '%s\n' '{"name":"@shukelabs/leg-linux-x64","version":"0.0.1"}' \
        >"${repo}/npm-packages/leg-linux-x64/package.json"
    status=0
    release_npm_validate_package_set "${version}" "${repo}/npm-packages" >/dev/null 2>&1 || status="$?"
    assert_rc_nonzero "${status}"
)

test_npm_staging_rejects_incomplete_archives() (
    set -euo pipefail
    local repo version name output status
    repo="$(mktemp -d)"
    trap 'rm -rf "${repo}"' EXIT
    version="0.4.25"
    make_npm_archive_fixture "${repo}" "${version}"
    mkdir "${repo}/unpacked"
    tar -xzf "${repo}/dist/leg-${version}-x86_64-unknown-linux-gnu.tar.gz" -C "${repo}/unpacked"
    for name in leg-tui leg-web leg-ui-supervisor; do
        mv "${repo}/unpacked/${name}" "${repo}/binary-backup"
        tar -C "${repo}/unpacked" -czf "${repo}/dist/leg-${version}-x86_64-unknown-linux-gnu.tar.gz" \
            .
        status=0
        output="$(release_npm_stage_packages "${version}" "${repo}/dist" "${repo}/npm-packages" 2>&1)" || status="$?"
        assert_rc_nonzero "${status}"
        [[ "${output}" == *"expected binary '${name}' missing"* ]] || fail "staging must identify missing ${name}: ${output}"
        [[ ! -e "${repo}/npm-packages" ]] || fail "failed staging must not publish a partial package set"
        mv "${repo}/binary-backup" "${repo}/unpacked/${name}"
    done
)

test_npm_pack_checksums() (
    set -euo pipefail
    local repo version package_dir tarball package_key target _npm_os _npm_cpu archive binary
    local expected actual suffix
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

    tarball="${repo}/npm-tarballs/shukelabs-leg-${version}.tgz"
    expected="$(printf '%s\n' \
        'package/THIRD_PARTY_NOTICES.txt' \
        'package/bin/leg.js' \
        'package/bin/leg-tui.js' \
        'package/bin/leg-web.js' \
        'package/package.json' | sort)"
    actual="$(tar -tzf "${tarball}" | sort)"
    assert_eq "${expected}" "${actual}" "root npm tarball contains only its three shims, manifest, and notice"
    while IFS='|' read -r package_key target _npm_os _npm_cpu archive binary; do
        tarball="${repo}/npm-tarballs/shukelabs-leg-${package_key}-${version}.tgz"
        suffix="${binary#leg}"
        expected="$(printf '%s\n' \
            'package/THIRD_PARTY_NOTICES.txt' \
            "package/bin/${binary}" \
            "package/bin/leg-tui${suffix}" \
            "package/bin/leg-web${suffix}" \
            "package/bin/leg-ui-supervisor${suffix}" \
            'package/package.json' | sort)"
        actual="$(tar -tzf "${tarball}" | sort)"
        assert_eq "${expected}" "${actual}" \
            "${package_key} npm tarball contains only its four native binaries, manifest, and notice"
    done < <(release_npm_platform_rows)

    release_npm_verify_tarballs "${repo}/npm-tarballs"
    for tarball in "${repo}"/npm-tarballs/*.tgz; do
        tar -xzOf "${tarball}" package/THIRD_PARTY_NOTICES.txt | cmp -s - "${ROOT}/THIRD_PARTY_NOTICES.txt" || \
            fail "${tarball##*/} carries the generated notice"
        ! tar -tzf "${tarball}" | grep -qx 'package/LICENSE' || \
            fail "${tarball##*/} excludes the proprietary LICENSE"
    done

    tarball="${repo}/npm-tarballs/shukelabs-leg-linux-arm-${version}.tgz"
    cp "${tarball}" "${repo}/tarball-backup"
    mkdir "${repo}/repack"
    tar -xzf "${tarball}" -C "${repo}/repack"
    rm "${repo}/repack/package/THIRD_PARTY_NOTICES.txt"
    tar -C "${repo}/repack" -czf "${tarball}" package
    status=0
    release_npm_verify_tarballs "${repo}/npm-tarballs" >/dev/null 2>&1 || status="$?"
    assert_rc_nonzero "${status}"

    cp "${ROOT}/THIRD_PARTY_NOTICES.txt" "${repo}/repack/package/THIRD_PARTY_NOTICES.txt"
    cp "${ROOT}/LICENSE" "${repo}/repack/package/LICENSE"
    tar -C "${repo}/repack" -czf "${tarball}" package
    status=0
    release_npm_verify_tarballs "${repo}/npm-tarballs" >/dev/null 2>&1 || status="$?"
    assert_rc_nonzero "${status}"

    cp "${repo}/tarball-backup" "${tarball}"
    release_npm_verify_tarballs "${repo}/npm-tarballs"
)

# Writes cargo-metadata-shaped JSON for a root crate with one normal, one build,
# and one dev-only dependency. `alpha` takes the license expression under test.
make_notice_fixture() {
    local dir="${1}" alpha_license="${2}" name

    for name in alpha beta devonly; do
        mkdir -p "${dir}/crates/${name}"
        : >"${dir}/crates/${name}/Cargo.toml"
    done
    printf 'alpha MIT text\r\nsecond line\r\n' >"${dir}/crates/alpha/LICENSE-MIT"
    printf 'alpha readme\n' >"${dir}/crates/alpha/README.md"
    printf 'beta Apache text' >"${dir}/crates/beta/LICENSE"
    printf 'devonly MIT text\n' >"${dir}/crates/devonly/LICENSE"
    mkdir -p "${dir}/third-party/vendor"
    printf 'vendor license\n' >"${dir}/third-party/vendor/LICENSE"
    node - "${dir}" "${alpha_license}" >"${dir}/metadata.json" <<'NODE'
const path = require('node:path');
const [, , dir, alphaLicense] = process.argv;
const pkg = (name, license) => ({
  id: `${name}-id`, name, version: '1.0.0', license,
  source: 'registry+https://github.com/rust-lang/crates.io-index',
  manifest_path: path.join(dir, 'crates', name, 'Cargo.toml'),
});
const dep = (name, kind) => ({ pkg: `${name}-id`, dep_kinds: [{ kind, target: null }] });
console.log(JSON.stringify({
  packages: [
    { ...pkg('root', null), source: null, manifest_path: path.join(dir, 'Cargo.toml') },
    pkg('alpha', alphaLicense === 'null' ? null : alphaLicense),
    pkg('beta', 'Apache-2.0'),
    pkg('devonly', 'MIT'),
  ],
  resolve: {
    root: 'root-id',
    nodes: [
      { id: 'root-id', deps: [dep('alpha', null), dep('beta', 'build'), dep('devonly', 'dev')] },
      { id: 'alpha-id', deps: [] },
      { id: 'beta-id', deps: [] },
      { id: 'devonly-id', deps: [] },
    ],
  },
}));
NODE
}

# Prints each expected fragment that is missing from the file; empty on success.
missing_fragments() {
    node - "$@" <<'NODE'
const fs = require('node:fs');
const [, , filePath, ...fragments] = process.argv;
const text = fs.readFileSync(filePath, 'utf8');
for (const fragment of fragments) if (!text.includes(fragment)) console.log(JSON.stringify(fragment));
NODE
}

test_third_party_notices_fixture_rendering() (
    set -euo pipefail
    local dir status license missing
    dir="$(mktemp -d)"
    trap 'rm -rf "${dir}"' EXIT

    make_notice_fixture "${dir}" 'MIT OR (Apache-2.0 AND ISC)'
    release_third_party_notices_render "${dir}/third-party" "${dir}/metadata.json" "${dir}/metadata.json" \
        >"${dir}/notice.txt"
    missing="$(missing_fragments "${dir}/notice.txt" \
        $'Crate: alpha 1.0.0\nLicense: MIT OR (Apache-2.0 AND ISC)\n' \
        $'--- alpha 1.0.0: LICENSE-MIT ---\nalpha MIT text\nsecond line\n' \
        $'Crate: beta 1.0.0\nLicense: Apache-2.0\n' \
        $'--- beta 1.0.0: LICENSE ---\nbeta Apache text\n' \
        $'Bundled material: THIRD_PARTY_LICENSES/vendor/LICENSE\n\nvendor license\n')" || \
        fail "fragment check ran"
    assert_eq "" "${missing}" \
        "normal and build dependency license texts and bundled material are rendered"
    assert_eq "1" "$(grep -c '^Crate: alpha ' "${dir}/notice.txt")" "duplicate metadata inputs are unioned"
    ! grep -Eq 'devonly|alpha readme|root|'$'\r' "${dir}/notice.txt" || \
        fail "dev-only dependency, non-license files, root crate, and CR bytes are excluded"

    for license in null 'GPL-3.0-only' 'MIT OR GPL-3.0-only' 'MIT WITH LLVM-exception' \
        'MIT OR' '(MIT AND)' 'AND MIT' '(MIT OR Apache-2.0' 'MIT)' 'MIT Apache-2.0' '()'; do
        make_notice_fixture "${dir}" "${license}"
        status=0
        release_third_party_notices_render "${dir}/third-party" "${dir}/metadata.json" \
            >/dev/null 2>&1 || status="$?"
        assert_rc_nonzero "${status}" || fail "license '${license}' is rejected"
    done

    make_notice_fixture "${dir}" 'MIT'
    rm "${dir}/crates/alpha/LICENSE-MIT"
    status=0
    release_third_party_notices_render "${dir}/third-party" "${dir}/metadata.json" \
        >/dev/null 2>&1 || status="$?"
    assert_rc_nonzero "${status}" || fail "a crate without a license file is rejected"
)

test_third_party_notices_cover_release_graph() (
    set -euo pipefail
    local dir target status missing manifest
    local -a fragments=()
    dir="$(mktemp -d)"
    trap 'rm -rf "${dir}"' EXIT

    release_third_party_notices_generate "${ROOT}" >"${dir}/first.txt"
    release_third_party_notices_generate "${ROOT}" >"${dir}/second.txt"
    cmp -s "${dir}/first.txt" "${dir}/second.txt" || fail "notice generation is reproducible"
    release_third_party_notices_check "${ROOT}"

    # Cross-check the crate set with cargo tree rather than the generator's
    # own cargo metadata walk. Coverage is one-way: cargo metadata's resolve
    # also keeps optional deps named only by weak `dep?/feature` entries
    # (zlib-rs via flate2 today), so the notice may list a few crates that are
    # never compiled.
    while IFS='|' read -r _key target _os _cpu _archive _binary; do
        for manifest in Cargo.toml companions/leg-ui-client/Cargo.toml companions/leg-tui/Cargo.toml companions/leg-web/Cargo.toml; do
            cargo tree --locked --manifest-path "${ROOT}/${manifest}" -e normal,build \
                --target "${target}" --prefix none --format '{p}'
        done
    done < <(release_npm_platform_rows) | awk '$1 !~ /^(leg|leg-ui-client|leg-tui|leg-web)$/ { sub(/^v/, "", $2); print $1, $2 }' | \
        sort -u >"${dir}/expected"
    grep '^Crate: ' "${ROOT}/THIRD_PARTY_NOTICES.txt" | sed 's/^Crate: //' | sort -u >"${dir}/actual"
    [[ -s "${dir}/expected" ]] || fail "cargo tree reported release dependencies"
    assert_eq "" "$(comm -23 "${dir}/expected" "${dir}/actual")" \
        "notice covers every resolved runtime dependency"

    cargo metadata --locked --format-version 1 --manifest-path "${ROOT}/Cargo.toml" >"${dir}/metadata.json"
    cargo metadata --locked --format-version 1 --manifest-path "${ROOT}/companions/Cargo.toml" >"${dir}/companions.json"
    node - "${dir}/metadata.json" "${ROOT}/THIRD_PARTY_NOTICES.txt" "${dir}/actual" \
        "${dir}/companions.json" \
        >"${dir}/unrendered" <<'NODE' || fail "dependency license text check ran"
const fs = require('node:fs');
const path = require('node:path');
const [, , metadataPath, noticePath, cratesPath, companionsPath] = process.argv;
const notice = fs.readFileSync(noticePath, 'utf8');
const packages = [metadataPath, companionsPath].flatMap(file => JSON.parse(fs.readFileSync(file, 'utf8')).packages);
let checked = 0;
for (const line of fs.readFileSync(cratesPath, 'utf8').trim().split('\n')) {
  const [name, version] = line.split(' ');
  const pkg = packages.find((p) => p.name === name && p.version === version);
  if (!pkg) throw new Error(`no cargo metadata package for ${line}`);
  const dir = path.dirname(pkg.manifest_path);
  for (const file of fs.readdirSync(dir)) {
    if (!/^(licen[cs]e|copying|notice|unlicense|copyright)/i.test(file)) continue;
    let text = fs.readFileSync(path.join(dir, file), 'utf8').replace(/\r\n?/g, '\n');
    if (!text.endsWith('\n')) text += '\n';
    checked++;
    if (!notice.includes(`--- ${name} ${version}: ${file} ---\n${text}`)) console.log(`${line}: ${file}`);
  }
}
if (checked === 0) throw new Error('no dependency license files were checked');
NODE
    assert_eq "" "$(cat "${dir}/unrendered")" "every dependency license file is rendered verbatim"

    while IFS= read -r file; do
        fragments+=("Bundled material: ${file#"${ROOT}/"}"$'

'"$(cat "${file}")"$'
')
    done < <(find "${ROOT}/THIRD_PARTY_LICENSES" -type f | sort)
    (( ${#fragments[@]} > 0 )) || fail "THIRD_PARTY_LICENSES has material"
    missing="$(missing_fragments "${ROOT}/THIRD_PARTY_NOTICES.txt" "${fragments[@]}")" || \
        fail "fragment check ran"
    assert_eq "" "${missing}" "notice embeds all THIRD_PARTY_LICENSES material"
    missing="$(missing_fragments "${ROOT}/THIRD_PARTY_NOTICES.txt" "$(cat "${ROOT}/LICENSE")")" || \
        fail "fragment check ran"
    [[ -n "${missing}" ]] || fail "notice excludes the proprietary repository LICENSE"

    cp "${ROOT}/THIRD_PARTY_NOTICES.txt" "${dir}/stale.txt"
    printf 'stale\n' >>"${dir}/stale.txt"
    status=0
    release_third_party_notices_check "${ROOT}" "${dir}/stale.txt" >/dev/null 2>&1 || status="$?"
    assert_rc_nonzero "${status}" || fail "stale notice is rejected"
)

test_web_trial_notices_cover_companion_graph() (
    set -euo pipefail
    local dir target manifest
    dir="$(mktemp -d)"
    trap 'rm -rf "${dir}"' EXIT
    target="$(rustc -vV | sed -n 's/^host: //p')"

    release_web_trial_notices_generate "${target}" >"${dir}/first.txt"
    release_web_trial_notices_generate "${target}" >"${dir}/second.txt"
    cmp -s "${dir}/first.txt" "${dir}/second.txt" || fail "Web notice generation is reproducible"
    grep -q '^THIRD-PARTY NOTICES FOR leg Web trial bundle$' "${dir}/first.txt" || \
        fail "Web notice identifies its bundle"
    grep -q '^Crate: axum ' "${dir}/first.txt" || fail "Web notice includes the HTTP host dependency"

    for manifest in Cargo.toml companions/leg-ui-client/Cargo.toml companions/leg-web/Cargo.toml; do
        cargo tree --locked --manifest-path "${ROOT}/${manifest}" -e normal,build \
            --target "${target}" --prefix none --format '{p}'
    done | awk '$1 !~ /^(leg|leg-web|leg-ui-client)$/ { sub(/^v/, "", $2); print $1, $2 }' \
        | sort -u >"${dir}/expected"
    grep '^Crate: ' "${dir}/first.txt" | sed 's/^Crate: //' | sort -u >"${dir}/actual"
    assert_eq "" "$(comm -23 "${dir}/expected" "${dir}/actual")" \
        "Web notice covers core, host, and supervisor dependency graphs"

    release_tui_trial_notices_generate "${target}" >"${dir}/tui.txt"
    grep -q '^THIRD-PARTY NOTICES FOR leg TUI trial bundle$' "${dir}/tui.txt" || \
        fail "TUI reference notice identifies its bundle"
    grep -q '^Crate: crossterm ' "${dir}/tui.txt" || \
        fail "TUI reference notice includes terminal dependencies"
)

tests=(
    test_manifest_and_lockfile_version_reads
    test_verify_tag_matches_manifest
    test_release_bump_rules
    test_first_release_uses_current_manifest_version
    test_first_release_commits_generated_changelog_after_tag
    test_feature_release_updates_manifest_and_lockfile
    test_patch_release_updates_manifest_and_lockfile
    test_changelog_groups_tags_and_filters_skip_ci_commits
    test_release_notes_cover_only_requested_tag
    test_changelog_writes_idempotently_and_preserves_existing_file_on_error
    test_changelog_without_release_tags
    test_npm_platform_matrix_and_staging
    test_npm_staging_rejects_incomplete_archives
    test_npm_pack_checksums
    test_third_party_notices_fixture_rendering
    test_third_party_notices_cover_release_graph
    test_web_trial_notices_cover_companion_graph
)

for test_name in "${tests[@]}"; do
    "${test_name}"
    printf 'ok - %s\n' "${test_name}"
done
printf 'release tests: %s passed\n' "${#tests[@]}"
