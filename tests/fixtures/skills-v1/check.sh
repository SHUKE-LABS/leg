#!/usr/bin/env bash
set -euo pipefail

fixture_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
manifest="$fixture_root/expected-outcomes.txt"

fail() {
    printf 'skills-v1 fixture check: %s\n' "$1" >&2
    exit 1
}

require_file() {
    [[ -f "$1" ]] || fail "missing file: ${1#"$fixture_root/"}"
}

require_marker() {
    local marker=$1
    local file=$2
    grep -Fqx -- "$marker" "$file" || fail "missing marker '$marker' in ${file#"$fixture_root/"}"
}

require_contains() {
    local marker=$1
    local file=$2
    grep -Fq -- "$marker" "$file" || fail "missing text '$marker' in ${file#"$fixture_root/"}"
}

require_file "$manifest"
require_file "$fixture_root/README.md"

valid="$fixture_root/valid/fixture-valid"
malformed="$fixture_root/malformed/fixture-malformed"
duplicate_project="$fixture_root/duplicate/project/.agents/skills/fixture-duplicate"
duplicate_user="$fixture_root/duplicate/user/.agents/skills/fixture-duplicate"

for package in "$valid" "$malformed" "$duplicate_project" "$duplicate_user"; do
    require_file "$package/SKILL.md"
done

require_file "$valid/references/guide.md"
require_file "$valid/scripts/emit-fixture.sh"
[[ -x "$valid/scripts/emit-fixture.sh" ]] || fail "fixture script is not executable"

require_marker 'name: fixture-valid' "$valid/SKILL.md"
require_marker 'description: Valid fixture for Skills v1 discovery and activation.' "$valid/SKILL.md"
require_marker 'x-fixture-unknown: accepted-and-ignored' "$valid/SKILL.md"
require_marker 'VALID_FIXTURE_REFERENCE' "$valid/references/guide.md"
require_contains 'VALID_FIXTURE_SCRIPT' "$valid/scripts/emit-fixture.sh"

require_marker 'name: fixture-malformed' "$malformed/SKILL.md"
require_marker 'description: [unterminated' "$malformed/SKILL.md"

require_marker 'name: fixture-duplicate' "$duplicate_project/SKILL.md"
require_marker 'metadata:' "$duplicate_project/SKILL.md"
require_contains 'winner: project' "$duplicate_project/SKILL.md"
require_contains 'winner: user' "$duplicate_user/SKILL.md"

for marker in \
    'valid.catalog=fixture-valid' \
    'valid.activation=body-only' \
    'valid.resource=references/guide.md-on-demand' \
    'valid.script=not-auto-run' \
    'malformed.catalog=empty' \
    'malformed.warning=invalid-package' \
    'malformed.activation=unavailable' \
    'duplicate.catalog=fixture-duplicate' \
    'duplicate.winner=duplicate/project/.agents/skills/fixture-duplicate' \
    'duplicate.loser-warning=duplicate/user/.agents/skills/fixture-duplicate'; do
    require_marker "$marker" "$manifest"
done

printf '%s\n' 'skills-v1 fixture check: ok'
