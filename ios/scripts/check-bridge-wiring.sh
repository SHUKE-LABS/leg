#!/usr/bin/env bash
#
# Check that the capability bridge is wired into the LegHost build: every
# source and test file is in the build phase of its target, and the bundled
# Skill is in the built app.
#
# Xcode runs it as the last build phase of the LegHost target. Without
# TARGET_BUILD_DIR it checks the project only.

set -euo pipefail

SRCROOT="${SRCROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
PROJECT="${SRCROOT}/LegHost.xcodeproj/project.pbxproj"

# The files of one build phase, by the id of the phase.
phase_files() {
    awk -v id="$1" '
        index($0, id) == 3 && /= \{/ { inside = 1 }
        inside && /in (Sources|Resources) \*\// { print }
        inside && /^\t\t\};/ { exit }
    ' "${PROJECT}"
}

failed=0
expect() {
    local phase="$1" target="$2" file="$3" kind="$4"
    if ! phase_files "${phase}" | grep -qF "/* ${file} in ${kind} */"; then
        echo "error: ${file} is not in the ${kind} of ${target}" >&2
        failed=1
    fi
}

for file in BridgeCanonicalJSON.swift BridgeListener.swift PhotosAdapter.swift FilesAdapter.swift GuestSkillInstaller.swift; do
    expect AA11AA11AA11AA11AA110032 LegHost "${file}" Sources
done
expect AA11AA11AA11AA11AA110034 LegHost skills Resources
for file in BridgeCanonicalJSONTests.swift BridgeListenerTests.swift PhotosAdapterTests.swift FilesAdapterTests.swift GuestSkillInstallerTests.swift; do
    expect EE11EE11EE11EE11EE110041 LegHostTests "${file}" Sources
done
for file in BridgeCapabilityUITests.swift SkillRelaunchUITests.swift; do
    expect CC11CC11CC11CC11CC110042 LegHostUITests "${file}" Sources
done

if ! grep -q '#include "fs/IshBridgeSocketHook.h"' "${SRCROOT}/LegHost/LegHost-Bridging-Header.h"; then
    echo "error: the bridging header does not expose the bridge hook" >&2
    failed=1
fi

if [[ -n "${TARGET_BUILD_DIR:-}" && -n "${UNLOCALIZED_RESOURCES_FOLDER_PATH:-}" ]]; then
    source="${SRCROOT}/resources/skills/mobile-media-files"
    bundled="${TARGET_BUILD_DIR}/${UNLOCALIZED_RESOURCES_FOLDER_PATH}/skills/mobile-media-files"
    while IFS= read -r file; do
        if ! cmp -s "${source}/${file}" "${bundled}/${file}"; then
            echo "error: the app bundle does not carry skills/mobile-media-files/${file}" >&2
            failed=1
        fi
    done < <(cd "${source}" && find . -type f | sed 's|^\./||')
fi

exit "${failed}"
