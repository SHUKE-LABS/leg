#!/usr/bin/env bash
#
# Run the lifecycle and the preflight tests of the iOS host.
#
# The tests drive the state machine of the session and the checks the host
# makes before it touches the guest root. They need a simulator, but no
# guest: the host they drive is a fake one the test controls.
#
# Usage: ios/tests/run-lifecycle-tests.sh [--udid <simulator udid>]

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PROJECT="${ROOT}/ios/LegHost.xcodeproj"
SCHEME="LegHost"
DERIVED_DATA="${ROOT}/build/dd"
TEST_CLASS="LegHostTests"

export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"

udid=""
while [[ $# -gt 0 ]]; do
    case "$1" in
    --udid)
        udid="${2:-}"
        shift 2
        ;;
    *)
        echo "error: unknown argument ${1}" >&2
        exit 2
        ;;
    esac
done

if [[ -z "${udid}" ]]; then
    udid="$(xcrun simctl list devices booted |
        sed -n 's/.*(\([0-9A-Fa-f-]\{36\}\)) (Booted).*/\1/p' | head -n 1)"
fi
if [[ -z "${udid}" ]]; then
    echo "error: no booted simulator. Boot one or pass --udid." >&2
    exit 1
fi
echo "simulator: ${udid}"

xcodebuild test \
    -project "${PROJECT}" \
    -scheme "${SCHEME}" \
    -sdk iphonesimulator \
    -destination "platform=iOS Simulator,id=${udid}" \
    -derivedDataPath "${DERIVED_DATA}" \
    -only-testing:"${TEST_CLASS}"

echo "lifecycle tests passed"
