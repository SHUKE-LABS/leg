#!/usr/bin/env bash
#
# Run the fixture tests of the iOS host, one test per simulator boot.
#
# The accessibility server of a simulator wedges when a long run of the
# fixture tests drives the app again and again, and the test that follows
# the wedge fails with an accessibility timeout instead of an assertion.
# Every test therefore gets a simulator of its own.
#
# The tests run from the build of `xcodebuild build-for-testing` in build/dd.
# `--class` runs the named tests of another UI test class the same way, such
# as LegHostUITests/BridgeCapabilityUITests; it needs the test ids.
#
# Usage: ios/tests/run-fixture-tests.sh [--udid <simulator udid>]
#            [--class <target/class> test id ...] [test id ...]

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PROJECT="${ROOT}/ios/LegHost.xcodeproj"
SCHEME="LegHost"
DERIVED_DATA="${ROOT}/build/dd"
TEST_CLASS="LegHostUITests/LegHostFixtureUITests"

export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"

ALL_TESTS=(
    testTheSuccessScenarioCompletesOneTurn
    testTheProviderErrorScenarioReportsAServerError
    testTheUnreachableScenarioReportsATransportError
    testTheRuntimeFailureScenarioReportsTheRuntimeDiagnostic
    testAMissingConfigurationIsReportedBeforeTheGuestIsProvisioned
    testTheProviderSetupSavesAndStartsTheFixture
    testTheProviderSetupRejectsInvalidValuesBeforeProvisioning
    testProviderSettingsReplacementIsReadAfterRelaunch
    testProviderSettingsCancelKeepsTheSavedConfiguration
    testClearingProviderSettingsBlocksTheNextLaunch
    testTheOpenAIProvidersAreRefusedBeforeTheGuestIsProvisioned
    testTheRelaunchKeepsTheWorkspaceTheTrailAndTheRequests
    testTheBackgroundInterruptsTheTurnAndKeepsTheTrail
    testTheRetryAfterAnInterruptionKeepsTheStateOfTheTurn
    testTheStoppedGuestCannotWriteAfterTheRetry
    testTheStoppedGuestCannotWriteAfterManyRetries
    testTheDeviceFaultsAreReportedBeforeTheGuestIsProvisioned
)

udid=""
tests=()
custom_class=0
while [[ $# -gt 0 ]]; do
    case "$1" in
    --udid)
        udid="${2:-}"
        shift 2
        ;;
    --class)
        TEST_CLASS="${2:-}"
        custom_class=1
        shift 2
        ;;
    *)
        tests+=("$1")
        shift
        ;;
    esac
done
if [[ ${#tests[@]} -eq 0 ]]; then
    if [[ "${custom_class}" -eq 1 ]]; then
        echo "error: --class needs the test ids to run" >&2
        exit 2
    fi
    tests=("${ALL_TESTS[@]}")
fi
if [[ -z "${udid}" ]]; then
    udid="$(xcrun simctl list devices booted |
        sed -n 's/.*(\([0-9A-Fa-f-]\{36\}\)) (Booted).*/\1/p' | head -n 1)"
fi
if [[ -z "${udid}" ]]; then
    echo "error: no booted simulator. Boot one or pass --udid." >&2
    exit 1
fi
echo "simulator: ${udid}"

status=0
for test in "${tests[@]}"; do
    echo "=== ${test}"
    xcrun simctl shutdown "${udid}" >/dev/null 2>&1 || true
    sleep 5
    xcrun simctl boot "${udid}" >/dev/null 2>&1
    xcrun simctl bootstatus "${udid}" -b >/dev/null 2>&1
    xcrun simctl terminate "${udid}" com.shukelabs.leg.ioshost >/dev/null 2>&1 || true
    sleep 2
    if ! xcodebuild test-without-building \
        -project "${PROJECT}" \
        -scheme "${SCHEME}" \
        -sdk iphonesimulator \
        -destination "platform=iOS Simulator,id=${udid}" \
        -derivedDataPath "${DERIVED_DATA}" \
        -only-testing:"${TEST_CLASS}/${test}"; then
        echo "error: ${test} failed" >&2
        status=1
    fi
done

if [[ "${status}" -ne 0 ]]; then
    echo "fixture tests failed" >&2
    exit 1
fi
echo "fixture tests passed"
